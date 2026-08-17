//! 书签目录树：将拍平的 `browser_folder`（如 `AI/Agent`）组装为嵌套树，供前端直读 DB 渲染。

use serde::Serialize;

use super::{Bookmark, BookmarkStore};

/// 书签树节点。目录节点 `url=None` 且有 children；叶子节点 `url=Some` 且无 children。
#[derive(Debug, Clone, Default, Serialize)]
pub struct BookmarkTreeNode {
    /// 目录名或书签标题
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// tags（JSON 数组字符串，如 `["AI","Agent"]`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// 状态（pending/ready/failed），供前端区分展示
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 死链标记（抓取失败）
    #[serde(default)]
    pub dead: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<BookmarkTreeNode>,
}

impl BookmarkTreeNode {
    fn dir(title: &str) -> Self {
        BookmarkTreeNode {
            title: Some(title.to_string()),
            ..Default::default()
        }
    }

    fn leaf(b: &Bookmark) -> Self {
        BookmarkTreeNode {
            title: b.title.clone(),
            url: Some(b.url.clone()),
            added_at: b.added_at,
            summary: b.summary.clone(),
            tags: b.tags.clone(),
            category: b.category.clone(),
            status: Some(b.status.clone()),
            dead: b.dead,
            children: Vec::new(),
        }
    }

    /// 在 children 中查找或创建同名目录节点（仅当 child 是目录且 title 匹配）。
    fn ensure_child_dir(&mut self, name: &str) -> &mut BookmarkTreeNode {
        if let Some(pos) = self
            .children
            .iter()
            .position(|c| c.url.is_none() && c.title.as_deref() == Some(name))
        {
            return &mut self.children[pos];
        }
        self.children.push(BookmarkTreeNode::dir(name));
        let last = self.children.len() - 1;
        &mut self.children[last]
    }
}

impl BookmarkStore {
    /// 组装书签树（根节点固定 title="书签栏"）。按 `browser_folder` 的 `/` 分层。
    /// 全部书签（含 failed/dead）入树，叶子带 status/dead 标记。
    pub fn tree(&self) -> Result<BookmarkTreeNode, String> {
        let mut root = BookmarkTreeNode::dir("书签栏");
        for b in self.all()? {
            let segments: Vec<&str> = b
                .browser_folder
                .as_deref()
                .unwrap_or("")
                .split('/')
                .filter(|s| !s.trim().is_empty())
                .collect();
            let mut cur = &mut root;
            for seg in &segments {
                cur = cur.ensure_child_dir(seg);
            }
            cur.children.push(BookmarkTreeNode::leaf(&b));
        }
        Ok(root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::knowledge::bookmark::BookmarkEntry;

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
            url: url.into(),
            title: Some(title.into()),
            folder: Some(folder.into()),
            added_at: None,
        }
    }

    /// 收集树中所有叶子 url
    fn collect_urls(node: &BookmarkTreeNode, out: &mut Vec<String>) {
        for c in &node.children {
            if let Some(u) = &c.url {
                out.push(u.clone());
            }
            if !c.children.is_empty() {
                collect_urls(c, out);
            }
        }
    }

    #[test]
    fn tree_builds_hierarchy_and_carries_status_dead() {
        let (_dir, store) = open_temp();
        store
            .import_entries(
                vec![
                    entry("https://a.com", "Alpha", "AI/Agent"),
                    entry("https://b.com", "Beta", "AI/Tools"),
                    entry("https://c.com", "Gamma", "编程/Rust"),
                ],
                None,
            )
            .unwrap();
        // 标记 b.com 为死链失败
        let bid = store.get_by_canonical_url("https://b.com").unwrap().unwrap().id;
        store.mark_failed(&bid, "404", true).unwrap();

        let t = store.tree().unwrap();
        assert_eq!(t.title.as_deref(), Some("书签栏"));
        assert_eq!(t.children.len(), 2, "根下应有 AI、编程 两个目录");
        let ai = t
            .children
            .iter()
            .find(|c| c.title.as_deref() == Some("AI"))
            .expect("AI 目录");
        assert_eq!(ai.children.len(), 2, "AI 下有 Agent、Tools 两子目录");
        let mut urls = Vec::new();
        collect_urls(&t, &mut urls);
        assert!(urls.contains(&"https://a.com".to_string()));
        assert!(urls.contains(&"https://c.com".to_string()));

        // 叶子携带 status/dead
        let mut dead_count = 0;
        let mut status_count = 0;
        fn walk(n: &BookmarkTreeNode, dead: &mut usize, st: &mut usize) {
            if n.url.is_some() {
                if n.dead {
                    *dead += 1;
                }
                if n.status.is_some() {
                    *st += 1;
                }
            }
            for c in &n.children {
                walk(c, dead, st);
            }
        }
        walk(&t, &mut dead_count, &mut status_count);
        assert_eq!(dead_count, 1, "b.com 应标记死链");
        assert_eq!(status_count, 3, "所有叶子带 status");
    }
}
