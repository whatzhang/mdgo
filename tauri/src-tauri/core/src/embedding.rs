use std::path::Path;
use std::sync::{Mutex, OnceLock};

use ndarray::{Array2, Axis, s};
use ort::{
    session::Session,
    value::Tensor,
};

/// BGE-Small-ZH 模型隐藏层维度（ONNX 输出维度）
const HIDDEN_SIZE: usize = 512;
/// BGE-Small-ZH 模型的向量维度（截取前 384 维作为检索用）
pub const EMBEDDING_DIMENSION: usize = 384;

/// BERT 最大序列长度
const MAX_SEQ_LEN: usize = 512;

/// 每个推理批次的文本数（长度相近的文本分在一组）
const BATCH_SIZE: usize = 20;

// ─── 全局缓存 ───

/// 首次初始化时缓存 tokenizer.json 原始字节（每线程按需解析）
static TOKENIZER_JSON: OnceLock<Vec<u8>> = OnceLock::new();
/// 模型文件目录（用于编译时找 model.onnx）
static MODEL_DIR: OnceLock<String> = OnceLock::new();
/// 全局 ONNX Runtime Session（Mutex 封装以支持 &mut self 的 run 调用，线程安全）
///
/// ONNX Runtime 内部 Session 是线程安全的（可并发 run），但 ort Rust 绑定的
/// `run(&'s mut self)` 签名需要可变引用，因此通过 Mutex 序列化访问。
/// GPU 批处理本身提供足够的加速，Mutex 争用极小。
static GLOBAL_SESSION: OnceLock<Mutex<Session>> = OnceLock::new();

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

/// 创建 ONNX Runtime Session（平台特定 GPU 后端，回退到 CPU）
fn create_session(model_path: &Path) -> Result<Session, String> {
    let mut builder = Session::builder()
        .map_err(|e| format!("创建 ONNX Runtime 配置失败: {}", e))?;

    // 平台特定的 GPU 加速：
    // - macOS: CoreML（Apple Silicon Metal + ANE / Intel GPU）
    // - Windows: DirectML（DX12 覆盖 NVIDIA/AMD/Intel）
    // - CPU 始终作为显式最终回退，保证 GPU 不可用时不中断服务
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .with_execution_providers([
                ort::ep::CoreMLExecutionProvider::default().build(),
                ort::ep::CPUExecutionProvider::default().build(),
            ])
            .map_err(|e| format!("设置 CoreML GPU 执行提供者失败: {}", e))?;
        log::info!("[ort_embedding] 启用 CoreML (macOS GPU) + CPU 回退");
    }
    #[cfg(target_os = "windows")]
    {
        builder = builder
            .with_execution_providers([
                ort::ep::DirectMLExecutionProvider::default().build(),
                ort::ep::CPUExecutionProvider::default().build(),
            ])
            .map_err(|e| format!("设置 DirectML GPU 执行提供者失败: {}", e))?;
        log::info!("[ort_embedding] 启用 DirectML (Windows GPU) + CPU 回退");
    }

    let session = builder
        .commit_from_file(model_path)
        .map_err(|e| format!("加载 ONNX 模型失败: {}", e))?;

    log::info!("[ort_embedding] ONNX Runtime session 创建成功");
    Ok(session)
}

/// 初始化全局缓存：检查模型文件完整性，缓存 tokenizer.json 字节，创建 Session。
/// 幂等安全，重复调用无副作用。
fn ensure_initialized(models_dir: &Path) -> Result<(), String> {
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

    let model_path = models_dir.join("model.onnx");
    let session = create_session(&model_path)?;

    let model_dir_str = models_dir.to_string_lossy().to_string();

    // OnceLock::set 仅首次成功，后续静默失败，保证幂等
    let _ = TOKENIZER_JSON.set(tokenizer_raw);
    let _ = MODEL_DIR.set(model_dir_str);
    let _ = GLOBAL_SESSION.set(Mutex::new(session));

    log::info!("[ort_embedding] 初始化完成");
    Ok(())
}

// ─── Mean Pooling + L2 Normalize ───

