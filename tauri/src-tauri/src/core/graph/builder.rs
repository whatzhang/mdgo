//! Document Graph 构建器（规则抽取，Level 1：零 LLM 成本）。
//!
//! 来源（全部为确定性规则，无模型调用）：
//! - 目录树 → `folder` 节点 + `CONTAINS` 边（folder→subfolder / folder→doc）
//! - 已索引文件 → `doc` 节点
//! - Markdown 内链（`[[wikilink]]` / `[text](path)`）→ `REFERENCES` 边（doc→doc）
//! - 代码符号占位（Phase 3 由 extractor 补充 entity 节点）
//!
//! 增量语义：
//! - `build_file`：单文件（新增/修改）→ 更新 doc 节点 + 重写该文件的 REFERENCES 出边
//!   （先 `delete_edges_by_source(doc_id)` 再重建，幂等）；
//! - `remove_file`：删除 doc 节点 + 级联边（storage.delete_by_path 已含入边）；
//! - `build_all`：全量重建（先 clear 再扫描，挂 kb_index 全量索引后）。

use std::path::Path;

use regex::Regex;

use super::model::{GraphEdge, GraphNode, NodeType, Relation};
use super::storage::{node_id_for, GraphStore};

/// 目录树遍历：收集全部目录与文件（不限定扩展名；跳过 .mdgo / 垃圾箱）。
/// 返回 (目录相对路径列表, 文件相对路径列表)。
pub fn collect_tree(base_dir: &Path, ignore: &crate::core::db::utils::IgnoreMatcher) -> (Vec<String>, Vec<String>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(base_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                if name == ".mdgo" || name == crate::core::db::utils::TRASH_DIR_NAME {
                    return false;
                }
                let rel = e.path().strip_prefix(base_dir).unwrap_or(e.path());
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                return ignore.is_kb_dir_allowed(&name, &rel_str);
            }
            true
        });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log::warn!("[graph] 遍历目录跳过不可访问路径: {}", e);
                continue;
            }
        };
        if entry.file_type().is_dir() {
            if entry.depth() > 0 {
                let rel = entry.path().strip_prefix(base_dir).unwrap_or(entry.path());
                dirs.push(rel.to_string_lossy().replace('\\', "/"));
            }
        } else if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(base_dir).unwrap_or(entry.path());
            files.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    (dirs, files)
}

/// 提取 Markdown 内链目标列表（wikilink + 标准链接 + 自动链接）。
///
/// 与前端 `extractLinks` 语义对齐：剥代码块/行内代码/YAML frontmatter 后正则提取，
/// 返回原始 target 列表（相对路径解析由 [`resolve_link`] 完成）。
static LINK_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"!?\[\[([^\[\]\|]+?)(?:\|[^\[\]]+?)?\]\]|!?\[(?:[^\]\\]|\\.)*\]\(([^)]+)\)|<([a-zA-Z][a-zA-Z0-9+.-]{1,31}:[^<>]+)>")
        .expect("LINK_RE 编译失败")
});

