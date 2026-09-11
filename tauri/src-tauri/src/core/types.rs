use serde::{Deserialize, Serialize};

/// 文件类型计数（用于文档类型分布）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTypeCount {
    pub file_type: String,
    pub count: u32,
    pub percentage: f32,
}

#[derive(Debug, Serialize)]
pub struct KbIndexResult {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
    /// 显式降级截断的 chunk 数（P0-1 可观测性；健康态应为 0）
    #[serde(default)]
    pub truncated_chunks: u32,
    /// 超限后被重切的 chunk 数（P0-1 可观测性）
    #[serde(default)]
    pub resplit_chunks: u32,
    /// **整文件未入库**清单（Plan B v2 §5.3 / §7.2 F9）：路径 + 原因（+ 需 OCR 的页号）
    #[serde(default)]
    pub skipped_files: Vec<crate::core::pipeline::SkippedFile>,
    /// **部分索引**清单（§5.3：与 `skipped_files` 语义不同，必须分开显示）：
    /// 文件进了库，但有页被跳过（Q8）——路径 + 被跳过页号 + 页级诊断
    #[serde(default)]
    pub partial_files: Vec<crate::core::pipeline::PartialFile>,
}

#[derive(Debug, Serialize)]
pub struct KbStatus {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
    pub status: String,
    /// 分块参数已变更但索引未重建（P0-4 配置版本化；true 时前端提示重建）
    #[serde(default)]
    pub stale: bool,
    /// **按文件类型**的过期集合（Plan B v2 / Phase 0C）。
    ///
    /// 取值如 `["pdf"]` / `["office","pdf"]`；空表示各类型转换器均与注册表一致。
    /// `stale` 仍表示"任意类型过期"（`stale == !stale_kinds.is_empty() || 参数版本不一致`），
    /// 保持旧前端兼容；新前端可据此只重建受影响类型。
    #[serde(default)]
    pub stale_kinds: Vec<String>,
    /// **整文件未入库**清单（Plan B v2 §5.3 / §7.2 F9，持久化于 `IndexMeta`）。
    /// 让用户在仪表盘随时看到「哪个文件没进库 + 为什么」，而不是只能看索引当次的日志。
    #[serde(default)]
    pub skipped_files: Vec<crate::core::pipeline::SkippedFile>,
    /// **部分索引**清单（§5.3）：文件进了库但有页被跳过（含页号）。
    #[serde(default)]
    pub partial_files: Vec<crate::core::pipeline::PartialFile>,
}

/// 索引元数据（持久化到 JSON）
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexMeta {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
    /// 已索引文件的类型分布（由 index_all/index_file 写入，chart 消费）
    #[serde(default)]
    pub type_distribution: Vec<FileTypeCount>,
    /// 分块参数版本（P0-4）：由 `IndexerConfig::chunk_params_version()` 写入；
    /// 旧索引（无该字段）反序列化为空串 → 视为 stale
    #[serde(default)]
    pub chunk_params_version: String,
    /// **每个 source_kind 实际使用的转换器**（Phase 0C）：`{"pdf": "pdf-inspector@1.19.0", ...}`。
    ///
    /// 作为 `stale_kinds` 判定的**快照**（避免每次 status 都全表扫 LanceDB）；
    /// 由索引写入路径维护，`kb_reindex_kinds` 会同步刷新对应键。
    #[serde(default)]
    pub kind_converters: std::collections::BTreeMap<String, String>,
    /// **整文件未入库**清单（§5.3 / §7.2 F9）：按 `rel_path` 去重维护。
    #[serde(default)]
    pub skipped_files: Vec<crate::core::pipeline::SkippedFile>,
    /// **部分索引**清单（§5.3）：按 `rel_path` 去重维护，含被跳过页号。
    #[serde(default)]
    pub partial_files: Vec<crate::core::pipeline::PartialFile>,
}

/// 嵌入模型信息（动态获取，非硬编码）
#[derive(Debug, Serialize)]
pub struct KbEmbeddingInfo {
    pub model_name: String,
    pub dimension: u32,
    pub status: String,
    /// 模型 max_position_embeddings（P0-1：前端据此约束 chunk_size 上限）
    #[serde(default)]
    pub max_position_embeddings: u32,
}

/// 检索链路分阶段耗时（A2：Latency 分阶段计时；benchmark 消费，定位瓶颈）。
#[derive(Debug, Default, Clone, Serialize)]
pub struct RetrievalTimings {
    /// 查询理解（intent 路由 + 符号提取 + Filter 构造）
    pub planner_ms: u64,
    /// 向量路召回
    pub dense_ms: u64,
    /// BM25 路召回
    pub bm25_ms: u64,
    /// 符号路召回
    pub symbol_ms: u64,
    /// RRF 融合
    pub rrf_ms: u64,
    /// cross-encoder 精排（未启用为 0）
    pub rerank_ms: u64,
    /// 聚簇 + 上下文扩展
    pub finalize_ms: u64,
    /// 全链路
    pub total_ms: u64,
}
