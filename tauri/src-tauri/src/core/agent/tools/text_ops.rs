//! 文本编辑的纯函数核心：换行风格、归一化匹配、行范围替换、省略锚点、unified diff。
//!
//! 设计目标（对应大文档编辑的三个真实痛点）：
//! 1. **CRLF / 行尾空白**：模型复述原文时几乎总会把 `\r\n` 写成 `\n`、把行尾空格吃掉。
//!    这里提供「归一化后匹配、按原文位置替换」，写回保持文件原有换行风格，
//!    避免一次失配就整轮重试，也避免把 CRLF 文档改成混合行尾。
//! 2. **不必复制整段原文**：`replace_line_range` 用行号定位，`replace_with_anchors`
//!    允许 `…` 省略中段，模型只写"首锚点 + 新内容 + 尾锚点"即可替换大段。
//! 3. **一次改多处**：`apply_unified_patch` 支持标准 unified diff 的多 hunk 应用。
//!
//! 所有函数均为纯函数（不碰文件系统），便于单测与复用。

/// 换行风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

impl Eol {
    pub fn as_str(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::Crlf => "\r\n",
        }
    }
}

/// 检测文本的主导换行风格（CRLF 行数 ≥ 纯 LF 行数时判为 CRLF）。
pub fn detect_eol(text: &str) -> Eol {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    if crlf > 0 && crlf * 2 >= lf {
        Eol::Crlf
    } else {
        Eol::Lf
    }
}

/// 把任意换行写法的文本统一成目标风格（用于把模型给的 `new_string` 写回文件）。
pub fn to_eol(text: &str, eol: Eol) -> String {
    let lf = text.replace("\r\n", "\n").replace('\r', "\n");
    match eol {
        Eol::Lf => lf,
        Eol::Crlf => lf.replace('\n', "\r\n"),
    }
}

/// 归一化文本 + `规范化字节偏移 → 原文字节偏移` 的映射。
///
/// 归一化规则（只做"模型几乎必然会有的差异"，不做激进改写）：
/// - `\r\n` / 孤立 `\r` → `\n`
/// - 每行行尾的空格 / 制表符删除
pub struct Normalized {
    pub text: String,
    /// 每个**规范化文本字符**的 `(原文字节起点, 规范化字节起点)`，按规范化位置升序。
    map: Vec<(usize, usize)>,
    pub orig_len: usize,
}

impl Normalized {
    /// 规范化的字节偏移 → 原文字节偏移（必须落在字符边界上）。
    pub fn to_orig(&self, norm_byte: usize) -> usize {
        if norm_byte >= self.text.len() {
            return self.orig_len;
        }
        // map 按 norm 位置升序：二分找第一个 norm_start >= norm_byte 的项
        let idx = self.map.partition_point(|(_, n)| *n < norm_byte);
        self.map
            .get(idx)
            .map(|(o, _)| *o)
            .unwrap_or(self.orig_len)
    }
}

/// 归一化文本（供匹配用）。
pub fn normalize(text: &str) -> Normalized {
    let mut out = String::with_capacity(text.len());
    let mut map: Vec<(usize, usize)> = Vec::with_capacity(text.len());
    let mut pos = 0usize;
    for raw_line in text.split_inclusive('\n') {
        let has_nl = raw_line.ends_with('\n');
        let mut body_end = raw_line.len() - usize::from(has_nl);
        if body_end > 0 && raw_line.as_bytes()[body_end - 1] == b'\r' {
            body_end -= 1;
        }
        let body = &raw_line[..body_end];
        let trimmed = body.trim_end_matches([' ', '\t']);
        for (off, ch) in trimmed.char_indices() {
            map.push((pos + off, out.len()));
            out.push(ch);
        }
        if has_nl {
            // 换行映射到原行尾（CRLF 时指向 `\r` 之前的边界；替换端点只用字符起点）
            let nl_orig = pos + if raw_line.as_bytes()[body_end..].starts_with(b"\r\n") {
                body_end
            } else {
                raw_line.len() - 1
            };
            map.push((nl_orig, out.len()));
            out.push('\n');
        }
        pos += raw_line.len();
    }
    Normalized {
        text: out,
        map,
        orig_len: text.len(),
    }
}

