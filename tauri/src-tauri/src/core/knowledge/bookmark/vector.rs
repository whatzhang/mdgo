//! `bookmark_vectors`：LanceDB 独立向量表（`{知识库}/.mdgo/lancedb/`，与文档 `vectors` 同目录不同表）。
//!
//! 独立建表原因：文档检索管线强绑定文件扩展名语义（`ext_filter_sql` 等），
//! 书签向量混入 `vectors` 表会被误过滤；独立表零回归。
//!
//! 写入策略：**增量 upsert**（`upsert_batch` 按 bookmark_id 覆盖，避免全量替换）。

use std::collections::HashSet;
use std::sync::Arc;

use arrow_array::{Array, FixedSizeListArray, RecordBatch, Float32Array, StringArray};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use futures_util::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;

pub const BOOKMARK_VECTORS_TABLE: &str = "bookmark_vectors";

/// 向量检索命中
#[derive(Debug, Clone, serde::Serialize)]
pub struct BookmarkVectorHit {
    pub bookmark_id: String,
    pub distance: f32,
}

fn table_schema(dim: i32) -> ArrowSchema {
    ArrowSchema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("bookmark_id", DataType::Utf8, false),
        // 冗余存储实际送入模型的 embedding_text，便于人工核对与重建
        Field::new("text", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            true,
        ),
    ])
}

async fn open_table(uri: &str, dim: i32) -> Result<lancedb::Table, String> {
    let conn = lancedb::connect(uri)
        .execute()
        .await
        .map_err(|e| format!("连接书签向量库失败: {}", e))?;
    match conn.open_table(BOOKMARK_VECTORS_TABLE).execute().await {
        Ok(t) => Ok(t),
        Err(_) => {
            conn.create_empty_table(BOOKMARK_VECTORS_TABLE, table_schema(dim).into())
                .execute()
                .await
                .map_err(|e| format!("创建书签向量表失败: {}", e))?;
            conn.open_table(BOOKMARK_VECTORS_TABLE)
                .execute()
                .await
                .map_err(|e| format!("打开书签向量表失败: {}", e))
        }
    }
}

/// 增量 upsert 批次：按 `bookmark_id` 覆盖旧行（delete old + append new）。
/// 每次只处理本批新 READY 的一小批，避免大规模书签下反复全表重建。
pub async fn upsert_batch(
    uri: &str,
    rows: Vec<(String, String, String, Vec<f32>)>,
) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    let dim = rows[0].3.len() as i32;
    let conn = lancedb::connect(uri)
        .execute()
        .await
        .map_err(|e| format!("连接书签向量库失败: {}", e))?;

    // 1. 确保表存在
    let table = ensure_table(&conn, dim).await?;

    // 2. 先删除本批 bookmark_id 的旧行（增量覆盖语义）
    //    LanceDB delete 只接受字符串谓词；bookmark_id 为内部生成(bm_...)，转义后拼接 IN 列表
    let pred_ids: Vec<String> = {
        let mut seen = HashSet::new();
        rows.iter()
            .filter_map(|(_, bid, _, _)| {
                if seen.insert(bid.clone()) {
                    Some(format!(
                        "'{}'",
                        crate::core::db::lance::escape_sql_string(bid)
                    ))
                } else {
                    None
                }
            })
            .collect()
    };
    if !pred_ids.is_empty() {
        let predicate = format!("bookmark_id IN ({})", pred_ids.join(","));
        table
            .delete(&predicate)
            .await
            .map_err(|e| format!("删除旧书签向量失败: {}", e))?;
    }

    // 3. append 新行
    let n = rows.len();
    let mut id_arr = Vec::with_capacity(n);
    let mut bookmark_id_arr = Vec::with_capacity(n);
    let mut text_arr = Vec::with_capacity(n);
    for (id, bid, text, _) in &rows {
        id_arr.push(id.as_str());
        bookmark_id_arr.push(bid.as_str());
        text_arr.push(text.as_str());
    }
    let vector_arrays: Vec<Option<Vec<Option<f32>>>> = rows
        .iter()
        .map(|(_, _, _, v)| Some(v.iter().map(|x| Some(*x)).collect()))
        .collect();
    let vector_arr = FixedSizeListArray::from_iter_primitive::<arrow_array::types::Float32Type, _, _>(
        vector_arrays.into_iter(),
        dim,
    );
    let batch = RecordBatch::try_new(
        table_schema(dim).into(),
        vec![
            Arc::new(StringArray::from(id_arr)),
            Arc::new(StringArray::from(bookmark_id_arr)),
            Arc::new(StringArray::from(text_arr)),
            Arc::new(vector_arr),
        ],
    )
    .map_err(|e| format!("构建书签向量 RecordBatch 失败: {}", e))?;
    table
        .add(batch)
        .execute()
        .await
        .map_err(|e| format!("写入书签向量失败: {}", e))?;
    log::info!(
        "[bookmark] LanceDB 增量 upsert 完成：{} 条（覆盖 {} 个书签）",
        rows.len(),
        pred_ids.len()
    );
    Ok(())
}

