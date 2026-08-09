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
    /// 精排分数（本地 bge-reranker sigmoid 相关性分数，仅精排启用时有值）
    pub score_rerank: Option<f32>,
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
    /// 如果表已存在则直接返回（无需检查维度——全局统一使用本地模型）。
    pub async fn create_table(&self) -> Result<(), String> {
        let db = self.get_connection().await?;

        // 表已存在 → 迁移新列（向后兼容）
        let open_result = tokio::time::timeout(
            Duration::from_secs(30),
            db.open_table(&self.table_name).execute(),
        )
        .await;
        if let Ok(Ok(table)) = open_result {
            let _ = Self::migrate_add_column(&table, "path_depth", DataType::UInt32).await;
            let _ = Self::migrate_add_column(&table, "path_json", DataType::Utf8).await;
            let _ = Self::migrate_add_column(&table, "sentence_window", DataType::Utf8).await;
            let _ = Self::migrate_add_column(&table, "symbol_name", DataType::Utf8).await;
            let _ = Self::migrate_add_column(&table, "symbol_kind", DataType::Utf8).await;
            let _ = Self::migrate_add_column(&table, "chunk_type", DataType::Utf8).await;
            // 兼容旧表：补建向量索引（已有索引则瞬间跳过；构建失败不阻断，仅影响检索性能）
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
            log::debug!("[lance] 向量表为空，跳过索引创建");
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

    /// 尝试为已有表添加新列（兼容旧版本创建的 schema）
    async fn migrate_add_column(table: &lancedb::Table, name: &str, dtype: DataType) {
        use lancedb::table::NewColumnTransform;
        // 构建新列 schema（仅包含要添加的列）
        let new_schema = Arc::new(ArrowSchema::new(vec![Field::new(name, dtype.clone(), true)]));
        let transform = NewColumnTransform::AllNulls(new_schema);
        let _ = table.add_columns(transform, None).await;
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
                log::debug!(
                    "[lance] search_vectors 索引状态: has_vector_index={} indices={}",
                    has_vector,
                    indices
                        .iter()
                        .map(|i| i.columns.join(","))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            Err(e) => log::debug!("[lance] search_vectors 读取索引状态失败: {}", e),
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
                    score_rerank: None,
                });
            }
        }

        log::debug!(
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
                    score_rerank: None,
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
                });
            }
        }
        log::debug!("[lance] 符号缓存加载完成: entries={}", entries.len());
        Ok(entries)
    }

    /// 写操作后失效符号缓存（下次查询自动重新加载）
    fn invalidate_symbol_cache(&self) {
        if let Ok(mut guard) = self.symbol_cache.lock() {
            *guard = None;
        }
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

// 别名，用于构建 Schema 时避免歧义
use arrow_schema::Schema as ArrowSchema;

// 导入 Stream 扩展
use futures::TryStreamExt;
