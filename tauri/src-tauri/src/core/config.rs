use std::sync::{Arc, RwLock};
use serde::Serialize;

/// 索引器配置，定义索引时需要排除的目录和文件黑名单，以及分块/检索参数
#[derive(Clone, Debug, Serialize)]
pub struct IndexerConfig {
    pub dir_blacklist: Vec<String>,
    pub file_blacklist: Vec<String>,
    /// 分块目标规模（**token 数**，P0-2 语义升级：单位从"字符"改为"token"）。
    /// 对应 `ChunkBudget.target_tokens`；最终硬上限由 TokenBudgetValidator 按
    /// 模型窗口裁决（见 `core/db/token_budget.rs`）。
    pub chunk_size: usize,
    /// 相邻 chunk 重叠规模（**token 数**，语义与 chunk_size 同步升级）。
    pub chunk_overlap: usize,
    pub top_k: u32,
    pub min_score: f32,
    /// 向量/BM25 融合权重（0~1，越高越偏向语义向量，越低越偏向关键词）
    pub fusion_alpha: f32,
    /// 送入 LLM 上下文的最大文档数
    pub max_context_docs: usize,
    /// 单文档最多保留并送入上下文的 chunk 数
    pub max_chunks_per_doc: usize,
    /// 每路召回（向量/BM25）的候选池大小（Filter 前置后的检索上限）
    pub candidate_k: u32,
    /// RRF 融合常数 k（Azure / Elasticsearch / Weaviate 通用取值 60）
    pub rrf_k: u32,
    /// 纯向量命中（无 BM25 佐证）的绝对余弦阈值，过滤语义噪声
    pub vec_min_score: f32,
    /// 精排器 sigmoid 相关性阈值，低于此值的候选被丢弃
    pub rerank_min_score: f32,
    /// BM25 词间最低命中比例（minimum_should_match，0.6 = 至少 60% 查询词命中）
    pub bm25_msm_ratio: f32,
    /// 是否启用本地 cross-encoder 精排（模型未就绪时自动降级 RRF 排序）
    pub reranker_enabled: bool,
    /// C2：是否启用证据校验（默认关——开启时回答尾部标注无证据断言，增加一次规则计算）
    #[serde(default)]
    pub evidence_check_enabled: bool,
}

impl IndexerConfig {
    /// 分块参数版本（P0-4 配置版本化）：任何影响分块输出的参数变化都会改变该值。
    ///
    /// 用途：写入 `IndexMeta.chunk_params_version`；`status()` 比对当前配置，
    /// 不一致时标记 `stale=true`（旧索引与新参数混用会导致检索质量不可预期，
    /// 需提示用户全量重建）。
    pub fn chunk_params_version(&self) -> String {
        // 🟠 修复（M22）：版本串纳入 embedding 模型窗口与分块身份版本——
        // 换模型（窗口变化）而 chunk_size 不变时旧索引也能被识别为 stale；
        // 分块器/身份哈希版本（CHUNK_IDENTITY_VERSION）变化同样失效。
        // 注意：get_max_seq_len() 在模型未初始化时回退 512，属既有惰性初始化语义。
        format!(
            "budget-v1:{}:{}:{}:{}",
            crate::core::db::utils::CHUNK_IDENTITY_VERSION,
            crate::core::embedding::get_max_seq_len(),
            self.chunk_size,
            self.chunk_overlap
        )
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        // 分块默认值由 ChunkBudget 单一来源决定（token 预算，对齐本地嵌入模型的
        // 窗口，如 bge-small-zh 512 token → target=448 / overlap=56）。
        let budget = crate::core::db::token_budget::ChunkBudget::from_model_window(
            crate::core::embedding::get_max_seq_len(),
        );
        Self {
            dir_blacklist: Vec::new(),
            file_blacklist: Vec::new(),
            chunk_size: budget.target_tokens,
            chunk_overlap: budget.overlap_tokens,
            top_k: 10,
            min_score: 0.3,
            fusion_alpha: 0.6,
            max_context_docs: 4,
            max_chunks_per_doc: 3,
            candidate_k: 100,
            rrf_k: 60,
            vec_min_score: 0.35,
            rerank_min_score: 0.2,
            bm25_msm_ratio: 0.6,
            reranker_enabled: true,
            evidence_check_enabled: false,
        }
    }
}

/// 线程安全的可热更新配置容器
pub struct ConfigStore {
    inner: Arc<RwLock<IndexerConfig>>,
}

impl ConfigStore {
    pub fn new(initial: IndexerConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(initial)),
        }
    }

    pub fn update(&self, config: IndexerConfig) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *guard = config;
    }

    pub fn read(&self) -> IndexerConfig {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn clone_inner(&self) -> Arc<RwLock<IndexerConfig>> {
        self.inner.clone()
    }
}
