use std::path::Path;
use std::sync::{Mutex, OnceLock};

#[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
use std::sync::Arc;

use ndarray::{Array2, Axis, s};
use rayon::prelude::*;

// ─── 按平台选择后端 ───
//
// Windows / macOS Apple Silicon 使用 ort crate，通过原生 ONNX Runtime + GPU 加速。
// Intel Mac / Linux 使用 tract-onnx 直接推理（纯 Rust，零原生依赖）。

#[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
type SessionType = ort::session::Session;

#[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
type SessionType = std::sync::Arc<tract_onnx::prelude::RunnableModel<
    tract_onnx::prelude::TypedFact,
    Box<dyn tract_onnx::prelude::TypedOp>,
>>;

/// 从 config.json 加载的实际 hidden_size，初始化为 512 作为安全默认值
static HIDDEN_SIZE: OnceLock<usize> = OnceLock::new();
/// 从 config.json 读取 hidden_size，未初始化时返回 512
fn get_hidden_size() -> usize {
    *HIDDEN_SIZE.get().unwrap_or(&512)
}
/// 从 config.json 加载的实际 max_position_embeddings，初始化为 512 作为安全默认值
static MAX_SEQ_LEN: OnceLock<usize> = OnceLock::new();
/// 从 config.json 读取 max_position_embeddings，未初始化时返回 512
fn get_max_seq_len() -> usize {
    *MAX_SEQ_LEN.get().unwrap_or(&512)
}
/// 输出的向量维度，等于模型的 hidden_size
pub fn get_embedding_dimension() -> usize {
    get_hidden_size()
}

/// 模型显示名称（从 MODEL_DIR 目录名提取，如 bge-small-zh-v1.5）
pub fn get_model_name() -> String {
    MODEL_DIR
        .get()
        .and_then(|d| std::path::Path::new(d).file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("BGE-Small-ZH v1.5")
        .to_string()
}

/// 从 config.json 加载的 pad_token_id，初始化为 0 作为安全默认值
static PAD_TOKEN_ID: OnceLock<i64> = OnceLock::new();
/// 从 config.json 读取 pad_token_id，未初始化时返回 0
fn get_pad_token_id() -> i64 {
    *PAD_TOKEN_ID.get().unwrap_or(&0)
}

/// 每个推理批次的文本数（长度相近的文本分在一组）
///
/// 20 → 128：BGE-Small 单条序列很短（一般 <128 token），ort 动态形状下
/// 单批 128 条对显存/内存压力极小（128 × max_seq_len × hidden_size × 4B，
/// max_seq_len=512 / hidden=384 ≈ 100MB），而批内 padding 浪费持平，吞吐提升显著。
/// 依赖该常量的批量 embedding（书签/文档增量）均受益。
const BATCH_SIZE: usize = 128;

// ─── 全局缓存 ───

/// 首次初始化时缓存 tokenizer.json 原始字节（每线程按需解析）
static TOKENIZER_JSON: OnceLock<Vec<u8>> = OnceLock::new();
/// 模型文件目录（用于编译时找 model.onnx）
static MODEL_DIR: OnceLock<String> = OnceLock::new();
/// 全局 Session（Mutex 封装以支持 &mut self 的 run 调用，线程安全）
///
/// ort 的 `run(&'s mut self)` 需要可变引用，tract 同理，因此通过 Mutex 序列化访问。
/// GPU 批处理本身提供足够的加速，Mutex 争用极小。
static GLOBAL_SESSION: OnceLock<Mutex<SessionType>> = OnceLock::new();

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
                TOKENIZER_JSON.get().expect("模型尚未初始化，请先调用 ensure_initialized"),
            )
            .expect("tokenizer.json 解析失败，请检查模型文件完整性")
        });
        f(tok)
    })
}

// ─── ONNX Session 创建（按平台两条独立实现路径）───

/// 创建 tract-onnx 推理 Session（Intel Mac / Linux 纯 CPU 后端）。
///
/// tract 支持动态形状（batch & seq_len 均为运行时变量），加载后优化并编译为可执行模型。
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

    log::info!("[ort_embedding] tract-onnx session 创建成功, 模型路径: {}", model_path.display());
    Ok(model)
}

