//! 书签检索：LIKE 关键词为主（单表直扫，检索目的仅 URL/摘要/分类/标签），
//! READY 书签的向量召回作为补位（语义兜底）。
//!
//! Send 约束：`search_with_vectors` 为关联函数，接收 `&Mutex<BookmarkStore>`，
//! 锁只在同步段内短暂持有（LIKE 查询 / 详情补位），**锁不跨 await**——
//! 因 `BookmarkStore`（rusqlite Connection 含 RefCell）非 `Sync`，
//! 持 `MutexGuard` 跨 await 会导致 Tauri 命令 future 非 `Send`。

use std::sync::Mutex;

use super::{BookmarkSearchHit, BookmarkStore};

impl BookmarkStore {
    /// 关键词检索（同步；命令层/工具层调用）。LIKE 匹配 title/summary/category/tags，
    /// 可选 category / folder 过滤。failed/dead 书签同样可检索（结果带状态，调用方自行判断）。
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        category: Option<&str>,
        folder: Option<&str>,
    ) -> Result<Vec<BookmarkSearchHit>, String> {
        let mut sql = String::from(
            "SELECT id, title, url, summary, tags, category, status, dead FROM bookmarks
             WHERE (title LIKE ? ESCAPE ? OR summary LIKE ? ESCAPE ? OR category LIKE ? ESCAPE ? OR tags LIKE ? ESCAPE ?)",
        );
        // 全部使用无名占位符 `?`（rusqlite 顺序绑定）
        let mut vals: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let kw = format!("%{}%", super::escape_like(query));
        for _ in 0..4 {
            vals.push(Box::new(kw.clone()));
            vals.push(Box::new("\\".to_string()));
        }
        if let Some(f) = folder {
            if !f.is_empty() {
                sql.push_str(" AND browser_folder LIKE ? ESCAPE ?");
                vals.push(Box::new(format!("{}%", super::escape_like(f))));
                vals.push(Box::new("\\".to_string()));
            }
        }
        if let Some(c) = category {
            if !c.is_empty() {
                sql.push_str(" AND category = ?");
                vals.push(Box::new(c.to_string()));
            }
        }
        sql.push_str(" ORDER BY updated_at DESC LIMIT ?");
        vals.push(Box::new(limit.min(20) as i64));

        let mut stmt = self
            .conn
            .prepare_cached(&sql)
            .map_err(|e| format!("书签检索失败: {}", e))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(vals.iter().map(|v| v.as_ref())), |r| {
                Ok(BookmarkSearchHit {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    url: r.get(2)?,
                    summary: r.get(3)?,
                    tags: r.get(4)?,
                    category: r.get(5)?,
                    status: r.get(6)?,
                    dead: r.get::<_, i64>(7)? != 0,
                })
            })
            .map_err(|e| format!("书签检索失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }
}

/// LIKE ∪ 向量补位的异步检索（分段锁，Send 安全）。
/// 流程：① 锁内 LIKE（同步）→ 释放；② 无锁 embedding + 向量检索（await）；
///       ③ 锁内按 id 补位详情（同步）→ 释放。
pub async fn search_with_vectors(
    store: &Mutex<BookmarkStore>,
    dir_path: &str,
    query: &str,
    limit: usize,
    category: Option<&str>,
    folder: Option<&str>,
) -> Result<Vec<BookmarkSearchHit>, String> {
    let like_hits = {
        let s = store.lock().map_err(|e| e.to_string())?;
        s.search(query, limit, category, folder)?
    };
    if like_hits.len() >= limit {
        return Ok(like_hits);
    }
    // 向量召回补位（embedding 失败静默降级 LIKE-only）
    let embed = crate::core::db::utils::call_embedding_query(query);
    let query_vec = match embed {
        Ok(v) => v.into_iter().next().unwrap_or_default(),
        Err(e) => {
            log::warn!("[bookmark] 向量召回跳过（embedding 失败）: {}", e);
            return Ok(like_hits);
        }
    };
    if query_vec.is_empty() {
        return Ok(like_hits);
    }
    let uri = crate::core::db::utils::get_data_dir(dir_path);
    let hits = super::vector::search(&uri, &query_vec, limit as u32 * 3)
        .await
        .unwrap_or_default();
    let existing: std::collections::HashSet<String> = like_hits.iter().map(|h| h.id.clone()).collect();
    let like_len = like_hits.len();
    let mut out = like_hits;
    for hit in hits {
        if out.len() >= limit {
            break;
        }
        if existing.contains(&hit.bookmark_id) {
            continue;
        }
        let b = {
            let s = store.lock().map_err(|e| e.to_string())?;
            s.get(&hit.bookmark_id)?
        };
        if let Some(b) = b {
            out.push(BookmarkSearchHit {
                id: b.id.clone(),
                title: b.title,
                url: b.url,
                summary: b.summary,
                tags: b.tags,
                category: b.category,
                status: b.status,
                dead: b.dead,
            });
        }
    }
    log::info!(
        "[bookmark] 检索命中 {} 条（LIKE {} / 向量补位 {}）",
        out.len(), like_len, out.len().saturating_sub(like_len)
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, BookmarkStore) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let store = BookmarkStore::open_for_dir(
            dir.path().to_str().unwrap(),
            dir.path().join("test.db"),
        )
        .expect("打开测试库失败");
        (dir, store)
    }

    fn entry(url: &str, title: &str, folder: &str) -> crate::core::knowledge::bookmark::BookmarkEntry {
        crate::core::knowledge::bookmark::BookmarkEntry {
            url: url.to_string(),
            title: Some(title.to_string()),
            folder: Some(folder.to_string()),
            added_at: None,
        }
    }

    #[test]
    fn search_finds_by_title_summary_tags_and_filters() {
        let (_dir, store) = open_temp();
        store
            .import_entries(
                vec![
                    entry("https://a.com", "LangChain 框架", "AI/Agent"),
                    entry("https://b.com", "Rust 教程", "编程"),
                ],
                None,
            )
            .unwrap();
        // title 命中
        let hits = store.search("LangChain", 10, None, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://a.com");
        assert_eq!(hits[0].status, "pending");
        // 写入 summary/tags 后可检索
        let id = hits[0].id.clone();
        store
            .update_summary(&id, Some("大模型应用框架".into()), Some("AI/LLM".into()), Some("[\"Agent\",\"RAG\"]".into()))
            .unwrap();
        assert_eq!(store.search("RAG", 10, None, None).unwrap().len(), 1);
        // folder 前缀过滤
        assert_eq!(store.search("框架", 10, None, Some("AI")).unwrap().len(), 1);
        assert_eq!(store.search("框架", 10, None, Some("编程")).unwrap().len(), 0);
        // category 过滤
        assert_eq!(store.search("LangChain", 10, Some("AI/LLM"), None).unwrap().len(), 1);
        assert_eq!(store.search("LangChain", 10, Some("编程"), None).unwrap().len(), 0);
    }

    #[test]
    fn search_excludes_dead_flag_but_keeps_failed_rows() {
        let (_dir, store) = open_temp();
        store.import_entries(vec![entry("https://a.com", "Alpha", "AI")], None).unwrap();
        let id = store.get_by_canonical_url("https://a.com").unwrap().unwrap().id;
        store.mark_failed(&id, "404", true).unwrap();
        // failed/dead 仍可检索（结果带状态标记）
        let hits = store.search("Alpha", 10, None, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].dead);
        assert_eq!(hits[0].status, "failed");
    }
}
