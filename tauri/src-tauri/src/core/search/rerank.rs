//! 精排层：本地 Cross-Encoder（bge-reranker）重排序。
//!
//! # 设计（Broker 模型，参考 `embedding.rs` 的平台适配）
//! - Windows / macOS Apple Silicon：ONNX Runtime，**优先 GPU**（DirectML / CoreML），
//!   GPU 执行提供者初始化失败时自动回退 CPU 重建 Session
//! - Intel Mac / Linux：tract-onnx 纯 Rust CPU 推理
//! - 模型来源：`Xenova/bge-reranker-base`（XLM-RoBERTa 架构，ONNX 导出版），
//!   通过 `model_download::ensure_reranker_downloaded` 多源下载
//!
//! # 降级策略
//! 模型缺失 / 推理失败时返回 `Err`，调用方回退到 RRF 排序（检索永不阻断）。

use std::path::Path;
use std::sync::{Mutex, OnceLock};

#[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
use std::sync::Arc;

use ndarray::Array2;

use crate::core::db::lance::SearchHit;
use crate::core::model_download::reranker_cache_dir;

// ─── 按平台选择后端（与 embedding.rs 一致）───

#[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
type SessionType = ort::session::Session;

#[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
type SessionType = std::sync::Arc<tract_onnx::prelude::RunnableModel<
    tract_onnx::prelude::TypedFact,
    Box<dyn tract_onnx::prelude::TypedOp>,
>>;

/// 精排器抽象（依赖倒置：检索管线只依赖 trait）。
pub trait Reranker: Send + Sync {
    /// 对候选集重排序：以 query 为基准，返回按相关性降序且过滤低分后的结果。
    ///
    /// - `min_score`：sigmoid 相关性分数阈值，低于阈值的结果被丢弃（绝对值语义）
    /// - 失败返回 `Err`（模型缺失/推理失败），调用方应回退 RRF 排序
    fn rerank(
        &self,
        query: &str,
        candidates: &[SearchHit],
        min_score: f32,
    ) -> Result<Vec<SearchHit>, String>;
}

// ─── 全局缓存（Session 单例，与 embedding.rs 模式一致）───

/// 首次初始化时缓存 tokenizer.json 原始字节
static TOKENIZER_JSON: OnceLock<Vec<u8>> = OnceLock::new();
/// 模型文件目录（供 ensure_initialized 幂等检查）
static MODEL_DIR: OnceLock<String> = OnceLock::new();
/// 全局 Session（Mutex 封装以支持 &mut self 的 run 调用）
static GLOBAL_SESSION: OnceLock<Mutex<SessionType>> = OnceLock::new();
/// 模型最大序列长度（从 config.json 读取）
static MAX_SEQ_LEN: OnceLock<usize> = OnceLock::new();
/// pad_token_id（从 config.json 读取）
static PAD_TOKEN_ID: OnceLock<i64> = OnceLock::new();

/// 每个推理批次的 (query, passage) 对数
const BATCH_SIZE: usize = 16;

fn get_max_seq_len() -> usize {
    *MAX_SEQ_LEN.get().unwrap_or(&512)
}

fn get_pad_token_id() -> i64 {
    *PAD_TOKEN_ID.get().unwrap_or(&1)
}

/// 线程级 Tokenizer 缓存
fn with_tokenizer<F, R>(f: F) -> R
where
    F: FnOnce(&tokenizers::Tokenizer) -> R,
{
    thread_local! {
        static TOKENIZER: OnceLock<tokenizers::Tokenizer> = OnceLock::new();
    }
    TOKENIZER.with(|cache| {
        let tok = cache.get_or_init(|| {
            tokenizers::Tokenizer::from_bytes(
                TOKENIZER_JSON.get().expect("reranker 尚未初始化，请先调用 ensure_initialized"),
            )
            .expect("tokenizer.json 解析失败，请检查模型文件完整性")
        });
        f(tok)
    })
}

// ─── ONNX Session 创建（Broker：GPU 优先 → CPU 回退）───