/// 创建原生 ONNX Runtime Session（Windows / macOS Apple Silicon GPU 后端）。
#[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
fn create_session(model_path: &Path) -> Result<SessionType, String> {
    let mut builder = ort::session::Session::builder()
        .map_err(|e| format!("创建 ONNX Runtime 配置失败: {}", e))?;

    // Windows：DirectML GPU 加速
    #[cfg(target_os = "windows")]
    {
        builder = builder
            .with_execution_providers([ort::ep::DirectML::default().build()])
            .map_err(|e| format!("设置 DirectML GPU 执行提供者失败: {}", e))?;
        log::info!("[ort_embedding] 启用 windows embedding 模型驱动");
    }

    // macOS Apple Silicon（M 系列）：CoreML GPU 加速
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        builder = builder
            .with_execution_providers([ort::ep::CoreML::default().build()])
            .map_err(|e| format!("设置 CoreML GPU 执行提供者失败: {}", e))?;
        log::info!("[ort_embedding] 启用 macos embedding 模型驱动");
    }

    let session = builder
        .commit_from_file(model_path)
        .map_err(|e| format!("加载 ONNX 模型失败: {}", e))?;

    log::info!("[ort_embedding] embedding session 创建成功, 模型路径: {}", model_path.display());
    Ok(session)
}

// ─── 按平台运行批量推理 ───

/// 对一批填充后的张量执行原生 ONNX Runtime 推理（Windows / macOS Apple Silicon）。
#[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
fn run_batch(
    session: &mut SessionType,
    input_ids: Array2<i64>,
    attention_mask: Array2<i64>,
    token_type_ids: Array2<i64>,
) -> Result<ndarray::ArrayD<f32>, String> {
    let input_tensor = ort::value::Tensor::<i64>::from_array(input_ids)
        .map_err(|e| format!("创建 input_ids 张量失败: {}", e))?;
    let mask_tensor = ort::value::Tensor::<i64>::from_array(attention_mask)
        .map_err(|e| format!("创建 attention_mask 张量失败: {}", e))?;

    // 兼容部分 ONNX 导出（如 XLM-R 架构）不接收 token_type_ids，
    // 仅需要 input_ids + attention_mask 两个输入
    let outputs = if session.inputs().len() <= 2 {
        session
            .run([input_tensor.into(), mask_tensor.into()])
    } else {
        let type_ids_tensor = ort::value::Tensor::<i64>::from_array(token_type_ids)
            .map_err(|e| format!("创建 token_type_ids 张量失败: {}", e))?;
        session.run([
            input_tensor.into(),
            mask_tensor.into(),
            type_ids_tensor.into(),
        ])
    }
    .map_err(|e| format!("ONNX Runtime 推理失败: {}", e))?;

    if outputs.len() == 0 {
        return Err("ONNX Runtime 推理返回空输出".to_string());
    }

    let hidden = outputs[0]
        .try_extract_array::<f32>()
        .map_err(|e| format!("解析输出张量失败: {}", e))?;

    Ok(hidden.to_owned())
}

/// 对一批填充后的张量执行 tract-onnx 推理（Intel Mac / Linux 纯 CPU）。
#[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
fn run_batch(
    session: &SessionType,
    input_ids: Array2<i64>,
    attention_mask: Array2<i64>,
    token_type_ids: Array2<i64>,
) -> Result<ndarray::ArrayD<f32>, String> {
    use tract_onnx::prelude::*;

    // 使用 ndarray::Array 直接转换为 Tensor（通过 Into trait）
    let input_tensor: Tensor = input_ids.into_dyn().into();
    let mask_tensor: Tensor = attention_mask.into_dyn().into();
    let type_tensor: Tensor = token_type_ids.into_dyn().into();

    // 兼容 2 输入（无 token_type_ids）与 3 输入模型
    let outputs = if session.model().inputs.len() <= 2 {
        session.run(tvec!(
            input_tensor.into(),
            mask_tensor.into(),
        ))
    } else {
        session.run(tvec!(
            input_tensor.into(),
            mask_tensor.into(),
            type_tensor.into(),
        ))
    }
    .map_err(|e| format!("tract-onnx 推理失败: {}", e))?;

    if outputs.is_empty() {
        return Err("tract-onnx 推理返回空输出".to_string());
    }

    let hidden = outputs[0]
        .to_plain_array_view::<f32>()
        .map_err(|e| format!("解析输出张量失败: {}", e))?
        .to_owned()
        .into_dyn();

    Ok(hidden)
}