/// 对一组 ONNX 推理结果进行 mean pooling + L2 normalize。
fn post_process_batch(
    hidden: ndarray::ArrayViewD<'_, f32>,
    batch_size: usize,
    masks: &[Vec<i64>],
    valid_counts: &[usize],
) -> Vec<Vec<f32>> {
    let shape = hidden.shape();
    if shape.len() != 3 {
        log::error!("[ort_embedding] 意外的输出形状: {:?}, 期望 3D", shape);
        return vec![vec![0.0f32; EMBEDDING_DIMENSION]; batch_size];
    }
    let actual_batch = shape[0];
    let seq_len = shape[1];
    let actual_hidden = shape[2];

    if actual_batch != batch_size || actual_hidden != HIDDEN_SIZE {
        log::error!(
            "[ort_embedding] 输出形状不匹配: {:?}, 期望 ({}, ?, {})",
            shape, batch_size, HIDDEN_SIZE
        );
        return vec![vec![0.0f32; EMBEDDING_DIMENSION]; batch_size];
    }

    // hidden 是 3D 视图，reshape 为 (batch, seq_len, HIDDEN_SIZE)
    let hidden_3d = hidden
        .into_shape_with_order((batch_size, seq_len, HIDDEN_SIZE))
        .expect("hidden 形状应匹配 (batch_size, seq_len, HIDDEN_SIZE)");

    let mut all_embeddings = Vec::with_capacity(batch_size);

    for i in 0..batch_size {
        let valid = valid_counts[i];
        if valid == 0 {
            all_embeddings.push(vec![0.0f32; EMBEDDING_DIMENSION]);
            continue;
        }

        let valid_len = masks[i].len();
        let sum = hidden_3d
            .slice(s![i, 0..valid_len, 0..EMBEDDING_DIMENSION])
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

    all_embeddings
}

// ─── 公开 API ───

/// 对一批文本生成向量（动态 padding + 长度分组批量推理）。
///
/// # 算法
/// 1. 分词所有文本 → 获取实际 token 长度
/// 2. 按长度降序排序 → 每 BATCH_SIZE 条分一组
/// 3. 各组依次在 ONNX Runtime Session 上推理（ORT 内部分配 GPU/CPU 资源）
/// 4. 恢复原始输入顺序返回
///
/// # 性能
/// - GPU 加速（CoreML / DirectML）提供 ~5-10x 加速
/// - ORT 动态形状免除 tract-onnx 每批重编译的开销
/// - 分组降低 padding 浪费
pub fn call_embedding_parallel(texts: &[&str], models_dir: &Path) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }

    // ── 1. 初始化（文件检查 + tokenizer 缓存 + Session 创建）──
    ensure_initialized(models_dir)?;

    let session_mutex = GLOBAL_SESSION
        .get()
        .ok_or_else(|| "ONNX Runtime 未初始化，请先调用 ensure_initialized".to_string())?;

    // ── 2. 分词所有文本 ──
    let mut items: Vec<(usize, Vec<i64>, Vec<i64>, Vec<i64>, usize)> =
        Vec::with_capacity(texts.len());

    for (idx, &text) in texts.iter().enumerate() {
        let (ids, mask, type_ids) = with_tokenizer(|tok| -> Result<_, String> {
            let enc = tok
                .encode(text, true)
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
            let truncated_len = ids.len().min(MAX_SEQ_LEN);
            let ids = ids[..truncated_len].to_vec();
            let mask = mask[..truncated_len].to_vec();
            let type_ids = type_ids[..truncated_len].to_vec();
            Ok((ids, mask, type_ids))
        })?;

        let valid_count = mask.iter().filter(|&&m| m > 0).count();
        items.push((idx, ids, mask, type_ids, valid_count));
    }

    // ── 3. 按长度降序排序 + 分组 ──
    items.sort_unstable_by(|a, b| b.1.len().cmp(&a.1.len()));
    let groups: Vec<&[(usize, Vec<i64>, Vec<i64>, Vec<i64>, usize)]> =
        items.chunks(BATCH_SIZE).collect();

    // ── 4. 依次对每组推理（ORT Session 需要 &mut self，各组串行执行）──
    // ONNX Runtime 内部 GPU 流已经提供并行度；CPU 模式下 ORT 内部线程池也会利用多核。
    let mut session = session_mutex
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let mut results = vec![vec![0.0f32; EMBEDDING_DIMENSION]; texts.len()];

    for group in &groups {
        let group_size = group.len();
        let max_len = group
            .iter()
            .map(|(_, ids, ..)| ids.len())
            .max()
            .unwrap_or(1);

        // ── 构建 batched 张量 ──
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
                    ids_flat.push(0i64);
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

        // ── 创建 ONNX Runtime Tensor ──
        let input_tensor =
            Tensor::<i64>::from_array(input_ids)
                .map_err(|e| format!("创建 input_ids 张量失败: {}", e))?;
        let mask_tensor =
            Tensor::<i64>::from_array(attention_mask)
                .map_err(|e| format!("创建 attention_mask 张量失败: {}", e))?;
        let type_ids_tensor =
            Tensor::<i64>::from_array(token_type_ids)
                .map_err(|e| format!("创建 token_type_ids 张量失败: {}", e))?;

        // ── 推理 ──
        let outputs = session
            .run(ort::inputs![input_tensor, mask_tensor, type_ids_tensor])
            .map_err(|e| format!("ONNX Runtime 推理失败: {}", e))?;
        // 防御性检查：确保模型至少返回一个输出张量
        if outputs.len() < 1 {
            return Err("ONNX Runtime 推理返回空输出，请检查模型文件完整性".to_string());
        }

        // ── 提取输出 ──
        let hidden = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|e| format!("解析输出张量失败: {}", e))?;

        // ── Mean Pooling + L2 Normalize ──
        let group_masks: Vec<Vec<i64>> = group.iter().map(|(_, _, mask, ..)| mask.clone()).collect();
        let valid_counts: Vec<usize> = group.iter().map(|(_, _, _, _, vc)| *vc).collect();

        let embeddings = post_process_batch(hidden, group_size, &group_masks, &valid_counts);

        // ── 恢复原始顺序 ──
        for (i, (orig_idx, ..)) in group.iter().enumerate() {
            results[*orig_idx] = embeddings[i].clone();
        }
    }

    Ok(results)
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
        call_embedding_parallel(texts, model_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_embedding_load_and_infer() {
        let models_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("models")
            .join("bge-small-zh-v1.5");

        let mut model = LocalEmbedding::new(&models_dir).expect("初始化应成功");

        let texts = &["今天天气真好", "测试嵌入向量"];
        let embeddings = model.embed(texts).expect("推理应成功");

        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), EMBEDDING_DIMENSION);

        let norm0: f32 = embeddings[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(norm0 > 0.0);
        let diff: f32 = embeddings[0]
            .iter()
            .zip(embeddings[1].iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(diff > 0.0);
    }

    #[test]
    fn test_parallel_embedding() {
        let models_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("models")
            .join("bge-small-zh-v1.5");

        let texts: Vec<&str> = (0..20).map(|_| "测试并行嵌入向量").collect();
        let embeddings = call_embedding_parallel(&texts, &models_dir).expect("并行推理应成功");
        assert_eq!(embeddings.len(), 20);
        assert_eq!(embeddings[0].len(), EMBEDDING_DIMENSION);
    }
}
