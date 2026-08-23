//! Chunk Graph Builder（知识图谱底座 Layer 1：文档结构层）。
//!
//! 把「文件系统层」升级为「文档结构层」：Markdown/文本 → AST →
//! [`SemanticChunkEngine`] → **chunk 节点 + Section 节点 + 层级边**。
//!
//! - chunk 节点 id：`chunk:<doc相对路径>#<idx>`（确定性，内容/配置不变则稳定，增量幂等）
//! - section 节点 id：`section:<doc相对路径>#<h1>#<h2>...`（标题路径）
//! - 边：`doc CONTAINS section` → `section CONTAINS chunk`；无标题文档 `doc CONTAINS chunk`
//! - 增量：单文档先删旧 chunk/section 再重建；删除文档时 `delete_by_path` 级联清内容节点
//! - 与索引管线同源：复用 `ComrakMarkdownParser` + `SemanticChunkEngine`，分块结果与 LanceDB 一致
//!
//! 产出喂给前端 L4 细粒度层（chunk 内容/标题/位置证据）与后续语义聚类（Phase 2）。

use super::builder::is_linkable;
use super::model::{GraphEdge, GraphNode, NodeType, Relation};
use super::storage::{node_id_for, GraphStore};
use crate::core::document::chunk_engine::{Chunk, ChunkEngine, SemanticChunkEngine};
use crate::core::document::node::{DocumentNode, NodeType as DocNodeType};
use crate::core::document::parser::MarkdownParser;
use crate::core::document::ComrakMarkdownParser;

/// chunk 内容入库上限（证据/展示足够，避免 content 列膨胀）
const CHUNK_CONTENT_MAX: usize = 2000;

/// 默认分块参数（与索引管线同源；确定性，保证 chunk id 稳定）
pub const CHUNK_MAX_CHARS: usize = 800;
pub const CHUNK_OVERLAP: usize = 100;

/// chunk 节点 id 约定：`chunk:<doc相对路径>#<idx>`（与 LanceDB chunk_index 对齐）
pub fn chunk_node_id(rel_path: &str, idx: u32) -> String {
    format!("chunk:{}#{}", rel_path.replace('\\', "/"), idx)
}

/// section 节点 id 约定：`section:<doc相对路径>#<h1>#<h2>...`
pub fn section_node_id(rel_path: &str, heading_path: &[String]) -> String {
    let joined = heading_path.iter().map(|h| h.trim()).filter(|h| !h.is_empty()).collect::<Vec<_>>().join("#");
    if joined.is_empty() {
        format!("section:{}#__root__", rel_path.replace('\\', "/"))
    } else {
        format!("section:{}#{}", rel_path.replace('\\', "/"), joined)
    }
}

/// 构建统计
#[derive(Debug, Default, Clone, Copy)]
pub struct ChunkBuildStats {
    pub docs: u32,
    pub chunks: u32,
    pub sections: u32,
}

/// Chunk 图构建器（纯逻辑，不依赖 Tauri；调用方持锁）。
pub struct ChunkGraphBuilder<'a> {
    store: &'a GraphStore,
}

impl<'a> ChunkGraphBuilder<'a> {
    pub fn new(store: &'a GraphStore) -> Self {
        Self { store }
    }

    /// 全量：遍历 doc 节点，重建每篇的 chunk/section 子图（幂等，可重复执行）。
    /// 非 Markdown（代码）文件同时解析 import 依赖 → IMPORTS 边。
    pub fn build_all(
        &self,
        base_dir: &str,
        max_chars: usize,
        overlap: usize,
    ) -> Result<ChunkBuildStats, String> {
        let mut stats = ChunkBuildStats::default();
        let docs = self.store.all_nodes(500_000)?;
        // 全量文件集（import 目标解析用）
        let all_paths: std::collections::HashSet<String> = self.store.list_doc_paths()?.into_iter().collect();
        for doc in docs {
            if doc.node_type != NodeType::Doc {
                continue;
            }
            let path = match &doc.path {
                Some(p) => p.clone(),
                None => continue,
            };
            let abs = std::path::Path::new(base_dir).join(&path);
            let content = match std::fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let s = self.build_doc(&path, &content, max_chars, overlap, Some(&all_paths))?;
            stats.docs += 1;
            stats.chunks += s.chunks;
            stats.sections += s.sections;
        }
        Ok(stats)
    }

