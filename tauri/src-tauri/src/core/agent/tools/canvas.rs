//! Canvas 知识画布格式校验（确定性内部模块，非 Agent 工具，不做任何布局）。
//!
//! 架构（v6）：Canvas 是「Agent 可以读写的知识文件格式」，**布局完全由模型负责**。
//! - **LLM（经 Skill 引导）负责全部语义与空间**：理解知识、设计节点/关系、
//!   决定布局模式/层级/坐标/尺寸/连线方向，输出带最终 x/y/width/height 的完整 Canvas
//! - **本模块只做机器可验证的合法性校验**：JSON parse、schema 校验、ID 唯一化、
//!   edge 引用完整性、file 路径存在性、坐标/尺寸合法性；**绝不计算/覆盖坐标**，
//!   模型写入的 x/y/width/height 原样保留
//! - **write 工具检测 `.canvas` 扩展名后自动调用本管线**（见 tools::write_file）
//!
//! 与前端 D3 渲染器（main.html renderCanvasFile）共用 JSON Canvas 数据格式 `{nodes, edges}`：
//! - 节点：`text` / `file`（绑定知识库文件）/ `image` / `link` / `url` / `bookmark` / `code`
//! - 边：`fromNode -> toNode`（带方向与可选 label）
//! - 节点必须包含模型计算的 `x/y/width/height`（缺失或非法 → 拒绝写入）

use std::collections::{HashMap, HashSet};

use super::safe_resolve_new;

// ─────────────────────────── JSON Canvas 数据模型 ───────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasNode {
    pub id: String,
    #[serde(rename = "type", default = "default_node_type")]
    pub ty: String,
    /// 模型负责布局：坐标/尺寸必须由模型给出，系统只校验不重算
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_root: Option<bool>,
}