fn strip_code_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if (trimmed.starts_with("```") && trimmed.ends_with("```") && trimmed.len() > 3)
            || (trimmed.starts_with("~~~") && trimmed.ends_with("~~~") && trimmed.len() > 3)
        {
            // 单行围栏（```code``` 同行开合）：整行是代码，跳过
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue; // 围栏行本身不入输出
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// 提取文本中的链接 target 列表（去重，保留原始形式）。
pub fn extract_link_targets(text: &str) -> Vec<String> {
    // 剥代码块与行内代码
    let cleaned = strip_code_blocks(text);
    // 剥 YAML frontmatter
    let cleaned = if cleaned.trim_start().starts_with("---") {
        match cleaned.find("\n---") {
            Some(idx) => cleaned[idx + 4..].to_string(),
            None => cleaned.clone(),
        }
    } else {
        cleaned.clone()
    };
    let mut targets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cap in LINK_RE.captures_iter(&cleaned) {
        let raw = cap
            .get(1)
            .map(|m| m.as_str())
            .or_else(|| cap.get(2).map(|m| m.as_str()))
            .or_else(|| cap.get(3).map(|m| m.as_str()));
        if let Some(t) = raw {
            let t = t.trim();
            if t.is_empty() {
                continue;
            }
            // 去掉锚点 / 查询串（内部链接语义）
            let t = t.split('#').next().unwrap_or(t).split('?').next().unwrap_or(t).to_string();
            if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("mailto:") {
                continue; // 外部链接不进 Document Graph（Phase 3 entity 再处理）
            }
            if seen.insert(t.clone()) {
                targets.push(t);
            }
        }
    }
    targets
}

/// 解析链接 target 为知识库内相对路径（与前端 resolveLinkTargetAll 语义对齐）：
/// 1. 原样相对路径（带扩展名）已存在于文件集合 → 直接命中；
/// 2. 补 .md 后存在于文件集合 → 命中；
/// 3. 相对当前文件目录解析；
/// 4. 按文件名（去扩展名）匹配唯一候选。
pub fn resolve_link(target: &str, source_rel: &str, all_files: &std::collections::HashSet<String>) -> Option<String> {
    let norm = target.replace('\\', "/");
    if norm.starts_with('/') {
        return None;
    }
    // 1. 原样
    if all_files.contains(&norm) {
        return Some(norm.clone());
    }
    // 2. 补 .md
    if !norm.ends_with(".md") {
        let with_md = format!("{}.md", norm);
        if all_files.contains(&with_md) {
            return Some(with_md);
        }
    }
    // 3. 相对当前文件目录
    let source_dir = match source_rel.rfind('/') {
        Some(i) => &source_rel[..i],
        None => "",
    };
    let cand = if source_dir.is_empty() {
        norm.clone()
    } else {
        format!("{}/{}", source_dir, norm)
    };
    if all_files.contains(&cand) {
        return Some(cand);
    }
    if !cand.ends_with(".md") {
        let with_md = format!("{}.md", cand);
        if all_files.contains(&with_md) {
            return Some(with_md);
        }
    }
    // 4. 按文件名匹配（去扩展名，小写）
    let last = norm.rsplit('/').next().unwrap_or(&norm).to_lowercase();
    let stem = last.rsplit('.').next().unwrap_or(&last).to_string();
    let mut match_count = 0;
    let mut matched = None;
    for f in all_files {
        let name = f.rsplit('/').next().unwrap_or(f).to_lowercase();
        let fstem = name.rsplit('.').next().unwrap_or(&name);
        if fstem == stem {
            match_count += 1;
            matched = Some(f.clone());
        }
    }
    if match_count == 1 {
        matched
    } else {
        None
    }
}

/// Document Graph 构建器（纯逻辑，不依赖 Tauri）。
///
/// 持有 `&'a GraphStore` 引用（借用 MutexGuard 生命周期），调用方在锁内构建。
pub struct GraphBuilder<'a> {
    store: &'a GraphStore,
}

impl<'a> GraphBuilder<'a> {
    pub fn new(store: &'a GraphStore) -> Self {
        Self { store }
    }

    /// 取底层 store 引用（外部读路径用；当前引擎门面已封装，预留）
    #[allow(dead_code)]
    pub fn store(&self) -> &GraphStore {
        self.store
    }

    /// 全量重建 Document Graph（先清空再扫描）。
    pub fn build_all(&self, base_dir: &str, ignore: &crate::core::db::utils::IgnoreMatcher) -> Result<(), String> {
        self.store.clear()?;
        self.build_incremental(base_dir, ignore)?;
        Ok(())
    }

