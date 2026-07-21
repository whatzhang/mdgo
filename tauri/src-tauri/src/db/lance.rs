use arrow_array::{
    FixedSizeListArray, Float32Array, RecordBatch, StringArray, UInt32Array,
    types::Float32Type,
};
use arrow_schema::{DataType, Field, Schema};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

fn escape_sql_string(s: &str) -> String {
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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SearchHit {
    pub text: String,
    pub doc_name: String,
    pub chunk_index: u32,
    pub score: f32,
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

    /// 连接数据库并创建向量表（首次索引时调用）
    pub async fn create_table(&self, dimension: u32) -> Result<(), String> {
        let db = self.get_connection().await?;

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("text", DataType::Utf8, false),
            Field::new("doc_name", DataType::Utf8, false),
            Field::new("chunk_index", DataType::UInt32, false),
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    dimension as i32,
                ),
                true,
            ),
        ]));

        // 先尝试打开已有表，不存在则创建
        if db.open_table(&self.table_name).execute().await.is_ok() {
            return Ok(());
        }

        db.create_empty_table(&self.table_name, schema)
            .execute()
            .await
            .map_err(|e| format!("LanceDB 创建表失败: {}", e))?;

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

    /// 批量写入文档块 + 向量，含维度校验（解决 M4）
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

        // 校验所有向量维度一致（解决 M4）
        if dim == 0 {
            return Err("向量维度为 0，请检查 Embedding 模型配置".into());
        }
        for (i, v) in vectors.iter().enumerate() {
            if v.len() as i32 != dim {
                return Err(format!(
                    "向量维度不一致: 第 0 个维度 {}，第 {} 个维度 {}",
                    dim,
                    i,
                    v.len()
                ));
            }
        }

        let table = self.open_table().await?;

        // 构建 RecordBatch
        let mut id_arr = Vec::with_capacity(n);
        let mut text_arr = Vec::with_capacity(n);
        let mut doc_name_arr = Vec::with_capacity(n);
        let mut chunk_idx_arr = Vec::with_capacity(n);

        for chunk in chunks {
            id_arr.push(chunk.id.as_str());
            text_arr.push(chunk.text.as_str());
            doc_name_arr.push(chunk.doc_name.as_str());
            chunk_idx_arr.push(chunk.chunk_index);
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
                Arc::new(vector_arr),
            ],
        )
        .map_err(|e| format!("构建 RecordBatch 失败: {}", e))?;

        table
            .add(batch)
            .execute()
            .await
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

            for i in 0..batch.num_rows() {
                let dist = distances.value(i);
                let score: f32 = 1.0 - dist;
                hits.push(SearchHit {
                    text: texts.value(i).to_string(),
                    doc_name: doc_names.value(i).to_string(),
                    chunk_index: chunk_idxs.value(i),
                    score: score.max(0.0),
                });
            }
        }

        Ok(hits)
    }

    /// 清空表
    ///
    /// 使用 drop + 重建的方式替代逐行删除，速度提升几个数量级。
    /// `table.delete("true")` 是 O(N) 且不释放磁盘空间，大数据量时极慢。
    pub async fn clear(&self) -> Result<(), String> {
        let db = self.get_connection().await?;
        let _ = db.drop_table(&self.table_name, &[]).await;
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
        if doc_name.contains('\x00') {
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