/// 节点类型缺省值（JSON Canvas 中最常用的 text 节点）
fn default_node_type() -> String {
    "text".into()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasEdge {
    // id 可选（缺失时由 sanitize_ids 统一补唯一 id）
    #[serde(default)]
    pub id: String,
    #[serde(rename = "fromNode")]
    pub from_node: String,
    #[serde(rename = "toNode")]
    pub to_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// 布局分组（模型声明的空间区域提示；系统不据此重排）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasGroup {
    pub id: String,
    #[serde(default)]
    pub nodes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// 模型声明的布局意图记录（仅持久化保存；**系统不解释、不据此重排坐标**）。
///
/// v6 边界：`layout` 是「AI 的布局意图」，最终空间状态以模型写入的
/// `x/y/width/height` 为准。本模块保留该字段原样序列化，仅在校验/重编号
/// 时同步其内部 id 引用。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasLayout {
    /// 布局算法版本（当前 1；仅记录）
    #[serde(default = "default_layout_version")]
    pub version: u32,
    /// 布局模式：hierarchy / flow / radial / grouped（仅记录，不执行）
    #[serde(default = "default_layout_mode")]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub main_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<CanvasGroup>,
}

fn default_layout_version() -> u32 {
    1
}

fn default_layout_mode() -> String {
    "hierarchy".into()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasFile {
    /// 模型布局意图（仅记录；缺省不参与任何布局计算）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<CanvasLayout>,
    pub nodes: Vec<CanvasNode>,
    #[serde(default)]
    pub edges: Vec<CanvasEdge>,
}

// ─────────────────────────── 校验与规整 ───────────────────────────

/// 节点 id 重编号（n1..nN）并同步更新边的引用（模型输出 id 可能重复/含非法字符），
/// 同时清理引用不存在节点的悬空边，并为缺失/重复的边 id 补唯一 id。
fn sanitize_ids(canvas: &mut CanvasFile) {
    let mut map: HashMap<String, String> = HashMap::new();
    for (i, n) in canvas.nodes.iter_mut().enumerate() {
        let new_id = format!("n{}", i + 1);
        map.insert(n.id.clone(), new_id.clone());
        n.id = new_id;
    }
    for e in &mut canvas.edges {
        if let Some(f) = map.get(&e.from_node) {
            e.from_node = f.clone();
        }
        if let Some(t) = map.get(&e.to_node) {
            e.to_node = t.clone();
        }
    }
    // 过滤悬空边：from/to 必须指向重编号后仍存在的节点
    let valid: HashSet<&str> = canvas.nodes.iter().map(|n| n.id.as_str()).collect();
    canvas
        .edges
        .retain(|e| valid.contains(e.from_node.as_str()) && valid.contains(e.to_node.as_str()));
    // 补全边 id：为空或与已有 id 重复时生成 e{seq} 唯一 id
    let mut used: HashSet<String> = HashSet::new();
    let mut seq = canvas.edges.len();
    for e in &mut canvas.edges {
        if !e.id.is_empty() && used.insert(e.id.clone()) {
            continue;
        }
        loop {
            seq += 1;
            let cand = format!("e{}", seq);
            if used.insert(cand.clone()) {
                e.id = cand;
                break;
            }
        }
    }
    // 同步更新 layout 意图中的 id 引用（root / 分组节点列表 / 主链）
    if let Some(layout) = &mut canvas.layout {
        if let Some(r) = layout.root.as_mut() {
            if let Some(new) = map.get(r) {
                *r = new.clone();
            }
        }
        for g in &mut layout.groups {
            for nid in g.nodes.iter_mut() {
                if let Some(new) = map.get(nid) {
                    *nid = new.clone();
                }
            }
        }
        for nid in &mut layout.main_path {
            if let Some(new) = map.get(nid) {
                *nid = new.clone();
            }
        }
    }
}

/// 校验 file/image 节点引用的文件是否真实存在于知识库根目录（dir），
/// 不存在的路径降级为 text 节点（防模型编造路径）；返回降级数量。
fn degrade_missing_file_nodes(canvas: &mut CanvasFile, dir: &str) -> usize {
    let mut degraded = 0usize;
    for n in &mut canvas.nodes {
        if n.ty != "file" && n.ty != "image" {
            continue;
        }
        let path_ok = n
            .file
            .as_ref()
            .and_then(|f| safe_resolve_new(dir, f).ok())
            .is_some_and(|p| p.is_file());
        if !path_ok {
            n.ty = "text".into();
            n.file = None;
            degraded += 1;
        }
    }
    degraded
}

/// 坐标/尺寸合法性校验（v6：布局由模型负责，系统只校验参数合法）。
///
/// - `width`/`height` 必须 > 0（节点有实际尺寸，模型必须给出布局）
/// - `x`/`y` 必须是有限数（防 NaN/Infinity 污染画布坐标）
/// 不做任何位置计算或重排——模型写入的坐标原样保留。
fn validate_geometry(canvas: &CanvasFile) -> Result<(), String> {
    for n in &canvas.nodes {
        if !n.width.is_finite() || !n.height.is_finite() || n.width <= 0.0 || n.height <= 0.0 {
            return Err(format!(
                "节点 {}（{}）缺少有效尺寸：width/height 必须 > 0（当前 {} x {}）。布局由模型负责，请为每个节点提供完整的 x/y/width/height",
                n.id, n.ty, n.width, n.height
            ));
        }
        if !n.x.is_finite() || !n.y.is_finite() {
            return Err(format!(
                "节点 {}（{}）坐标非法：x/y 必须是有限数值（当前 x={} y={}）",
                n.id, n.ty, n.x, n.y
            ));
        }
    }
    Ok(())
}

/// **Canvas 确定性校验管线入口**（供 write 工具对 `.canvas` 文件调用）：
/// parse → schema 校验 → 空节点清理 → ID 唯一化 → edge 引用校验 →
/// file 存在性校验 → 坐标/尺寸合法性校验 → 原样序列化。
///
/// v6 边界：**不执行任何布局计算、不覆盖模型坐标**。模型写入的
/// x/y/width/height 原样保留；`layout` 意图仅作记录一并保存。
/// 任一步失败返回明确错误（write 拒绝写入），保证落盘的 `.canvas` 合法可渲染。
pub fn validate_canvas_json(content: &str, dir: &str) -> Result<String, String> {
    let mut canvas: CanvasFile = serde_json::from_str(content)
        .map_err(|e| format!("无效的 JSON Canvas 格式: {}", e))?;
    if canvas.nodes.is_empty() {
        return Err("画布无节点（nodes 为空数组）".into());
    }
    sanitize_ids(&mut canvas);
    let _degraded = degrade_missing_file_nodes(&mut canvas, dir);
    validate_geometry(&canvas)?;
    serde_json::to_string_pretty(&canvas).map_err(|e| format!("序列化画布失败: {}", e))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn text_node(id: &str) -> CanvasNode {
        CanvasNode {
            id: id.into(),
            ty: "text".into(),
            x: 0.0,
            y: 0.0,
            width: 240.0,
            height: 120.0,
            text: Some(id.into()),
            file: None,
            url: None,
            code: None,
            is_root: None,
        }
    }

    #[test]
    fn sanitize_ids_renumbers_and_keeps_edge_references() {
        let mut canvas = CanvasFile {
            layout: None,
            nodes: vec![text_node("a-b"), text_node("c d"), text_node("root")],
            edges: vec![
                CanvasEdge { id: "e1".into(), from_node: "a-b".into(), to_node: "c d".into(), label: None },
                CanvasEdge { id: "e2".into(), from_node: "root".into(), to_node: "a-b".into(), label: None },
            ],
        };
        sanitize_ids(&mut canvas);
        let mut by_id = std::collections::HashMap::new();
        for n in &canvas.nodes {
            by_id.insert(n.id.clone(), n.text.clone().unwrap_or_default());
        }
        let ids: Vec<&str> = canvas.nodes.iter().map(|n| n.id.as_str()).collect();
        let uniq: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(uniq.len(), ids.len(), "id 应唯一");
        assert!(ids.iter().all(|i| i.starts_with('n')), "id 应重编号为 n1..nN: {:?}", ids);
        for e in &canvas.edges {
            assert!(by_id.contains_key(&e.from_node), "边 from {}", e.from_node);
            assert!(by_id.contains_key(&e.to_node), "边 to {}", e.to_node);
        }
        assert_eq!(by_id.len(), 3);
    }

    #[test]
    fn sanitize_ids_drops_dangling_edges() {
        let mut canvas = CanvasFile {
            layout: None,
            nodes: vec![text_node("a"), text_node("b")],
            edges: vec![
                CanvasEdge { id: "e1".into(), from_node: "a".into(), to_node: "b".into(), label: None },
                CanvasEdge { id: "e2".into(), from_node: "ghost".into(), to_node: "a".into(), label: None },
                CanvasEdge { id: "e3".into(), from_node: "b".into(), to_node: "ghost".into(), label: None },
            ],
        };
        sanitize_ids(&mut canvas);
        assert_eq!(canvas.edges.len(), 1, "悬空边应被过滤，仅保留 e1");
        assert_eq!(canvas.edges[0].from_node, "n1");
        assert_eq!(canvas.edges[0].to_node, "n2");
    }

    #[test]
    fn sanitize_ids_fills_missing_and_duplicate_edge_ids() {
        let mut canvas = CanvasFile {
            layout: None,
            nodes: vec![text_node("n1"), text_node("n2")],
            edges: vec![
                CanvasEdge { id: "".into(), from_node: "n1".into(), to_node: "n2".into(), label: None },
                CanvasEdge { id: "dup".into(), from_node: "n2".into(), to_node: "n1".into(), label: None },
                CanvasEdge { id: "dup".into(), from_node: "n1".into(), to_node: "n1".into(), label: None },
            ],
        };
        sanitize_ids(&mut canvas);
        let ids: Vec<&str> = canvas.edges.iter().map(|e| e.id.as_str()).collect();
        let uniq: std::collections::HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(uniq.len(), ids.len(), "边 id 应全部唯一: {:?}", ids);
        assert!(ids.iter().all(|i| !i.is_empty()), "边 id 不应为空");
    }

    #[test]
    fn degrade_missing_file_nodes_downgrades_fake_paths() {
        let tmp = std::env::temp_dir().join(format!(
            "mdgo-canvas-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(tmp.join("docs")).unwrap();
        std::fs::write(tmp.join("docs/real.md"), "# real").unwrap();
        let mut canvas = CanvasFile {
            layout: None,
            nodes: vec![
                CanvasNode { id: "n1".into(), ty: "file".into(), x: 0.0, y: 0.0, width: 240.0, height: 120.0, text: Some("真实文件".into()), file: Some("docs/real.md".into()), url: None, code: None, is_root: None },
                CanvasNode { id: "n2".into(), ty: "file".into(), x: 0.0, y: 0.0, width: 240.0, height: 120.0, text: Some("编造路径".into()), file: Some("docs/fake.md".into()), url: None, code: None, is_root: None },
                CanvasNode { id: "n3".into(), ty: "image".into(), x: 0.0, y: 0.0, width: 240.0, height: 120.0, text: Some("无路径图片".into()), file: None, url: None, code: None, is_root: None },
            ],
            edges: vec![],
        };
        let degraded = degrade_missing_file_nodes(&mut canvas, tmp.to_str().unwrap());
        assert_eq!(degraded, 2, "n2/n3 应降级，n1 保留");
        assert_eq!(canvas.nodes[0].ty, "file");
        assert_eq!(canvas.nodes[1].ty, "text");
        assert!(canvas.nodes[1].file.is_none(), "降级后 file 字段应清空");
        assert_eq!(canvas.nodes[2].ty, "text");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_canvas_json_preserves_model_coordinates() {
        // v6 核心：模型提供的坐标/尺寸原样保留（不重算、不覆盖）
        let raw = r#"{
            "layout": {"mode": "flow", "direction": "LR", "root": "n1"},
            "nodes": [
                {"id": "n1", "type": "text", "text": "A", "x": 100, "y": 50, "width": 200, "height": 80},
                {"id": "n2", "type": "text", "text": "B", "x": 400, "y": 50, "width": 240, "height": 100}
            ],
            "edges": [{"fromNode": "n1", "toNode": "n2"}]
        }"#;
        let out = validate_canvas_json(raw, ".").expect("合法画布应通过");
        let parsed: CanvasFile = serde_json::from_str(&out).expect("输出应为合法 JSON Canvas");
        // 坐标/尺寸原样保留
        assert_eq!(parsed.nodes[0].x, 100.0);
        assert_eq!(parsed.nodes[0].y, 50.0);
        assert_eq!(parsed.nodes[0].width, 200.0);
        assert_eq!(parsed.nodes[1].x, 400.0);
        assert_eq!(parsed.nodes[1].height, 100.0);
        // layout 意图保留
        let lay = parsed.layout.expect("layout 应保留");
        assert_eq!(lay.mode, "flow");
        assert_eq!(lay.direction.as_deref(), Some("LR"));
        // 边 id 补全、引用有效
        assert_eq!(parsed.edges.len(), 1);
        assert!(!parsed.edges[0].id.is_empty());
    }

    #[test]
    fn validate_geometry_rejects_missing_size_or_bad_coordinates() {
        // 缺尺寸（width/height <= 0，即模型未布局）→ 拒绝
        let no_size = r#"{"nodes":[{"id":"n1","type":"text","text":"x","x":0,"y":0}]}"#;
        let err = validate_canvas_json(no_size, ".").unwrap_err();
        assert!(err.contains("width/height 必须 > 0"), "缺尺寸应提示: {}", err);
        // 坐标 NaN → 拒绝
        let nan_coord = r#"{"nodes":[{"id":"n1","type":"text","text":"x","x":"NaN","y":0,"width":200,"height":80}]}"#;
        assert!(validate_canvas_json(nan_coord, ".").is_err(), "NaN 坐标应拒绝");
        // 非 JSON / 空 nodes → 拒绝
        assert!(validate_canvas_json("not json", ".").is_err());
        assert!(validate_canvas_json(r#"{"nodes":[],"edges":[]}"#, ".").is_err(), "空画布应拒绝");
    }

    #[test]
    fn sanitize_ids_remaps_layout_root_and_groups() {
        let mut canvas = CanvasFile {
            layout: Some(CanvasLayout {
                version: 1,
                mode: "grouped".into(),
                root: Some("root".into()),
                direction: None,
                main_path: vec!["root".into(), "a".into()],
                groups: vec![CanvasGroup { id: "g1".into(), nodes: vec!["root".into(), "a".into()], title: None }],
            }),
            nodes: vec![
                CanvasNode { id: "root".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 240.0, height: 120.0, text: Some("R".into()), file: None, url: None, code: None, is_root: Some(true) },
                CanvasNode { id: "a".into(), ty: "text".into(), x: 300.0, y: 0.0, width: 240.0, height: 120.0, text: Some("A".into()), file: None, url: None, code: None, is_root: None },
            ],
            edges: vec![CanvasEdge { id: "e1".into(), from_node: "root".into(), to_node: "a".into(), label: None }],
        };
        sanitize_ids(&mut canvas);
        let layout = canvas.layout.expect("layout 应保留");
        assert_eq!(layout.root.as_deref(), Some("n1"), "root 应重映射为 n1");
        assert_eq!(layout.main_path, vec!["n1".to_string(), "n2".to_string()], "main_path 应重映射");
        assert_eq!(layout.groups[0].nodes, vec!["n1".to_string(), "n2".to_string()], "groups 应重映射");
    }

    #[test]
    fn benchmark_cases_parse_and_preserve_coordinates() {
        // 回归基线：docs/canvas-benchmark-cases/ 下 10 个固定用例
        // 必须可被 CanvasFile 解析、含模型坐标、经管线后坐标原样保留（不被改写）。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/canvas-benchmark-cases");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("benchmark-cases 目录应存在")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".canvas"))
            .collect();
        assert_eq!(entries.len(), 10, "应有 10 个 benchmark 用例");
        for entry in &entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let text = std::fs::read_to_string(entry.path())
                .unwrap_or_else(|e| panic!("{} 读取失败: {}", name, e));
            let parsed: CanvasFile = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} 解析失败: {}", name, e));
            assert!(!parsed.nodes.is_empty(), "{} 应有节点", name);
            // 用例含模型坐标（v6：x/y/width/height 由用例文件提供）
            assert!(
                parsed.nodes.iter().all(|n| n.width > 0.0 && n.height > 0.0),
                "{} 的节点应含有效尺寸（v6 布局由模型提供）",
                name
            );
            // 管线通过且坐标原样保留
            let out = validate_canvas_json(&text, ".")
                .unwrap_or_else(|e| panic!("{} 管线失败: {}", name, e));
            let relaid: CanvasFile = serde_json::from_str(&out).unwrap();
            for (a, b) in parsed.nodes.iter().zip(relaid.nodes.iter()) {
                assert_eq!(a.x, b.x, "{} 节点 {} x 应原样保留", name, a.id);
                assert_eq!(a.y, b.y, "{} 节点 {} y 应原样保留", name, a.id);
                assert_eq!(a.width, b.width, "{} 节点 {} width 应原样保留", name, a.id);
            }
        }
    }
}
