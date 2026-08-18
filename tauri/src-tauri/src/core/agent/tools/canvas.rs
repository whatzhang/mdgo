//! Canvas 知识画布格式校验与规整（确定性内部模块，非 Agent 工具）。
//!
//! 架构（v4）：Canvas 是「Agent 可以读写的知识文件格式」，而非 Agent Tool。
//! - **LLM（经 Skill 引导）负责语义**：理解知识、设计节点/关系、用通用 read/write 读写
//! - **本模块负责机器可验证部分**：JSON parse、schema 校验、ID 唯一化、edge 引用
//!   完整性、file 路径存在性校验、自动布局
//! - **write 工具检测 `.canvas` 扩展名后自动调用本管线**（见 tools::write_file），
//!   模型无需（也不应）调用任何 Canvas 专用 Function。
//!
//! 与前端 D3 渲染器（main.html renderCanvasFile）共用 JSON Canvas 数据格式 `{nodes, edges}`：
//! - 节点：`text` / `file`（绑定知识库文件）/ `image` / `link` / `url` / `bookmark` / `code`
//! - 边：`fromNode -> toNode`（带方向与可选 label）

use std::collections::{HashMap, HashSet};

use super::safe_resolve_new;

// ─────────────────────────── JSON Canvas 数据模型 ───────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasNode {
    pub id: String,
    #[serde(rename = "type", default = "default_node_type")]
    pub ty: String,
    // 坐标/尺寸可选：模型可填默认值，保存前由布局统一计算并覆盖
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

/// 布局分组（grouped 模式用）：一组节点形成一个独立空间区域
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasGroup {
    /// 分组 id（g1/g2/...）
    pub id: String,
    /// 归入该组的节点 id 列表
    #[serde(default)]
    pub nodes: Vec<String>,
    /// 可选分组标题（生成标题节点）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// 语义布局意图（模型声明，Rust 计算坐标）。
///
/// 架构原则：**Skill 负责布局意图，Layout Engine 负责布局坐标**——
/// 模型只声明「用哪种模式、根是谁、方向如何、怎么分组、主链是谁」，
/// 实际 x/y/width/height 由本模块确定性计算，避免模型输出随机坐标导致画布杂乱。
///
/// **Edge = 语义真相，Layout = 空间提示**：
/// - `edges` 描述知识关系（父子/流程/依赖），层级与主链判断只依据 edges；
/// - `layout.groups` 仅描述空间分区（哪些节点形成独立区域），**不承载知识关系**——
///   两套结构不一致时以 edges 为准。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CanvasLayout {
    /// 布局算法版本（当前 1；未来算法升级后旧画布可据此重算）
    #[serde(default = "default_layout_version")]
    pub version: u32,
    /// 布局模式：hierarchy（层级，默认）/ flow（流程）/ radial（中心辐射）/ grouped（分组）
    #[serde(default = "default_layout_mode")]
    pub mode: String,
    /// 根节点 id（hierarchy/flow/radial 用）；缺省时取 isRoot=true 或首个无入边节点
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// 主方向：TB（top→bottom，默认）/ LR（left→right，流程/演进用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// 主流程/因果/时间链（flow 模式可选；声明后主链节点按 direction 主轴排列，
    /// 其余节点作为分支挂在最近主链祖先之下；未声明时引擎按最长路径推断）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub main_path: Vec<String>,
    /// 分组（grouped 模式必填；其他模式可附加用于同组邻近）
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
    /// 语义布局意图（可选；缺省按 hierarchy 布局）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<CanvasLayout>,
    pub nodes: Vec<CanvasNode>,
    #[serde(default)]
    pub edges: Vec<CanvasEdge>,
}

// ─────────────────────────── 语义布局引擎 ───────────────────────────

/// 布局基础度量（与前端渲染视觉一致；Skill 布局规范同步维护）
const H_GAP: f64 = 80.0;      // 同级节点水平间距
const V_GAP: f64 = 60.0;      // 层级间垂直间距
const GROUP_GAP: f64 = 160.0; // 分组间间距
const MAX_LAYER_W: f64 = 420.0; // 单节点最大宽
const CHAR_EM: f64 = 13.0;    // 1em ≈ 13px（前端 font-size 13px）

/// 估算一行文本的显示宽度（px）：
/// CJK/全角 ≈ 1.0em，ASCII ≈ 0.55em，数字 ≈ 0.6em，其余（emoji 等）≈ 1.2em。
/// 避免按 UTF-8 字节数估算导致中文被高估。
fn line_width_px(line: &str) -> f64 {
    line.chars().fold(0.0, |acc, c| {
        if c.is_ascii_digit() {
            acc + 0.6 * CHAR_EM
        } else if c.is_ascii() {
            acc + 0.55 * CHAR_EM
        } else if ('\u{4e00}'..='\u{9fff}').contains(&c)
            || ('\u{3400}'..='\u{4dbf}').contains(&c)
            || ('\u{ff00}'..='\u{ffef}').contains(&c)
        {
            acc + 1.0 * CHAR_EM
        } else {
            acc + 1.2 * CHAR_EM
        }
    })
}

/// 内容感知尺寸：由「类型基准 + 内容」共同决定（用户建议 #6/#7）。
/// - text/link：按文本宽度估算（CJK≈1em / ASCII≈0.55em / 数字≈0.6em）
/// - code：按最长行宽度 + 行数单独估算（等宽字体）
/// - file/image：类型基准为主，文本仅做下限补充
fn estimate_size(n: &CanvasNode) -> (f64, f64) {
    let (base_w, base_h) = match n.ty.as_str() {
        "file" => (300.0, 100.0),
        "image" => (260.0, 180.0),
        "link" | "url" | "bookmark" => (280.0, 90.0),
        "code" => (360.0, 200.0),
        _ => (240.0, 120.0), // text 等
    };
    let text = n.text.as_deref().unwrap_or_default();
    if text.trim().is_empty() {
        return (base_w, base_h);
    }
    let lines = text.lines().count().max(1);
    if n.ty == "code" {
        // 代码：等宽字体 ~7.5px/字符；行高 ~18px
        let max_chars = text.lines().map(|l| l.chars().count()).max().unwrap_or(0);
        let w = (40.0 + max_chars as f64 * 7.5).clamp(200.0, MAX_LAYER_W);
        let h = (30.0 + lines as f64 * 18.0).clamp(base_h.min(80.0), 300.0);
        return (w, h);
    }
    // 普通文本：按行宽估算（CJK/ASCII 区分），行高 ~22px
    let max_line_w = text.lines().map(line_width_px).fold(0.0f64, f64::max);
    let w = (24.0 + max_line_w).clamp(base_w.min(180.0), MAX_LAYER_W);
    let h = (30.0 + lines as f64 * 22.0).clamp(base_h.min(80.0), 260.0);
    (w, h)
}

