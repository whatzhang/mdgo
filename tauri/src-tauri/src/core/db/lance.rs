use arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray, UInt32Array,
    types::Float32Type,
};
use arrow_schema::{DataType, Field, Schema};
use lancedb::index::vector::IvfSqIndexBuilder;
use lancedb::index::Index as LanceIndex;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::core::db::utils::get_local_embedding_dimension;

pub(crate) fn escape_sql_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DocumentChunk {
    pub id: String,
    pub doc_name: String,
    pub chunk_index: u32,
    pub text: String,
    /// OPML 节点在树中的深度（仅 OPML 文件有值）
    pub path_depth: Option<u32>,
    /// OPML/FreeMind 节点路径的 JSON 数组
    pub path_json: Option<String>,
    /// 句子级 chunk 的上下文窗口文本（SentenceWindow 用）
    pub sentence_window: Option<String>,
    /// 代码符号名（仅代码文件有值），如函数名、类名
    pub symbol_name: Option<String>,
    /// 代码符号类型（仅代码文件有值），如 "function"、"class"
    pub symbol_kind: Option<String>,
    /// 向量化文本（AST 语义分块用）：与 `text` 分离，`None` 表示直接用 `text`。
    /// 仅用于写入前向量化，不落库。
    pub embedding_text: Option<String>,
    /// 分块类型（AST 语义分块用）：paragraph/code/table/list/quote/section 等
    pub chunk_type: Option<String>,
    /// **版本失效粒度键**（Plan B v2 / Phase 0C）：pdf / office / markdown / code / text / data。
    /// 用于"按文件类型失效"——升级 pdf-inspector 只让 PDF 相关索引 stale（方案 §5.5）。
    /// 必须落库（与仅进 BM25 的 doc_title/tags 不同）。
    pub source_kind: Option<String>,
    /// **转换器身份** `id@version`（如 `pdf-inspector@1.19.0` / `anydoc@0.2.4` / `native@1`）。
    /// 与 `source_kind` 一起构成"索引里实际用了哪个转换器"的事实记录。
    pub converter: Option<String>,
    /// **页码 provenance**（Plan B v2 / Phase 1）：起始页（1-indexed）。
    /// 仅分页来源（如 PDF）有值；**不是分块边界**——跨页 chunk 会 `page_end > page_start`。
    pub page_start: Option<u32>,
    /// 结束页（1-indexed）；单页 chunk 与 `page_start` 相同
    pub page_end: Option<u32>,
    /// 成员行区间的页归属明细（JSON：`[{"page":1,"line_start":10,"line_end":24}]`）
    pub source_spans: Option<String>,
    /// **表格表头列名**（JSON 数组，Plan B v2 §4.7 / 决策 R3）；非表格块为 `None`。
    /// 只进 metadata，**不进 `embedding_text`**。
    pub table_headers: Option<String>,
    /// 文档显式标题（P0-1：frontmatter title；BM25 title 字段优先使用，不落 LanceDB 列）
    pub doc_title: Option<String>,
    /// 文档标签（P0-1：frontmatter tags+aliases 的 JSON 数组字符串；BM25 tags 字段，不落 LanceDB 列）
    pub tags: Option<String>,
}

/// 命中来源查询（P1 预检索优化器：跨查询一致性统计）。
///
/// `Original` = 用户原始查询；`Expanded(n)` = 第 n 条扩展查询（0 起）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QuerySource {
    Original,
    Expanded(u8),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SearchHit {
    pub text: String,
    pub doc_name: String,
    pub chunk_index: u32,
    pub score: f32,
    pub score_vec: f32,
    pub score_bm25: f32,
    /// OPML 节点路径 JSON 数组（仅 OPML 文件有值），用于层级去重和前端展示
    pub path_json: Option<String>,
    /// 句子级 chunk 的上下文窗口文本（SentenceWindow 用）
    pub sentence_window: Option<String>,
    /// 代码符号名（仅代码文件有值）
    pub symbol_name: Option<String>,
    /// 代码符号类型（仅代码文件有值）
    pub symbol_kind: Option<String>,
    /// 分块类型（AST 语义分块用）
    pub chunk_type: Option<String>,
    /// 版本失效粒度键（Phase 0C；前端可据此对 PDF/Office 显示不同徽标）
    #[serde(default)]
    pub source_kind: Option<String>,
    /// 转换器身份 `id@version`（Phase 0C）
    #[serde(default)]
    pub converter: Option<String>,
    /// 起始页（1-indexed；Phase 1，仅分页来源有值）
    #[serde(default)]
    pub page_start: Option<u32>,
    /// 结束页（1-indexed；单页与 page_start 相同）
    #[serde(default)]
    pub page_end: Option<u32>,
    /// 页归属明细 JSON（Phase 1）
    #[serde(default)]
    pub source_spans: Option<String>,
    /// 表格表头列名 JSON 数组（§4.7 / R3）；非表格块为 None
    #[serde(default)]
    pub table_headers: Option<String>,
    /// 文档标签（P0-1：frontmatter tags+aliases 的 JSON 数组字符串；
    /// 🟠 M9：落 SearchHit 供融合后内存标签过滤——BM25/符号路无法 SQL 下推）
    pub tags: Option<String>,
    /// 精排分数（本地 bge-reranker sigmoid 相关性分数，仅精排启用时有值）
    pub score_rerank: Option<f32>,
    /// 命中来源查询列表（原始 / 扩展），预检索多查询路径打标；
    /// 其他路径（kb_search 工具/索引管线）为空。`#[serde(default)]` 保证兼容。
    #[serde(default)]
    pub query_sources: Vec<QuerySource>,
}