/// 创建原生 ONNX Runtime Session（Windows / macOS Apple Silicon）。
///
/// GPU 执行提供者（DirectML / CoreML）初始化失败时**回退 CPU** 重建：
/// 无 GPU 环境（虚拟机、无驱动、远程桌面等）下检索功能仍可用，仅精排速度下降。
#[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
fn create_session(model_path: &Path) -> Result<SessionType, String> {
    let mut builder = ort::session::Session::builder()
        .map_err(|e| format!("创建 ONNX Runtime 配置失败: {}", e))?;

    #[cfg(target_os = "windows")]
    {
        builder = builder
            .with_execution_providers([ort::ep::DirectML::default().build()])
            .map_err(|e| format!("设置 DirectML GPU 执行提供者失败: {}", e))?;
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        builder = builder
            .with_execution_providers([ort::ep::CoreML::default().build()])
            .map_err(|e| format!("设置 CoreML GPU 执行提供者失败: {}", e))?;
    }

    match builder.commit_from_file(model_path) {
        Ok(session) => {
            log::info!("[reranker] 启用 GPU 推理（DirectML/CoreML），session 创建成功");
            Ok(session)
        }
        Err(gpu_err) => {
            log::warn!("[reranker] GPU 执行提供者初始化失败，回退 CPU: {}", gpu_err);
            ort::session::Session::builder()
                .map_err(|e| format!("创建 ONNX Runtime 配置失败（CPU 回退）: {}", e))?
                .commit_from_file(model_path)
                .map_err(|e| format!("加载 ONNX 模型失败（CPU 回退后仍失败）: {}", e))
        }
    }
}

/// 创建 tract-onnx 推理 Session（Intel Mac / Linux 纯 CPU 后端）。
#[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
fn create_session(model_path: &Path) -> Result<SessionType, String> {
    use tract_onnx::prelude::*;

    let model = tract_onnx::onnx()
        .model_for_path(model_path)
        .map_err(|e| format!("加载 ONNX 模型失败: {}", e))?
        .into_optimized()
        .map_err(|e| format!("模型优化失败: {}", e))?
        .into_runnable()
        .map_err(|e| format!("模型编译失败: {}", e))?;

    log::info!("[reranker] tract-onnx session 创建成功, 模型路径: {}", model_path.display());
    Ok(model)
}

// ─── 按平台运行批量推理（与 embedding.rs 一致）───

/// 对一批填充后的张量执行原生 ONNX Runtime 推理（Windows / macOS Apple Silicon）。
#[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
fn run_batch(
    session: &mut SessionType,
    input_ids: Array2<i64>,
    attention_mask: Array2<i64>,
) -> Result<ndarray::ArrayD<f32>, String> {
    let input_tensor = ort::value::Tensor::<i64>::from_array(input_ids)
        .map_err(|e| format!("创建 input_ids 张量失败: {}", e))?;
    let mask_tensor = ort::value::Tensor::<i64>::from_array(attention_mask)
        .map_err(|e| format!("创建 attention_mask 张量失败: {}", e))?;

    let outputs = session
        .run([input_tensor.into(), mask_tensor.into()])
        .map_err(|e| format!("ONNX Runtime 推理失败: {}", e))?;

    if outputs.len() == 0 {
        return Err("ONNX Runtime 推理返回空输出".to_string());
    }

    outputs[0]
        .try_extract_array::<f32>()
        .map_err(|e| format!("解析输出张量失败: {}", e))
        .map(|a| a.to_owned())
}

/// 对一批填充后的张量执行 tract-onnx 推理（Intel Mac / Linux 纯 CPU）。
#[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
fn run_batch(
    session: &SessionType,
    input_ids: Array2<i64>,
    attention_mask: Array2<i64>,
) -> Result<ndarray::ArrayD<f32>, String> {
    use tract_onnx::prelude::*;

    let input_tensor: Tensor = input_ids.into_dyn().into();
    let mask_tensor: Tensor = attention_mask.into_dyn().into();

    let outputs = session
        .run(tvec!(input_tensor.into(), mask_tensor.into()))
        .map_err(|e| format!("tract-onnx 推理失败: {}", e))?;

    if outputs.is_empty() {
        return Err("tract-onnx 推理返回空输出".to_string());
    }

    outputs[0]
        .to_plain_array_view::<f32>()
        .map_err(|e| format!("解析输出张量失败: {}", e))
        .map(|a| a.to_owned().into_dyn())
}