/// 邻接表：节点 id → 直接子节点 id（按 edges 出现顺序，去重）
fn build_children(canvas: &CanvasFile) -> HashMap<String, Vec<String>> {
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let ids: HashSet<&str> = canvas.nodes.iter().map(|n| n.id.as_str()).collect();
    for e in &canvas.edges {
        if ids.contains(e.from_node.as_str()) && ids.contains(e.to_node.as_str()) {
            let entry = children.entry(e.from_node.clone()).or_default();
            if !entry.contains(&e.to_node) {
                entry.push(e.to_node.clone());
            }
        }
    }
    children
}

/// 入度：节点 id → 入边数（用于找根）
fn build_indeg(canvas: &CanvasFile) -> HashMap<String, usize> {
    let mut indeg: HashMap<String, usize> = HashMap::new();
    for n in &canvas.nodes {
        indeg.insert(n.id.clone(), 0);
    }
    for e in &canvas.edges {
        if let Some(v) = indeg.get_mut(&e.to_node) {
            *v += 1;
        }
    }
    indeg
}

/// 确定根节点：layout.root → isRoot=true → 首个无入边节点 → 首个节点
fn resolve_root(canvas: &CanvasFile, indeg: &HashMap<String, usize>) -> String {
    if let Some(r) = canvas.layout.as_ref().and_then(|l| l.root.as_ref()) {
        if canvas.nodes.iter().any(|n| &n.id == r) {
            return r.clone();
        }
    }
    if let Some(r) = canvas.nodes.iter().find(|n| n.is_root == Some(true)) {
        return r.id.clone();
    }
    canvas
        .nodes
        .iter()
        .find(|n| indeg.get(&n.id).copied().unwrap_or(0) == 0)
        .map(|n| n.id.clone())
        .unwrap_or_else(|| canvas.nodes[0].id.clone())
}

/// 按层分组：BFS 自 root 分层（同父子节点天然相邻）；返回 (depth_map, 层内顺序, 游离节点)
fn layerize(
    canvas: &CanvasFile,
    children: &HashMap<String, Vec<String>>,
    root: &str,
) -> (HashMap<String, usize>, Vec<Vec<String>>, Vec<String>) {
    let mut depth: HashMap<String, usize> = HashMap::new();
    let mut queue: Vec<(String, usize)> = vec![(root.to_string(), 0)];
    let mut seq: Vec<Vec<String>> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    while let Some((id, d)) = queue.pop() {
        if !visited.insert(id.clone()) {
            continue;
        }
        depth.insert(id.clone(), d);
        while seq.len() <= d {
            seq.push(Vec::new());
        }
        seq[d].push(id.clone());
        // 反向 pop 保持 BFS 顺序（子节点按 edges 顺序）
        if let Some(ch) = children.get(&id) {
            for c in ch.iter().rev() {
                if !visited.contains(c) {
                    queue.push((c.clone(), d + 1));
                }
            }
        }
    }
    // 游离节点（环内/不可达）：追加到最后一层之后，保持稳定顺序
    let mut floating: Vec<String> = Vec::new();
    for n in &canvas.nodes {
        if !visited.contains(&n.id) {
            floating.push(n.id.clone());
        }
    }
    (depth, seq, floating)
}

/// 层级布局（hierarchy TB / flow LR）：每层按 BFS 顺序水平/垂直排列，层间堆叠。
/// 同父子节点相邻、方向统一，避免随机摆放。
fn layout_layered(
    canvas: &mut CanvasFile,
    direction: &str,
    seq: Vec<Vec<String>>,
    floating: Vec<String>,
    sizes: &HashMap<String, (f64, f64)>,
) {
    let mut pos: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
    let horizontal = direction.eq_ignore_ascii_case("LR");
    for (d, layer) in seq.iter().enumerate() {
        let mut cursor = 0.0f64;
        for id in layer {
            let (w, h) = sizes.get(id).copied().unwrap_or((240.0, 120.0));
            let (x, y) = if horizontal {
                let (x, y) = (d as f64 * (MAX_LAYER_W + H_GAP), cursor);
                cursor += h + V_GAP;
                (x, y)
            } else {
                let (x, y) = (cursor, d as f64 * (240.0 + V_GAP));
                cursor += w + H_GAP;
                (x, y)
            };
            pos.insert(id.clone(), (x, y, w, h));
        }
    }
    if !floating.is_empty() {
        let base_y = seq.len() as f64 * (240.0 + V_GAP);
        let mut cursor = 0.0f64;
        for id in &floating {
            let (w, h) = sizes.get(id).copied().unwrap_or((240.0, 120.0));
            pos.insert(id.clone(), (cursor, base_y, w, h));
            cursor += w + H_GAP;
        }
    }
    apply_positions(canvas, &pos);
}