async fn ensure_table(conn: &lancedb::Connection, dim: i32) -> Result<lancedb::Table, String> {
    match conn.open_table(BOOKMARK_VECTORS_TABLE).execute().await {
        Ok(t) => Ok(t),
        Err(_) => {
            conn.create_empty_table(BOOKMARK_VECTORS_TABLE, table_schema(dim).into())
                .execute()
                .await
                .map_err(|e| format!("创建书签向量表失败: {}", e))?;
            conn.open_table(BOOKMARK_VECTORS_TABLE)
                .execute()
                .await
                .map_err(|e| format!("打开书签向量表失败: {}", e))
        }
    }
}

/// 向量检索（cosine，top-k）。返回 bookmark_id + 距离。
pub async fn search(
    uri: &str,
    query_vec: &[f32],
    top_k: u32,
) -> Result<Vec<BookmarkVectorHit>, String> {
    let dim = crate::core::embedding::get_embedding_dimension() as i32;
    let table = match open_table(uri, dim).await {
        Ok(t) => t,
        Err(_) => return Ok(Vec::new()), // 无向量数据（未 READY）→ 空结果，LIKE 兜底
    };
    let batches: Vec<RecordBatch> = table
        .query()
        .nearest_to(query_vec)
        .map_err(|e| format!("书签向量查询格式错误: {}", e))?
        .distance_type(DistanceType::Cosine)
        .limit(top_k as usize)
        .execute()
        .await
        .map_err(|e| format!("书签向量检索失败: {}", e))?
        .try_collect()
        .await
        .map_err(|e| format!("读取书签向量结果失败: {}", e))?;
    let mut out = Vec::new();
    for batch in batches {
        let ids = batch
            .column_by_name("bookmark_id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>());
        let dists = batch.column_by_name("_distance");
        if let Some(ids) = ids {
            let n = ids.len();
            for i in 0..n {
                let distance = dists
                    .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
                    .map(|a| a.value(i))
                    .unwrap_or(0.0);
                out.push(BookmarkVectorHit {
                    bookmark_id: ids.value(i).to_string(),
                    distance,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_of(dim: usize, seed: f32) -> Vec<f32> {
        (0..dim).map(|i| seed + i as f32).collect()
    }

    /// 统计表中行数（供 upsert 覆盖语义断言）
    async fn count_rows(uri: &str, dim: i32) -> usize {
        let table = match open_table(uri, dim).await {
            Ok(t) => t,
            Err(_) => return 0,
        };
        let batches: Vec<RecordBatch> = table
            .query()
            .execute()
            .await
            .expect("查询失败")
            .try_collect()
            .await
            .expect("收集失败");
        batches.iter().map(|b| b.num_rows()).sum()
    }

    /// 增量 upsert：首次写入 append，再次覆盖同一 bookmark_id 时应删除旧行再插入（行数不变）。
    #[tokio::test]
    async fn upsert_batch_appends_then_overwrites_same_bookmark() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let uri = dir.path().to_string_lossy().to_string();
        let dim = 4i32;

        // 首次写入两个书签
        upsert_batch(
            &uri,
            vec![
                ("r1".into(), "bm1".into(), "text1".into(), vec_of(4, 1.0)),
                ("r2".into(), "bm2".into(), "text2".into(), vec_of(4, 2.0)),
            ],
        )
        .await
        .unwrap();
        assert_eq!(count_rows(&uri, dim).await, 2, "首次写入 append 两行");

        // 再次覆盖 bm2（同 bookmark_id、新 id 与新向量）→ 行数仍为 2（删除旧 bm2 + 插入新）
        upsert_batch(
            &uri,
            vec![("r3".into(), "bm2".into(), "text2v2".into(), vec_of(4, 2.5))],
        )
        .await
        .unwrap();
        let rows = count_rows(&uri, dim).await;
        assert_eq!(rows, 2, "覆盖同一 bookmark_id 不应新增行（删除旧 + 追加新）");
    }
}