/// 一次匹配结果（原文字节区间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// 查找 `needle` 在 `content` 中的所有出现：先精确匹配；精确匹配为 0 时才做归一化匹配。
///
/// 返回**原文字节区间**。归一化匹配命中时，区间端点也落在原文的字符边界上，
/// 因此可直接用于 `String` 切片替换。
pub fn find_spans(content: &str, needle: &str) -> Vec<Span> {
    if needle.is_empty() {
        return Vec::new();
    }
    let exact: Vec<Span> = content
        .match_indices(needle)
        .map(|(s, m)| Span {
            start: s,
            end: s + m.len(),
        })
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    let n_content = normalize(content);
    let n_needle = normalize(needle);
    if n_needle.text.is_empty() {
        return Vec::new();
    }
    n_content
        .text
        .match_indices(n_needle.text.as_str())
        .map(|(s, m)| Span {
            start: n_content.to_orig(s),
            end: n_content.to_orig(s + m.len()),
        })
        .collect()
}

/// 把 `old` 的唯一一次出现替换为 `new`。
///
/// - `replace_all=false`：必须唯一匹配，0 次/多次都报错（错误信息包含发现次数，便于模型修正）。
/// - `replace_all=true`：替换全部出现（0 次报错）。
/// - `new` 会先转换成 `content` 的主导换行风格，避免写出混合行尾。
pub fn replace_once(content: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, usize), String> {
    if old.is_empty() {
        return Err("old_string 不能为空".into());
    }
    let spans = find_spans(content, old);
    if spans.is_empty() {
        return Err(match nearest_hint(content, old) {
            Some(hint) => format!(
                "未找到与 old_string 匹配的内容（已尝试忽略行尾空白与 CRLF 差异）。最近似位置：{hint}。请用 read 确认原文后重试"
            ),
            None => "未找到与 old_string 匹配的内容（已尝试忽略行尾空白与 CRLF 差异），请用 read 确认原文后重试".into(),
        });
    }
    if !replace_all && spans.len() > 1 {
        return Err(format!(
            "old_string 匹配到 {} 处，请提供更长的上下文使其唯一，或改用 replace_all=true / 行范围替换",
            spans.len()
        ));
    }
    let eol = detect_eol(content);
    let new_text = to_eol(new, eol);
    let mut out = String::with_capacity(content.len() + new_text.len());
    let mut cursor = 0usize;
    for span in &spans {
        out.push_str(&content[cursor..span.start]);
        out.push_str(&new_text);
        cursor = span.end;
    }
    out.push_str(&content[cursor..]);
    Ok((out, spans.len()))
}

/// 0 次匹配时的「最近似位置」提示：用首行前缀做归一化子串探测，返回行号与原文行。
fn nearest_hint(content: &str, old: &str) -> Option<String> {
    let first = old.lines().next()?.trim();
    let chars: Vec<char> = first.chars().collect();
    if chars.len() < 4 {
        return None;
    }
    let n = normalize(content);
    // 从「整行」逐步缩短前缀：模型常在行尾多写/少写几个字，前缀仍能命中
    for take in (4..=chars.len().min(40)).rev() {
        let probe: String = chars[..take].iter().collect();
        let np = normalize(&probe);
        if np.text.is_empty() {
            continue;
        }
        let Some(idx) = n.text.find(np.text.as_str()) else {
            continue;
        };
        let orig_byte = n.to_orig(idx);
        let line_no = content[..orig_byte].matches('\n').count() + 1;
        let line = content
            .lines()
            .nth(line_no.saturating_sub(1))
            .unwrap_or("")
            .trim();
        return Some(format!("第 {line_no} 行（内容：{line}）"));
    }
    None
}

/// 省略锚点标记行：独占一行的 `…` / `...` / `// ... existing ...` 等。
fn is_elision_line(line: &str) -> bool {
    let t = line.trim();
    matches!(
        t,
        "…" | "..." | "……" | "// ... existing ..." | "//... existing ..." | "/* ... */" | "<!-- ... -->" | "<!-- ...existing... -->"
    )
}