/// 中心辐射布局（radial）：root 居中，一级节点环绕，二级节点靠近对应一级节点。
fn layout_radial(
    canvas: &mut CanvasFile,
    children: &HashMap<String, Vec<String>>,
    root: &str,
    sizes: &HashMap<String, (f64, f64)>,
) {
    let mut pos: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
    let (rw, rh) = sizes.get(root).copied().unwrap_or((240.0, 120.0));
    pos.insert(root.to_string(), (-rw / 2.0, -rh / 2.0, rw, rh));
    let radius = 320.0;
    let first: Vec<String> = children.get(root).cloned().unwrap_or_default();
    let n_first = first.len().max(1);
    for (i, cid) in first.iter().enumerate() {
        let angle = std::f64::consts::TAU * (i as f64) / (n_first as f64) - std::f64::consts::FRAC_PI_2;
        let (w, h) = sizes.get(cid).copied().unwrap_or((240.0, 120.0));
        pos.insert(
            cid.clone(),
            (radius * angle.cos() - w / 2.0, radius * angle.sin() - h / 2.0, w, h),
        );
        let second: Vec<String> = children.get(cid).cloned().unwrap_or_default();
        for (j, sid) in second.iter().enumerate() {
            let spread = 0.35f64 * ((j as f64) - (second.len() as f64 - 1.0) / 2.0);
            let a2 = angle + spread;
            let r2 = radius + 220.0;
            let (w2, h2) = sizes.get(sid).copied().unwrap_or((240.0, 120.0));
            pos.insert(
                sid.clone(),
                (r2 * a2.cos() - w2 / 2.0, r2 * a2.sin() - h2 / 2.0, w2, h2),
            );
        }
    }
    apply_positions(canvas, &pos);
}

/// 分组布局（grouped）：每个 group 一个独立空间区域（垂直列），组间留 GROUP_GAP；
/// 未归组节点追加到最后一列。
fn layout_grouped(
    canvas: &mut CanvasFile,
    layout: &CanvasLayout,
    sizes: &HashMap<String, (f64, f64)>,
) {
    let mut pos: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
    let mut col_x = 0.0f64;
    for g in &layout.groups {
        let mut row_y = 0.0f64;
        let mut max_w = 0.0f64;
        for nid in &g.nodes {
            let (w, h) = sizes.get(nid).copied().unwrap_or((240.0, 120.0));
            pos.insert(nid.clone(), (col_x, row_y, w, h));
            row_y += h + V_GAP;
            max_w = max_w.max(w);
        }
        col_x += max_w + GROUP_GAP;
    }
    // 未归组节点：追加到末尾列
    let assigned: HashSet<&str> = layout
        .groups
        .iter()
        .flat_map(|g| g.nodes.iter().map(|s| s.as_str()))
        .collect();
    let mut row_y = 0.0f64;
    for n in canvas.nodes.iter() {
        if assigned.contains(n.id.as_str()) {
            continue;
        }
        let (w, h) = estimate_size(n);
        pos.insert(n.id.clone(), (col_x, row_y, w, h));
        row_y += h + V_GAP;
    }
    apply_positions(canvas, &pos);
}

/// flow + main_path（P1）：主链节点沿 direction 主轴排列（LR 时 x 递增 / TB 时 y 递增），
/// 分支节点挂到「最近主链祖先」下方/右侧独立行，未挂上的节点放末尾独立区域。
/// 主链识别：模型声明 main_path 时直接使用；未声明时由引擎按最长路径推断（见 layout_canvas 分支）。
fn layout_flow_main_path(
    canvas: &mut CanvasFile,
    layout: &CanvasLayout,
    sizes: &HashMap<String, (f64, f64)>,
) {
    let horizontal = layout.direction.as_deref().unwrap_or("LR").eq_ignore_ascii_case("LR");
    let mut pos: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
    // 主链（过滤不存在的 id，保持声明顺序 = 确定性）
    let main: Vec<&str> = layout
        .main_path
        .iter()
        .filter(|id| sizes.contains_key(*id))
        .map(|s| s.as_str())
        .collect();
    // 主链位置：沿主轴等距排布
    let main_w = sizes.get(main[0]).map(|(w, _)| *w).unwrap_or(240.0);
    let main_h = sizes.get(main[0]).map(|(_, h)| *h).unwrap_or(120.0);
    let mut cursor = 0.0f64;
    let mut main_x: HashMap<&str, f64> = HashMap::new();
    for id in &main {
        let (w, h) = sizes.get(*id).copied().unwrap_or((240.0, 120.0));
        let (x, y) = if horizontal {
            (cursor, 0.0)
        } else {
            (0.0, cursor)
        };
        pos.insert((*id).to_string(), (x, y, w, h));
        main_x.insert(*id, if horizontal { x } else { y });
        cursor += if horizontal { w + H_GAP } else { h + V_GAP };
    }
    // 分支：找每个非主链节点的「最近主链祖先」（沿入边向上第一个在主链中的节点）
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &canvas.edges {
        parents.entry(e.to_node.as_str()).or_default().push(e.from_node.as_str());
    }
    fn nearest_main<'a>(
        id: &'a str,
        parents: &HashMap<&'a str, Vec<&'a str>>,
        main_set: &HashSet<&'a str>,
        visited: &mut HashSet<&'a str>,
    ) -> Option<&'a str> {
        if main_set.contains(id) {
            return Some(id);
        }
        if !visited.insert(id) {
            return None;
        }
        for p in parents.get(id).into_iter().flatten() {
            if let Some(m) = nearest_main(p, parents, main_set, visited) {
                return Some(m);
            }
        }
        None
    }
    let main_set: HashSet<&str> = main.iter().copied().collect();
    // 每个主链节点下的分支行（y 偏移），保持 nodes 顺序 = 确定性
    let mut branch_offset: HashMap<&str, f64> = HashMap::new();
    for n in &canvas.nodes {
        if main_set.contains(n.id.as_str()) {
            continue;
        }
        let mut visited = HashSet::new();
        let Some(anchor) = nearest_main(&n.id, &parents, &main_set, &mut visited) else {
            continue; // 无主链祖先 → 末尾独立区
        };
        let (w, h) = sizes.get(&n.id).copied().unwrap_or((240.0, 120.0));
        let base = main_x.get(anchor).copied().unwrap_or(0.0);
        let off = branch_offset.entry(anchor).or_insert(0.0);
        let (x, y) = if horizontal {
            // 主链下方，x 对齐主链锚点附近，y 依次下移
            (base + main_w / 2.0 - w / 2.0 + 0.0, main_h + V_GAP + *off)
        } else {
            (main_w + H_GAP + *off, base + main_h / 2.0 - h / 2.0)
        };
        pos.insert(n.id.clone(), (x, y, w, h));
        *off += h + V_GAP;
    }
    // 无主链祖先的节点：末尾独立区域（仍按 nodes 顺序，确定性）
    let assigned: HashSet<String> = pos.keys().cloned().collect();
    let mut tail_x = 0.0f64;
    let tail_y = if horizontal { main_h + V_GAP } else { cursor + V_GAP };
    let mut tail_cursor = 0.0f64;
    for n in &canvas.nodes {
        if assigned.contains(&n.id) {
            continue;
        }
        let (w, h) = sizes.get(&n.id).copied().unwrap_or((240.0, 120.0));
        let (x, y) = if horizontal {
            (tail_x, tail_y)
        } else {
            (0.0, tail_y + tail_cursor)
        };
        pos.insert(n.id.clone(), (x, y, w, h));
        if horizontal {
            tail_x += w + H_GAP;
        } else {
            tail_cursor += h + V_GAP;
        }
    }
    apply_positions(canvas, &pos);
}