// ─── 初始化 ───

/// 初始化全局缓存：检查模型文件完整性，缓存 tokenizer.json 字节，创建 Session。
/// 幂等安全，重复调用无副作用（双检锁，与 embedding::ensure_initialized 一致）。
pub(crate) fn ensure_initialized(models_dir: &Path) -> Result<(), String> {
    // 快速路径（无锁）：已初始化直接返回
    if GLOBAL_SESSION.get().is_some() && MODEL_DIR.get().is_some() {
        return Ok(());
    }

    // 慢路径（加锁）：并发首次调用时仅首个线程完整初始化
    static INIT_LOCK: Mutex<()> = Mutex::new(());
    let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    if GLOBAL_SESSION.get().is_some() && MODEL_DIR.get().is_some() {
        return Ok(());
    }

    for (name, p) in [
        ("model.onnx", models_dir.join("model.onnx")),
        ("tokenizer.json", models_dir.join("tokenizer.json")),
        ("config.json", models_dir.join("config.json")),
    ] {
        if !p.exists() {
            return Err(format!("reranker 模型文件缺失: {} ({})", name, p.display()));
        }
    }

    let tokenizer_raw = std::fs::read(models_dir.join("tokenizer.json"))
        .map_err(|e| format!("读取 tokenizer.json 失败: {}", e))?;

    let config_raw = std::fs::read_to_string(models_dir.join("config.json"))
        .map_err(|e| format!("读取 config.json 失败: {}", e))?;
    let config: serde_json::Value =
        serde_json::from_str(&config_raw).map_err(|e| format!("解析 config.json 失败: {}", e))?;
    let ms = config["max_position_embeddings"].as_u64().unwrap_or(512) as usize;
    let _ = MAX_SEQ_LEN.set(ms);
    let pt = config["pad_token_id"].as_i64().unwrap_or(1);
    let _ = PAD_TOKEN_ID.set(pt);

    let session = create_session(&models_dir.join("model.onnx"))?;

    let _ = TOKENIZER_JSON.set(tokenizer_raw);
    let _ = MODEL_DIR.set(models_dir.to_string_lossy().to_string());
    let _ = GLOBAL_SESSION.set(Mutex::new(session));

    log::info!(
        "[reranker] 初始化完成, 模型目录: {}, max_position_embeddings: {}, pad_token_id: {}",
        models_dir.display(),
        ms,
        pt
    );
    Ok(())
}

// ─── 本地 BGE Reranker 实现 ───

/// 本地 bge-reranker-base Cross-Encoder 精排器。
///
/// 无状态结构体：Session 为全局单例（`GLOBAL_SESSION`），实例可廉价克隆/共享。
pub struct LocalBgeReranker;

impl Reranker for LocalBgeReranker {
    fn rerank(
        &self,
        query: &str,
        candidates: &[SearchHit],
        min_score: f32,
    ) -> Result<Vec<SearchHit>, String> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let models_dir = reranker_cache_dir();
        ensure_initialized(&models_dir)?;

        let session_mutex = GLOBAL_SESSION
            .get()
            .ok_or_else(|| "reranker 未初始化".to_string())?;

