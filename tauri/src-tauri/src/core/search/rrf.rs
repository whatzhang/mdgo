//! 多路召回融合：Reciprocal Rank Fusion（RRF）。
//!
//! # 为什么用 RRF 而非线性加权
//! 向量余弦分数（0~1 窄带）与 BM25 归一化分数（最高分恒 1.0、长尾极陡）**物理意义不可比**，
//! 直接 `alpha*vec + (1-alpha)*bm25` 会让某一路异常分数压制另一路，且对语料分布敏感。
//! RRF 只依赖**排名**（`score = w/(k+rank)`），对分数尺度完全鲁棒——
//! Azure AI Search / Elasticsearch / Weaviate 混合检索均默认采用 RRF(k=60)。
//!
//! # 扩展：加权 RRF
//! 保留现有 `fusion_alpha` 配置作为**每路权重偏置**（`weight_vec = alpha`，
//! `weight_bm25 = 1-alpha`），既兼容现有配置语义（代码查询偏 BM25），
//! 又避免线性加权在原理层面的不可比问题。

use std::collections::HashMap;

use crate::core::db::lance::SearchHit;

/// RRF 融合配置。
#[derive(Debug, Clone)]
pub struct RrfConfig {
    /// RRF 常数 k（生产默认 60：Elasticsearch / Azure / Weaviate 通用取值）
    pub k: u32,
    /// 向量路权重（= 现有 fusion_alpha 语义：越高越偏语义）
    pub weight_vec: f32,
    /// BM25 路权重（= 1 - fusion_alpha）
    pub weight_bm25: f32,
    /// 符号路权重（代码查询专用，固定权重）
    pub weight_symbol: f32,
    /// 图路权重（GraphRAG：文档关系图召回，默认 0.15——图路为佐证/关联补充，
    /// 权重低于向量/BM25，避免图路噪声压过语义精确匹配）
    pub weight_graph: f32,
}

impl Default for RrfConfig {
    fn default() -> Self {
        Self {
            k: 60,
            weight_vec: 0.6,
            weight_bm25: 0.4,
            weight_symbol: 1.0,
            weight_graph: 0.15,
        }
    }
}

/// 融合中间条目（按 doc_name + chunk_index 合并各路贡献）。
#[derive(Default)]
struct Entry {
    rrf_score: f32,
    score_vec: f32,
    score_bm25: f32,
    text: String,
    path_json: Option<String>,
    sentence_window: Option<String>,
    symbol_name: Option<String>,
    symbol_kind: Option<String>,
    chunk_type: Option<String>,
}

