//! embedding 结果持久缓存（P0-5）。
//!
//! 目标：增量索引只对**内容变化**的 chunk 重新推理。
//!
//! ```text
//! 文件修改 → 重新分块 → 内容哈希 → 缓存命中（未变化）→ 跳过 embedding
//! ```
//!
//! 键 = `model|dimension|content_hash`（`content_hash` = 最终送进 embedding 的文本的
//! 稳定 FNV-1a 128 哈希）。模型/维度变化 → 键变化 → 自然失效；分块参数变化 →
//! 文本本身变化 → 哈希变化 → 自然失效。**缓存正确性不依赖人工失效**。
//!
//! 存储：`{dir}/.mdgo/embedding_cache.sqlite`（SQLite 单表，按最旧裁剪，
//! 上限 [`CACHE_MAX_ENTRIES`] 条 ≈ 150MB）。索引由 `indexing_lock` 串行化，无并发写。

use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Mutex;

/// 缓存最大条目数（~100k × 384-dim × 4B ≈ 150MB；超出按 created_at 最旧裁剪）
const CACHE_MAX_ENTRIES: usize = 100_000;

/// 单条 SQL 的最大占位符数（🟠 L7：SQLite 默认变量上限 999，留余量防边界）
const GET_BATCH_SIZE: usize = 500;

/// embedding 结果缓存
pub struct EmbeddingCache {
    conn: Mutex<Connection>,
}