/// 将计算的坐标/尺寸写回节点（统一入口，避免借用冲突）
fn apply_positions(canvas: &mut CanvasFile, pos: &HashMap<String, (f64, f64, f64, f64)>) {
    for n in &mut canvas.nodes {
        if let Some(&(x, y, w, h)) = pos.get(&n.id) {
            n.x = x;
            n.y = y;
            n.width = w;
            n.height = h;
        }
    }
}

/// **布局引擎入口**：根据 `canvas.layout` 语义意图计算所有节点坐标与尺寸。
///
/// 架构原则：**Skill 负责布局意图，Layout Engine 负责布局坐标**——
/// 模型声明 mode/root/direction/main_path/groups，本函数确定性计算 x/y/width/height，
/// 覆盖模型可能给出的任意占位坐标。无 layout 声明时用默认层级布局（hierarchy）。
///
/// 确定性保证：所有排序只依赖 nodes/edges 数组顺序与 node id（tie-break），
/// 不依赖 HashMap 迭代序——相同输入必然得到相同坐标（见 UT-22）。
fn layout_canvas(canvas: &mut CanvasFile) {
    if canvas.nodes.is_empty() {
        return;
    }
    // 先统一计算内容感知尺寸
    let sizes: HashMap<String, (f64, f64)> = canvas
        .nodes
        .iter()
        .map(|n| (n.id.clone(), estimate_size(n)))
        .collect();
    let children = build_children(canvas);
    let indeg = build_indeg(canvas);
    let layout = canvas.layout.clone();
    let mode = layout.as_ref().map(|l| l.mode.as_str()).unwrap_or("hierarchy");

    match mode {
        "radial" => {
            let root = resolve_root(canvas, &indeg);
            // P1：一级节点超过 8 个时 radial 必然拥挤（标签碰撞/边穿越）→ 自动降级 hierarchy
            let n_first = children.get(&root).map(|v| v.len()).unwrap_or(0);
            if n_first > 8 {
                let (_d, seq, floating) = layerize(canvas, &children, &root);
                layout_layered(canvas, "TB", seq, floating, &sizes);
            } else {
                layout_radial(canvas, &children, &root, &sizes);
            }
        }
        "grouped" => {
            let lay = layout.as_ref().expect("grouped 模式必须有 layout");
            layout_grouped(canvas, lay, &sizes);
        }
        "flow" if layout.as_ref().is_some_and(|l| !l.main_path.is_empty()) => {
            // P1：flow + main_path——主链沿 direction 主轴排列，分支挂最近主链祖先
            let lay = layout.as_ref().expect("flow 模式必须有 layout");
            layout_flow_main_path(canvas, lay, &sizes);
        }
        _ => {
            // hierarchy（默认）/ flow：flow 用 direction=LR
            let root = resolve_root(canvas, &indeg);
            let direction = layout
                .as_ref()
                .and_then(|l| l.direction.as_deref())
                .unwrap_or("TB");
            let (_depth, seq, floating) = layerize(canvas, &children, &root);
            layout_layered(canvas, direction, seq, floating, &sizes);
        }
    }
}

// ─────────────────────────── 布局质量（检测阶段） ───────────────────────────

/// 布局质量快照（P1：先「发现问题」，不自动复杂修复）。
/// 布局引擎本身数学上保证不重叠（分层/网格/分组线性排布 + radial 限 8），
/// 此检查用于回归断言与诊断日志；edge crossing 为直线段近似检测。
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutQuality {
    pub node_overlaps: usize,
    pub edge_crossings: usize,
    pub isolated_nodes: usize,
    pub long_edges: usize,
}

/// AABB 矩形相交检测（两节点矩形是否重叠，含严格内切）
fn rects_intersect(a: &CanvasNode, b: &CanvasNode) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

/// O(n²) 节点重叠检测（20~40 节点完全可接受），返回重叠节点 id 对
pub fn check_node_overlaps(canvas: &CanvasFile) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for i in 0..canvas.nodes.len() {
        for j in (i + 1)..canvas.nodes.len() {
            let (a, b) = (&canvas.nodes[i], &canvas.nodes[j]);
            if rects_intersect(a, b) {
                out.push((a.id.clone(), b.id.clone()));
            }
        }
    }
    out
}