/// 将单路 hits 的排名贡献累加到融合表。
///
/// 同路内同一 key 出现多次（理论不会，防御处理）取首次（rank 更优）贡献。
fn accumulate(
    map: &mut HashMap<(String, u32), Entry>,
    hits: Vec<SearchHit>,
    weight: f32,
    k: u32,
    field: FuseField,
) {
    for (rank, hit) in hits.into_iter().enumerate() {
        let key = (hit.doc_name.clone(), hit.chunk_index);
        let entry = map.entry(key).or_default();
        entry.rrf_score += weight / (k as f32 + (rank as u32 + 1) as f32);
        match field {
            FuseField::Vec => {
                entry.score_vec = entry.score_vec.max(hit.score_vec.max(hit.score));
                entry.text = hit.text.clone();
                entry.path_json = entry.path_json.clone().or(hit.path_json);
                entry.sentence_window = entry.sentence_window.clone().or(hit.sentence_window);
                if entry.symbol_name.is_none() {
                    entry.symbol_name = hit.symbol_name;
                    entry.symbol_kind = hit.symbol_kind;
                }
                entry.chunk_type = entry.chunk_type.clone().or(hit.chunk_type);
            }
            FuseField::Bm25 => {
                entry.score_bm25 = entry.score_bm25.max(hit.score_bm25.max(hit.score));
                if entry.text.is_empty() {
                    entry.text = hit.text.clone();
                }
                if entry.symbol_name.is_none() {
                    entry.symbol_name = hit.symbol_name;
                    entry.symbol_kind = hit.symbol_kind;
                }
                if entry.path_json.is_none() {
                    entry.path_json = hit.path_json;
                }
                if entry.sentence_window.is_none() {
                    entry.sentence_window = hit.sentence_window;
                }
                if entry.chunk_type.is_none() {
                    entry.chunk_type = hit.chunk_type;
                }
            }
            // 符号路：携带符号证据（symbol_name/symbol_kind）供下游保留与展示；
            // 文本/路径等元数据仅当其他路尚未填充时补充（向量路文本为基准，
            // 避免覆盖向量路的更完整文本）。
            FuseField::Symbol => {
                if entry.text.is_empty() {
                    entry.text = hit.text.clone();
                }
                if entry.path_json.is_none() {
                    entry.path_json = hit.path_json;
                }
                if entry.sentence_window.is_none() {
                    entry.sentence_window = hit.sentence_window;
                }
                if entry.chunk_type.is_none() {
                    entry.chunk_type = hit.chunk_type;
                }
                if entry.symbol_name.is_none() {
                    entry.symbol_name = hit.symbol_name;
                    entry.symbol_kind = hit.symbol_kind;
                }
            }
            // 图路：仅贡献 RRF 排名分 + 文本兜底，不写分数域（保持分数语义纯净）。
            // 图路命中的 doc 多为"关联文档"（图结构佐证），文本取自该文档 top chunk。
            FuseField::Graph => {
                if entry.text.is_empty() {
                    entry.text = hit.text.clone();
                }
                if entry.path_json.is_none() {
                    entry.path_json = hit.path_json;
                }
                if entry.sentence_window.is_none() {
                    entry.sentence_window = hit.sentence_window;
                }
                if entry.chunk_type.is_none() {
                    entry.chunk_type = hit.chunk_type;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FuseField {
    Vec,
    Bm25,
    Symbol,
    Graph,
}

/// 多路 RRF 融合（向量 / BM25 / 代码符号 / 图路），返回按融合分数降序的结果。
///
/// - 多路命中的文档获得叠加贡献 → 自然奖励"多路共识"
/// - 单路命中的低质量文档仅获单路贡献 → 排名靠后，被精排/阈值过滤
/// - 结果 `score` 归一化到 [0,1]（除以本批最高分），供无精排阶段的排序与阈值使用；
///   精排阶段会以 `score_rerank` 覆盖最终分
///
/// `graph_hits` 为空时行为与旧三路完全一致（图路不参与累加）。
/// 兼容包装（保留旧三路 API 形态；新调用应使用 [`rrf_fuse_graph`]）。
#[allow(dead_code)]
pub fn rrf_fuse(
    vec_hits: Vec<SearchHit>,
    bm25_hits: Vec<SearchHit>,
    symbol_hits: Vec<SearchHit>,
    cfg: &RrfConfig,
) -> Vec<SearchHit> {
    rrf_fuse_graph(vec_hits, bm25_hits, symbol_hits, Vec::new(), cfg)
}

/// 四路 RRF 融合（含图路）。图路命中在 `Entry` 中仅累加 rrf 贡献与补充文本，
/// 不写入 `score_vec/score_bm25`（保持其它路分数语义纯净）。
pub fn rrf_fuse_graph(
    vec_hits: Vec<SearchHit>,
    bm25_hits: Vec<SearchHit>,
    symbol_hits: Vec<SearchHit>,
    graph_hits: Vec<SearchHit>,
    cfg: &RrfConfig,
) -> Vec<SearchHit> {
    let k = cfg.k.max(1);
    let mut map: HashMap<(String, u32), Entry> = HashMap::new();

    let sym_count = symbol_hits.len();
    let graph_count = graph_hits.len();
    accumulate(&mut map, vec_hits, cfg.weight_vec, k, FuseField::Vec);
    accumulate(&mut map, bm25_hits, cfg.weight_bm25, k, FuseField::Bm25);
    if !symbol_hits.is_empty() {
        accumulate(&mut map, symbol_hits, cfg.weight_symbol, k, FuseField::Symbol);
    }
    if !graph_hits.is_empty() {
        accumulate(&mut map, graph_hits, cfg.weight_graph, k, FuseField::Graph);
    }

    let mut entries: Vec<(String, u32, Entry)> = map
        .drain()
        .map(|((doc_name, chunk_index), e)| (doc_name, chunk_index, e))
        .collect();

    entries.sort_by(|a, b| {
        b.2.rrf_score
            .partial_cmp(&a.2.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let max_score = entries
        .first()
        .map(|(_, _, e)| e.rrf_score)
        .unwrap_or(0.0);
    let norm = if max_score > 0.0 { 1.0 / max_score } else { 0.0 };

    log::info!(
        "[rrf] 融合完成: candidates={} 符号路命中={} 图路命中={} max_rrf={:.4}",
        entries.len(),
        sym_count,
        graph_count,
        max_score
    );

    entries
        .into_iter()
        .map(|(doc_name, chunk_index, e)| SearchHit {
            text: e.text,
            doc_name,
            chunk_index,
            score: (e.rrf_score * norm).clamp(0.0, 1.0),
            score_vec: e.score_vec.clamp(0.0, 1.0),
            score_bm25: e.score_bm25.clamp(0.0, 1.0),
            path_json: e.path_json,
            sentence_window: e.sentence_window,
            symbol_name: e.symbol_name,
            symbol_kind: e.symbol_kind,
            chunk_type: e.chunk_type,
            score_rerank: None,
            query_sources: Vec::new(),
        })
        .collect()
}