/// 按省略锚点替换大段内容。
///
/// `old` 形如：
/// ```text
/// ## 安装
/// …
/// ## 卸载
/// ```
/// 中段被省略，只要求各段按顺序出现。替换范围 = 首段起点 → 末段终点。
/// 返回 `(新内容, 锚点段数)`；无省略行时等价于唯一替换。
pub fn replace_with_anchors(content: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, usize), String> {
    let lines: Vec<&str> = old.split_inclusive('\n').collect();
    if !lines.iter().any(|l| is_elision_line(l)) {
        return replace_once(content, old, new, replace_all);
    }
    let mut segments: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in &lines {
        if is_elision_line(line) {
            if !cur.trim().is_empty() {
                segments.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push_str(line);
    }
    if !cur.trim().is_empty() {
        segments.push(cur);
    }
    if segments.len() < 2 {
        return Err("省略锚点无效：省略号两侧都需要至少一段原文".into());
    }
    // 逐段向后查找，要求严格递增且不重叠
    let mut spans: Vec<Span> = Vec::new();
    let mut search_from = 0usize;
    for seg in &segments {
        let seg_spans = find_spans(&content[search_from..], seg);
        let Some(span) = seg_spans.into_iter().next() else {
            return Err(format!(
                "省略锚点的第 {} 段未在文件中找到：{}",
                spans.len() + 1,
                seg.lines().next().unwrap_or("").trim()
            ));
        };
        let abs = Span {
            start: search_from + span.start,
            end: search_from + span.end,
        };
        search_from = abs.end;
        spans.push(abs);
    }
    let start = spans.first().unwrap().start;
    let end = spans.last().unwrap().end;
    let eol = detect_eol(content);
    let new_text = to_eol(new, eol);
    let mut out = String::with_capacity(content.len() + new_text.len());
    out.push_str(&content[..start]);
    out.push_str(&new_text);
    out.push_str(&content[end..]);
    Ok((out, segments.len()))
}

/// 行号（1 起，含端点）→ 原文字节区间。`end_line` 超出总行数时截断到末行。
pub fn line_span(content: &str, start_line: usize, end_line: usize) -> Result<Span, String> {
    if start_line == 0 || end_line == 0 {
        return Err("行号从 1 开始".into());
    }
    if start_line > end_line {
        return Err(format!("行范围非法：start_line={start_line} > end_line={end_line}"));
    }
    let mut starts: Vec<usize> = vec![0];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' && i + 1 < content.len() {
            starts.push(i + 1);
        }
    }
    let total = starts.len();
    if start_line > total {
        return Err(format!("start_line={start_line} 超出文件总行数 {total}"));
    }
    let end = end_line.min(total);
    let start_byte = starts[start_line - 1];
    let end_byte = if end < total {
        starts[end]
    } else {
        content.len()
    };
    Ok(Span {
        start: start_byte,
        end: end_byte,
    })
}

/// 行号区间替换：把第 `start_line..=end_line` 行（含行尾换行）整体替换为 `new`。
///
/// 语义：`new` 非空且不以换行结尾时补一个文件风格的换行；`new` 为空即删除这些行。
pub fn replace_line_range(
    content: &str,
    start_line: usize,
    end_line: usize,
    new: &str,
) -> Result<(String, usize), String> {
    let span = line_span(content, start_line, end_line)?;
    let eol = detect_eol(content);
    let mut body = to_eol(new, eol);
    if !body.is_empty() && !body.ends_with('\n') {
        body.push_str(eol.as_str());
    }
    let mut out = String::with_capacity(content.len() + body.len());
    out.push_str(&content[..span.start]);
    out.push_str(&body);
    out.push_str(&content[span.end..]);
    // 实际替换的行数（end_line 超出总行数时按截断后的行数计）
    let replaced = content[span.start..span.end].lines().count();
    Ok((out, replaced.max(1)))
}

/// 在指定行**之后**插入内容（`line` 为 0 表示插入到文件最前面）。
pub fn insert_after_line(content: &str, line: usize, new: &str) -> Result<(String, usize), String> {
    let eol = detect_eol(content);
    let mut body = to_eol(new, eol);
    if !body.is_empty() && !body.ends_with('\n') {
        body.push_str(eol.as_str());
    }
    if line == 0 {
        let mut out = String::with_capacity(content.len() + body.len());
        out.push_str(&body);
        out.push_str(content);
        return Ok((out, 1));
    }
    let span = line_span(content, line, line)?;
    // 插入点：该行行尾（含换行）
    let mut out = String::with_capacity(content.len() + body.len());
    out.push_str(&content[..span.end]);
    out.push_str(&body);
    out.push_str(&content[span.end..]);
    Ok((out, 1))
}

/// unified diff 的单行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// 单个 hunk（`@@ -a,b +c,d @@`）。
#[derive(Debug, Clone)]
pub struct PatchHunk {
    pub old_start: usize,
    pub old_len: usize,
    pub lines: Vec<PatchLine>,
}

/// 单个文件的补丁。
#[derive(Debug, Clone)]
pub struct FilePatch {
    pub path: String,
    pub hunks: Vec<PatchHunk>,
}