/// 初始化全局缓存：检查模型文件完整性，缓存 tokenizer.json 字节，创建 Session。
/// 幂等安全，重复调用无副作用。
///
/// 并发安全：多个线程（如多路并行检索）同时首次调用时，通过双检锁保证
/// 只执行一次完整初始化，其余调用等待后复用已缓存的 Session。
pub(crate) fn ensure_initialized(models_dir: &Path) -> Result<(), String> {
    // 快速路径（无锁）：已初始化直接返回，避免每次调用抢锁
    if GLOBAL_SESSION.get().is_some() && MODEL_DIR.get().is_some() {
        return Ok(());
    }

    // 慢路径（加锁）：并发首次调用时仅首个线程完整初始化
    static INIT_LOCK: Mutex<()> = Mutex::new(());
    let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // 双检锁：获取锁后可能已被其他线程初始化，直接复用
    if GLOBAL_SESSION.get().is_some() && MODEL_DIR.get().is_some() {
        return Ok(());
    }

    if let Some(cached) = MODEL_DIR.get() {
        if cached != &models_dir.to_string_lossy().as_ref() {
            log::warn!(
                "[ort_embedding] 忽略不同的模型目录请求: 已缓存 '{}', 请求 '{}'",
                cached,
                models_dir.display()
            );
        }
        return Ok(());
    }

    for (name, p) in [
        ("model.onnx", models_dir.join("model.onnx")),
        ("tokenizer.json", models_dir.join("tokenizer.json")),
        ("config.json", models_dir.join("config.json")),
        ("tokenizer_config.json", models_dir.join("tokenizer_config.json")),
        ("special_tokens_map.json", models_dir.join("special_tokens_map.json")),
    ] {
        if !p.exists() {
            return Err(format!("模型文件缺失: {} ({})", name, p.display()));
        }
    }

    let tokenizer_raw =
        std::fs::read(models_dir.join("tokenizer.json"))
            .map_err(|e| format!("读取 tokenizer.json 失败: {}", e))?;

    // 从 config.json 读取 hidden_size，以兼容不同版本的 BGE 模型
    let config_raw =
        std::fs::read_to_string(models_dir.join("config.json"))
            .map_err(|e| format!("读取 config.json 失败: {}", e))?;
    let config: serde_json::Value =
        serde_json::from_str(&config_raw)
            .map_err(|e| format!("解析 config.json 失败: {}", e))?;
    let hs = config["hidden_size"].as_u64().unwrap_or(512) as usize;
    let _ = HIDDEN_SIZE.set(hs);

    let ms = config["max_position_embeddings"].as_u64().unwrap_or(512) as usize;
    let _ = MAX_SEQ_LEN.set(ms);

    let pt = config["pad_token_id"].as_i64().unwrap_or(0);
    let _ = PAD_TOKEN_ID.set(pt);

    let model_path = models_dir.join("model.onnx");
    let session = create_session(&model_path)?;

    let model_dir_str = models_dir.to_string_lossy().to_string();

    // OnceLock::set 仅首次成功，后续静默失败，保证幂等
    let _ = TOKENIZER_JSON.set(tokenizer_raw);
    let _ = MODEL_DIR.set(model_dir_str.clone());
    let _ = GLOBAL_SESSION.set(Mutex::new(session));

    log::info!("[ort_embedding] 初始化完成, 模型目录: {}, hidden_size: {}, max_position_embeddings: {}, pad_token_id: {}", model_dir_str, hs, ms, pt);
    Ok(())
}

// ─── Mean Pooling + L2 Normalize ───