/// 代码符号条目缓存（search_symbols 内存过滤用，避免每次查询全表 LIKE 扫描）
#[derive(Debug, Clone)]
struct SymbolEntry {
    text: String,
    doc_name: String,
    chunk_index: u32,
    symbol_name: String,
    symbol_kind: Option<String>,
    path_json: Option<String>,
    sentence_window: Option<String>,
    chunk_type: Option<String>,
    /// Phase 0C：版本失效粒度键（符号路命中同样要携带 provenance）
    source_kind: Option<String>,
    /// Phase 0C：转换器身份
    converter: Option<String>,
    /// Phase 1：页码 provenance
    page_start: Option<u32>,
    page_end: Option<u32>,
    source_spans: Option<String>,
    /// §4.7 / R3：表格表头列名
    table_headers: Option<String>,
    /// 🟠 M9：文档标签（JSON 数组字符串），供融合后标签过滤
    tags: Option<String>,
}

pub struct LanceStore {
    uri: String,
    table_name: String,
    /// 缓存连接，同一实例内复用（解决 C4）
    db: Mutex<Option<lancedb::connection::Connection>>,
    /// 代码符号名缓存（首次查询全量加载，写操作后自动失效）
    symbol_cache: std::sync::Mutex<Option<Vec<SymbolEntry>>>,
}

impl LanceStore {
    pub fn new(base_uri: &str, table_name: &str) -> Self {
        Self {
            uri: base_uri.to_string(),
            table_name: table_name.to_string(),
            db: Mutex::new(None),
            symbol_cache: std::sync::Mutex::new(None),
        }
    }

    /// 获取或创建缓存连接
    async fn get_connection(&self) -> Result<lancedb::connection::Connection, String> {
        let mut guard = self.db.lock().await;
        if let Some(ref conn) = *guard {
            return Ok(conn.clone());
        }
        let conn = lancedb::connect(&self.uri)
            .execute()
            .await
            .map_err(|e| format!("LanceDB 连接失败: {}", e))?;
        let cloned = conn.clone();
        *guard = Some(conn);
        Ok(cloned)
    }