    /// 单文档增量（watcher / index_file 后调用）：先删旧内容节点再重建（幂等）。
    pub fn build_file(
        &self,
        base_dir: &str,
        rel_path: &str,
        max_chars: usize,
        overlap: usize,
    ) -> Result<ChunkBuildStats, String> {
        let rel = rel_path.replace('\\', "/");
        let abs = std::path::Path::new(base_dir).join(&rel);
        let content = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => {
                // 文件不可读（删除/移动中）→ 清理旧内容节点即可
                self.store.delete_content_nodes_for_doc(&rel)?;
                return Ok(ChunkBuildStats::default());
            }
        };
        // 增量场景目标文件集（import 解析；图内已有 doc 路径）
        let all_paths: std::collections::HashSet<String> = self.store.list_doc_paths()?.into_iter().collect();
        self.build_doc(&rel, &content, max_chars, overlap, Some(&all_paths))
    }

    /// 单文档构建：清理旧内容节点 → AST 分块 → 写 chunk/section 节点与层级边；
    /// 代码文件额外解析 import 依赖（IMPORTS 边）。
    fn build_doc(
        &self,
        rel_path: &str,
        content: &str,
        max_chars: usize,
        overlap: usize,
        all_paths: Option<&std::collections::HashSet<String>>,
    ) -> Result<ChunkBuildStats, String> {
        // 1) 清理旧内容节点（chunk/section，path 与 doc 相同；delete_node 级联清边）
        self.store.delete_content_nodes_for_doc(rel_path)?;

        let doc_id = node_id_for(NodeType::Doc, rel_path);
        // 文档节点必须存在（CONTAINS 挂点）
        if self.store.get_node(&doc_id)?.is_none() {
            return Ok(ChunkBuildStats::default());
        }
        if content.trim().is_empty() {
            return Ok(ChunkBuildStats::default());
        }

        // 2) AST 分块：md 家族走标题路径（→ Section）；其余纯文本兜底（无 Section）
        let chunks: Vec<Chunk> = if is_linkable(rel_path) {
            let document = ComrakMarkdownParser.parse(content, true);
            SemanticChunkEngine::new(max_chars, overlap, 1.25, 50).build(&document)
        } else {
            // 非 md：整篇作为单个段落交给引擎（按 max_size 切块，无标题路径）
            let mut root = DocumentNode::new(DocNodeType::Root, "");
            root.children.push(DocumentNode::new(DocNodeType::Paragraph, content.to_string()));
            SemanticChunkEngine::new(max_chars, overlap, 1.25, 50).build(&root)
        };
        if chunks.is_empty() {
            return Ok(ChunkBuildStats::default());
        }

        // 3) 写节点与边
        let mut stats = ChunkBuildStats { docs: 1, chunks: chunks.len() as u32, sections: 0 };
        let mut section_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let now = super::storage::now_ms_public();
        for (idx, chunk) in chunks.iter().enumerate() {
            let chunk_id = chunk_node_id(rel_path, idx as u32);
            let heading = chunk.path.last().cloned().unwrap_or_default();
            let name = if heading.is_empty() {
                let first_line = chunk.text.lines().next().unwrap_or("").trim();
                let short: String = first_line.chars().take(20).collect();
                if short.is_empty() { format!("chunk {}", idx) } else { short }
            } else {
                heading.clone()
            };
            let meta = serde_json::json!({
                "doc": rel_path,
                "idx": idx,
                "heading": heading,
                "chunk_type": chunk.chunk_type,
                "heading_path": chunk.path.clone(),
            });
            self.store.upsert_node(&GraphNode {
                id: chunk_id.clone(),
                node_type: NodeType::Chunk,
                name: truncate_chars(&name, 60),
                path: Some(rel_path.to_string()),
                meta: Some(meta.to_string()),
                degree: None,
                created_at: Some(now),
                content: Some(truncate_chars(&chunk.text, CHUNK_CONTENT_MAX)),
            })?;

            // 3a) Section 挂点（md 标题路径；空路径直接挂 doc）
            if !chunk.path.is_empty() {
                let section_id = section_node_id(rel_path, &chunk.path);
                if section_ids.insert(section_id.clone()) {
                    let section_name = chunk.path.last().cloned().unwrap_or_default();
                    let section_meta = serde_json::json!({
                        "doc": rel_path,
                        "heading_path": chunk.path.clone(),
                    });
                    self.store.upsert_node(&GraphNode {
                        id: section_id.clone(),
                        node_type: NodeType::Section,
                        name: truncate_chars(&section_name, 60),
                        path: Some(rel_path.to_string()),
                        meta: Some(section_meta.to_string()),
                        degree: None,
                        created_at: Some(now),
                        content: None,
                    })?;
                    // doc CONTAINS section
                    self.store.upsert_edge(
                        &GraphEdge {
                            source: doc_id.clone(),
                            target: section_id.clone(),
                            relation: Relation::Contains,
                            weight: Some(1.0),
                            confidence: Some(1.0),
                        },
                        Some(&doc_id),
                    )?;
                    stats.sections += 1;
                }
                // section CONTAINS chunk
                self.store.upsert_edge(
                    &GraphEdge {
                        source: section_id.clone(),
                        target: chunk_id.clone(),
                        relation: Relation::Contains,
                        weight: Some(1.0),
                        confidence: Some(1.0),
                    },
                    Some(&doc_id),
                )?;
            } else {
                // doc CONTAINS chunk（无标题文档）
                self.store.upsert_edge(
                    &GraphEdge {
                        source: doc_id.clone(),
                        target: chunk_id.clone(),
                        relation: Relation::Contains,
                        weight: Some(1.0),
                        confidence: Some(1.0),
                    },
                    Some(&doc_id),
                )?;
            }
        }

        // 4) 代码 import 依赖（非 Markdown；IMPORTS 边，幂等重写）
        if !is_linkable(rel_path) {
            if let Some(paths) = all_paths {
                let doc_id = node_id_for(NodeType::Doc, rel_path);
                // 先删旧 IMPORTS 出边（增量重写）
                self.store.delete_edges_by_relation(&doc_id, "IMPORTS")?;
                for target in extract_code_imports(content) {
                    if let Some(dst) = resolve_import_target(&target, paths) {
                        if dst == rel_path {
                            continue;
                        }
                        let dst_id = node_id_for(NodeType::Doc, &dst);
                        // 目标必须已入图（避免幽灵节点）
                        if self.store.get_node(&dst_id)?.is_none() {
                            continue;
                        }
                        self.store.upsert_edge(
                            &GraphEdge {
                                source: doc_id.clone(),
                                target: dst_id,
                                relation: Relation::Imports,
                                weight: Some(1.0),
                                confidence: Some(0.9),
                            },
                            Some(&doc_id),
                        )?;
                    }
                }
            }
        }
        Ok(stats)
    }
}