/// 解析 unified diff（支持多文件、多 hunk）。
///
/// 接受形如：
/// ```text
/// --- a/docs/a.md
/// +++ b/docs/a.md
/// @@ -10,3 +10,4 @@
///  context
/// -old
/// +new
/// ```
/// 路径前缀 `a/`、`b/` 会被剥离。
pub fn parse_unified_patch(input: &str) -> Result<Vec<FilePatch>, String> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut lines = input.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("--- ") {
            // 期待紧随 +++. 也可能是 `--- /dev/null`
            let old_path = rest.split_whitespace().next().unwrap_or("");
            let Some(next) = lines.next() else {
                return Err("补丁不完整：--- 之后缺少 +++ 行".into());
            };
            let next = next.trim_end_matches('\r');
            let Some(new_raw) = next.strip_prefix("+++ ") else {
                return Err("补丁格式错误：--- 之后必须紧跟 +++ 行".into());
            };
            let new_path = new_raw.split_whitespace().next().unwrap_or("");
            let path = strip_diff_prefix(if new_path == "/dev/null" { old_path } else { new_path });
            if path.is_empty() {
                return Err("补丁格式错误：文件路径为空".into());
            }
            let mut hunks: Vec<PatchHunk> = Vec::new();
            while let Some(peek) = lines.peek() {
                let peek = peek.trim_end_matches('\r');
                if peek.starts_with("--- ") || peek.starts_with("diff --git") {
                    break;
                }
                if let Some(h) = peek.strip_prefix("@@ ") {
                    lines.next();
                    let (old_start, old_len) = parse_hunk_range(h)?;
                    let mut plines: Vec<PatchLine> = Vec::new();
                    while let Some(p) = lines.peek() {
                        let p = p.trim_end_matches('\r');
                        if p.starts_with("@@ ") || p.starts_with("--- ") || p.starts_with("diff --git") {
                            break;
                        }
                        if p.is_empty() {
                            lines.next();
                            continue;
                        }
                        let (kind, body) = p.split_at(1);
                        let body = body.to_string();
                        match kind {
                            " " => plines.push(PatchLine::Context(body)),
                            "-" => plines.push(PatchLine::Remove(body)),
                            "+" => plines.push(PatchLine::Add(body)),
                            _ => {
                                // 容错：空白被吃掉的行按上下文处理
                                plines.push(PatchLine::Context(p.to_string()));
                            }
                        }
                        lines.next();
                    }
                    if plines.is_empty() {
                        return Err("补丁格式错误：空 hunk".into());
                    }
                    hunks.push(PatchHunk {
                        old_start,
                        old_len,
                        lines: plines,
                    });
                } else {
                    lines.next();
                }
            }
            if hunks.is_empty() {
                return Err(format!("补丁格式错误：{path} 没有任何 @@ hunk"));
            }
            files.push(FilePatch { path, hunks });
        }
    }
    if files.is_empty() {
        return Err("补丁为空：未找到 ---/+++ 文件头".into());
    }
    Ok(files)
}

fn strip_diff_prefix(p: &str) -> String {
    let p = p.trim();
    let p = p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p);
    p.to_string()
}

fn parse_hunk_range(s: &str) -> Result<(usize, usize), String> {
    // s 形如 "-10,3 +10,4 @@"
    let old = s
        .split_whitespace()
        .next()
        .ok_or_else(|| "hunk 头缺少范围".to_string())?;
    let old = old.trim_start_matches('-');
    let mut it = old.split(',');
    let start: usize = it
        .next()
        .unwrap_or("0")
        .parse()
        .map_err(|_| format!("hunk 起始行非法: {old}"))?;
    let len: usize = it.next().unwrap_or("1").parse().unwrap_or(1);
    Ok((start, len))
}