/// 对一组 ONNX 推理结果进行 mean pooling + L2 normalize。
///
/// 返回 `Result`，形状不匹配时返回 `Err`，避免静默写入零向量导致数据损坏。
fn post_process_batch(
    hidden: ndarray::ArrayViewD<'_, f32>,
    batch_size: usize,
    masks: &[Vec<i64>],
    valid_counts: &[usize],
) -> Result<Vec<Vec<f32>>, String> {
    let shape = hidden.shape();
    if shape.len() != 3 {
        return Err(format!(
            "[ort_embedding] 意外的输出形状: {:?}, 期望 3D", shape
        ));
    }
    let actual_batch = shape[0];
    let seq_len = shape[1];
    let actual_hidden = shape[2];
    let hidden_size = get_hidden_size();

    if actual_batch != batch_size || actual_hidden != hidden_size {
        return Err(format!(
            "[ort_embedding] 输出形状不匹配: {:?}, 期望 ({}, ?, {})",
            shape, batch_size, hidden_size
        ));
    }

    // hidden 是 3D 视图，reshape 为 (batch, seq_len, hidden_size)
    let hidden_3d = hidden
        .into_shape_with_order((batch_size, seq_len, hidden_size))
        .expect("hidden 形状应匹配 (batch_size, seq_len, hidden_size)");

    let mut all_embeddings = Vec::with_capacity(batch_size);

    for i in 0..batch_size {
        let valid = valid_counts[i];
        if valid == 0 {
            // 防御：整条文本未产生任何有效 token（如纯空白），
            // 显式报错而非写入全零向量，避免零向量入库污染检索结果
            return Err(format!(
                "[embedding] 第 {} 条文本未生成有效 token（可能为空白内容）",
                i + 1
            ));
        }

        let valid_len = masks[i].len();
        let sum = hidden_3d
            .slice(s![i, 0..valid_len, 0..get_embedding_dimension()])
            .sum_axis(Axis(0));

        let count_f = valid as f32;
        let mut embedding: Vec<f32> = sum.iter().map(|&v| v / count_f).collect();

        // L2 normalize
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut embedding {
                *v /= norm;
            }
        }

        all_embeddings.push(embedding);
    }

    Ok(all_embeddings)
}

// ─── 模型窗口对齐 ───

/// 推荐的分块字符数：中文文本最坏情况下 1 字符 ≈ 1 token，
/// 预留余量使 chunk 落在模型窗口内避免静默截断。范围 [256, 1024]。
pub fn recommended_chunk_size() -> usize {
    get_max_seq_len().saturating_sub(64).clamp(256, 1024)
}

/// 推荐的分块重叠字符数（约为分块大小的 1/8，范围 [16, 128]）。
pub fn recommended_chunk_overlap() -> usize {
    (recommended_chunk_size() / 8).clamp(16, 128)
}

// ─── 公开 API ───