impl EmbeddingCache {
    /// 打开（或创建）缓存。`dir` 不存在时自动创建目录。
    pub fn open(dir: &str) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("创建 embedding 缓存目录失败 ({}): {}", dir, e))?;
        let path = std::path::Path::new(dir).join("embedding_cache.sqlite");
        let conn = Connection::open(&path)
            .map_err(|e| format!("打开 embedding 缓存失败 ({}): {}", path.display(), e))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS embedding_cache (
                cache_key TEXT PRIMARY KEY,
                vector BLOB NOT NULL,
                created_at INTEGER NOT NULL
            );
            -- 🟠 L12：created_at 索引（裁剪查询 ORDER BY 不再全表排序）
            CREATE INDEX IF NOT EXISTS idx_embedding_cache_created ON embedding_cache(created_at);",
        )
        .map_err(|e| format!("初始化 embedding 缓存表失败: {}", e))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 🟠 L13：按目录复用的进程级缓存连接——全量索引几百个批次不再几百次
    /// `open` + `create_dir_all`。模型名按需实时计算（`key()`），模型初始化后
    /// 自动切换到真实键前缀，连接复用不引入「回退键永久化」问题。
    pub fn open_shared(dir: &str) -> Result<std::sync::Arc<EmbeddingCache>, String> {
        use std::collections::HashMap as Map;
        use std::sync::OnceLock;
        static REGISTRY: OnceLock<Mutex<Map<String, std::sync::Arc<EmbeddingCache>>>> =
            OnceLock::new();
        let registry = REGISTRY.get_or_init(|| Mutex::new(Map::new()));
        let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = guard.get(dir) {
            return Ok(std::sync::Arc::clone(c));
        }
        let cache = std::sync::Arc::new(EmbeddingCache::open(dir)?);
        guard.insert(dir.to_string(), std::sync::Arc::clone(&cache));
        Ok(cache)
    }

    /// 当前模型键前缀（`model|dimension`）。**按需实时计算**——模型首次初始化前
    /// 返回回退名（键 miss，无害）；初始化后自动用真实名，连接复用不受影响。
    fn model_key_now(&self) -> String {
        format!(
            "{}|{}",
            crate::core::embedding::get_model_name(),
            crate::core::embedding::get_embedding_dimension()
        )
    }

    /// 内容哈希：对**最终送进 embedding 的文本**（`embedding_text` 或 `text`）做稳定哈希
    pub fn content_hash(text: &str) -> String {
        crate::core::db::utils::stable_hash_hex(text)
    }

    /// 缓存键：`model|dim|content_hash`
    pub fn key(&self, text_hash: &str) -> String {
        format!("{}|{}", self.model_key_now(), text_hash)
    }

    /// 批量读取；缺失的键不在结果中（🟠 L7：分批 ≤500 键，避免占位符超 SQLite 上限）
    pub fn get_many(&self, keys: &[String]) -> Result<HashMap<String, Vec<f32>>, String> {
        if keys.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut map = HashMap::new();
        for chunk in keys.chunks(GET_BATCH_SIZE) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT cache_key, vector FROM embedding_cache WHERE cache_key IN ({})",
                placeholders
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| format!("embedding 缓存查询失败: {}", e))?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    let key: String = row.get(0)?;
                    let blob: Vec<u8> = row.get(1)?;
                    Ok((key, blob))
                })
                .map_err(|e| format!("embedding 缓存读取失败: {}", e))?;
            for (key, blob) in rows.flatten() {
                let floats = blob
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect::<Vec<f32>>();
                map.insert(key, floats);
            }
        }
        Ok(map)
    }

    /// 批量写入（INSERT OR REPLACE；超上限按最旧裁剪）
    pub fn put_many(&self, entries: &[(String, Vec<f32>)]) -> Result<(), String> {
        if entries.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("embedding 缓存事务开启失败: {}", e))?;
        for (key, vec) in entries {
            let blob: Vec<u8> = vec.iter().flat_map(|f| f.to_le_bytes()).collect();
            tx.execute(
                "INSERT OR REPLACE INTO embedding_cache (cache_key, vector, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![key, blob, now],
            )
            .map_err(|e| format!("embedding 缓存写入失败: {}", e))?;
        }
        // 超上限裁剪最旧（INSERT OR REPLACE 刷新 created_at，近似 LRU）
        let over: i64 = tx
            .query_row(
                "SELECT COUNT(*) - ?1 FROM embedding_cache",
                rusqlite::params![CACHE_MAX_ENTRIES as i64],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if over > 0 {
            let _ = tx.execute(
                "DELETE FROM embedding_cache WHERE cache_key IN (
                    SELECT cache_key FROM embedding_cache ORDER BY created_at ASC LIMIT ?1
                )",
                rusqlite::params![over],
            );
        }
        tx.commit()
            .map_err(|e| format!("embedding 缓存提交失败: {}", e))?;
        Ok(())
    }

    /// 清空缓存（模型变更/手动重建时调用）
    pub fn clear(&self) -> Result<(), String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute("DELETE FROM embedding_cache", [])
            .map_err(|e| format!("embedding 缓存清空失败: {}", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_overwrite_clear_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = EmbeddingCache::open(dir.path().to_str().unwrap()).unwrap();
        let key1 = cache.key(&EmbeddingCache::content_hash("文本一"));
        let key2 = cache.key(&EmbeddingCache::content_hash("文本二"));

        cache
            .put_many(&[
                (key1.clone(), vec![1.0, 2.0, 3.0]),
                (key2.clone(), vec![4.0, 5.0, 6.0]),
            ])
            .unwrap();
        let got = cache
            .get_many(&[key1.clone(), key2.clone(), "不存在|键".into()])
            .unwrap();
        assert_eq!(got.get(&key1).unwrap(), &vec![1.0, 2.0, 3.0]);
        assert_eq!(got.get(&key2).unwrap(), &vec![4.0, 5.0, 6.0]);
        assert!(!got.contains_key("不存在|键"), "缺失键不应返回");

        // 覆盖写（INSERT OR REPLACE）
        cache.put_many(&[(key1.clone(), vec![9.0, 9.0, 9.0])]).unwrap();
        let got2 = cache
            .get_many(std::slice::from_ref(&key1))
            .unwrap();
        assert_eq!(got2.get(&key1).unwrap(), &vec![9.0, 9.0, 9.0]);

        // 清空
        cache.clear().unwrap();
        let got3 = cache.get_many(&[key1.clone(), key2.clone()]).unwrap();
        assert!(got3.is_empty(), "清空后应无命中");
    }

    #[test]
    fn key_contains_model_dim_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cache = EmbeddingCache::open(dir.path().to_str().unwrap()).unwrap();
        let k = cache.key("abc123");
        // model|dim|hash 三段
        assert_eq!(k.matches('|').count(), 2, "键格式应为 model|dim|hash: {}", k);
        assert!(k.ends_with("|abc123"));
        // 同内容哈希稳定
        assert_eq!(
            EmbeddingCache::content_hash("相同文本"),
            EmbeddingCache::content_hash("相同文本")
        );
        assert_ne!(
            EmbeddingCache::content_hash("相同文本"),
            EmbeddingCache::content_hash("不同文本")
        );
    }

    #[test]
    fn empty_batches_noop() {
        let dir = tempfile::tempdir().unwrap();
        let cache = EmbeddingCache::open(dir.path().to_str().unwrap()).unwrap();
        assert!(cache.get_many(&[]).unwrap().is_empty());
        assert!(cache.put_many(&[]).is_ok());
    }
}