    /// 创建或确保向量表存在（固定 384 维，本地 bge-small-zh-v1.5 模型）
    ///
    /// 开发阶段策略：表已存在则直接返回（不做列迁移——表结构以本文件 schema 为准；
    /// 结构变更时由调用方 drop_table_only 后重建）；表不存在则按当前 schema 创建。
    pub async fn create_table(&self) -> Result<(), String> {
        let db = self.get_connection().await?;

        // 表已存在 → 直接返回（开发阶段不做向后兼容迁移）
        let open_result = tokio::time::timeout(
            Duration::from_secs(30),
            db.open_table(&self.table_name).execute(),
        )
        .await;
        if let Ok(Ok(_table)) = open_result {
            // 补建向量索引（已有索引则瞬间跳过；构建失败不阻断，仅影响检索性能）
            if let Err(e) = self.ensure_vector_index().await {
                log::warn!("[lance] 确保向量索引失败（检索将退化为全表扫描）: {}", e);
            }
            return Ok(());
        }

        // 表不存在 → 创建新表（维度由本地 bge 模型决定）
        // 维度获取可能在首次使用时触发模型下载/初始化（秒~分钟级），
        // 移入 spawn_blocking 避免阻塞 Tokio worker
        let dim = tokio::task::spawn_blocking(get_local_embedding_dimension)
            .await
            .map_err(|e| format!("获取模型维度任务失败: {}", e))??;
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("text", DataType::Utf8, false),
            Field::new("doc_name", DataType::Utf8, false),
            Field::new("chunk_index", DataType::UInt32, false),
            Field::new("path_depth", DataType::UInt32, true),
            Field::new("path_json", DataType::Utf8, true),
            Field::new("sentence_window", DataType::Utf8, true),
            Field::new("symbol_name", DataType::Utf8, true),
            Field::new("symbol_kind", DataType::Utf8, true),
            Field::new("chunk_type", DataType::Utf8, true),
            // A3：frontmatter 标签（JSON 数组字符串），供 metadata 过滤下推
            Field::new("tags", DataType::Utf8, true),
            // Plan B v2 / 0C：版本失效粒度（source_kind）与转换器身份（converter）
            Field::new("source_kind", DataType::Utf8, true),
            Field::new("converter", DataType::Utf8, true),
            // Plan B v2 / Phase 1：页码 provenance（分页来源）
            Field::new("page_start", DataType::UInt32, true),
            Field::new("page_end", DataType::UInt32, true),
            Field::new("source_spans", DataType::Utf8, true),
            // Plan B v2 / §4.7（决策 R3）：表格表头列名（JSON 数组字符串）
            Field::new("table_headers", DataType::Utf8, true),
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    dim as i32,
                ),
                true,
            ),
        ]));

        tokio::time::timeout(
            Duration::from_secs(30),
            db.create_empty_table(&self.table_name, schema).execute(),
        )
        .await
        .map_err(|_| "LanceDB 创建表超时 (30s)".to_string())?
        .map_err(|e| format!("LanceDB 创建表失败: {}", e))?;

        Ok(())
    }

    /// 确保向量表上存在向量索引（消除全表暴力扫描，大幅降低检索延迟）。
    ///
    /// - 已存在向量索引 → 直接返回（Lance 会自动维护后续增量数据）
    /// - 表为空 → 跳过（全量重建时由 index_all 写入完成后再次调用）
    /// - 构建失败仅记日志，不阻断查询（索引只影响性能，不影响正确性）
    pub async fn ensure_vector_index(&self) -> Result<(), String> {
        let table = self.open_table().await?;

        let indices = table
            .list_indices()
            .await
            .map_err(|e| format!("读取向量索引列表失败: {}", e))?;
        if indices
            .iter()
            .any(|idx| idx.columns.iter().any(|c| c == "vector"))
        {
            return Ok(());
        }

        let row_count = table
            .count_rows(None)
            .await
            .map_err(|e| format!("读取向量表行数失败: {}", e))?;
        if row_count == 0 {
            log::info!("[lance] 向量表为空，跳过索引创建");
            return Ok(());
        }

        // 距离类型必须与检索时一致（Cosine），否则搜索结果不准确。
        // 索引选型与训练参数（36k 行实测）：
        // - IVF-PQ 需做 32 个子向量的 kmeans 训练，512 维下超 10 分钟无法完成 → 弃用
        // - IVF-SQ 仅做 IVF 分区 kmeans + 逐维 min/max 标量化，训练快 5-10 倍，
        //   且 SQ 压缩率（1 字节/维）低于 PQ（32 字节/向量），召回率更高
        // - sample_rate=128、max_iterations=20：削减 kmeans 训练量，召回损失可忽略
        let builder = IvfSqIndexBuilder::default()
            .distance_type(DistanceType::Cosine)
            .sample_rate(128)
            .max_iterations(20);
        log::info!("[lance] 开始创建 IVF-SQ 向量索引（{} 行）...", row_count);
        tokio::time::timeout(
            Duration::from_secs(1800),
            table.create_index(&["vector"], LanceIndex::IvfSq(builder)).execute(),
        )
        .await
        .map_err(|_| "创建向量索引超时 (1800s)，训练任务可能仍在后台继续，下次启动将自动跳过已建索引".to_string())?
        .map_err(|e| format!("创建向量索引失败: {}", e))?;
        log::info!("[lance] IVF-SQ 向量索引创建完成");
        Ok(())
    }

    /// 获取或打开已有表
    pub async fn open_table(&self) -> Result<lancedb::Table, String> {
        let db = self.get_connection().await?;
        db.open_table(&self.table_name)
            .execute()
            .await
            .map_err(|e| format!("打开表失败: {}", e))
    }

    /// 批量写入文档块 + 向量（维度校验：仅检查非零，一致性由单一模型保证）
    pub async fn add_chunks(
        &self,
        chunks: &[DocumentChunk],
        vectors: &[Vec<f32>],
    ) -> Result<(), String> {
        if chunks.is_empty() || vectors.is_empty() {
            return Ok(());
        }
        if chunks.len() != vectors.len() {
            return Err(format!(
                "chunks 数量 ({}) 与 vectors 数量 ({}) 不匹配",
                chunks.len(),
                vectors.len()
            ));
        }

        let n = chunks.len();
        let dim = vectors[0].len() as i32;

        if dim == 0 {
            return Err("向量维度为 0，请检查 Embedding 模型配置".into());
        }

        // 校验所有向量维度一致
        for (i, v) in vectors.iter().enumerate() {
            if v.len() as i32 != dim {
                return Err(format!(
                    "向量维度不一致：第 {} 个向量维度为 {}，期望 {}",
                    i,
                    v.len(),
                    dim
                ));
            }
        }

        let table = self.open_table().await?;

        // 构建 RecordBatch
        let mut id_arr = Vec::with_capacity(n);
        let mut text_arr = Vec::with_capacity(n);
        let mut doc_name_arr = Vec::with_capacity(n);
        let mut chunk_idx_arr = Vec::with_capacity(n);
        let mut path_depth_arr: Vec<Option<u32>> = Vec::with_capacity(n);
        let mut path_json_arr: Vec<Option<&str>> = Vec::with_capacity(n);
        let mut sentence_window_arr: Vec<Option<&str>> = Vec::with_capacity(n);
        let mut symbol_name_arr: Vec<Option<&str>> = Vec::with_capacity(n);
        let mut symbol_kind_arr: Vec<Option<&str>> = Vec::with_capacity(n);
        let mut chunk_type_arr: Vec<Option<&str>> = Vec::with_capacity(n);
        let mut tags_arr: Vec<Option<&str>> = Vec::with_capacity(n);
        let mut source_kind_arr: Vec<Option<&str>> = Vec::with_capacity(n);
        let mut converter_arr: Vec<Option<&str>> = Vec::with_capacity(n);
        let mut page_start_arr: Vec<Option<u32>> = Vec::with_capacity(n);
        let mut page_end_arr: Vec<Option<u32>> = Vec::with_capacity(n);
        let mut source_spans_arr: Vec<Option<&str>> = Vec::with_capacity(n);
        let mut table_headers_arr: Vec<Option<&str>> = Vec::with_capacity(n);

        for chunk in chunks {
            id_arr.push(chunk.id.as_str());
            text_arr.push(chunk.text.as_str());
            doc_name_arr.push(chunk.doc_name.as_str());
            chunk_idx_arr.push(chunk.chunk_index);
            path_depth_arr.push(chunk.path_depth);
            path_json_arr.push(chunk.path_json.as_deref());
            sentence_window_arr.push(chunk.sentence_window.as_deref());
            symbol_name_arr.push(chunk.symbol_name.as_deref());
            symbol_kind_arr.push(chunk.symbol_kind.as_deref());
            chunk_type_arr.push(chunk.chunk_type.as_deref());
            tags_arr.push(chunk.tags.as_deref());
            source_kind_arr.push(chunk.source_kind.as_deref());
            converter_arr.push(chunk.converter.as_deref());
            page_start_arr.push(chunk.page_start);
            page_end_arr.push(chunk.page_end);
            source_spans_arr.push(chunk.source_spans.as_deref());
            table_headers_arr.push(chunk.table_headers.as_deref());
        }

        let vector_arrays: Vec<Option<Vec<Option<f32>>>> = vectors
            .iter()
            .map(|v| Some(v.iter().map(|x| Some(*x)).collect()))
            .collect();

        let vector_arr = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            vector_arrays.into_iter(),
            dim,
        );

        let batch = RecordBatch::try_new(
            ArrowSchema::new(vec![
                Field::new("id", DataType::Utf8, false),
                Field::new("text", DataType::Utf8, false),
                Field::new("doc_name", DataType::Utf8, false),
                Field::new("chunk_index", DataType::UInt32, false),
                Field::new("path_depth", DataType::UInt32, true),
                Field::new("path_json", DataType::Utf8, true),
                Field::new("sentence_window", DataType::Utf8, true),
                Field::new("symbol_name", DataType::Utf8, true),
                Field::new("symbol_kind", DataType::Utf8, true),
                Field::new("chunk_type", DataType::Utf8, true),
                Field::new("tags", DataType::Utf8, true),
                Field::new("source_kind", DataType::Utf8, true),
                Field::new("converter", DataType::Utf8, true),
                Field::new("page_start", DataType::UInt32, true),
                Field::new("page_end", DataType::UInt32, true),
                Field::new("source_spans", DataType::Utf8, true),
                Field::new("table_headers", DataType::Utf8, true),
                Field::new(
                    "vector",
                    DataType::FixedSizeList(
                        Arc::new(Field::new("item", DataType::Float32, true)),
                        dim,
                    ),
                    true,
                ),
            ])
            .into(),
            vec![
                Arc::new(StringArray::from(id_arr)),
                Arc::new(StringArray::from(text_arr)),
                Arc::new(StringArray::from(doc_name_arr)),
                Arc::new(UInt32Array::from(chunk_idx_arr)),
                Arc::new(UInt32Array::from(path_depth_arr)),
                Arc::new(StringArray::from(path_json_arr)),
                Arc::new(StringArray::from(sentence_window_arr)),
                Arc::new(StringArray::from(symbol_name_arr)),
                Arc::new(StringArray::from(symbol_kind_arr)),
                Arc::new(StringArray::from(chunk_type_arr)),
                Arc::new(StringArray::from(tags_arr)),
                Arc::new(StringArray::from(source_kind_arr)),
                Arc::new(StringArray::from(converter_arr)),
                Arc::new(UInt32Array::from(page_start_arr)),
                Arc::new(UInt32Array::from(page_end_arr)),
                Arc::new(StringArray::from(source_spans_arr)),
                Arc::new(StringArray::from(table_headers_arr)),
                Arc::new(vector_arr),
            ],
        )
        .map_err(|e| format!("构建 RecordBatch 失败: {}", e))?;

        tokio::time::timeout(Duration::from_secs(120), table.add(batch).execute())
            .await
            .map_err(|_| "LanceDB 写入超时 (120s)，请检查磁盘空间或数据一致性".to_string())?
            .map_err(|e| format!("LanceDB 写入失败: {}", e))?;

        // 数据变更后失效符号缓存
        self.invalidate_symbol_cache();
        Ok(())
    }

    /// 向量检索（无预过滤）。
    pub async fn search_vectors(
        &self,
        query: &[f32],
        top_k: u32,
    ) -> Result<Vec<SearchHit>, String> {
        self.search_vectors_impl(query, top_k, None).await
    }

    /// 带 SQL 预过滤的向量检索（**Filter 前置**）。
    ///
    /// `filter_sql` 在 ANN 检索前限定候选行范围（如 `LOWER(doc_name) LIKE '%.rs'`），
    /// 保证被过滤类型外的文档不占用候选池名额——旧"检索后过滤"方案中，
    /// 大量无关文档会把相关候选挤出 `top_k` 窗口，是本项目"查出许多不相关文档"的
    /// 核心根因之一。
    pub async fn search_vectors_with_filter(
        &self,
        query: &[f32],
        top_k: u32,
        filter_sql: &str,
    ) -> Result<Vec<SearchHit>, String> {
        self.search_vectors_impl(query, top_k, Some(filter_sql)).await
    }

    async fn search_vectors_impl(
        &self,
        query: &[f32],
        top_k: u32,
        filter_sql: Option<&str>,
    ) -> Result<Vec<SearchHit>, String> {
        let t0 = std::time::Instant::now();
        let table = self.open_table().await?;
        let open_elapsed = t0.elapsed();

        // 诊断：输出当前向量索引状态（元数据读取，开销极小），
        // 用于确认是否因缺少向量索引而退化为全表暴力扫描
        match table.list_indices().await {
            Ok(indices) => {
                let has_vector = indices
                    .iter()
                    .any(|idx| idx.columns.iter().any(|c| c == "vector"));
                log::info!(
                    "[lance] search_vectors 索引状态: has_vector_index={} indices={}",
                    has_vector,
                    indices
                        .iter()
                        .map(|i| i.columns.join(","))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            Err(e) => log::info!("[lance] search_vectors 读取索引状态失败: {}", e),
        }

        let mut query_builder = table
            .query()
            .nearest_to(query)
            .map_err(|e| format!("查询向量格式错误: {}", e))?
            .distance_type(DistanceType::Cosine)
            .limit(top_k as usize);
        if let Some(sql) = filter_sql {
            query_builder = query_builder.only_if(sql);
        }
        let batches: Vec<arrow_array::RecordBatch> = query_builder
            .execute()
            .await
            .map_err(|e| format!("LanceDB 检索失败: {}", e))?
            .try_collect()
            .await
            .map_err(|e| format!("读取检索结果失败: {}", e))?;
        let query_elapsed = t0.elapsed();

        let mut hits = Vec::new();
        for batch in &batches {
            let texts = batch
                .column_by_name("text")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or("缺少 text 列")?;
            let doc_names = batch
                .column_by_name("doc_name")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or("缺少 doc_name 列")?;
            let chunk_idxs = batch
                .column_by_name("chunk_index")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
                .ok_or("缺少 chunk_index 列")?;
            let distances = batch
                .column_by_name("_distance")
                .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
                .ok_or("缺少 _distance 列")?;

            let path_jsons = batch
                .column_by_name("path_json")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let sentence_windows = batch
                .column_by_name("sentence_window")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let symbol_names = batch
                .column_by_name("symbol_name")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let symbol_kinds = batch
                .column_by_name("symbol_kind")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let chunk_types = batch
                .column_by_name("chunk_type")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            // 🟠 M9：tags 列（旧表迁移前可能缺失 → None，融合后过滤时视为无标签）
            let tags_col = batch
                .column_by_name("tags")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            // Phase 0C：旧表（0C 之前建的表）无这两列 → None，由 stale 判定要求重建
            let source_kind_col = batch
                .column_by_name("source_kind")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let converter_col = batch
                .column_by_name("converter")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            // Phase 1：旧表无这三列 → None
            let page_start_col = batch
                .column_by_name("page_start")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());
            let page_end_col = batch
                .column_by_name("page_end")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());
            let source_spans_col = batch
                .column_by_name("source_spans")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            // §4.7 / R3：旧表无该列 → None（与上面三列同一"缺列即 None"策略）
            let table_headers_col = batch
                .column_by_name("table_headers")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());

            for i in 0..batch.num_rows() {
                let dist = distances.value(i);
                let score: f32 = 1.0 - dist;
                let path_json_val = path_jsons.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                let sentence_window_val = sentence_windows.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                let symbol_name_val = symbol_names.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                let symbol_kind_val = symbol_kinds.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                let chunk_type_val = chunk_types.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                let tags_val = tags_col.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                let source_kind_val = source_kind_col.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                let converter_val = converter_col.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                let page_start_val = page_start_col.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i)) }
                });
                let page_end_val = page_end_col.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i)) }
                });
                let source_spans_val = source_spans_col.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                let table_headers_val = table_headers_col.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                hits.push(SearchHit {
                    text: texts.value(i).to_string(),
                    doc_name: doc_names.value(i).to_string(),
                    chunk_index: chunk_idxs.value(i),
                    score: score.max(0.0),
                    score_vec: score.max(0.0),
                    score_bm25: 0.0,
                    path_json: path_json_val,
                    sentence_window: sentence_window_val,
                    symbol_name: symbol_name_val,
                    symbol_kind: symbol_kind_val,
                    chunk_type: chunk_type_val,
                    tags: tags_val,
                    source_kind: source_kind_val,
                    converter: converter_val,
                    page_start: page_start_val,
                    page_end: page_end_val,
                    source_spans: source_spans_val,
                    table_headers: table_headers_val,
                    score_rerank: None,
                    query_sources: Vec::new(),
                });
            }
        }

        log::info!(
            "[lance] [向量查库结果] open_table={:.3}s query={:.3}s total={:.3}s hits={}",
            open_elapsed.as_secs_f64(),
            query_elapsed.as_secs_f64(),
            t0.elapsed().as_secs_f64(),
            hits.len()
        );
        Ok(hits)
    }

    /// 按代码符号名检索（内存过滤 `symbol_name`），用于代码语义问答。
    ///
    /// 与向量检索互补：向量检索找"语义相关"，此函数精确找"符号定义"所在 chunk。
    /// 只返回代码 chunk（`symbol_name` 非空），按匹配质量（精确 > 前缀 > 包含）排序。
    ///
    /// 性能：符号条目首次查询时全量加载进内存缓存（只读含符号的行），
    /// 后续查询直接内存过滤（毫秒级），避免反复对 LanceDB 做全表 LIKE 扫描
    /// （36k 行量级每次可达数秒）。写操作（add_chunks/delete 等）会自动失效缓存。
    pub async fn search_symbols(
        &self,
        symbol: &str,
        top_k: u32,
    ) -> Result<Vec<SearchHit>, String> {
        let sym = symbol.trim();
        if sym.is_empty() {
            return Ok(Vec::new());
        }
        let entries = self.get_symbol_entries().await?;
        let sym_lower = sym.to_lowercase();

        // (hit, 匹配质量)：0 精确匹配，1 前缀匹配，2 包含匹配
        let mut hits: Vec<(SearchHit, u8)> = Vec::new();
        for e in entries.iter() {
            let sn = e.symbol_name.to_lowercase();
            if !sn.contains(&sym_lower) {
                continue;
            }
            let quality = if sn == sym_lower {
                0u8
            } else if sn.starts_with(&sym_lower) {
                1u8
            } else {
                2u8
            };
            hits.push((
                SearchHit {
                    text: e.text.clone(),
                    doc_name: e.doc_name.clone(),
                    chunk_index: e.chunk_index,
                    score: 0.0,
                    score_vec: 0.0,
                    score_bm25: 0.0,
                    path_json: e.path_json.clone(),
                    sentence_window: e.sentence_window.clone(),
                    symbol_name: Some(e.symbol_name.clone()),
                    symbol_kind: e.symbol_kind.clone(),
                    chunk_type: e.chunk_type.clone(),
                    tags: e.tags.clone(),
                    source_kind: e.source_kind.clone(),
                    converter: e.converter.clone(),
                    page_start: e.page_start,
                    page_end: e.page_end,
                    source_spans: e.source_spans.clone(),
                    table_headers: e.table_headers.clone(),
                    score_rerank: None,
                    query_sources: Vec::new(),
                },
                quality,
            ));
        }

        hits.sort_by_key(|(_, q)| *q);
        hits.truncate(top_k as usize);
        // score 按匹配质量归一（供注入 RRF 融合时参考排序）
        Ok(hits
            .into_iter()
            .enumerate()
            .map(|(i, (mut h, q))| {
                let base = if q == 0 { 0.95 } else if q == 1 { 0.85 } else { 0.7 };
                h.score = (base - i as f32 * 0.02).max(0.1);
                h
            })
            .collect())
    }

    /// 获取代码符号缓存条目（未缓存则全量加载）。
    async fn get_symbol_entries(&self) -> Result<Vec<SymbolEntry>, String> {
        {
            let guard = self.symbol_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref v) = *guard {
                return Ok(v.clone());
            }
        }
        let entries = self.load_symbol_entries().await?;
        *self.symbol_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(entries.clone());
        Ok(entries)
    }

    /// 全量加载符号条目：只读取 `symbol_name` 非空的行及所需列（不含 vector 列）。
    async fn load_symbol_entries(&self) -> Result<Vec<SymbolEntry>, String> {
        let table = self.open_table().await?;
        let batches: Vec<arrow_array::RecordBatch> = table
            .query()
            .only_if("symbol_name IS NOT NULL")
            .select(lancedb::query::Select::columns(&[
                "text",
                "doc_name",
                "chunk_index",
                "symbol_name",
                "symbol_kind",
                "path_json",
                "sentence_window",
                "chunk_type",
                "tags",
                "source_kind",
                "converter",
                "page_start",
                "page_end",
                "source_spans",
                "table_headers",
            ]))
            .execute()
            .await
            .map_err(|e| format!("LanceDB 符号条目加载失败: {}", e))?
            .try_collect()
            .await
            .map_err(|e| format!("读取符号条目失败: {}", e))?;

        let mut entries: Vec<SymbolEntry> = Vec::new();
        for batch in &batches {
            let texts = batch
                .column_by_name("text")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or("缺少 text 列")?;
            let doc_names = batch
                .column_by_name("doc_name")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or("缺少 doc_name 列")?;
            let chunk_idxs = batch
                .column_by_name("chunk_index")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
                .ok_or("缺少 chunk_index 列")?;
            let symbol_names = batch
                .column_by_name("symbol_name")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or("缺少 symbol_name 列")?;
            let symbol_kinds = batch
                .column_by_name("symbol_kind")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let path_jsons = batch
                .column_by_name("path_json")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let sentence_windows = batch
                .column_by_name("sentence_window")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let chunk_types = batch
                .column_by_name("chunk_type")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            // 🟠 M9：tags 列（旧表迁移前可能缺失 → None）
            let tags_col = batch
                .column_by_name("tags")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            // Phase 0C：旧表无这两列 → None
            let source_kind_col = batch
                .column_by_name("source_kind")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let converter_col = batch
                .column_by_name("converter")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let page_start_col = batch
                .column_by_name("page_start")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());
            let page_end_col = batch
                .column_by_name("page_end")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>());
            let source_spans_col = batch
                .column_by_name("source_spans")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let table_headers_col = batch
                .column_by_name("table_headers")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());

            for i in 0..batch.num_rows() {
                if symbol_names.is_null(i) {
                    continue;
                }
                entries.push(SymbolEntry {
                    text: texts.value(i).to_string(),
                    doc_name: doc_names.value(i).to_string(),
                    chunk_index: chunk_idxs.value(i),
                    symbol_name: symbol_names.value(i).to_string(),
                    symbol_kind: symbol_kinds.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                    }),
                    path_json: path_jsons.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                    }),
                    sentence_window: sentence_windows.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                    }),
                    chunk_type: chunk_types.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                    }),
                    tags: tags_col.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                    }),
                    source_kind: source_kind_col.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                    }),
                    converter: converter_col.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                    }),
                    page_start: page_start_col.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i)) }
                    }),
                    page_end: page_end_col.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i)) }
                    }),
                    source_spans: source_spans_col.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                    }),
                    table_headers: table_headers_col.and_then(|arr| {
                        if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                    }),
                });
            }
        }
        log::info!("[lance] 符号缓存加载完成: entries={}", entries.len());
        Ok(entries)
    }

    /// 写操作后失效符号缓存（下次查询自动重新加载）
    fn invalidate_symbol_cache(&self) {
        if let Ok(mut guard) = self.symbol_cache.lock() {
            *guard = None;
        }
    }


    /// **Phase 0C**：读取索引中实际出现的 `(source_kind, converter)` 去重集合。
    ///
    /// 用途：`KbStatus.stale_kinds` 的判定依据——把"索引里真实用的转换器"与
    /// 注册表的**期望值**比对，得到**按文件类型**的过期集合（方案 §5.5）。
    ///
    /// 行为约定：
    /// - `source_kind` 为 NULL 的行（0C 之前的旧索引）**不返回**，由调用方判定为"全部过期"；
    /// - 只 select 两列，不读 vector（避免全量向量解码）。
    pub async fn kind_converter_pairs(
        &self,
    ) -> Result<std::collections::HashSet<(String, String)>, String> {
        let table = self.open_table().await?;
        let batches: Vec<RecordBatch> = table
            .query()
            .select(lancedb::query::Select::columns(&["source_kind", "converter"]))
            .limit(100_000)
            .execute()
            .await
            .map_err(|e| format!("读取 source_kind/converter 失败: {}", e))?
            .try_collect()
            .await
            .map_err(|e| format!("读取 source_kind/converter 失败: {}", e))?;

        let mut pairs = std::collections::HashSet::new();
        for batch in &batches {
            // 旧表缺列 → 视为空集合（调用方按"无 kind 记录"处理）
            let kinds = batch
                .column_by_name("source_kind")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let converters = batch
                .column_by_name("converter")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let (Some(kinds), Some(converters)) = (kinds, converters) else {
                continue;
            };
            for i in 0..batch.num_rows() {
                if kinds.is_null(i) {
                    continue;
                }
                let k = kinds.value(i).to_string();
                let c = if converters.is_null(i) {
                    String::new()
                } else {
                    converters.value(i).to_string()
                };
                pairs.insert((k, c));
            }
        }
        Ok(pairs)
    }

    /// 获取所有已索引的文档名列表（去重）。
    ///
    /// 用于 `index_unindexed` 中批量判断哪些文件已索引，避免逐文件 O(N) 查询。
    pub async fn list_document_names(&self) -> Result<std::collections::HashSet<String>, String> {
        let table = self.open_table().await?;
        let batches: Vec<RecordBatch> = table
            .query()
            .limit(10_000)
            .execute()
            .await
            .map_err(|e| format!("扫描文档名失败: {}", e))?
            .try_collect()
            .await
            .map_err(|e| format!("读取文档名失败: {}", e))?;

        let mut names = std::collections::HashSet::new();
        for batch in &batches {
            let doc_names = batch
                .column_by_name("doc_name")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            if let Some(arr) = doc_names {
                for i in 0..batch.num_rows() {
                    if !arr.is_null(i) {
                        names.insert(arr.value(i).to_string());
                    }
                }
            }
        }
        Ok(names)
    }

    /// 获取指定文档 `[start, end]` 闭区间内的所有 chunks（任意区间版本）。
    ///
    /// 供混合检索 Context 扩展使用：多个命中 chunk 可合并为一次区间查询
    /// （区间并集），避免同文档重复全表扫描。
    ///
    /// 实现说明：lancedb 0.31 的 Query 类型（不带 nearest_to）无 execute() 方法，
    /// 因此仍走零向量 + Cosine 的向量查询；通过 `only_if(doc_name = ...)` 把
    /// 扫描范围限制到单文档行（SQL 预过滤），从根本上避免全表 limit 截断风险
    /// （旧实现 limit(2000) 在行数超过上限时目标 chunk 可能被丢弃）。
    pub async fn fetch_chunks_between(
        &self,
        doc_name: &str,
        start: u32,
        end: u32,
    ) -> Result<Vec<(u32, String, Option<String>)>, String> {
        if end < start {
            return Ok(Vec::new());
        }
        let table = self.open_table().await?;
        let expected = (end - start + 1) as usize;

        // 维度直接从表 schema 读取，无需依赖 embedding 模型（模型不可用时上下文功能仍可用）
        let schema = table
            .schema()
            .await
            .map_err(|e| format!("读取表 schema 失败: {}", e))?;
        let dim = schema
            .fields()
            .iter()
            .find_map(|f| match f.data_type() {
                DataType::FixedSizeList(_, size) => Some(*size as usize),
                _ => None,
            })
            .ok_or_else(|| "无法从表 schema 读取向量维度".to_string())?;
        let query_vec = vec![0.0f32; dim];
        // SQL 单引号转义（doc_name 可能含引号），与搜索路径的过滤语义一致
        let escaped = doc_name.replace('\'', "''");
        let filter_sql = format!("doc_name = '{}'", escaped);
        let batches: Vec<RecordBatch> = table
            .query()
            .nearest_to(query_vec)
            .map_err(|e| format!("nearest_to 失败: {}", e))?
            .only_if(&filter_sql)
            .distance_type(DistanceType::Cosine)
            .limit(5000) // 单文档行规模，5000 上限覆盖超大文档；仍远超典型 chunk 数
            .execute()
            .await
            .map_err(|e| format!("上下文范围查询失败: {}", e))?
            .try_collect()
            .await
            .map_err(|e| format!("读取上下文范围结果失败: {}", e))?;

        let mut results = Vec::new();
        for batch in &batches {
            let doc_names = batch
                .column_by_name("doc_name")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or("缺少 doc_name 列")?;
            let texts = batch
                .column_by_name("text")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or("缺少 text 列")?;
            let chunk_idxs = batch
                .column_by_name("chunk_index")
                .and_then(|c| c.as_any().downcast_ref::<UInt32Array>())
                .ok_or("缺少 chunk_index 列")?;
            let path_jsons = batch
                .column_by_name("path_json")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());

            for i in 0..batch.num_rows() {
                if doc_names.value(i) != doc_name { continue; }
                let idx = chunk_idxs.value(i);
                if idx < start || idx > end { continue; }
                let path_json_val = path_jsons.and_then(|arr| {
                    if arr.is_null(i) { None } else { Some(arr.value(i).to_string()) }
                });
                results.push((idx, texts.value(i).to_string(), path_json_val));
                if results.len() >= expected { break; }
            }
            if results.len() >= expected { break; }
        }

        results.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(results)
    }

    /// 仅删除当前表（不删除数据目录），用于知识库重新索引时保留对话索引数据
    pub async fn drop_table_only(&self) -> Result<(), String> {
        let db = self.get_connection().await?;
        let _ = db.drop_table(&self.table_name, &[]).await;
        // 重置连接缓存与符号缓存
        let mut guard = self.db.lock().await;
        *guard = None;
        self.invalidate_symbol_cache();
        Ok(())
    }

    /// 删除指定文档的所有块
    ///
    /// 注意：doc_name 由 Rust 端内部生成（文件相对路径），不直接来自前端输入。
    /// LanceDB delete 接口只接受字符串谓词，这里做严格的转义防止边界情况。
    pub async fn delete_document(&self, doc_name: &str) -> Result<(), String> {
        if doc_name.is_empty() {
            return Err("doc_name 不能为空".into());
        }
        // 拒绝控制字符，防止 SQL 谓词注入
        if doc_name.chars().any(|c| c.is_control()) {
            return Err("doc_name 包含非法字符".into());
        }
        let table = self.open_table().await?;
        let escaped = escape_sql_string(doc_name);
        let predicate = format!("doc_name = '{}'", escaped);
        table
            .delete(&predicate)
            .await
            .map_err(|e| format!("删除文档失败: {}", e))?;
        self.invalidate_symbol_cache();
        Ok(())
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个含全部 provenance/元数据列的 chunk（其余列用占位值）
    fn sample_chunk(id: &str) -> DocumentChunk {
        DocumentChunk {
            id: id.to_string(),
            doc_name: "表.md".to_string(),
            chunk_index: 0,
            text: "| Postcode | Sales_Rep_Name |\n|---|---|\n| 2121 | Jane |".to_string(),
            path_depth: None,
            path_json: Some(r#"["销售"]"#.to_string()),
            sentence_window: None,
            symbol_name: None,
            symbol_kind: None,
            embedding_text: None,
            chunk_type: Some("table".to_string()),
            source_kind: Some("markdown".to_string()),
            converter: Some("native@1".to_string()),
            page_start: Some(2),
            page_end: Some(3),
            source_spans: Some(r#"[{"page":2,"line_start":1,"line_end":3}]"#.to_string()),
            table_headers: Some(r#"["Postcode","Sales_Rep_Name"]"#.to_string()),
            doc_title: None,
            tags: None,
        }
    }

    /// **LanceDB 落库往返**（Plan B v2 的 provenance/元数据列）：
    /// 这条路径此前**完全没有测试**，而它是"schema 字段顺序 ↔ RecordBatch 数组顺序"
    /// 必须严格对齐的地方——顺序错位（尤其是相邻的两个 Utf8 列互换）不会编译报错，
    /// 只会在运行时静默把 A 列的值写进 B 列。
    ///
    /// 需要本地 embedding 模型只为拿到建表维度（`create_table` 的 384 维来自模型）；
    /// 模型不可用时跳过并说明原因，避免把环境依赖带进日常 `cargo test`。
    #[tokio::test]
    async fn chunk_metadata_round_trips_through_lancedb() {
        if crate::core::db::utils::get_local_embedding_dimension().is_err() {
            eprintln!("[skip] 本地 embedding 模型不可用，跳过 LanceDB 往返测试");
            return;
        }
        let dim = crate::core::db::utils::get_local_embedding_dimension().unwrap() as usize;

        let dir = std::env::temp_dir().join("mdgo_lance_roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let store = LanceStore::new(&dir.to_string_lossy(), "vectors");
        store.create_table().await.expect("建表");

        let chunk = sample_chunk("row1");
        let vectors = vec![vec![0.01f32; dim]];
        store.add_chunks(&[chunk.clone()], &vectors).await.expect("写入 chunks");

        // 读回全部列，逐列核对（含 vector 列，确保数组与 schema 仍对齐）
        let table = store.open_table().await.expect("打开表");
        let batches: Vec<arrow_array::RecordBatch> = table
            .query()
            .execute()
            .await
            .expect("查询")
            .try_collect()
            .await
            .expect("收集");
        assert_eq!(batches.len(), 1, "应恰好一个 batch");
        let batch = &batches[0];
        assert_eq!(batch.num_rows(), 1, "应恰好一行");

        let s = |col: &str| -> Option<String> {
            batch
                .column_by_name(col)
                .and_then(|c| c.as_any().downcast_ref::<arrow_array::StringArray>())
                .map(|a| a.value(0).to_string())
        };

        // 关键：这几列都是 Utf8 且相邻，最容易被"顺序错位"写串
        assert_eq!(s("chunk_type").as_deref(), Some("table"), "chunk_type 列串位");
        assert_eq!(s("source_kind").as_deref(), Some("markdown"), "source_kind 列串位");
        assert_eq!(s("converter").as_deref(), Some("native@1"), "converter 列串位");
        assert_eq!(
            s("source_spans").as_deref(),
            Some(r#"[{"page":2,"line_start":1,"line_end":3}]"#),
            "source_spans 列串位"
        );
        assert_eq!(
            s("table_headers").as_deref(),
            Some(r#"["Postcode","Sales_Rep_Name"]"#),
            "table_headers 列串位（§4.7 / R3）"
        );

        let u = |col: &str| -> Option<u32> {
            batch
                .column_by_name(col)
                .and_then(|c| c.as_any().downcast_ref::<arrow_array::UInt32Array>())
                .map(|a| a.value(0))
        };
        assert_eq!(u("page_start"), Some(2), "page_start 列串位");
        assert_eq!(u("page_end"), Some(3), "page_end 列串位");

        // vector 列必须仍在最后且维度正确（数组顺序与 schema 对齐的最终凭据）
        let vec_col = batch.column_by_name("vector").expect("vector 列存在");
        assert_eq!(
            vec_col.len(),
            1,
            "vector 列行数不符（数组整体错位的典型症状）"
        );
    }
}

// 别名，用于构建 Schema 时避免歧义
use arrow_schema::Schema as ArrowSchema;

// 导入 Stream 扩展
use futures::TryStreamExt;
