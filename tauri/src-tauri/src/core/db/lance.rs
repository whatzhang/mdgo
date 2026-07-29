use arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray, UInt32Array,
    types::Float32Type,
};
use arrow_schema::{DataType, Field, Schema};
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
}

pub struct LanceStore {
    uri: String,
    table_name: String,
    /// 缓存连接，同一实例内复用（解决 C4）
    db: Mutex<Option<lancedb::connection::Connection>>,
}

impl LanceStore {
    pub fn new(base_uri: &str, table_name: &str) -> Self {
        Self {
            uri: base_uri.to_string(),
            table_name: table_name.to_string(),
            db: Mutex::new(None),
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
            return Ok(());
        }

        // 表不存在 → 创建新表（固定 384 维）
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
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    get_local_embedding_dimension() as i32,
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
                Arc::new(vector_arr),
            ],
        )
        .map_err(|e| format!("构建 RecordBatch 失败: {}", e))?;

        tokio::time::timeout(Duration::from_secs(120), table.add(batch).execute())
            .await
            .map_err(|_| "LanceDB 写入超时 (120s)，请检查磁盘空间或数据一致性".to_string())?
            .map_err(|e| format!("LanceDB 写入失败: {}", e))?;

        Ok(())
    }

    /// 向量检索
    pub async fn search_vectors(
        &self,
        query: &[f32],
        top_k: u32,
    ) -> Result<Vec<SearchHit>, String> {
        let table = self.open_table().await?;

        let batches: Vec<arrow_array::RecordBatch> = table
            .query()
            .nearest_to(query)
            .map_err(|e| format!("查询向量格式错误: {}", e))?
            .distance_type(DistanceType::Cosine)
            .limit(top_k as usize)
            .execute()
            .await
            .map_err(|e| format!("LanceDB 检索失败: {}", e))?
            .try_collect()
            .await
            .map_err(|e| format!("读取检索结果失败: {}", e))?;

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
                });
            }
        }

        Ok(hits)
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

    /// 获取指定文档中某个 chunk 的上下文窗口（前后相邻 chunks）。
    ///
    /// 用于后检索上下文扩展：命中某个 chunk 后，将 ±window 范围内的相邻 chunks
    /// 一并拉取作为上下文，使 LLM 获得更完整的文档内容。
    ///
    /// 返回 `(chunk_index, text, path_json)` 列表，按 chunk_index 升序排列。
    /// 适用于 Markdown（按文档顺序分块）、OPML/FreeMind（DFS 遍历顺序）所有文档类型。
    pub async fn fetch_chunks_in_range(
        &self,
        doc_name: &str,
        center_index: u32,
        window: u32,
    ) -> Result<Vec<(u32, String, Option<String>)>, String> {
        let table = self.open_table().await?;
        let start = center_index.saturating_sub(window);
        let end = center_index + window;
        // 最大期望结果数
        let expected = (window * 2 + 1) as usize;

        // 使用向量检索 + 大范围 top_k，在内存中按 doc_name + chunk_index 过滤。
        // 原因：lancedb 0.31 的 Query 类型（不带 nearest_to）无 execute() 方法。
        // 使用零向量 + Cosine 距离，所有结果分数 ≈ 1.0，不依赖向量质量。
        let dim = get_local_embedding_dimension() as usize;
        let query_vec = vec![0.0f32; dim];
        let batches: Vec<RecordBatch> = table
            .query()
            .nearest_to(query_vec)
            .map_err(|e| format!("nearest_to 失败: {}", e))?
            .distance_type(DistanceType::Cosine)
            .limit(2000) // 拉取较大候选池，确保目标范围 chunks 被覆盖
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
        // 重置连接缓存
        let mut guard = self.db.lock().await;
        *guard = None;
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
        Ok(())
    }

}

// 别名，用于构建 Schema 时避免歧义
use arrow_schema::Schema as ArrowSchema;

// 导入 Stream 扩展
use futures::TryStreamExt;