    /// 增量构建：目录树 + 全部文件节点与链接边（不清理；供全量/启动同步复用）。
    pub fn build_incremental(&self, base_dir: &str, ignore: &crate::core::db::utils::IgnoreMatcher) -> Result<(), String> {
        let base = Path::new(base_dir);
        let (dirs, files) = collect_tree(base, ignore);

        // 文件集合（供链接解析）
        let all_files: std::collections::HashSet<String> = files.iter().cloned().collect();

        // ── folder 节点 + CONTAINS（folder→folder）──
        for dir in &dirs {
            let id = node_id_for(NodeType::Folder, dir);
            let name = dir.rsplit('/').next().unwrap_or(dir).to_string();
            self.store.upsert_node(&GraphNode {
                id: id.clone(),
                node_type: NodeType::Folder,
                name,
                path: Some(dir.clone()),
                meta: None,
                degree: None,
            })?;
            // 父目录边
            if let Some(parent) = parent_dir(dir) {
                let pid = node_id_for(NodeType::Folder, &parent);
                self.store.upsert_edge(
                    &GraphEdge {
                        source: pid.clone(),
                        target: id,
                        relation: Relation::Contains,
                        weight: Some(1.0),
                        confidence: Some(1.0),
                    },
                    Some(&pid),
                )?;
            }
        }

        // ── doc 节点 + CONTAINS（folder→doc）──
        for f in &files {
            let id = node_id_for(NodeType::Doc, f);
            let name = f.rsplit('/').next().unwrap_or(f).to_string();
            let ext = f.rsplit('.').next().unwrap_or("").to_lowercase();
            self.store.upsert_node(&GraphNode {
                id: id.clone(),
                node_type: NodeType::Doc,
                name,
                path: Some(f.clone()),
                meta: Some(format!("{{\"ext\":\"{}\"}}", ext)),
                degree: None,
            })?;
            if let Some(parent) = parent_dir(f) {
                let pid = node_id_for(NodeType::Folder, &parent);
                self.store.upsert_edge(
                    &GraphEdge {
                        source: pid.clone(),
                        target: id,
                        relation: Relation::Contains,
                        weight: Some(1.0),
                        confidence: Some(1.0),
                    },
                    Some(&pid),
                )?;
            }
        }

        // ── REFERENCES（doc→doc，Markdown 内链）──
        for f in &files {
            if !is_linkable(f) {
                continue;
            }
            let abs = base.join(f);
            let content = match std::fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let targets = extract_link_targets(&content);
            if targets.is_empty() {
                continue;
            }
            let src_id = node_id_for(NodeType::Doc, f);
            for t in targets {
                if let Some(dst) = resolve_link(&t, f, &all_files) {
                    if dst == *f {
                        continue;
                    }
                    let dst_id = node_id_for(NodeType::Doc, &dst);
                    self.store.upsert_edge(
                        &GraphEdge {
                            source: src_id.clone(),
                            target: dst_id,
                            relation: Relation::References,
                            weight: Some(1.0),
                            confidence: Some(1.0),
                        },
                        Some(&src_id),
                    )?;
                }
            }
        }

        Ok(())
    }