/// 截断到指定字符数（UTF-8 安全）
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

// ─── 代码 import 依赖（Phase 1b：Code File IMPORTS Code File） ───

/// 从代码文件内容提取 import 目标（多语言正则；返回归一化模块名）。
/// 语言覆盖：Python / JS/TS / Rust / Java / Go / C/C++。
pub fn extract_code_imports(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let push = |t: &str, seen: &mut std::collections::HashSet<String>, out: &mut Vec<String>| {
        let t = t.trim().trim_matches('"').trim_matches('\'').trim();
        if t.is_empty() || t.starts_with("std::") || t == "react" || t.starts_with("node:") {
            return;
        }
        if seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    };
    for line in content.lines() {
        let line = line.trim_start();
        // JS/TS ESM：import ... from 'x'（具名/默认导入都含 " from "）
        if line.starts_with("import") && line.contains(" from ") {
            if let Some(idx) = line.find(" from ") {
                let after = &line[idx + 6..];
                let q = after.trim();
                if let Some(start) = q.find(['\'', '"']) {
                    let quote = q.as_bytes()[start] as char;
                    let rest = &q[start + 1..];
                    if let Some(end) = rest.find(quote) {
                        push(&rest[..end], &mut seen, &mut out);
                    }
                }
            }
            continue;
        }
        // JS/TS require('x')
        if line.contains("require(") {
            if let Some(start) = line.find(['\'', '"']) {
                let quote = line.as_bytes()[start] as char;
                let rest = &line[start + 1..];
                if let Some(end) = rest.find(quote) {
                    push(&rest[..end], &mut seen, &mut out);
                }
            }
            continue;
        }
        // Python：import X / from X import ...
        if line.starts_with("import ") {
            let rest = line.trim_start_matches("import ").trim();
            let first = rest.split(',').next().unwrap_or("").trim().split(" as ").next().unwrap_or("").trim();
            push(first, &mut seen, &mut out);
            continue;
        }
        if line.starts_with("from ") {
            let rest = line.trim_start_matches("from ").trim();
            let module = rest.split(' ').next().unwrap_or("").trim();
            push(module, &mut seen, &mut out);
            continue;
        }
        // Rust use
        if line.starts_with("use ") {
            let rest = line.trim_start_matches("use ").trim();
            let first = rest.split("::").next().unwrap_or("").trim().split(" as ").next().unwrap_or("").trim();
            push(first, &mut seen, &mut out);
            continue;
        }
        // Java import
        if line.starts_with("import ") {
            let rest = line.trim_start_matches("import ").trim().trim_end_matches(';').trim();
            let first = rest.split('.').next().unwrap_or("").trim();
            push(first, &mut seen, &mut out);
            continue;
        }
        // Go: import "pkg" / import ( "pkg" ... ) / import "pkg"
        if line.starts_with("import") {
            let rest = line.trim_start_matches("import").trim();
            if let Some(start) = rest.find(['\'', '"']) {
                let quote = rest.as_bytes()[start] as char;
                let after = &rest[start + 1..];
                if let Some(end) = after.find(quote) {
                    let pkg = &after[..end];
                    push(pkg.rsplit('/').next().unwrap_or(pkg), &mut seen, &mut out);
                }
            }
            continue;
        }
        // C/C++ #include
        if line.starts_with('#') && line.contains("include") {
            if let Some(start) = line.find(['<', '"']) {
                let open = line.as_bytes()[start] as char;
                let close = if open == '<' { '>' } else { '"' };
                let after = &line[start + 1..];
                if let Some(end) = after.find(close) {
                    let inc = &after[..end];
                    let name = inc.rsplit('/').next().unwrap_or(inc);
                    push(name.trim_end_matches(".h").trim_end_matches(".hpp"), &mut seen, &mut out);
                }
            }
            continue;
        }
    }
    out
}

