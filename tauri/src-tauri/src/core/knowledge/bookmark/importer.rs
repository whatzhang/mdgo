//! 书签导入：前端解析后的结构化 JSON → 校验 → 规范化 → 按 URL 去重入库。
//!
//! 去重规则：以 `canonical_url` 为键——**已存在直接跳过**（不更新、不重新入队）；
//! 不存在则插入（status=pending，交由后台 Worker 抓取 → 总结 → 向量）。

use std::collections::HashSet;
use rusqlite::params;

use super::{normalize_url, Bookmark, BookmarkEntry, BookmarkImportStats, BookmarkStore, STATUS_PENDING};

impl BookmarkStore {
    /// 导入书签条目（前端 `parseBookmarkHtml` 结果）。
    ///
    /// - 数量上限：超过 5 万条截断（前端已提示，此处为后端兜底）；
    /// - 协议白名单：http/https（`normalize_url`），非法条目计入 failed 并跳过；
    /// - 去重：按 `canonical_url` 幂等，已存在直接跳过（计入 skipped）；
    /// - 内部去重：同一批内重复的 canonical_url 只入库一次，其余计入 skipped。
    ///
    /// # 性能
    /// 单趟流式处理，去重在内存 `HashSet` 完成（O(1)/条），避免逐条回查 DB；
    /// 全部写入复用同一条 prepared `INSERT`（SQL 只编译一次）。
    pub fn import_entries(
        &self,
        entries: Vec<BookmarkEntry>,
        source_file: Option<&str>,
    ) -> Result<BookmarkImportStats, String> {
        const MAX_IMPORT_ENTRIES: usize = 50_000;
        let raw_len = entries.len();
        if raw_len > MAX_IMPORT_ENTRIES {
            log::warn!(
                "[bookmark] 书签文件过大（{} 条），截断至 {} 条",
                raw_len, MAX_IMPORT_ENTRIES
            );
        }
        log::info!(
            "[bookmark] 开始导入书签：{} 条{}",
            entries.len(),
            source_file.map(|f| format!("，来源 {}", f)).unwrap_or_default()
        );
        let mut stats = BookmarkImportStats {
            total: raw_len.min(MAX_IMPORT_ENTRIES),
            ..Default::default()
        };
        let now = BookmarkStore::now_ms();

        // 事务包裹批量写（中途失败整体回滚，避免部分入库不一致）
        self.conn
            .execute_batch("BEGIN")
            .map_err(|e| format!("开启导入事务失败: {}", e))?;
        let tx_result = (|| -> Result<(), String> {
            // 一次性载入表中已有的 canonical_url（去重键），避免逐条回查 DB。
            let mut existing: HashSet<String> = self.all_canonical_urls()?;

            // 本批内部去重集合：同一文件重复书签仅在内存判定，不写 DB。
            let mut seen_in_batch: HashSet<String> = HashSet::with_capacity(raw_len.min(8192));

            // 复用同一条 prepared INSERT（SQL 仅编译一次，极大减少 5 万条下的开销）。
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "INSERT INTO bookmarks ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                    COLS = Self::COLS
                ))
                .map_err(|e| format!("准备导入插入语句失败: {}", e))?;

            let mut iter = entries.into_iter();
            // 受截断的条目：避免无限迭代 take 之外的数据
            let mut processed = 0usize;
            while processed < MAX_IMPORT_ENTRIES {
                let Some(entry) = iter.next() else { break };
                processed += 1;

                // 1. URL 校验 + 规范化
                let canonical = match normalize_url(&entry.url) {
                    Ok(c) => c,
                    Err(e) => {
                        log::warn!("[bookmark] 跳过非法书签: {}", e);
                        stats.failed += 1;
                        continue;
                    }
                };
                // 2. 去重：库内已存在，或本批内已出现过 → 直接跳过（不更新、不重新入队）
                if existing.contains(&canonical) || !seen_in_batch.insert(canonical.clone()) {
                    stats.skipped += 1;
                    continue;
                }
                // 3. 新书签入库（status=pending，由 Worker 处理）
                let b = Bookmark {
                    id: BookmarkStore::new_id(),
                    url: entry.url.trim().to_string(),
                    canonical_url: Some(canonical.clone()),
                    title: entry.title.clone(),
                    browser_folder: entry.folder.clone(),
                    added_at: entry.added_at,
                    source_file: source_file.map(|s| s.to_string()),
                    category: None,
                    summary: None,
                    tags: None,
                    raw_content: None,
                    embedding_text: None,
                    status: STATUS_PENDING.to_string(),
                    dead: false,
                    last_error: None,
                    revision: 1,
                    created_at: now,
                    updated_at: now,
                };
                let affected = stmt
                    .execute(params![
                        b.id, b.url, b.canonical_url, b.title, b.browser_folder, b.added_at,
                        b.source_file, b.category, b.summary, b.tags, b.raw_content, b.embedding_text,
                        b.status, i64::from(b.dead), b.last_error, b.revision, b.created_at, b.updated_at
                    ])
                    .map_err(|e| format!("插入书签失败: {}", e))?;
                if affected == 1 {
                    // 插入成功 → 同步登记到 existing，保证后续相同 URL 走跳过路径
                    existing.insert(canonical);
                }
                stats.inserted += 1;
            }
            Ok(())
        })();
        match tx_result {
            Ok(()) => self
                .conn
                .execute_batch("COMMIT")
                .map_err(|e| format!("提交导入事务失败: {}", e))?,
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }

        log::info!(
            "[bookmark] 导入完成：共 {} 条，新增 {} / 跳过 {} / 失败 {}",
            stats.total, stats.inserted, stats.skipped, stats.failed
        );
        Ok(stats)
    }
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

    fn entry(url: &str, title: &str, folder: &str) -> BookmarkEntry {
        BookmarkEntry {
            url: url.to_string(),
            title: Some(title.to_string()),
            folder: Some(folder.to_string()),
            added_at: Some(1_700_000_000_000),
        }
    }

    #[test]
    fn import_inserts_and_skips_existing_by_url() {
        let (_dir, store) = open_temp();
        let entries = vec![
            entry("https://a.com/", "Alpha", "AI"),
            entry("https://b.com", "Beta", "AI/Agent"),
        ];
        let s1 = store.import_entries(entries.clone(), Some("bookmarks.html")).unwrap();
        assert_eq!(s1.inserted, 2);
        assert_eq!(s1.total, 2);
        assert_eq!(store.stats().unwrap().total, 2);

        // 重复导入（含尾斜杠变体）→ 已存在直接跳过，不更新、不新增
        let dup = vec![
            entry("https://a.com", "Alpha v2", "AI"),
            entry("https://b.com", "Beta v2", "AI/Agent"),
            entry("https://c.com", "Gamma", "编程"),
        ];
        let s2 = store.import_entries(dup, Some("bookmarks.html")).unwrap();
        assert_eq!(s2.inserted, 1, "仅新增 c.com");
        assert_eq!(s2.skipped, 2, "a.com / b.com 已存在跳过");
        assert_eq!(store.stats().unwrap().total, 3);
        // 跳过时不更新标题（保留原数据）
        let a = store.get_by_canonical_url("https://a.com").unwrap().unwrap();
        assert_eq!(a.title.as_deref(), Some("Alpha"));
    }

    #[test]
    fn import_rejects_bad_protocol_and_counts_failed() {
        let (_dir, store) = open_temp();
        let entries = vec![
            entry("https://ok.com", "OK", "AI"),
            BookmarkEntry { url: "javascript:alert(1)".into(), title: None, folder: None, added_at: None },
        ];
        let s = store.import_entries(entries, None).unwrap();
        assert_eq!(s.inserted, 1);
        assert_eq!(s.failed, 1);
    }

    #[test]
    fn import_truncates_over_50000_entries() {
        let (_dir, store) = open_temp();
        let entries: Vec<BookmarkEntry> = (0..50_001)
            .map(|i| entry(&format!("https://site{}.com", i), "T", "F"))
            .collect();
        let s = store.import_entries(entries, None).unwrap();
        assert_eq!(s.total, 50_000, "超 5 万条截断");
        assert_eq!(store.stats().unwrap().total, 50_000);
    }
}