/// 对一批文本生成向量（动态 padding + 长度分组批量推理）。
///
/// # 算法
/// 1. 分词所有文本 → 获取实际 token 长度
/// 2. 按长度降序排序 → 每 BATCH_SIZE 条分一组
/// 3. 各组依次推理（按平台使用原生 ONNX Runtime 或 tract-onnx）
/// 4. 恢复原始输入顺序返回
///
/// # 性能
/// - Windows / Apple Silicon 使用 GPU 加速（DirectML / CoreML）
/// - Intel Mac / Linux 使用 tract 纯 Rust CPU 推理
/// - 分组降低 padding 浪费
/// - 动态形状免除每批重编译的开销
///
/// progress 回调：`(已完成组数, 总组数, "状态消息")`
pub fn call_embedding_parallel(
    texts: &[&str],
    models_dir: &Path,
    progress: Option<&(dyn Fn(usize, usize, &str) + Send + Sync)>,
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }

    // ── 1. 初始化（文件检查 + tokenizer 缓存 + Session 创建）──
    ensure_initialized(models_dir)?;

    let session_mutex = GLOBAL_SESSION
        .get()
        .ok_or_else(|| "ONNX Runtime 未初始化，请先调用 ensure_initialized".to_string())?;

    // ── 2. 并行分词 ──
    let mut items: Vec<(usize, Vec<i64>, Vec<i64>, Vec<i64>, usize)> =
        Vec::with_capacity(texts.len());

    // 使用 rayon 并行分词，每个线程使用自己的 thread_local tokenizer
    let tokenized: Vec<Result<(usize, Vec<i64>, Vec<i64>, Vec<i64>, usize), String>> = texts
        .par_iter()
        .enumerate()
        .map(|(idx, text)| {
            let (ids, mask, type_ids) = with_tokenizer(|tok| -> Result<_, String> {
                let enc = tok
                    .encode(*text, true)
                    .map_err(|e| format!("分词失败: {}", e))?;
                let ids: Vec<i64> = enc.get_ids().iter().map(|&id| id as i64).collect();
                let mask: Vec<i64> = enc
                    .get_attention_mask()
                    .iter()
                    .map(|&m| m as i64)
                    .collect();
                let type_ids: Vec<i64> = enc
                    .get_type_ids()
                    .iter()
                    .map(|&id| id as i64)
                    .collect();

                // 截断至模型支持的最大长度
                let truncated_len = ids.len().min(get_max_seq_len());
                let ids = ids[..truncated_len].to_vec();
                let mask = mask[..truncated_len].to_vec();
                let type_ids = type_ids[..truncated_len].to_vec();
                Ok((ids, mask, type_ids))
            })?;

            let valid_count = mask.iter().filter(|&&m| m > 0).count();
            Ok((idx, ids, mask, type_ids, valid_count))
        })
        .collect();

    // 收集结果
    for r in tokenized {
        let (idx, ids, mask, type_ids, valid_count) = r?;
        items.push((idx, ids, mask, type_ids, valid_count));
    }

    // ── 3. 按长度降序排序 + 分组 ──
    items.sort_unstable_by(|a, b| b.1.len().cmp(&a.1.len()));
    let groups: Vec<&[(usize, Vec<i64>, Vec<i64>, Vec<i64>, usize)]> =
        items.chunks(BATCH_SIZE).collect();

    let total_groups = groups.len();

    // 分词完成，报告进度
    if let Some(p) = progress.as_ref() {
        if total_groups > 0 {
            p(0, total_groups, &format!("分词完成，共 {} 组", total_groups));
        }
    }

    // ── 4. 获取 Session ──
    //   - ort 路径: 需要持有 MutexGuard 以支持 &mut self 的 run 调用
    //   - tract 路径: 克隆 Arc 后立即释放锁，多个 group 可并行推理
    #[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
    let session = {
        let guard = session_mutex
            .lock()
            .unwrap_or_else(|e| {
                log::warn!("[ort_embedding] tract session mutex 锁获取失败，已恢复（session 可能处于不一致状态）");
                e.into_inner()
            });
        Arc::clone(&guard)
    };

    #[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
    let mut session = session_mutex
        .lock()
        .unwrap_or_else(|e| {
            log::warn!("[ort_embedding] ORT session mutex 锁获取失败，已恢复（session 可能处于不一致状态）");
            e.into_inner()
        });

    let mut results = vec![vec![0.0f32; get_embedding_dimension()]; texts.len()];

    // ── 5. 批量推理（各组可并行）──
    #[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
    {
        // tract 路径: Arc 支持共享，各组可并行推理
        use std::sync::atomic::{AtomicUsize, Ordering};
        let done = AtomicUsize::new(0);
        let group_results: Vec<Result<Vec<(usize, Vec<f32>)>, String>> = groups
            .par_iter()
            .map(|group| {
                let result = process_one_group(group, &session);
                let completed = done.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(p) = progress.as_ref() {
                    p(completed, total_groups, &format!("向量化中 {}/{}", completed, total_groups));
                }
                result
            })
            .collect();

        for gr in group_results {
            for (orig_idx, emb) in gr? {
                results[orig_idx] = emb;
            }
        }
    }

    #[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
    {
        // ort 路径: 需要 &mut self，各组串行执行
        for (gi, group) in groups.iter().enumerate() {
            if let Some(p) = progress.as_ref() {
                p(gi + 1, total_groups, &format!("向量化中 {}/{}", gi + 1, total_groups));
            }
            let embeddings = process_one_group(group, &mut session)?;
            for (orig_idx, emb) in embeddings {
                results[orig_idx] = emb;
            }
        }
    }

    Ok(results)
}

