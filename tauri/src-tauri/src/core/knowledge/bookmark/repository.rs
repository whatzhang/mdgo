//! Bookmark 持久化操作（SQL 参数化，单表 `bookmarks`）。

use rusqlite::params;

use super::{Bookmark, BookmarkStats, BookmarkStore};

impl BookmarkStore {
    /// 插入单条（重复主键返回 Err）
    pub fn insert(&self, b: &Bookmark) -> Result<(), String> {
        self.conn
            .execute(
                &format!(
                    "INSERT INTO bookmarks ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                    COLS = Self::COLS
                ),
                params![
                    b.id, b.url, b.canonical_url, b.title, b.browser_folder, b.added_at,
                    b.source_file, b.category, b.summary, b.tags, b.raw_content, b.embedding_text,
                    b.status, i64::from(b.dead), b.last_error, b.revision, b.created_at, b.updated_at
                ],
            )
            .map_err(|e| format!("插入书签失败: {}", e))?;
        Ok(())
    }

    /// 按 id 查询
    pub fn get(&self, id: &str) -> Result<Option<Bookmark>, String> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("SELECT {COLS} FROM bookmarks WHERE id=?1", COLS = Self::COLS))
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query_map([id], Self::row_to_bookmark)
            .map_err(|e| e.to_string())?;
        match rows.next() {
            Some(r) => Ok(Some(r.map_err(|e| e.to_string())?)),
            None => Ok(None),
        }
    }

    /// 一次性载入表中全部 `canonical_url`（导入去重用：把 O(n) 次回查压缩为单次全量读取，
    /// 去重在内存 `HashSet` 完成）。表内 `canonical_url` 为 NOT NULL，无需处理 NULL。
    pub fn all_canonical_urls(&self) -> Result<std::collections::HashSet<String>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT canonical_url FROM bookmarks")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut set = std::collections::HashSet::new();
        for r in rows {
            set.insert(r.map_err(|e| e.to_string())?);
        }
        Ok(set)
    }

    /// 按 canonical_url 查询（幂等去重键）
    pub fn get_by_canonical_url(&self, canonical_url: &str) -> Result<Option<Bookmark>, String> {        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT {COLS} FROM bookmarks WHERE canonical_url=?1 LIMIT 1",
                COLS = Self::COLS
            ))
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query_map([canonical_url], Self::row_to_bookmark)
            .map_err(|e| e.to_string())?;
        match rows.next() {
            Some(r) => Ok(Some(r.map_err(|e| e.to_string())?)),
            None => Ok(None),
        }
    }

    /// 认领一批待处理书签（`status='pending'`，按入库时间序）。
    /// 单 Worker 串行 tick，无需 RUNNING 标记；崩溃遗留的 pending 会在下次启动后重新处理。
    pub fn claim_pending(&self, limit: usize) -> Result<Vec<Bookmark>, String> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!(
                "SELECT {COLS} FROM bookmarks WHERE status=?1 ORDER BY created_at LIMIT ?2",
                COLS = Self::COLS
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![super::STATUS_PENDING, limit as i64], Self::row_to_bookmark)
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// 抓取成功：写入正文（raw_content 仅用于 LLM 总结分类标签，不参与检索）
    pub fn update_raw_content(&self, id: &str, raw_content: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE bookmarks SET raw_content=?2, updated_at=?3 WHERE id=?1",
                params![id, raw_content, Self::now_ms()],
            )
            .map_err(|e| format!("保存抓取内容失败: {}", e))?;
        Ok(())
    }

    /// LLM 总结成功：写入 summary/category/tags（tags 为 JSON 数组字符串）
    pub fn update_summary(
        &self,
        id: &str,
        summary: Option<String>,
        category: Option<String>,
        tags: Option<String>,
    ) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE bookmarks SET summary=?2, category=?3, tags=?4, updated_at=?5 WHERE id=?1",
                params![id, summary, category, tags, Self::now_ms()],
            )
            .map_err(|e| format!("保存摘要失败: {}", e))?;
        Ok(())
    }

    /// embedding 推理前：写入实际送入模型的 embedding_text（与向量库内容保持一致）
    pub fn update_embedding_text(&self, id: &str, text: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE bookmarks SET embedding_text=?2, updated_at=?3 WHERE id=?1",
                params![id, text, Self::now_ms()],
            )
            .map_err(|e| format!("保存 embedding 文本失败: {}", e))?;
        Ok(())
    }

    /// 处理完成：status → ready，清空错误
    pub fn mark_ready(&self, id: &str) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE bookmarks SET status=?2, last_error=NULL, updated_at=?3 WHERE id=?1",
                params![id, super::STATUS_READY, Self::now_ms()],
            )
            .map_err(|e| format!("更新书签状态失败: {}", e))?;
        Ok(())
    }

    /// 处理失败（终态）：status → failed。`dead=true` 表示抓取失败的死链。
    pub fn mark_failed(&self, id: &str, error: &str, dead: bool) -> Result<(), String> {
        self.conn
            .execute(
                "UPDATE bookmarks SET status=?2, dead=?3, last_error=?4, updated_at=?5 WHERE id=?1",
                params![id, super::STATUS_FAILED, i64::from(dead), error, Self::now_ms()],
            )
            .map_err(|e| format!("更新书签状态失败: {}", e))?;
        Ok(())
    }

    /// 列表（可选过滤；无默认排除——failed/dead 也展示，由前端按 status/dead 渲染）
    #[allow(clippy::too_many_arguments)]
    pub fn list(
        &self,
        folder: Option<&str>,
        category: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Bookmark>, String> {
        let mut sql = format!("SELECT {COLS} FROM bookmarks WHERE 1=1", COLS = Self::COLS);
        // 全部使用无名占位符 `?`（rusqlite 顺序绑定）
        let mut vals: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
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
        if let Some(s) = status {
            if !s.is_empty() {
                sql.push_str(" AND status = ?");
                vals.push(Box::new(s.to_string()));
            }
        }
        sql.push_str(" ORDER BY updated_at DESC LIMIT ?");
        vals.push(Box::new(limit.min(500) as i64));

        let mut stmt = self
            .conn
            .prepare_cached(&sql)
            .map_err(|e| format!("查询书签列表失败: {}", e))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(vals.iter().map(|v| v.as_ref())), Self::row_to_bookmark)
            .map_err(|e| format!("查询书签列表失败: {}", e))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// 全部书签（供目录树组装）
    pub fn all(&self) -> Result<Vec<Bookmark>, String> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("SELECT {COLS} FROM bookmarks", COLS = Self::COLS))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], Self::row_to_bookmark)
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// 数量统计（UI 统计卡）
    pub fn stats(&self) -> Result<BookmarkStats, String> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT
                    COUNT(*) AS total,
                    SUM(CASE WHEN status='ready' THEN 1 ELSE 0 END) AS ready,
                    SUM(CASE WHEN status='pending' THEN 1 ELSE 0 END) AS pending,
                    SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END) AS failed,
                    SUM(CASE WHEN dead=1 THEN 1 ELSE 0 END) AS dead
                 FROM bookmarks",
            )
            .map_err(|e| e.to_string())?;
        let s = stmt
            .query_row([], |r| {
                Ok(BookmarkStats {
                    total: r.get(0).unwrap_or(0),
                    ready: r.get(1).unwrap_or(0),
                    pending: r.get(2).unwrap_or(0),
                    failed: r.get(3).unwrap_or(0),
                    dead: r.get(4).unwrap_or(0),
                })
            })
            .map_err(|e| e.to_string())?;
        Ok(s)
    }
}