/// import 目标 → 图内文档路径（后缀匹配 → 文件名 stem 匹配）。
pub fn resolve_import_target(target: &str, all_paths: &std::collections::HashSet<String>) -> Option<String> {
    let norm = target.replace('\\', "/");
    let norm = norm.trim_start_matches(['.', '/']).to_string();
    if norm.is_empty() {
        return None;
    }
    // 文件名主干（去扩展名：split 从头取第一段）
    let last = norm.rsplit('/').next().unwrap_or(&norm);
    let stem = last.split('.').next().unwrap_or(last).to_lowercase();
    if stem.is_empty() {
        return None;
    }
    let mut best: Option<(&str, usize)> = None; // (path, score: 匹配强度)
    for p in all_paths {
        let p_norm = p.replace('\\', "/");
        let p_last = p_norm.rsplit('/').next().unwrap_or(&p_norm);
        let p_stem = p_last.split('.').next().unwrap_or(p_last).to_lowercase();
        // 1) 文件名主干精确匹配（bookmark_service → bookmark_service.py）
        if p_stem == stem {
            let score = p_norm.len();
            if best.map(|(_, s)| score > s).unwrap_or(true) {
                best = Some((p.as_str(), score));
            }
        }
        // 2) 路径后缀匹配（含扩展名，如 a/b.ts）
        if norm.contains('/') && (p_norm.ends_with(&norm) || p_norm.ends_with(&format!("{}.{}", norm, ext_of(p)))) {
            let score = p_norm.len() + 1000;
            if best.map(|(_, s)| score > s).unwrap_or(true) {
                best = Some((p.as_str(), score));
            }
        }
    }
    best.map(|(p, _)| p.to_string())
}