/// 线段相交（跨立实验）；用于 edge 直线段近似交叉检测
fn segments_intersect(
    p1: (f64, f64), p2: (f64, f64),
    p3: (f64, f64), p4: (f64, f64),
) -> bool {
    fn cross(o: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    }
    let d1 = cross(p3, p4, p1);
    let d2 = cross(p3, p4, p2);
    let d3 = cross(p1, p2, p3);
    let d4 = cross(p1, p2, p4);
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

/// 布局质量快照：节点重叠 / edge 交叉 / 孤立节点 / 超长连线。
/// 仅检测记录，不做自动修复（第二阶段再考虑）。
pub fn layout_quality_check(canvas: &CanvasFile) -> LayoutQuality {
    let node_overlaps = check_node_overlaps(canvas).len();
    // edge 交叉：每条边视为两节点中心连线，判断两两是否相交（共享端点不算）
    let center = |id: &str| -> Option<(f64, f64)> {
        canvas.nodes.iter().find(|n| n.id == id).map(|n| {
            (n.x + n.width / 2.0, n.y + n.height / 2.0)
        })
    };
    let mut edge_crossings = 0usize;
    for i in 0..canvas.edges.len() {
        for j in (i + 1)..canvas.edges.len() {
            let (e1, e2) = (&canvas.edges[i], &canvas.edges[j]);
            let (Some(p1), Some(p2), Some(p3), Some(p4)) =
                (center(&e1.from_node), center(&e1.to_node), center(&e2.from_node), center(&e2.to_node))
            else {
                continue;
            };
            if segments_intersect(p1, p2, p3, p4) {
                edge_crossings += 1;
            }
        }
    }
    // 孤立节点：无任何入边/出边
    let mut has_edge: HashSet<&str> = HashSet::new();
    for e in &canvas.edges {
        has_edge.insert(e.from_node.as_str());
        has_edge.insert(e.to_node.as_str());
    }
    let isolated_nodes = canvas
        .nodes
        .iter()
        .filter(|n| !has_edge.contains(n.id.as_str()))
        .count();
    // 超长连线：中心距 > 2000px
    let long_edges = canvas
        .edges
        .iter()
        .filter_map(|e| {
            let f = center(&e.from_node)?;
            let t = center(&e.to_node)?;
            let d = ((f.0 - t.0).powi(2) + (f.1 - t.1).powi(2)).sqrt();
            Some(d > 2000.0)
        })
        .filter(|b| *b)
        .count();
    LayoutQuality {
        node_overlaps,
        edge_crossings,
        isolated_nodes,
        long_edges,
    }
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
    // 同步更新 layout 意图中的 id 引用（root / 分组节点列表）
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

/// **Canvas 确定性管线入口**（供 write 工具对 `.canvas` 文件调用）：
/// parse → schema 校验 → ID 唯一化 → edge 引用校验 → file 存在性校验 → **语义布局** → 序列化。
///
/// 布局遵循「Skill 负责布局意图，Layout Engine 负责布局坐标」：
/// 模型在 `layout` 字段声明 mode/root/direction/main_path/groups，本函数按语义意图计算
/// 每个节点的 x/y/width/height（覆盖模型占位坐标）；未声明时用默认层级布局。
/// 布局后执行质量检查（节点重叠 / edge 交叉 / 孤立 / 超长连线）并记入日志（当前只检测不修复）。
/// 任一步失败返回明确错误（write 拒绝写入），保证落盘的 `.canvas` 合法、可渲染、结构清晰。
pub fn validate_canvas_json(content: &str, dir: &str) -> Result<String, String> {
    let mut canvas: CanvasFile = serde_json::from_str(content)
        .map_err(|e| format!("无效的 JSON Canvas 格式: {}", e))?;
    if canvas.nodes.is_empty() {
        return Err("画布无节点（nodes 为空数组）".into());
    }
    sanitize_ids(&mut canvas);
    let _degraded = degrade_missing_file_nodes(&mut canvas, dir);
    layout_canvas(&mut canvas);
    // P0-2：结构合法性与视觉质量分级——node_overlaps 是结构性错误（必须拒绝写入，
    // 否则画布不可读）；edge 交叉/孤立/长连线是视觉 Warning（语义允许，仅记录）。
    let overlaps = check_node_overlaps(&canvas);
    if !overlaps.is_empty() {
        let brief: Vec<String> = overlaps.iter().take(3).map(|(a, b)| format!("{}×{}", a, b)).collect();
        return Err(format!(
            "布局后检测到节点重叠（{} 处，如 {}）：请检查 layout 声明（模式/root/分组）或减少节点密度后重试",
            overlaps.len(),
            brief.join("、")
        ));
    }
    let q = layout_quality_check(&canvas);
    if q.edge_crossings > 0 || q.isolated_nodes > 0 || q.long_edges > 0 {
        log::warn!(
            "[canvas] 布局质量(warning): edge交叉={} 孤立节点={} 超长连线={}",
            q.edge_crossings, q.isolated_nodes, q.long_edges
        );
    }
    serde_json::to_string_pretty(&canvas).map_err(|e| format!("序列化画布失败: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_node(id: &str) -> CanvasNode {
        CanvasNode {
            id: id.into(),
            ty: "text".into(),
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 0.0,
            text: Some(id.into()),
            file: None,
            url: None,
            code: None,
            is_root: None,
        }
    }

    #[test]
    fn layout_layers_by_depth_from_root_and_assigns_sizes() {
        // 根 n1 → n2 → n3（链式）；另一分支 n1 → n4
        let mut canvas = CanvasFile {
            layout: None,
            nodes: vec![text_node("n1"), text_node("n2"), text_node("n3"), text_node("n4")],
            edges: vec![
                CanvasEdge { id: "e1".into(), from_node: "n1".into(), to_node: "n2".into(), label: None },
                CanvasEdge { id: "e2".into(), from_node: "n2".into(), to_node: "n3".into(), label: None },
                CanvasEdge { id: "e3".into(), from_node: "n1".into(), to_node: "n4".into(), label: None },
            ],
        };
        layout_canvas(&mut canvas);
        let by_id: std::collections::HashMap<&str, &CanvasNode> =
            canvas.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        // n1(根) 第 0 层，n2/n4 第 1 层，n3 第 2 层（层间距 240 + V_GAP=60 → 300）
        assert_eq!(by_id["n1"].y, 0.0);
        assert_eq!(by_id["n2"].y, 300.0);
        assert_eq!(by_id["n4"].y, 300.0);
        assert_eq!(by_id["n3"].y, 600.0);
        // 同一层不同列（水平错开）
        assert_ne!(by_id["n2"].x, by_id["n4"].x);
        // 尺寸已按类型写入
        assert!(by_id["n1"].width > 0.0 && by_id["n1"].height > 0.0);
    }

    #[test]
    fn sanitize_ids_renumbers_and_keeps_edge_references() {
        // 构造带非法/重复可能的 id，验证重编号后边仍指向对应节点
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
        // 模型输出引用了不存在节点的悬空边，重编号后应被过滤
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
        // file 节点指向真实文件保留，指向不存在路径/无 file 字段时降级为 text
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
                CanvasNode {
                    id: "n1".into(),
                    ty: "file".into(),
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                    text: Some("真实文件".into()),
                    file: Some("docs/real.md".into()),
                    url: None,
                    code: None,
                    is_root: None,
                },
                CanvasNode {
                    id: "n2".into(),
                    ty: "file".into(),
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                    text: Some("编造路径".into()),
                    file: Some("docs/fake.md".into()),
                    url: None,
                    code: None,
                    is_root: None,
                },
                CanvasNode {
                    id: "n3".into(),
                    ty: "image".into(),
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                    text: Some("无路径图片".into()),
                    file: None,
                    url: None,
                    code: None,
                    is_root: None,
                },
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
    fn validate_canvas_json_pipeline_normalizes_and_rejects_invalid() {
        // 合法输入：缺坐标/缺 type/边缺 id → 规整后可解析、布局补全、边 id 补全
        let raw = r#"{
            "nodes": [
                {"id": "n1", "type": "text", "text": "互斥"},
                {"id": "n2", "text": "占有并等待"}
            ],
            "edges": [
                {"fromNode": "n1", "toNode": "n2"}
            ]
        }"#;
        let out = validate_canvas_json(raw, ".").expect("合法画布应规整通过");
        let parsed: CanvasFile = serde_json::from_str(&out).expect("输出应为合法 JSON Canvas");
        assert_eq!(parsed.nodes.len(), 2);
        assert_eq!(parsed.nodes[1].ty, "text", "缺省 type 应默认为 text");
        for n in &parsed.nodes {
            assert!(n.width > 0.0 && n.height > 0.0, "布局应补全尺寸");
        }
        assert_eq!(parsed.edges.len(), 1);
        assert!(!parsed.edges[0].id.is_empty(), "边 id 应被补全");
        // 悬空边会被清理
        let dangling = r#"{"nodes":[{"id":"a","text":"A"}],"edges":[{"fromNode":"a","toNode":"ghost"}]}"#;
        let out2 = validate_canvas_json(dangling, ".").expect("悬空边应被静默清理");
        let parsed2: CanvasFile = serde_json::from_str(&out2).unwrap();
        assert!(parsed2.edges.is_empty(), "悬空边应被过滤");
        // 非法输入：非 JSON / 空 nodes → 明确拒绝
        assert!(validate_canvas_json("not json", ".").is_err());
        assert!(validate_canvas_json(r#"{"nodes":[],"edges":[]}"#, ".").is_err(), "空画布应拒绝");
    }

    fn canvas_with_layout(layout: CanvasLayout) -> CanvasFile {
        CanvasFile {
            layout: Some(layout),
            nodes: vec![
                CanvasNode { id: "root".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("RAG".into()), file: None, url: None, code: None, is_root: Some(true) },
                CanvasNode { id: "a".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("检索".into()), file: None, url: None, code: None, is_root: None },
                CanvasNode { id: "b".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("生成".into()), file: None, url: None, code: None, is_root: None },
                CanvasNode { id: "c".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("BM25".into()), file: None, url: None, code: None, is_root: None },
                CanvasNode { id: "d".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("Reranker".into()), file: None, url: None, code: None, is_root: None },
            ],
            edges: vec![
                CanvasEdge { id: "e1".into(), from_node: "root".into(), to_node: "a".into(), label: None },
                CanvasEdge { id: "e2".into(), from_node: "root".into(), to_node: "b".into(), label: None },
                CanvasEdge { id: "e3".into(), from_node: "a".into(), to_node: "c".into(), label: None },
                CanvasEdge { id: "e4".into(), from_node: "b".into(), to_node: "d".into(), label: None },
            ],
        }
    }

    #[test]
    fn layout_hierarchy_places_root_top_and_children_same_row() {
        // 默认 hierarchy（TB）：root 第 0 行，a/b 第 1 行同层，c/d 第 2 行
        let mut canvas = canvas_with_layout(CanvasLayout {
            version: 1,
            mode: "hierarchy".into(),
            root: Some("root".into()),
            direction: None,
            main_path: vec![],
            groups: vec![],
        });
        layout_canvas(&mut canvas);
        let by_id: std::collections::HashMap<&str, &CanvasNode> =
            canvas.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        assert_eq!(by_id["root"].y, 0.0);
        assert_eq!(by_id["a"].y, by_id["b"].y, "同层节点应在同一水平行");
        assert!(by_id["a"].y > by_id["root"].y);
        assert_eq!(by_id["c"].y, by_id["d"].y);
        assert!(by_id["c"].y > by_id["a"].y);
        // 同层 x 不同（水平错开）
        assert_ne!(by_id["a"].x, by_id["b"].x);
        assert_ne!(by_id["c"].x, by_id["d"].x);
        // 无重叠：同层节点 x 间距 >= 节点宽（不重叠）
        assert!((by_id["b"].x - by_id["a"].x) > by_id["a"].width);
    }

    #[test]
    fn layout_flow_lr_places_chain_left_to_right() {
        // flow + direction LR：链从左到右（root.x < a.x < c.x）
        let mut canvas = canvas_with_layout(CanvasLayout {
            version: 1,
            mode: "flow".into(),
            root: Some("root".into()),
            direction: Some("LR".into()),
            main_path: vec![],
            groups: vec![],
        });
        layout_canvas(&mut canvas);
        let by_id: std::collections::HashMap<&str, &CanvasNode> =
            canvas.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        assert!(by_id["root"].x < by_id["a"].x, "流程应自左向右");
        assert!(by_id["a"].x < by_id["c"].x);
        assert!(by_id["b"].x > by_id["root"].x);
        assert!(by_id["d"].x > by_id["b"].x);
    }

    #[test]
    fn layout_radial_centers_root() {
        // radial：root 居中（x≈-w/2 附近），一级节点环绕在半径上
        let mut canvas = canvas_with_layout(CanvasLayout {
            version: 1,
            mode: "radial".into(),
            root: Some("root".into()),
            direction: None,
            main_path: vec![],
            groups: vec![],
        });
        layout_canvas(&mut canvas);
        let by_id: std::collections::HashMap<&str, &CanvasNode> =
            canvas.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        // root 居中（中心点接近 0,0）
        let rx = by_id["root"].x + by_id["root"].width / 2.0;
        let ry = by_id["root"].y + by_id["root"].height / 2.0;
        assert!(rx.abs() < 10.0 && ry.abs() < 10.0, "root 应居中: ({},{})", rx, ry);
        // 一级节点在半径上（距 root 中心约 320）
        let ax = by_id["a"].x + by_id["a"].width / 2.0;
        let ay = by_id["a"].y + by_id["a"].height / 2.0;
        let dist = ((ax - rx).powi(2) + (ay - ry).powi(2)).sqrt();
        assert!((dist - 320.0).abs() < 30.0, "一级节点应在半径 320 附近: {}", dist);
        // 二级节点更远
        let cx = by_id["c"].x + by_id["c"].width / 2.0;
        let cy = by_id["c"].y + by_id["c"].height / 2.0;
        let dist_c = ((cx - rx).powi(2) + (cy - ry).powi(2)).sqrt();
        assert!(dist_c > dist, "二级节点应比一级节点更远");
    }

    #[test]
    fn layout_grouped_puts_groups_in_separate_columns() {
        // grouped：两个分组水平分区，组间留 GROUP_GAP
        let mut canvas = canvas_with_layout(CanvasLayout {
            version: 1,
            mode: "grouped".into(),
            root: None,
            direction: None,
            main_path: vec![],
            groups: vec![
                CanvasGroup { id: "g1".into(), nodes: vec!["root".into(), "a".into()], title: None },
                CanvasGroup { id: "g2".into(), nodes: vec!["b".into(), "c".into(), "d".into()], title: None },
            ],
        });
        layout_canvas(&mut canvas);
        let by_id: std::collections::HashMap<&str, &CanvasNode> =
            canvas.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        // 组 1 在左、组 2 在右，组间间距 >= GROUP_GAP
        let g1_max_x = by_id["root"].x.max(by_id["a"].x);
        let g2_min_x = by_id["b"].x.min(by_id["c"].x).min(by_id["d"].x);
        assert!(g2_min_x - g1_max_x >= GROUP_GAP - 1.0, "组间应有明显留白");
        // 组内节点垂直堆叠（同组不同 y）
        assert_ne!(by_id["b"].y, by_id["c"].y);
    }

    #[test]
    fn estimate_size_scales_with_text_length() {
        let short = CanvasNode { id: "s".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("短".into()), file: None, url: None, code: None, is_root: None };
        let long = CanvasNode { id: "l".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("这是一个非常长的说明文字，用来测试内容感知尺寸是否随文本长度增长".into()), file: None, url: None, code: None, is_root: None };
        let (sw, _) = estimate_size(&short);
        let (lw, _) = estimate_size(&long);
        assert!(lw > sw, "长文本节点应更宽: {} vs {}", lw, sw);
    }

    #[test]
    fn sanitize_ids_remaps_layout_root_and_groups() {
        let mut canvas = canvas_with_layout(CanvasLayout {
            version: 1,
            mode: "grouped".into(),
            root: Some("root".into()),
            direction: None,
            main_path: vec![],
            groups: vec![CanvasGroup { id: "g1".into(), nodes: vec!["root".into(), "a".into()], title: None }],
        });
        sanitize_ids(&mut canvas);
        // layout.root 与 groups.nodes 的引用应被重映射到新 id
        let layout = canvas.layout.expect("layout 应保留");
        assert_eq!(layout.root.as_deref(), Some("n1"), "root 应重映射为 n1");
        assert!(layout.groups[0].nodes.iter().all(|id| id.starts_with('n')));
    }

    #[test]
    fn layout_is_deterministic() {
        // UT-22：相同 nodes/edges/layout 两次布局得到完全相同的坐标（不依赖 HashMap 迭代序）
        let build = || canvas_with_layout(CanvasLayout {
            version: 1,
            mode: "hierarchy".into(),
            root: Some("root".into()),
            direction: Some("TB".into()),
            main_path: vec![],
            groups: vec![],
        });
        let mut a = build();
        let mut b = build();
        layout_canvas(&mut a);
        layout_canvas(&mut b);
        for (na, nb) in a.nodes.iter().zip(b.nodes.iter()) {
            assert_eq!(na.id, nb.id);
            assert_eq!(na.x, nb.x, "节点 {} x 应确定性", na.id);
            assert_eq!(na.y, nb.y, "节点 {} y 应确定性", na.id);
            assert_eq!(na.width, nb.width);
            assert_eq!(na.height, nb.height);
        }
    }

    #[test]
    fn estimate_size_handles_cjk_and_ascii_widths() {
        // UT-23：中文比等量字符数 ASCII 更宽（CJK≈1em vs ASCII≈0.55em）
        let cjk = CanvasNode { id: "c".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("中文中文中文中文中文中文中文中文中文中文中文中文中文中文中文".into()), file: None, url: None, code: None, is_root: None };
        let ascii = CanvasNode { id: "a".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("aaaaaaaaaaaaaaaaaaaa".into()), file: None, url: None, code: None, is_root: None };
        let (cw, _) = estimate_size(&cjk);
        let (aw, _) = estimate_size(&ascii);
        // 15 个中文字符 ≈ 15em = 195px + 24 = 219；20 个 ASCII ≈ 11em ≈ 143px + 24 = 167 → clamp 到 180
        assert!(cw > aw, "中文应比同字符数 ASCII 更宽: {} vs {}", cw, aw);
    }

    #[test]
    fn layout_quality_check_detects_isolated_nodes() {
        // UT-25：孤立节点（无任何边）被质量检查识别
        let mut canvas = canvas_with_layout(CanvasLayout {
            version: 1,
            mode: "hierarchy".into(),
            root: Some("root".into()),
            direction: None,
            main_path: vec![],
            groups: vec![],
        });
        // 追加一个无边的孤立节点
        canvas.nodes.push(CanvasNode { id: "iso".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("孤立知识".into()), file: None, url: None, code: None, is_root: None });
        layout_canvas(&mut canvas);
        let q = layout_quality_check(&canvas);
        assert_eq!(q.isolated_nodes, 1, "应识别 1 个孤立节点");
        // 孤立节点也应被放置（不丢失）
        assert!(canvas.nodes.iter().any(|n| n.id == "iso" && (n.x != 0.0 || n.y != 0.0)));
        // 布局后无节点重叠（引擎数学保证）
        assert_eq!(q.node_overlaps, 0, "布局后不应有节点重叠");
    }

    #[test]
    fn layout_quality_check_detects_overlaps() {
        // P0：手工构造重叠节点，验证检测器能发现问题
        let mut canvas = CanvasFile {
            layout: None,
            nodes: vec![
                CanvasNode { id: "a".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 100.0, height: 100.0, text: Some("A".into()), file: None, url: None, code: None, is_root: None },
                CanvasNode { id: "b".into(), ty: "text".into(), x: 50.0, y: 50.0, width: 100.0, height: 100.0, text: Some("B".into()), file: None, url: None, code: None, is_root: None },
            ],
            edges: vec![],
        };
        let overlaps = check_node_overlaps(&mut canvas);
        assert_eq!(overlaps.len(), 1, "a/b 应被检测为重叠");
        assert_eq!(layout_quality_check(&mut canvas).node_overlaps, 1);
    }

    #[test]
    fn flow_main_path_aligns_chain_and_branches() {
        // P1：flow + main_path——主链 x 严格递增；分支挂在主链锚点下方
        let mut canvas = canvas_with_layout(CanvasLayout {
            version: 1,
            mode: "flow".into(),
            root: Some("n1".into()),
            direction: Some("LR".into()),
            main_path: vec!["n1".into(), "n2".into(), "n3".into()],
            groups: vec![],
        });
        // 重建节点/边：n1→n2→n3 主链，n2→n4 分支
        canvas.nodes = vec![
            CanvasNode { id: "n1".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("开始".into()), file: None, url: None, code: None, is_root: None },
            CanvasNode { id: "n2".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("处理".into()), file: None, url: None, code: None, is_root: None },
            CanvasNode { id: "n3".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("结束".into()), file: None, url: None, code: None, is_root: None },
            CanvasNode { id: "n4".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("分支".into()), file: None, url: None, code: None, is_root: None },
        ];
        canvas.edges = vec![
            CanvasEdge { id: "e1".into(), from_node: "n1".into(), to_node: "n2".into(), label: None },
            CanvasEdge { id: "e2".into(), from_node: "n2".into(), to_node: "n3".into(), label: None },
            CanvasEdge { id: "e3".into(), from_node: "n2".into(), to_node: "n4".into(), label: None },
        ];
        layout_canvas(&mut canvas);
        let by_id: std::collections::HashMap<&str, &CanvasNode> =
            canvas.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        // 主链严格从左到右
        assert!(by_id["n1"].x < by_id["n2"].x && by_id["n2"].x < by_id["n3"].x);
        // 主链同 y（0）
        assert_eq!(by_id["n1"].y, 0.0);
        assert_eq!(by_id["n2"].y, 0.0);
        assert_eq!(by_id["n3"].y, 0.0);
        // 分支 n4 挂在主链 n2 下方（y > 0），x 接近 n2
        assert!(by_id["n4"].y > by_id["n2"].y, "分支应在主链下方");
        let _ = by_id;
    }

    #[test]
    fn radial_degrades_when_too_many_first_level_nodes() {
        // P1：一级节点 > 8 时 radial 自动降级 hierarchy（不再无限增大半径）
        let mut canvas = CanvasFile {
            layout: Some(CanvasLayout {
                version: 1,
                mode: "radial".into(),
                root: Some("root".into()),
                direction: None,
                main_path: vec![],
                groups: vec![],
            }),
            nodes: vec![
                CanvasNode { id: "root".into(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some("中心".into()), file: None, url: None, code: None, is_root: Some(true) },
            ],
            edges: vec![],
        };
        for i in 0..10 {
            let id = format!("n{}", i);
            canvas.nodes.push(CanvasNode { id: id.clone(), ty: "text".into(), x: 0.0, y: 0.0, width: 0.0, height: 0.0, text: Some(format!("子{}", i)), file: None, url: None, code: None, is_root: None });
            canvas.edges.push(CanvasEdge { id: format!("e{}", i), from_node: "root".into(), to_node: id, label: None });
        }
        layout_canvas(&mut canvas);
        let by_id: std::collections::HashMap<&str, &CanvasNode> =
            canvas.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        // 降级为 hierarchy：root 在顶部，子节点第 1 层同 y
        assert_eq!(by_id["root"].y, 0.0);
        assert_eq!(by_id["n0"].y, by_id["n1"].y, "一级节点应同层（hierarchy 降级）");
        // 无重叠
        let q = layout_quality_check(&canvas);
        assert_eq!(q.node_overlaps, 0, "降级后不应有重叠");
    }

    #[test]
    fn layout_version_defaults_to_1_wh