#[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
fn process_one_group(
    group: &[(usize, Vec<i64>, Vec<i64>, Vec<i64>, usize)],
    session: &SessionType,
) -> Result<Vec<(usize, Vec<f32>)>, String> {
    let group_size = group.len();
    let max_len = group
        .iter()
        .map(|(_, ids, ..)| ids.len())
        .max()
        .unwrap_or(1);

    // 构建 batched 张量
    let mut ids_flat = Vec::with_capacity(group_size * max_len);
    let mut mask_flat = Vec::with_capacity(group_size * max_len);
    let mut type_ids_flat = Vec::with_capacity(group_size * max_len);

    for (_, ids, mask, type_ids, _) in group.iter() {
        for j in 0..max_len {
            if j < ids.len() {
                ids_flat.push(ids[j]);
                mask_flat.push(mask[j]);
                type_ids_flat.push(type_ids[j]);
            } else {
                let pad_id = get_pad_token_id();
                ids_flat.push(pad_id);
                mask_flat.push(0i64);
                type_ids_flat.push(0i64);
            }
        }
    }

    let input_ids = Array2::from_shape_vec((group_size, max_len), ids_flat)
        .map_err(|e| format!("构建 input_ids 失败: {}", e))?;
    let attention_mask = Array2::from_shape_vec((group_size, max_len), mask_flat)
        .map_err(|e| format!("构建 attention_mask 失败: {}", e))?;
    let token_type_ids = Array2::from_shape_vec((group_size, max_len), type_ids_flat)
        .map_err(|e| format!("构建 token_type_ids 失败: {}", e))?;

    let hidden = run_batch(session, input_ids, attention_mask, token_type_ids)?;

    let group_masks: Vec<Vec<i64>> = group.iter().map(|(_, _, mask, ..)| mask.clone()).collect();
    let valid_counts: Vec<usize> = group.iter().map(|(_, _, _, _, vc)| *vc).collect();
    let embeddings = post_process_batch(hidden.view(), group_size, &group_masks, &valid_counts)?;

    let result: Vec<(usize, Vec<f32>)> = group
        .iter()
        .enumerate()
        .map(|(i, (orig_idx, ..))| (*orig_idx, embeddings[i].clone()))
        .collect();
    Ok(result)
}

/// 处理一个 group（ORT 路径，需要 &mut SessionType）
#[cfg(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64")))]
fn process_one_group(
    group: &[(usize, Vec<i64>, Vec<i64>, Vec<i64>, usize)],
    session: &mut SessionType,
) -> Result<Vec<(usize, Vec<f32>)>, String> {
    let group_size = group.len();
    let max_len = group
        .iter()
        .map(|(_, ids, ..)| ids.len())
        .max()
        .unwrap_or(1);

    // 构建 batched 张量
    let mut ids_flat = Vec::with_capacity(group_size * max_len);
    let mut mask_flat = Vec::with_capacity(group_size * max_len);
    let mut type_ids_flat = Vec::with_capacity(group_size * max_len);

    for (_, ids, mask, type_ids, _) in group.iter() {
        for j in 0..max_len {
            if j < ids.len() {
                ids_flat.push(ids[j]);
                mask_flat.push(mask[j]);
                type_ids_flat.push(type_ids[j]);
            } else {
                let pad_id = get_pad_token_id();
                ids_flat.push(pad_id);
                mask_flat.push(0i64);
                type_ids_flat.push(0i64);
            }
        }
    }

    let input_ids = Array2::from_shape_vec((group_size, max_len), ids_flat)
        .map_err(|e| format!("构建 input_ids 失败: {}", e))?;
    let attention_mask = Array2::from_shape_vec((group_size, max_len), mask_flat)
        .map_err(|e| format!("构建 attention_mask 失败: {}", e))?;
    let token_type_ids = Array2::from_shape_vec((group_size, max_len), type_ids_flat)
        .map_err(|e| format!("构建 token_type_ids 失败: {}", e))?;

    let hidden = run_batch(session, input_ids, attention_mask, token_type_ids)?;

    let group_masks: Vec<Vec<i64>> = group.iter().map(|(_, _, mask, ..)| mask.clone()).collect();
    let valid_counts: Vec<usize> = group.iter().map(|(_, _, _, _, vc)| *vc).collect();
    let embeddings = post_process_batch(hidden.view(), group_size, &group_masks, &valid_counts)?;

    let result: Vec<(usize, Vec<f32>)> = group
        .iter()
        .enumerate()
        .map(|(i, (orig_idx, ..))| (*orig_idx, embeddings[i].clone()))
        .collect();
    Ok(result)
}

// ── 兼容旧接口（单线程）───

#[allow(dead_code)]
pub struct LocalEmbedding;

#[allow(dead_code)]
impl LocalEmbedding {
    pub fn new(models_dir: &Path) -> Result<Self, String> {
        ensure_initialized(models_dir)?;
        Ok(Self)
    }

    pub fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        let models_dir = MODEL_DIR
            .get()
            .ok_or("模型未初始化，请先调用 new")?;
        let model_path = Path::new(models_dir);
        call_embedding_parallel(texts, model_path, None)
    }
}