        // ── 1. 并行分词：query + passage 拼接为 pair（cross-encoder 输入）──
        // passage 前缀拼接 doc_name：文件名是文档主题强信号（feature 化，替代旧的手工加分）
        let pairs: Vec<(usize, String, String)> = candidates
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let passage = if h.doc_name.is_empty() {
                    h.text.clone()
                } else {
                    format!("{}\n{}", h.doc_name, h.text)
                };
                (i, query.to_string(), passage)
            })
            .collect();

        let mut items: Vec<(usize, Vec<i64>, Vec<i64>)> = Vec::with_capacity(pairs.len());
        for (idx, first, second) in &pairs {
            let (ids, mask) = with_tokenizer(|tok| -> Result<(Vec<i64>, Vec<i64>), String> {
                let enc = tok
                    .encode((first.as_str(), second.as_str()), true)
                    .map_err(|e| format!("分词失败: {}", e))?;
                let ids: Vec<i64> = enc.get_ids().iter().map(|&id| id as i64).collect();
                let mask: Vec<i64> = enc
                    .get_attention_mask()
                    .iter()
                    .map(|&m| m as i64)
                    .collect();
                let truncated_len = ids.len().min(get_max_seq_len());
                Ok((ids[..truncated_len].to_vec(), mask[..truncated_len].to_vec()))
            })?;
            items.push((*idx, ids, mask));
        }

        // ── 2. 分组（长度相近的 pair 一组，降低 padding 浪费）──
        items.sort_unstable_by(|a, b| b.1.len().cmp(&a.1.len()));
        let groups: Vec<&[(usize, Vec<i64>, Vec<i64>)]> = items.chunks(BATCH_SIZE).collect();

        // ── 3. 获取 Session ──
        #[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
        let session = {
            let guard = session_mutex
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            Arc::clone(&guard)
        };

        #[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
        let mut session = session_mutex
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // ── 4. 批量推理 + sigmoid → 分数 ──
        let mut scores = vec![0.0f32; candidates.len()];
        for group in groups {
            let group_size = group.len();
            let max_len = group
                .iter()
                .map(|(_, ids, _)| ids.len())
                .max()
                .unwrap_or(1);
            let pad_id = get_pad_token_id();

            let mut ids_flat = Vec::with_capacity(group_size * max_len);
            let mut mask_flat = Vec::with_capacity(group_size * max_len);
            for (_, ids, mask) in group {
                for j in 0..max_len {
                    if j < ids.len() {
                        ids_flat.push(ids[j]);
                        mask_flat.push(mask[j]);
                    } else {
                        ids_flat.push(pad_id);
                        mask_flat.push(0i64);
                    }
                }
            }

            let input_ids = Array2::from_shape_vec((group_size, max_len), ids_flat)
                .map_err(|e| format!("构建 input_ids 失败: {}", e))?;
            let attention_mask = Array2::from_shape_vec((group_size, max_len), mask_flat)
                .map_err(|e| format!("构建 attention_mask 失败: {}", e))?;

            let logits = run_batch(&mut session, input_ids, attention_mask)?;

            // 输出形状 (batch, 1) 或 (batch,)：取每行首元素，sigmoid 映射到 (0,1)
            let shape = logits.shape();
            if shape.is_empty() || shape[0] != group_size {
                return Err(format!(
                    "[reranker] 意外的输出形状: {:?}, 期望首维 = {}",
                    shape, group_size
                ));
            }
            let is_2d = shape.len() == 2;
            for (gi, (orig_idx, _, _)) in group.iter().enumerate() {
                let raw = if is_2d {
                    logits[[gi, 0]]
                } else {
                    logits[gi]
                };
                // sigmoid：相关性概率
                let s = 1.0 / (1.0 + (-raw).exp());
                scores[*orig_idx] = s;
            }
        }

        // ── 5. 过滤低分 + 按精排分数降序 ──
        let mut reranked: Vec<(SearchHit, f32)> = candidates
            .iter()
            .cloned()
            .zip(scores.iter())
            .filter(|(_, s)| **s >= min_score)
            .map(|(mut h, s)| {
                h.score = *s;
                h.score_rerank = Some(*s);
                (h, *s)
            })
            .collect();
        reranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        log::info!(
            "[reranker] query='{}' candidates={} 通过阈值({})={}",
            query,
            candidates.len(),
            min_score,
            reranked.len()
        );

        Ok(reranked.into_iter().map(|(h, _)| h).collect())
    }
}