/// 应用单个文件的 hunks，返回新内容。
///
/// 定位策略：按 hunk 顺序向后查找「旧行序列」；先精确匹配，失败再归一化匹配
/// （忽略 CRLF/行尾空白）；仍失败则给出期望位置附近的行，便于模型修正。
pub fn apply_hunks(content: &str, hunks: &[PatchHunk]) -> Result<String, String> {
    let eol = detect_eol(content);
    let mut out = content.to_string();
    let mut search_from = 0usize;
    let mut shift: isize = 0;
    for (i, hunk) in hunks.iter().enumerate() {
        let old_lines: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|l| match l {
                PatchLine::Context(s) | PatchLine::Remove(s) => Some(s.as_str()),
                PatchLine::Add(_) => None,
            })
            .collect();
        if old_lines.is_empty() {
            return Err(format!("第 {} 个 hunk 没有上下文/删除行，无法定位", i + 1));
        }
        let old_block = join_lines(&old_lines, eol);
        let hint_start = ((hunk.old_start.saturating_sub(1)) as isize + shift).max(0) as usize;
        let hint = hint_start.min(out.len());
        let (span, text) = match find_block(&out, &old_block, hint, search_from) {
            Some(v) => v,
            None => {
                let line_no = out[..hint.min(out.len())].matches('\n').count() + 1;
                let actual = out
                    .lines()
                    .nth(line_no.saturating_sub(1))
                    .unwrap_or("")
                    .trim();
                return Err(format!(
                    "第 {} 个 hunk 未找到匹配位置（期望约第 {} 行、共 {} 行上下文，该行实际为：{}）",
                    i + 1,
                    line_no,
                    hunk.old_len.max(1),
                    actual
                ));
            }
        };
        let mut replacement = String::new();
        for line in &hunk.lines {
            match line {
                PatchLine::Context(s) => {
                    replacement.push_str(s);
                    replacement.push_str(eol.as_str());
                }
                PatchLine::Add(s) => {
                    replacement.push_str(s);
                    replacement.push_str(eol.as_str());
                }
                PatchLine::Remove(_) => {}
            }
        }
        let replaced_end = span.end;
        out.replace_range(span.start..span.end, &replacement);
        shift += replacement.len() as isize - (replaced_end - span.start) as isize;
        search_from = span.start + replacement.len();
        let _ = text;
    }
    Ok(out)
}

/// 在 `content[hint..]` 之后查找 `block`（先精确、后归一化），返回原文字节区间与命中文本。
fn find_block(content: &str, block: &str, hint: usize, min_from: usize) -> Option<(Span, String)> {
    let from = hint.max(min_from).min(content.len());
    let tail = &content[from..];
    if let Some(pos) = tail.find(block) {
        let start = from + pos;
        return Some((
            Span {
                start,
                end: start + block.len(),
            },
            block.to_string(),
        ));
    }
    // 归一化重试：在全文上找（限制在 from 之后）
    let n = normalize(content);
    let nb = normalize(block);
    // 把原文字节 from 映射到规范化位置：找第一个 orig >= from 的项
    let start_norm = n
        .map
        .iter()
        .position(|(o, _)| *o >= from)
        .unwrap_or(0);
    let hit = n.text[start_norm..].find(nb.text.as_str())?;
    let s_norm = start_norm + hit;
    let span = Span {
        start: n.to_orig(s_norm),
        end: n.to_orig(s_norm + nb.text.len()),
    };
    Some((span, content[span.start..span.end].to_string()))
}

fn join_lines(lines: &[&str], eol: Eol) -> String {
    let mut s = String::new();
    for l in lines {
        s.push_str(l);
        s.push_str(eol.as_str());
    }
    s
}

/// 把行区间渲染成带行号的读取结果（`1234| 内容`）。
///
/// 行号前缀只为定位；调用方应在文档中提示模型「不要复制行号」。
pub fn render_numbered_lines(content: &str, start_line: usize, end_line: usize, max_chars: usize) -> String {
    render_numbered_lines_with_end(content, start_line, end_line, max_chars).0
}