fn ext_of(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or("")
}

// ─── 单元测试 ───

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::graph::model::GraphNode;

    fn temp_store(name: &str) -> (GraphStore, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("mdgo_graph_chunk_test_{}_", name))
            .tempdir()
            .unwrap();
        let db = dir.path().join("mdgo.db");
        let store = GraphStore::open_for_dir(dir.path().to_string_lossy().as_ref(), &db).unwrap();
        (store, dir)
    }

    fn doc_node(id: &str, path: &str) -> GraphNode {
        GraphNode {
            id: id.into(),
            node_type: NodeType::Doc,
            name: path.into(),
            path: Some(path.into()),
            meta: None,
            degree: None,
            created_at: None,
            content: None,
        }
    }

    #[test]
    fn test_node_id_conventions() {
        assert_eq!(chunk_node_id("docs/a.md", 0), "chunk:docs/a.md#0");
        assert_eq!(section_node_id("docs/a.md", &["K8s".into(), "Network".into()]), "section:docs/a.md#K8s#Network");
    }

    #[test]
    fn test_build_doc_markdown_sections_and_chunks() {
        let (store, _dir) = temp_store("md");
        store.upsert_node(&doc_node("doc:docs/a.md", "docs/a.md")).unwrap();
        let md = "# 缓存穿透\n\n布隆过滤器可以解决缓存穿透问题。\n\n## 一致性\n\n先更新数据库再删缓存。\n";
        let builder = ChunkGraphBuilder::new(&store);
        let stats = builder.build_doc("docs/a.md", md, 200, 20, None).unwrap();
        assert!(stats.chunks >= 1, "chunks={}", stats.chunks);
        // section 节点应存在（缓存穿透 / 一致性）；内容节点走专用查询（all_nodes 排除 chunk/section）
        let content_nodes = store.list_content_nodes_for_doc("docs/a.md", 1000).unwrap();
        let sections: Vec<&GraphNode> = content_nodes.iter().filter(|n| n.node_type == NodeType::Section).collect();
        assert!(!sections.is_empty(), "expected section nodes");
        // doc → section → chunk 边
        let doc_id = "doc:docs/a.md";
        let nb = store.neighborhood(doc_id, 2, 500, 1000, None, 0.0).unwrap();
        assert!(nb.nodes.iter().any(|n| n.node_type == NodeType::Chunk));
        assert!(nb.nodes.iter().any(|n| n.node_type == NodeType::Section));
        // chunk 节点带内容（HashMap 迭代无序：按内容定位目标 chunk）
        let chunk = nb
            .nodes
            .iter()
            .find(|n| n.node_type == NodeType::Chunk && n.content.as_deref().map(|c| c.contains("布隆过滤器")).unwrap_or(false))
            .expect("chunk containing 布隆过滤器");
        assert!(chunk.content.as_deref().unwrap_or("").contains("布隆过滤器"));
        // 幂等：重复构建不产生重复 chunk 节点（用内容节点查询，all_nodes 排除内容层）
        let before = store.list_content_nodes_for_doc("docs/a.md", 10_000).unwrap()
            .iter().filter(|n| n.node_type == NodeType::Chunk).count();
        builder.build_doc("docs/a.md", md, 200, 20, None).unwrap();
        let after = store.list_content_nodes_for_doc("docs/a.md", 10_000).unwrap()
            .iter().filter(|n| n.node_type == NodeType::Chunk).count();
        assert_eq!(before, after, "rebuild should not duplicate chunk nodes");
    }

    #[test]
    fn test_build_doc_plain_text_no_sections() {
        let (store, _dir) = temp_store("plain");
        store.upsert_node(&doc_node("doc:src/main.rs", "src/main.rs")).unwrap();
        let code = "fn main() {\n    println!(\"hello\");\n}\n";
        let builder = ChunkGraphBuilder::new(&store);
        let stats = builder.build_doc("src/main.rs", code, 100, 10, None).unwrap();
        assert!(stats.chunks >= 1);
        // 无标题 → 无 section
        let nb = store.neighborhood("doc:src/main.rs", 1, 500, 1000, None, 0.0).unwrap();
        assert!(nb.nodes.iter().any(|n| n.node_type == NodeType::Chunk));
        assert!(!nb.nodes.iter().any(|n| n.node_type == NodeType::Section));
    }

    #[test]
    fn test_code_imports_edges() {
        // import 解析（多语言）
        let imports = extract_code_imports(
            "import os\nfrom flask import Flask\nimport React from 'react'\nconst x = require('lodash');\nuse std::collections::HashMap;\nimport java.util.List;\n#include <stdio.h>\n",
        );
        assert!(imports.contains(&"os".to_string()));
        assert!(imports.contains(&"flask".to_string()));
        assert!(imports.contains(&"lodash".to_string()));
        assert!(!imports.contains(&"react".to_string())); // 排除框架依赖
        assert!(!imports.iter().any(|i| i.starts_with("std::"))); // 排除标准库

        // import 目标 → 图内文档路径
        let paths: std::collections::HashSet<String> = [
            "src/services/bookmark_service.py".to_string(),
            "src/utils/logger.py".to_string(),
            "app/main.ts".to_string(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            resolve_import_target(".bookmark_service", &paths),
            Some("src/services/bookmark_service.py".to_string())
        );
        assert_eq!(resolve_import_target("src/services/bookmark_service", &paths), Some("src/services/bookmark_service.py".to_string()));
        assert_eq!(resolve_import_target("app/main.ts", &paths), Some("app/main.ts".to_string()));

        // 全量构建：代码文件产生 IMPORTS 边（目标需已入图，避免幽灵节点）
        let (store, _dir) = temp_store("imports");
        store.upsert_node(&doc_node("doc:app/main.ts", "app/main.ts")).unwrap();
        store.upsert_node(&doc_node("doc:src/services/bookmark_service.py", "src/services/bookmark_service.py")).unwrap();
        let builder = ChunkGraphBuilder::new(&store);
        let ts = "import { BookmarkService } from '../src/services/bookmark_service';\n";
        let paths: std::collections::HashSet<String> = [
            "app/main.ts".to_string(),
            "src/services/bookmark_service.py".to_string(),
        ]
        .into_iter()
        .collect();
        builder.build_doc("app/main.ts", ts, 200, 20, Some(&paths)).unwrap();
        let nb = store.neighborhood("doc:app/main.ts", 1, 500, 1000, None, 0.0).unwrap();
        assert!(
            nb.edges.iter().any(|e| e.relation == Relation::Imports),
            "expected IMPORTS edge, got {:?}",
            nb.edges.iter().map(|e| e.relation.as_str()).collect::<Vec<_>>()
        );
    }
}
