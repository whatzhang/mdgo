use std::path::Path;

use tract_onnx::prelude::*;

/// BGE-Small-ZH 模型的向量维度
pub const EMBEDDING_DIMENSION: usize = 384;

/// 本地 BGE-Small-ZH ONNX 模型封装
///
/// 使用 `tract-onnx`（纯 Rust ONNX 推理引擎）进行推理，
/// 通过 `tokenizers` 进行 BERT WordPiece 分词。
/// 零网络依赖 + 零外部动态库依赖，模型文件随安装包分发。
pub struct LocalEmbedding {
    model: Arc<TypedRunnableModel>,
    tokenizer: tokenizers::Tokenizer,
}

impl LocalEmbedding {
    /// 从模型目录加载 ONNX 模型和分词器
    ///
    /// 目录下需包含：
    /// - `model.onnx` — ONNX 模型权重
    /// - `tokenizer.json` — BERT WordPiece 分词器
    /// - `config.json` / `tokenizer_config.json` / `special_tokens_map.json`（仅检查存在性）
    pub fn new(models_dir: &Path) -> Result<Self, String> {
        let model_path = models_dir.join("model.onnx");
        let tokenizer_path = models_dir.join("tokenizer.json");

        // ── 检查必需文件 ──
        for (name, p) in [
            ("model.onnx", &model_path),
            ("tokenizer.json", &tokenizer_path),
            ("config.json", &models_dir.join("config.json")),
            ("tokenizer_config.json", &models_dir.join("tokenizer_config.json")),
            ("special_tokens_map.json", &models_dir.join("special_tokens_map.json")),
        ] {
            if !p.exists() {
                return Err(format!("模型文件缺失: {} ({})", name, p.display()));
            }
        }

        log::info!("[local_embedding] 加载 ONNX 模型: {}", model_path.display());

        let model = onnx()
            .model_for_path(&model_path)
            .map_err(|e| format!("加载 ONNX 模型失败: {}", e))?
            .into_optimized()
            .map_err(|e| format!("优化 ONNX 模型失败: {}", e))?
            .into_runnable()
            .map_err(|e| format!("编译推理图失败: {}", e))?;

        log::info!("[local_embedding] ONNX 模型加载完成");

        let tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path.to_string_lossy().as_ref())
            .map_err(|e| format!("加载 tokenizer 失败: {}", e))?;

        Ok(Self { model, tokenizer })
    }

    /// 对一批文本生成向量（纯 Rust 推理，零网络依赖）
    ///
    /// 流程：分词 → BERT 推理 → mean pooling → L2 normalize
    pub fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut all_embeddings = Vec::with_capacity(texts.len());

        for text in texts {
            let embedding = self.embed_single(text)?;
            all_embeddings.push(embedding);
        }

        Ok(all_embeddings)
    }

    /// 单条文本的向量推理
    fn embed_single(&mut self, text: &str) -> Result<Vec<f32>, String> {
        // ── 分词 ──
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| format!("分词失败: {}", e))?;

        let ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&m| m as i64)
            .collect();
        let type_ids: Vec<i64> = encoding
            .get_type_ids()
            .iter()
            .map(|&id| id as i64)
            .collect();

        let seq_len = ids.len();

        // ── 构建输入张量 ──
        let input_ids = tract_ndarray::Array2::from_shape_vec((1, seq_len), ids)
            .map_err(|e| format!("构建 input_ids 张量失败: {}", e))?;
        let attention_mask = tract_ndarray::Array2::from_shape_vec((1, seq_len), mask.clone())
            .map_err(|e| format!("构建 attention_mask 张量失败: {}", e))?;
        let token_type_ids = tract_ndarray::Array2::from_shape_vec((1, seq_len), type_ids)
            .map_err(|e| format!("构建 token_type_ids 张量失败: {}", e))?;

        // ── 推理 ──
        let outputs = self
            .model
            .run(tvec!(
                input_ids.into_tvalue(),
                attention_mask.into_tvalue(),
                token_type_ids.into_tvalue(),
            ))
            .map_err(|e| format!("模型推理失败: {}", e))?;

        // ── 解析输出：last_hidden_state ──
        // 输出 shape: (1, seq_len, 384)
        let hidden = outputs[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| format!("解析输出张量失败: {}", e))?;

        // ── Mean Pooling ──
        let valid_count = mask.iter().filter(|&&m| m > 0).count();
        if valid_count == 0 {
            return Ok(vec![0.0f32; EMBEDDING_DIMENSION]);
        }

        let mut embedding = vec![0.0f32; EMBEDDING_DIMENSION];
        for i in 0..seq_len {
            if mask[i] > 0 {
                for j in 0..EMBEDDING_DIMENSION {
                    embedding[j] += hidden[[0, i, j]];
                }
            }
        }

        let count_f = valid_count as f32;
        for j in 0..EMBEDDING_DIMENSION {
            embedding[j] /= count_f;
        }

        // ── L2 Normalize ──
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for j in 0..EMBEDDING_DIMENSION {
                embedding[j] /= norm;
            }
        }

        Ok(embedding)
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

        assert!(
            models_dir.join("model.onnx").exists(),
            "模型文件不存在: {}",
            models_dir.display()
        );

        let mut model = LocalEmbedding::new(&models_dir).expect("模型初始化应成功");

        let texts = &["今天天气真好", "测试嵌入向量"];
        let embeddings = model.embed(texts).expect("推理应成功");

        assert_eq!(embeddings.len(), 2, "应返回 2 个向量");
        assert_eq!(
            embeddings[0].len(),
            EMBEDDING_DIMENSION,
            "向量维度应为 {}",
            EMBEDDING_DIMENSION
        );
        assert_eq!(
            embeddings[1].len(),
            EMBEDDING_DIMENSION,
            "向量维度应为 {}",
            EMBEDDING_DIMENSION
        );

        // 验证向量不为零向量
        let norm0: f32 = embeddings[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(norm0 > 0.0, "向量不应为零向量");

        // 验证两段不同文本的向量不同
        let diff: f32 = embeddings[0]
            .iter()
            .zip(embeddings[1].iter())
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(diff > 0.0, "不同文本的向量应不同");
    }
}