    /// 单文件增量：更新 doc 节点 + 重写该文件 REFERENCES 出边（先删后写，幂等）。
    ///
    /// 调用时机：watcher 增量 / index_file 之后。文件内容读失败仅更新节点（降级）。
    pub fn build_file(&self, base_dir: &str, rel_path: &str, _ignore: &crate::core::db::utils::IgnoreMatcher) -> Result<(), String> {
        let rel = rel_path.replace('\\', "/");
        let id = node_id_for(NodeType::Doc, &rel);
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        let ext = rel.rsplit('.').next().unwrap_or("").to_lowercase();

        // 更新节点
        self.store.upsert_node(&GraphNode {
            id: id.clone(),
            node_type: NodeType::Doc,
            name,
            path: Some(rel.clone()),
            meta: Some(format!("{{\"ext\":\"{}\"}}", ext)),
            degree: None,
        })?;

        // 目录包含边（可能换目录/重命名）
        if let Some(parent) = parent_dir(&rel) {
            let pid = node_id_for(NodeType::Folder, &parent);
            self.store.upsert_edge(
                &GraphEdge {
                    source: pid.clone(),
                    target: id.clone(),
                    relation: Relation::Contains,
                    weight: Some(1.0),
                    confidence: Some(1.0),
                },
                Some(&pid),
            )?;
        }

        // 重写出边：先删该文件产出的 REFERENCES，再重建
        self.store.delete_edges_by_source(&id)?;
        if !is_linkable(&rel) {
            return Ok(());
        }
        let abs = Path::new(base_dir).join(&rel);
        let content = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("[graph] 读取文件内容失败（仅更新节点）: {} {}", rel, e);
                return Ok(());
            }
        };
        let targets = extract_link_targets(&content);
        if targets.is_empty() {
            return Ok(());
        }
        // 链接解析目标集合：从图内已有 doc 节点 path 构建（R2 修复）。
        // 旧实现每次 build_file 全量 walkdir（O(全库)），watcher 高频增量时性能灾难；
        // 改为查图内已有 doc path（增量场景目标文档通常已入图）。未建节点的链接目标
        // 暂跳过，待该目标文件入图（build_file/build_all）或下次全量重建时补充。
        let doc_paths = self.store.list_doc_paths()?;
        let all_files: std::collections::HashSet<String> = doc_paths.into_iter().collect();
        for t in targets {
            if let Some(dst) = resolve_link(&t, &rel, &all_files) {
                if dst == rel {
                    continue;
                }
                let dst_id = node_id_for(NodeType::Doc, &dst);
                self.store.upsert_edge(
                    &GraphEdge {
                        source: id.clone(),
                        target: dst_id,
                        relation: Relation::References,
                        weight: Some(1.0),
                        confidence: Some(1.0),
                    },
                    Some(&id),
                )?;
            }
        }
        Ok(())
    }

    /// 删除文件/目录的图数据（生命周期级联）。
    pub fn remove_path(&self, rel_path: &str) -> Result<u64, String> {
        self.store.delete_by_path(&rel_path.replace('\\', "/"))
    }
}

/// 相对路径的父目录（无父返回 None；根文件如 "a.md" 无父目录节点）
fn parent_dir(rel: &str) -> Option<String> {
    match rel.rfind('/') {
        Some(i) if i > 0 => Some(rel[..i].to_string()),
        _ => None,
    }
}

/// 该文件是否参与 REFERENCES 链接解析（Markdown 类）
pub(crate) fn is_linkable(rel: &str) -> bool {
    let lower = rel.to_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown") || lower.ends_with(".mdown") || lower.ends_with(".rst")
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_links() {
        let text = "见 [[README]] 和 [文档](docs/guide.md) 与 ![图](img/a.png)。";
        let targets = extract_link_targets(text);
        assert!(targets.contains(&"README".to_string()));
        assert!(targets.contains(&"docs/guide.md".to_string()));
        assert!(targets.contains(&"img/a.png".to_string()));
    }

    #[test]
    fn test_extract_links_skips_code() {
        let text = "```md\n[[inside-code]]\n```\n正文 [[real-link]]";
        let targets = extract_link_targets(text);
        assert!(!targets.contains(&"inside-code".to_string()));
        assert!(targets.contains(&"real-link".to_string()));
    }

    #[test]
    fn test_resolve_link_exact() {
        let mut files = std::collections::HashSet::new();
        files.insert("README.md".to_string());
        files.insert("docs/guide.md".to_string());
        assert_eq!(resolve_link("README", "src/a.md", &files), Some("README.md".to_string()));
        assert_eq!(resolve_link("docs/guide.md", "src/a.md", &files), Some("docs/guide.md".to_string()));
        assert_eq!(resolve_link("guide", "docs/b.md", &files), Some("docs/guide.md".to_string()));
        assert_eq!(resolve_link("missing", "src/a.md", &files), None);
    }

    #[test]
    fn test_parent_dir() {
        assert_eq!(parent_dir("a.md"), None);
        assert_eq!(parent_dir("docs/a.md"), Some("docs".to_string()));
        assert_eq!(parent_dir("docs/sub/a.md"), Some("docs/sub".to_string()));
    }
}