/// 同 [`render_numbered_lines`]，额外返回**实际渲染到的最后一行行号**。
///
/// 读取续页提示必须用真实末行（而不是另算一遍预算），否则两套算法有偏差时
/// 会把一行的开头截断在下一页、甚至跳过内容。
pub fn render_numbered_lines_with_end(
    content: &str,
    start_line: usize,
    end_line: usize,
    max_chars: usize,
) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0usize;
    let mut truncated = false;
    let mut last_line = start_line.saturating_sub(1);
    for (idx, line) in content.lines().enumerate() {
        let lnum = idx + 1;
        if lnum < start_line {
            continue;
        }
        if lnum > end_line {
            break;
        }
        let rendered = format!("{lnum}| {line}\n");
        if used + rendered.chars().count() > max_chars && used > 0 {
            truncated = true;
            break;
        }
        used += rendered.chars().count();
        last_line = lnum;
        out.push_str(&rendered);
    }
    if truncated {
        out.push_str("...（本页已达字符上限，用 start_line/end_line 继续）\n");
    }
    (out, last_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_eol_prefers_crlf() {
        assert_eq!(detect_eol("a\r\nb\r\nc\r\n"), Eol::Crlf);
        assert_eq!(detect_eol("a\nb\nc\n"), Eol::Lf);
        assert_eq!(detect_eol("no newline"), Eol::Lf);
    }

    #[test]
    fn to_eol_converts_both_ways() {
        assert_eq!(to_eol("a\nb\r\nc", Eol::Crlf), "a\r\nb\r\nc");
        assert_eq!(to_eol("a\r\nb\nc", Eol::Lf), "a\nb\nc");
    }

    #[test]
    fn exact_match_wins_and_replace_keeps_eol() {
        let content = "line1\r\nline2\r\nline3\r\n";
        let (out, n) = replace_once(content, "line2\r\n", "LINE2\r\n", false).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out, "line1\r\nLINE2\r\nline3\r\n");
    }

    #[test]
    fn normalized_match_tolerates_lf_needle_on_crlf_file() {
        let content = "alpha\r\nbeta\r\ngamma\r\n";
        let (out, n) = replace_once(content, "beta\ngamma", "BETA\nGAMMA", false).unwrap();
        assert_eq!(n, 1);
        // 写回保持 CRLF，且替换段用 CRLF
        assert_eq!(out, "alpha\r\nBETA\r\nGAMMA\r\n");
    }

    #[test]
    fn normalized_match_ignores_trailing_spaces() {
        let content = "row1  \nrow2\t\nrow3\n";
        let (out, _) = replace_once(content, "row1\nrow2", "x\ny", false).unwrap();
        assert_eq!(out, "x\ny\nrow3\n");
    }

    #[test]
    fn replace_all_requires_flag() {
        let content = "a\na\na\n";
        let err = replace_once(content, "a\n", "b\n", false).unwrap_err();
        assert!(err.contains("3 处"), "{err}");
        let (out, n) = replace_once(content, "a\n", "b\n", true).unwrap();
        assert_eq!(n, 3);
        assert_eq!(out, "b\nb\nb\n");
    }

    #[test]
    fn missing_match_reports_near_line() {
        let content = "# 标题\n\n正文第一段\n正文第二段\n";
        let err = replace_once(content, "正文第一段！", "x", false).unwrap_err();
        assert!(err.contains("第 3 行"), "{err}");
    }

    #[test]
    fn anchors_replace_between_segments() {
        let content = "## A\nold body\n## B\nkeep\n## C\nend\n";
        let (out, n) = replace_with_anchors(content, "## A\n…\n## B", "## A\nnew body\n## B", false).unwrap();
        assert_eq!(n, 2);
        assert_eq!(out, "## A\nnew body\n## B\nkeep\n## C\nend\n");
    }

    #[test]
    fn line_range_replace_and_insert() {
        let content = "l1\nl2\nl3\nl4\n";
        let (out, n) = replace_line_range(content, 2, 3, "new").unwrap();
        assert_eq!(n, 2);
        assert_eq!(out, "l1\nnew\nl4\n");

        let (out2, _) = insert_after_line(content, 2, "inserted").unwrap();
        assert_eq!(out2, "l1\nl2\ninserted\nl3\nl4\n");
    }

    #[test]
    fn numbered_lines_render_range() {
        let content = "a\nb\nc\nd\n";
        let s = render_numbered_lines(content, 2, 3, 10_000);
        assert!(s.contains("2| b"), "{s}");
        assert!(s.contains("3| c"), "{s}");
        assert!(!s.contains("4| d"), "{s}");
    }

    #[test]
    fn parse_and_apply_single_hunk_patch() {
        let patch = "--- a/doc.md\n+++ b/doc.md\n@@ -2,3 +2,4 @@\n line2\n-old3\n+new3\n+extra\n line4\n";
        let files = parse_unified_patch(patch).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "doc.md");
        let content = "line1\nline2\nold3\nline4\n";
        let out = apply_hunks(content, &files[0].hunks).unwrap();
        assert_eq!(out, "line1\nline2\nnew3\nextra\nline4\n");
    }

    #[test]
    fn apply_patch_tolerates_crlf_content() {
        let patch = "--- a/x.md\n+++ b/x.md\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n";
        let files = parse_unified_patch(patch).unwrap();
        let content = "a\r\nb\r\n";
        let out = apply_hunks(content, &files[0].hunks).unwrap();
        assert_eq!(out, "a\r\nc\r\n");
    }
}
