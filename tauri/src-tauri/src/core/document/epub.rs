//! `core/document/epub` —— EPUB 结构增强（Plan B v2 / Phase 4）。
//!
//! 本模块只做两件 anydoc **做不到**的事，且都保持「anydoc 仍是唯一 Markdown 转换器」：
//!
//! 1. **内嵌图片可见（缺口 G1）**：anydoc 的 Markdown 渲染器把 `ImageSource::Asset` 只在
//!    正文里留 **alt 文本**（`anydoc/src/render/markdown/inline.rs`），图片字节虽在
//!    `Document::assets` 里，但 Markdown 里没有任何占位，前端无从插入。
//!    本模块在**把字节交给 anydoc 之前**改写容器内 XHTML 的 `<img src="相对路径">` 为
//!    `<img src="mdgoasset://local/<base64url(绝对路径)>">`，并把对应资源导出到磁盘。
//!    anydoc 的 `image_source()` 见到「带 scheme 的绝对 URI」就产出 `ImageSource::External`
//!    （`anydoc/src/formats/epub/mod.rs`），于是 Markdown 里出现**真实图片**。
//!    为什么必须「先改字节」而不是「后改 Markdown」：图片在 Markdown 里没有任何标记，
//!    后处理拿不到位置信息；只有让 anydoc 自己渲染出 `![](url)` 才有位置。
//!
//! 2. **真目录（缺口 G4）**：anydoc 的 EPUB 实现**完全不读 `nav.xhtml` / `toc.ncx`**
//!    （`anydoc-0.2.4/src/formats/epub/mod.rs` 268 行内无任何 nav/ncx 代码），
//!    只按 spine 顺序拼接正文。本模块用 `rbook` 取回作者声明的目录树，
//!    并用 anydoc 的 `Document` 模型把每个目录条目**精确**对应到正文里的第几个标题。
//!
//! - **对应关系是构造出来的，不是猜出来的**：anydoc 的 `Block::Heading { anchor }`
//!   保留了源文档的 `id`（形如 `OEBPS/ch1.xhtml#s11`），章节起始处又有
//!   `Block::Paragraph([Inline::Anchor("OEBPS/ch1.xhtml")])`。两者构成
//!   `anchor → 标题序号` 与 `章节路径 → 该章首个标题序号` 两张表，
//!   于是 `OEBPS/ch1.xhtml#s11` 能直接查表命中，无需按标题文字做模糊匹配。
//!   （真实电子书上「目录标签 vs 章节标题」的文字匹配命中率很低：实测
//!   `Metamorphosis-jackson.epub` 只有 3/8 —— Cover / Title Page / Copyright
//!   这类条目根本没有对应标题。）
//!
//! # 失败姿态
//!
//! 图片导出与目录提取**都不允许把整本书弄失败**：任何一步出错都退回
//! 「原字节 + 无目录」，让 anydoc 照旧产出可读正文（最坏情况与改造前一致）。

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use anydoc::model::{Block, Document, Inline};
use sha2::{Digest, Sha256};

/// 前端识别「后端导出的 EPUB 内嵌资源」的 scheme 前缀。
///
/// 载荷是 **base64url(绝对路径)**：只用 `[A-Za-z0-9_-]`，不含 `%`、空白、括号、`|`，
/// 因此对 Markdown / `marked` 的 `encodeURI` / DOMPurify 等任何一层 URL 处理都免疫
/// （anydoc 的 `format_url` 只在含空白或括号时才加尖括号包裹，这里永远不会触发）。
pub const ASSET_URL_PREFIX: &str = "mdgoasset://local/";

/// 单张内嵌图片的体积上限；超过则不导出，正文退回该图的 alt 文本。
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// 单本书导出图片的总体积上限（防止图集/漫画类把缓存写爆）。
const MAX_TOTAL_ASSET_BYTES: usize = 64 * 1024 * 1024;

// ──────────────────────────── 目录 ────────────────────────────

/// 一个目录条目，直接喂给前端右侧大纲面板。
///
/// `Deserialize` 是转换缓存需要的：目录随 `CachedPayload` 一起落库，
/// 命中缓存时直接还原（否则缓存命中的 EPUB 会**丢掉大纲**）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TocItem {
    /// 目录显示文字
    pub label: String,
    /// 层级（1-based）
    pub level: usize,
    /// 归一化后的 href，如 `OEBPS/ch1.xhtml#s11`（已去前导 `/`、已百分号解码）
    pub href: String,
    /// 对应正文中第几个标题（1-based，与前端 `heading-{n}` 对齐）；`0` = 无法对应
    pub heading_index: usize,
}

// ──────────────────────────── 富化结果 ────────────────────────────

/// 图片富化结果。
#[derive(Debug, Clone)]
pub struct Enrichment {
    /// 重写后的 EPUB 字节；无图时**等于输入**（调用方应据 `changed` 决定用哪份）
    pub bytes: Vec<u8>,
    /// 是否真的发生了重写（无图 / 任何失败均为 false）
    pub changed: bool,
    /// 导出成功的图片张数
    pub images: usize,
    /// 资源目录绝对路径（非空时，前端据此校验 `mdgoasset` URL 合法性）
    pub asset_root: Option<String>,
    /// 诊断信息（不阻断流程）
    pub warnings: Vec<String>,
}

impl Enrichment {
    fn unchanged(bytes: &[u8], warnings: Vec<String>) -> Self {
        Self { bytes: bytes.to_vec(), changed: false, images: 0, asset_root: None, warnings }
    }
}

/// 内容文档扩展名（EPUB 正文只能是 XHTML；`.html`/`.htm` 是常见的非规范写法）
fn is_content_doc(name: &str) -> bool {
    matches!(ext_of(name).as_deref(), Some("xhtml" | "html" | "htm"))
}

fn ext_of(path: &str) -> Option<String> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let (_, ext) = file.rsplit_once('.')?;
    if ext.is_empty() { None } else { Some(ext.to_ascii_lowercase()) }
}

// ──────────────────────────── 入口：图片富化 ────────────────────────────

/// 把 EPUB 内嵌图片导出到 `asset_dir`，并把正文里的 `<img src>` 重写为
/// [`ASSET_URL_PREFIX`] 形式的绝对 URI。
///
/// 任何失败都退化为「原字节不变」，绝不返回错误。
pub fn enrich(bytes: &[u8], asset_dir: &Path) -> Enrichment {
    let mut warnings: Vec<String> = Vec::new();

    let mut zip = match zip::ZipArchive::new(Cursor::new(bytes)) {
        Ok(z) => z,
        Err(e) => return Enrichment::unchanged(bytes, vec![format!("EPUB 容器不可读: {e}")]),
    };

    // 第一遍：建 条目名 → 索引 表（解析相对路径时需要）
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index(i) else { continue };
        let name = normalize_entry_name(entry.name());
        by_name.insert(name, i);
    }

    // 第二遍：逐个内容文档重写
    let mut rewrites: HashMap<String, String> = HashMap::new();
    let mut total_asset_bytes = 0usize;
    let mut images = 0usize;

    for i in 0..zip.len() {
        let mut entry = match zip.by_index(i) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = normalize_entry_name(entry.name());
        if !is_content_doc(&name) || entry.size() == 0 {
            continue;
        }
        let mut raw = Vec::new();
        if entry.read_to_end(&mut raw).is_err() {
            continue;
        }
        let Ok(text) = String::from_utf8(raw) else {
            continue; // 非 UTF-8 正文：保持原样，交给 anydoc 处理
        };
        drop(entry); // 释放 `zip` 的可变借用，下面还要 by_index 取图片

        let base_doc = name.clone();
        let mut hit = 0usize;
        let (rewritten, _) = rewrite_image_tags(&text, |src| {
            let zip_path = resolve_zip_path(&base_doc, src)?;
            let idx = by_name.get(&zip_path).copied().or_else(|| {
                // 大小写不敏感兜底（Windows 产出 vs 规范书写）
                by_name
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(&zip_path))
                    .map(|(_, v)| *v)
            })?;
            let mut img = Vec::new();
            {
                // `take` 按值消费 reader，故这里不需要 `mut`
                let f = zip.by_index(idx).ok()?;
                if f.size() as usize > MAX_IMAGE_BYTES {
                    return None;
                }
                // 中央目录里的声明尺寸可能撒谎 → 用 take 再兜一道硬上限
                f.take(MAX_IMAGE_BYTES as u64 + 1).read_to_end(&mut img).ok()?;
            }
            if img.is_empty() || img.len() > MAX_IMAGE_BYTES {
                return None;
            }
            if total_asset_bytes + img.len() > MAX_TOTAL_ASSET_BYTES {
                return None;
            }
            let abs = match write_asset(asset_dir, &img, &zip_path) {
                Ok(p) => p,
                Err(_) => return None,
            };
            total_asset_bytes += img.len();
            images += 1;
            hit += 1;
            // URL 里只放**文件名**（见 `asset_url` 的隐私/体积/安全理由）
            let file_name = abs.file_name()?.to_string_lossy().to_string();
            Some(asset_url(&file_name))
        });

        if hit > 0 {
            rewrites.insert(name, rewritten);
        }
    }

    if rewrites.is_empty() {
        if images > 0 {
            warnings.push("图片已导出但正文未发生重写".to_string());
        }
        return Enrichment::unchanged(bytes, warnings);
    }

    // 第三遍：重建容器
    let rebuilt = match rebuild_zip(bytes, &rewrites) {
        Ok(b) => b,
        Err(e) => {
            warnings.push(format!("EPUB 容器重建失败，退回原字节: {e}"));
            return Enrichment::unchanged(bytes, warnings);
        }
    };

    Enrichment {
        bytes: rebuilt,
        changed: true,
        images,
        asset_root: Some(asset_dir.to_string_lossy().replace('\\', "/")),
        warnings,
    }
}

fn normalize_entry_name(name: &str) -> String {
    let n = name.replace('\\', "/");
    n.trim_start_matches("./").to_string()
}

/// 该 EPUB 的资源目录：`<用户缓存目录>/mdgo/epub-assets/<内容哈希前 16 位>`。
///
/// 用**内容哈希**做目录名的两个好处：
/// ① 同一本书反复预览/索引命中同一目录，图片不重复写；
/// ② 书的内容一改就换目录，不可能读到上一版的旧图（无缓存失效问题）。
pub fn asset_dir_for(bytes: &[u8]) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        hex.push_str(&format!("{b:02x}"));
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mdgo")
        .join("epub-assets")
        .join(&hex[..16])
}

/// 生成图片引用 URL：`mdgoasset://local/<文件名>`。
///
/// **只放文件名，不放绝对路径**（早先版本 base64 编码了绝对路径）。理由有三：
/// ① **隐私**：正文会被复制/导出，绝对路径里带用户名与目录结构，不应出现在文档里；
/// ② **体积**：文件名约 40 字符，绝对路径 base64 后往往 150+ 字符，而这段文本会进入
///    Markdown 源码（见 [`strip_image_destinations`] 说明为什么还要额外清理索引）；
/// ③ **安全**：前端只接受「资源目录 + 裸文件名」，不接受任何路径，构造过的 EPUB
///    连表达 `..` 或绝对路径的机会都没有。
///
/// 文件名是**图片字节的 SHA-256 前缀**，因此内容相同即去重、内容不同必不冲突。
fn asset_url(file_name: &str) -> String {
    format!("{ASSET_URL_PREFIX}{file_name}")
}

/// 把正文里的图片**目标地址**清空：`![alt](dest)` → `![alt]()`。
///
/// 为什么需要：Markdown 分块的 chunk 文本取的是**源码行切片**（`document/markdown.rs`
/// 的 sourcepos 切片，不是 inline 纯文本），因此图片 URL 会**原样进入 BM25 文本与
/// embedding 输入**。EPUB 一本书可能有几十张图，每张都会灌进一段 URL：
/// 既是无意义的词项噪声、又白占 token 预算。
///
/// 为什么改成空目标而不是直接删掉整段：`![alt]()` 仍是合法的 Markdown 图片节点，
/// **alt 文本（真正的检索价值）完整保留**，AST 形态与 `chunk_type` 也不变。
///
/// 注意这只作用于**索引侧文本**；预览渲染用的是原始 `src.text`，图片照常显示。
/// 行/字节偏移会变化，但 EPUB 的 `page_spans`/`line_page_map` 恒为空，
/// 不存在页归属错位风险（与 `html_clean` 对 Markdown 的清洗同一道理）。
pub fn strip_image_destinations(md: &str) -> String {
    if !md.contains("](") {
        return md.to_string();
    }
    let b = md.as_bytes();
    let mut out = String::with_capacity(md.len());
    let mut i = 0usize;
    while i < b.len() {
        // 找 `![`
        if b[i] == b'!' && i + 1 < b.len() && b[i + 1] == b'[' {
            // 匹配对应的 `]`（alt 内不允许嵌套未转义的 `[`，按 CommonMark 取第一个 `]`）
            let mut j = i + 2;
            let mut alt_end = None;
            while j < b.len() {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b']' {
                    alt_end = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(ae) = alt_end {
                // 其后必须紧跟 `(`，才算行内图片目标
                if ae + 1 < b.len() && b[ae + 1] == b'(' {
                    // 找配对的 `)`，支持 `<...>` 包裹的目标
                    let mut k = ae + 2;
                    let mut depth = 1usize;
                    let mut target_end = None;
                    let mut angle = false;
                    while k < b.len() {
                        let c = b[k];
                        if c == b'\\' {
                            k += 2;
                            continue;
                        }
                        if c == b'<' {
                            angle = true;
                        } else if c == b'>' {
                            angle = false;
                        } else if !angle && c == b'(' {
                            depth += 1;
                        } else if !angle && c == b')' {
                            depth -= 1;
                            if depth == 0 {
                                target_end = Some(k);
                                break;
                            }
                        }
                        k += 1;
                    }
                    if let Some(te) = target_end {
                        out.push_str(&md[i..=ae]); // `![alt]`
                        out.push_str("()");       // 目标清空
                        i = te + 1;
                        continue;
                    }
                }
            }
        }
        // 按字符推进，避免切坏 UTF-8
        let ch = md[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn write_asset(dir: &Path, bytes: &[u8], source_path: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        hex.push_str(&format!("{b:02x}"));
    }
    let ext = ext_of(source_path).unwrap_or_else(|| "bin".to_string());
    let file = dir.join(format!("{}.{}", &hex[..32], ext));
    if !file.exists() {
        // 先写临时文件再改名，避免并发读到半截文件
        let tmp = file.with_extension(format!("{ext}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &file)?;
    }
    Ok(file)
}

/// 用重写后的内容文档重建 EPUB 容器（其余条目原样拷贝）。
fn rebuild_zip(
    original: &[u8],
    rewrites: &HashMap<String, String>,
) -> Result<Vec<u8>, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(original)).map_err(|e| e.to_string())?;
    let mut out = zip::ZipWriter::new(Cursor::new(Vec::with_capacity(original.len())));
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let stored = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);

    // OCF 要求 `mimetype` 位于首位且不压缩：先定位它，单独写在最前
    let mut mimetype_idx: Option<usize> = None;
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index(i) else { continue };
        if normalize_entry_name(entry.name()) == "mimetype" {
            mimetype_idx = Some(i);
            break;
        }
    }
    if let Some(idx) = mimetype_idx {
        let mut entry = zip.by_index(idx).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        out.start_file(name, stored).map_err(|e| e.to_string())?;
        out.write_all(&buf).map_err(|e| e.to_string())?;
    }

    for i in 0..zip.len() {
        if Some(i) == mimetype_idx {
            continue;
        }
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let raw_name = entry.name().to_string();
        let norm = normalize_entry_name(&raw_name);
        // 保留条目原有的压缩方式：图片（PNG/JPEG）本身已是压缩数据，
        // 对它们再 Deflate 一遍既白费 CPU、又常把体积压得更大。
        // 无法用 `zip` 8.6 写出的方法（如 bzip2/zstd）统一退化为 Deflate。
        let opts = match entry.compression() {
            zip::CompressionMethod::Stored => stored,
            _ => deflated,
        };
        out.start_file(raw_name, opts).map_err(|e| e.to_string())?;
        if let Some(new_text) = rewrites.get(&norm) {
            out.write_all(new_text.as_bytes()).map_err(|e| e.to_string())?;
        } else {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            out.write_all(&buf).map_err(|e| e.to_string())?;
        }
    }
    let cur = out.finish().map_err(|e| e.to_string())?;
    Ok(cur.into_inner())
}

// ──────────────────────────── 图片标签重写 ────────────────────────────

/// 扫描 `html`，对每个 `<img>` / `<image>` 起始标签调用 `map_src`；
/// 返回（重写后的文本, 命中数）。标签之外的字节**逐字保留**（不做 XML 往返序列化，
/// 避免破坏 anydoc 依赖的文档结构）。
fn rewrite_image_tags<F>(html: &str, mut map_src: F) -> (String, usize)
where
    F: FnMut(&str) -> Option<String>,
{
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut copied = 0usize;
    let mut i = 0usize;
    let mut hits = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if j < bytes.len() && bytes[j] == b'/' {
            j += 1;
        }
        let name_start = j;
        while j < bytes.len()
            && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b':' || bytes[j] == b'-')
        {
            j += 1;
        }
        let name = &html[name_start..j];
        let is_img = name.eq_ignore_ascii_case("img");
        let is_image = name.eq_ignore_ascii_case("image");
        if !is_img && !is_image {
            i += 1;
            continue;
        }
        // 必须是起始标签：名字后紧跟分隔符
        if j >= bytes.len()
            || !(bytes[j].is_ascii_whitespace() || bytes[j] == b'>' || bytes[j] == b'/')
        {
            i += 1;
            continue;
        }
        let Some(end) = find_tag_end(html, j) else { break };
        let tag = &html[i..end];
        let attrs: &[&str] = if is_img { &["src"] } else { &["xlink:href", "href"] };

        if let Some((vs, ve, old_value)) = find_attr(tag, attrs) {
            if !old_value.is_empty() {
                if let Some(new_src) = map_src(&old_value) {
                    out.push_str(&html[copied..i]);
                    out.push_str(&tag[..vs]);
                    out.push_str(&new_src);
                    out.push_str(&tag[ve..]);
                    copied = end;
                    hits += 1;
                }
            }
        }
        i = end;
    }
    out.push_str(&html[copied..]);
    (out, hits)
}

/// 找标签结束位置（`>`），跳过引号内的内容。返回 `>` 之后的下标。
fn find_tag_end(s: &str, from: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = from;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == b'"' || c == b'\'' {
                    quote = Some(c);
                } else if c == b'>' {
                    return Some(i + 1);
                }
            }
        }
        i += 1;
    }
    None
}

/// 在标签内定位某个属性，返回（值起, 值止, 值）。
/// 按「名 = 值」逐段推进，因此不会把**属性值内部**的同名字符串误判为属性。
fn find_attr(tag: &str, names: &[&str]) -> Option<(usize, usize, String)> {
    let b = tag.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() {
            return None;
        }
        let name_start = i;
        while i < b.len()
            && !b[i].is_ascii_whitespace()
            && b[i] != b'='
            && b[i] != b'>'
            && b[i] != b'/'
        {
            i += 1;
        }
        let name = &tag[name_start..i];
        let mut k = i;
        while k < b.len() && b[k].is_ascii_whitespace() {
            k += 1;
        }
        if k < b.len() && b[k] == b'=' {
            k += 1;
            while k < b.len() && b[k].is_ascii_whitespace() {
                k += 1;
            }
            let (vs, ve) = if k < b.len() && (b[k] == b'"' || b[k] == b'\'') {
                let q = b[k];
                let vs0 = k + 1;
                let ve0 = tag[vs0..].find(q as char).map(|o| vs0 + o).unwrap_or(tag.len());
                (vs0, ve0)
            } else {
                let vs0 = k;
                let mut ve0 = k;
                while ve0 < b.len() && !b[ve0].is_ascii_whitespace() && b[ve0] != b'>' {
                    ve0 += 1;
                }
                (vs0, ve0)
            };
            if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                return Some((vs, ve, tag[vs..ve].to_string()));
            }
            i = ve.saturating_add(1);
            continue;
        }
        i = k.max(i + 1);
    }
    None
}

// ──────────────────────────── 路径解析 ────────────────────────────

/// 把内容文档里的相对 `src` 折算成 zip 条目名（与 anydoc `package::path::resolve` 同口径）。
fn resolve_zip_path(base_doc: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || has_scheme(href) {
        return None; // 空 / 外链（http、data、mdgoasset…）一律不处理
    }
    // 去掉 query 与 fragment，且**先切分再解码**（否则 %23 会被当成 fragment 分隔符）
    let without_frag = href.split('#').next().unwrap_or("");
    let without_query = without_frag.split('?').next().unwrap_or("");
    if without_query.is_empty() {
        return None;
    }
    let decoded = percent_decode(without_query);

    let dir = match base_doc.rfind('/') {
        Some(p) => &base_doc[..p],
        None => "",
    };
    let joined = if let Some(rest) = decoded.strip_prefix('/') {
        rest.to_string()
    } else if dir.is_empty() {
        decoded
    } else {
        format!("{dir}/{decoded}")
    };

    // 逐段解析 `.` / `..`
    let mut stack: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                stack.pop()?; // 越出包根 → 视为不可解析
            }
            s => stack.push(s),
        }
    }
    if stack.is_empty() { None } else { Some(stack.join("/")) }
}

fn has_scheme(s: &str) -> bool {
    let Some(colon) = s.find(':') else { return false };
    let mut chars = s[..colon].chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !ok {
        return false;
    }
    // Windows 盘符（C:\...）不是 scheme；EPUB 里不该出现，但别误判
    let b = s.as_bytes();
    !(b.len() >= 3 && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = hex_val(b[i + 1]);
            let lo = hex_val(b[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ──────────────────────────── 入口：真目录 ────────────────────────────

/// 用 rbook 读取作者声明的目录树（EPUB 3 `nav.xhtml` 优先，EPUB 2 `toc.ncx` 兜底）。
///
/// 只取**一个**目录源，不合并两版：生产者同时提供时二者常互相矛盾
/// （重新生成时间不同 / NCX 是扁平变体），合并会出现重复与幻影条目。
///
/// 本变体按字节解析；调用方手上有磁盘路径时应优先用 [`extract_toc_from_path`]。
pub fn extract_toc(bytes: &[u8]) -> Vec<TocItem> {
    // `Epub::read` 要求 `R: 'static`，`&[u8]` 不满足 → 只能复制一份字节。
    // 这是**兜底**路径（测试、内存字节），生产路径见下。
    collect_toc(rbook::Epub::read(Cursor::new(bytes.to_vec())).ok())
}

/// 与 [`extract_toc`] 同义，但直接按**磁盘路径**打开。
///
/// 为什么值得单独有：`rbook::Epub::read` 需要 `'static` 的 reader，因此字节变体必须
/// `to_vec()` 复制整本书（EPUB 政策上限 200MB）。有路径时直接 `Epub::open` 就没有这次复制。
/// 路径打不开（例如已被移动）时回退到字节变体，功能不变。
pub fn extract_toc_from_path(bytes: &[u8], abs_path: &Path) -> Vec<TocItem> {
    let epub = rbook::Epub::open(abs_path)
        .or_else(|_| rbook::Epub::read(Cursor::new(bytes.to_vec())));
    collect_toc(epub.ok())
}

fn collect_toc(epub: Option<rbook::Epub>) -> Vec<TocItem> {
    let Some(epub) = epub else {
        return Vec::new();
    };
    let Some(root) = epub.toc().contents() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // `contents()` 返回的是**合成根**（depth 0、无资源），真正的顶层条目是它的直接子节点
    for entry in root.iter() {
        walk_toc(&entry, 1, &mut out);
    }
    out
}

fn walk_toc(entry: &rbook::epub::toc::EpubTocEntry<'_>, level: usize, out: &mut Vec<TocItem>) {
    let label = entry.label().trim().to_string();
    let href = entry
        .href()
        .map(|h| h.decode().trim_start_matches('/').to_string())
        .unwrap_or_default();
    // 没有 href 且没有子节点的是纯分组标题，仍要展示（保持层级）
    out.push(TocItem { label, level, href, heading_index: 0 });
    for child in entry.iter() {
        walk_toc(&child, level + 1, out);
    }
}

/// 用 anydoc 的文档模型给每个目录条目算出「对应正文第几个标题」。
///
/// 两张表都由模型直接构造：
/// - `heading_anchor`：`Block::Heading { anchor }` 的 anchor（源文档 id）→ 标题序号；
/// - `chapter_start`：章节起始锚点（`Inline::Anchor`）→ 其后第一个标题的序号。
pub fn map_headings(items: &mut [TocItem], doc: &Document) {
    let mut heading_anchor: HashMap<String, usize> = HashMap::new();
    let mut chapter_start: HashMap<String, usize> = HashMap::new();
    let mut pending_chapter: Option<String> = None;
    let mut headings = 0usize;

    for block in &doc.blocks {
        match block {
            Block::Heading { anchor, .. } => {
                headings += 1;
                if let Some(a) = anchor {
                    heading_anchor.entry(a.to_string()).or_insert(headings);
                }
                if let Some(ch) = pending_chapter.take() {
                    chapter_start.entry(ch).or_insert(headings);
                }
            }
            // anydoc 在每个章节起始处插入仅含锚点的段落
            Block::Paragraph(inlines) => {
                if inlines.len() == 1 {
                    if let Inline::Anchor(id) = &inlines[0] {
                        pending_chapter = Some(id.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    for item in items.iter_mut() {
        if item.href.is_empty() {
            item.heading_index = 0;
            continue;
        }
        item.heading_index = heading_anchor
            .get(&item.href)
            .copied()
            .or_else(|| {
                let path = item.href.split('#').next().unwrap_or("");
                chapter_start.get(path).copied()
            })
            .unwrap_or(0);
    }
}

// ──────────────────────────── 测试 ────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    // `std::io::Write` 已由本模块顶部的 `use std::io::{Cursor, Read, Write}` 带入

    /// 结构合法的最小 EPUB：2 章（含 h1/h2）+ 1 张图 + EPUB3 nav + EPUB2 ncx。
    ///
    /// 刻意**不落盘**：仓库里不放二进制夹具（与 `loader::minimal_pdf` 同思路）。
    fn minimal_epub() -> Vec<u8> {
        // 1x1 透明 PNG
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];

        let container = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;

        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Probe Book</dc:title><dc:language>zh</dc:language>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="ch2.xhtml" media-type="application/xhtml+xml"/>
    <item id="img1" href="../images/pic.png" media-type="image/png"/>
  </manifest>
  <spine toc="ncx"><itemref idref="ch1"/><itemref idref="ch2"/></spine>
</package>"#;

        // 注意：这里必须用 `r##"..."##`——正文里有 `href="#fn1"`，其中的 `"#`
        // 会提前终止 `r#"..."#` 形式的原始字符串。
        let ch1 = r##"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Ch1</title></head>
<body>
  <h1 id="c1">Chapter One</h1>
  <p>Body with <a href="#fn1" id="ref1">[1]</a> and a cross ref <a href="ch2.xhtml#s21">see 2.1</a>.</p>
  <h2 id="s11">Section 1.1</h2>
  <img src="../images/pic.png" alt="figure one"/>
  <table><tr><th>A</th><th>B</th></tr><tr><td>a1</td><td>b1</td></tr></table>
  <p id="fn1">[1] footnote body</p>
</body></html>"##;

        let ch2 = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Ch2</title></head>
<body><h1 id="c2">Chapter Two</h1><p>Second body.</p><h2 id="s21">Section 2.1</h2></body></html>"#;

        let nav = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>Contents</title></head>
<body><nav epub:type="toc" id="toc"><h1>Contents</h1>
<ol>
  <li><a href="ch1.xhtml">Chapter One</a><ol><li><a href="ch1.xhtml#s11">Section 1.1</a></li></ol></li>
  <li><a href="ch2.xhtml">Chapter Two</a><ol><li><a href="ch2.xhtml#s21">Section 2.1</a></li></ol></li>
</ol></nav></body></html>"#;

        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        w.start_file("mimetype", stored).unwrap();
        w.write_all(b"application/epub+zip").unwrap();
        let deflated = zip::write::SimpleFileOptions::default();
        for (name, body) in [
            ("META-INF/container.xml", container),
            ("OEBPS/content.opf", opf),
            ("OEBPS/ch1.xhtml", ch1),
            ("OEBPS/ch2.xhtml", ch2),
            ("OEBPS/nav.xhtml", nav),
        ] {
            w.start_file(name, deflated).unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.start_file("images/pic.png", deflated).unwrap();
        w.write_all(PNG).unwrap();
        w.finish().unwrap().into_inner()
    }

    // ── 纯函数：标签重写 ──

    #[test]
    fn rewrite_handles_quotes_and_does_not_mistake_attribute_values() {
        // `alt="src=x.png"` 里的 src 不能被当成属性
        let html = r#"<img alt="src=bogus.png" src="../i/a.png"><image xlink:href='b.png'/><img src=plain.png>"#;
        let (out, hits) = rewrite_image_tags(html, |src| Some(format!("X({src})")));
        assert_eq!(hits, 3);
        assert!(out.contains(r#"src="X(../i/a.png)""#), "{out}");
        assert!(out.contains(r#"xlink:href='X(b.png)'"#), "{out}");
        assert!(out.contains("src=X(plain.png)"), "{out}");
        // alt 值原样保留
        assert!(out.contains(r#"alt="src=bogus.png""#), "{out}");
    }

    #[test]
    fn rewrite_leaves_untouched_bytes_verbatim() {
        let html = "<p>a &amp; b</p><img src=\"x.png\"/><p>tail</p>";
        let (out, hits) = rewrite_image_tags(html, |_| Some("NEW".into()));
        assert_eq!(hits, 1);
        assert_eq!(out, "<p>a &amp; b</p><img src=\"NEW\"/><p>tail</p>");
    }

    #[test]
    fn rewrite_reports_zero_when_mapper_declines() {
        let (out, hits) = rewrite_image_tags("<img src=\"x.png\">", |_| None);
        assert_eq!(hits, 0);
        assert_eq!(out, "<img src=\"x.png\">");
    }

    // ── 纯函数：路径解析 ──

    #[test]
    fn resolve_zip_path_follows_epub_uri_rules() {
        // 相对 OPF/章节所在目录
        assert_eq!(
            resolve_zip_path("OEBPS/ch1.xhtml", "../images/pic.png").as_deref(),
            Some("images/pic.png")
        );
        assert_eq!(
            resolve_zip_path("OEBPS/text/ch1.xhtml", "../images/a.png").as_deref(),
            Some("OEBPS/images/a.png")
        );
        // 包根相对
        assert_eq!(
            resolve_zip_path("OEBPS/ch1.xhtml", "/OEBPS/i.png").as_deref(),
            Some("OEBPS/i.png")
        );
        // fragment / query 先切分再解码：%23 不是 fragment 分隔符
        assert_eq!(
            resolve_zip_path("OEBPS/ch1.xhtml", "a%23b.png#frag").as_deref(),
            Some("OEBPS/a#b.png")
        );
        // 外链与空值一律不处理
        assert_eq!(resolve_zip_path("OEBPS/ch1.xhtml", "https://e.com/a.png"), None);
        assert_eq!(resolve_zip_path("OEBPS/ch1.xhtml", "data:image/png;base64,AA"), None);
        assert_eq!(resolve_zip_path("OEBPS/ch1.xhtml", ""), None);
        // 越出包根 → 不可解析
        assert_eq!(resolve_zip_path("OEBPS/ch1.xhtml", "../../../etc/passwd"), None);
    }

    // ── 集成：富化 + anydoc ──

    #[test]
    fn enrich_exports_image_and_makes_anydoc_emit_real_image() {
        let epub = minimal_epub();
        let dir = tempfile::tempdir().expect("临时目录");
        let out = enrich(&epub, dir.path());

        assert!(out.changed, "含图 EPUB 应发生重写");
        assert_eq!(out.images, 1, "应导出 1 张图");
        let root = out.asset_root.clone().expect("应给出资源目录");

        // 图片文件真的落盘了
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("读资源目录")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), 1, "应落盘 1 个资源文件");

        // 关键回归：anydoc 现在必须产出**真实图片**而不是只留 alt 文本
        let md = anydoc::to_markdown_bytes(&out.bytes, Some(anydoc::Format::Epub)).expect("转换");
        assert!(md.contains("!["), "anydoc 应产出 Markdown 图片: {md}");
        assert!(md.contains(ASSET_URL_PREFIX), "图片 URL 应使用 mdgoasset scheme: {md}");
        assert!(md.contains("figure one"), "alt 文本应保留: {md}");
        assert!(!root.is_empty());

        // ★ 端到端一致性：Markdown 里那个 URL 必须是**前端能解析、且磁盘上真实存在**的。
        //
        // 这条断言是为了防一类已经发生过的事故：改了 URL 形态却没同步前端解析器
        // （或没 bump `ConverterInfo::EPUB.version` 导致旧缓存回放旧形态），
        // 表现是「所有图片都不显示」，而单看后端"URL 里含 ASSET_URL_PREFIX"完全测不出来。
        let url = md
            .split(ASSET_URL_PREFIX)
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .expect("应从 Markdown 取出图片 URL 载荷");
        let name = url.split(')').next().unwrap_or(url).trim();
        // 与前端 `ASSET_NAME_RE = /^[0-9a-f]{32}\.[a-z0-9]{1,8}$/` 对齐
        let (stem, ext) = name.split_once('.').unwrap_or_else(|| panic!("URL 应为 <名>.<扩展名>: {name}"));
        assert_eq!(stem.len(), 32, "文件名前缀应是 sha256 前 32 位: {name}");
        assert!(
            stem.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "文件名前缀应是小写十六进制: {name}"
        );
        assert!(
            (1..=8).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "扩展名应是小写字母/数字且不超过 8 位: {name}"
        );
        // 前端会把它拼成 `asset_root + '/' + 文件名`，因此该文件必须真的在那儿
        let resolved = Path::new(&root).join(name);
        assert!(
            resolved.is_file(),
            "前端按 asset_root + 文件名解析出的路径必须存在: {}",
            resolved.display()
        );
    }

    #[test]
    fn enrich_is_idempotent_on_already_rewritten_bytes() {
        // 二次富化必须是无操作：重写后的 `<img src>` 已是带 scheme 的绝对 URI，
        // `resolve_zip_path` 会直接拒绝（`has_scheme`），因此不会重复导出图片。
        let epub = minimal_epub();
        let dir = tempfile::tempdir().expect("临时目录");
        let out = enrich(&epub, dir.path());
        assert!(out.changed);
        let again = enrich(&out.bytes, dir.path());
        assert!(!again.changed, "已重写的字节不应被二次改写");
        assert_eq!(again.images, 0);
    }

    #[test]
    fn enrich_rejects_non_zip_bytes_without_failing() {
        let dir = tempfile::tempdir().expect("临时目录");
        let out = enrich(b"not a zip at all", dir.path());
        assert!(!out.changed);
        assert_eq!(out.images, 0);
        assert!(!out.warnings.is_empty(), "应给出诊断而不是静默");
    }

    #[test]
    fn asset_url_carries_only_a_bare_file_name() {
        // URL 只放文件名：不带绝对路径（隐私）、不含 %/空白/括号（对任何 URL 处理层免疫）、
        // 不含 `/` 或 `..`（前端因此无法被诱导读出资源目录之外的文件）。
        let url = asset_url("0123456789abcdef0123456789abcdef.png");
        assert_eq!(url, "mdgoasset://local/0123456789abcdef0123456789abcdef.png");
        let name = &url[ASSET_URL_PREFIX.len()..];
        assert!(!name.contains('/'), "不得含路径分隔符: {name}");
        assert!(!name.contains(".."), "不得含向上穿越: {name}");
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'),
            "只应是裸文件名: {name}"
        );
    }

    #[test]
    fn strip_image_destinations_keeps_alt_and_clears_only_target() {
        // 基本形态
        assert_eq!(
            strip_image_destinations("前![图一](mdgoasset://local/a.png)后"),
            "前![图一]()后"
        );
        // 预览用不到的无关内容逐字保留
        assert_eq!(strip_image_destinations("纯文本，无图"), "纯文本，无图");
        assert_eq!(
            strip_image_destinations("[普通链接](https://e.com/x)"),
            "[普通链接](https://e.com/x)"
        );
        // 尖括号目标、含括号目标、中文与 UTF-8 边界
        assert_eq!(
            strip_image_destinations("![a](<C:/x y/z 的副本.png>)"),
            "![a]()"
        );
        assert_eq!(strip_image_destinations("![a](u(1).png)"), "![a]()");
        // 多图与行内混排
        assert_eq!(
            strip_image_destinations("![x](p.png) 与 ![y](q.png)"),
            "![x]() 与 ![y]()"
        );
        // 只清目标、不动 alt 里的转义
        assert_eq!(strip_image_destinations(r"![a\]b](x.png)"), r"![a\]b]()");
        // 未闭合的目标不处理（宁可留着，也不要改坏正文）
        assert_eq!(strip_image_destinations("![a](open"), "![a](open");
    }

    // ── 集成：真目录 ──

    #[test]
    fn extract_toc_reads_epub3_nav_with_levels() {
        let toc = extract_toc(&minimal_epub());
        assert_eq!(toc.len(), 4, "应读出 2 章 + 2 小节: {toc:?}");
        assert_eq!(toc[0].label, "Chapter One");
        assert_eq!(toc[0].level, 1);
        assert_eq!(toc[0].href, "OEBPS/ch1.xhtml");
        assert_eq!(toc[1].label, "Section 1.1");
        assert_eq!(toc[1].level, 2);
        assert_eq!(toc[1].href, "OEBPS/ch1.xhtml#s11");
    }

    #[test]
    fn map_headings_binds_toc_to_heading_ordinals() {
        let epub = minimal_epub();
        let mut toc = extract_toc(&epub);
        let doc = anydoc::to_document(&epub, Some(anydoc::Format::Epub)).expect("文档模型");
        map_headings(&mut toc, &doc);

        // anydoc 的标题顺序：1 = OPF 书名，2 = Chapter One，3 = Section 1.1，
        // 4 = Chapter Two，5 = Section 2.1
        assert_eq!(toc[0].heading_index, 2, "章首条目应落到该章第一个标题");
        assert_eq!(toc[1].heading_index, 3, "带 fragment 的条目应按 id 精确命中");
        assert_eq!(toc[2].heading_index, 4);
        assert_eq!(toc[3].heading_index, 5);
    }

    #[test]
    fn map_headings_leaves_unmappable_entries_at_zero() {
        let epub = minimal_epub();
        let doc = anydoc::to_document(&epub, Some(anydoc::Format::Epub)).expect("文档模型");
        // 真实电子书里 Cover / Title Page 这类条目根本没有对应标题
        let mut toc = vec![
            TocItem { label: "Cover".into(), level: 1, href: "OEBPS/cover.xhtml".into(), heading_index: 0 },
            TocItem { label: "No href".into(), level: 1, href: String::new(), heading_index: 0 },
        ];
        map_headings(&mut toc, &doc);
        assert_eq!(toc[0].heading_index, 0);
        assert_eq!(toc[1].heading_index, 0);
    }

    // ── 索引侧副作用：图片 URL 不得污染检索文本 ──

    /// `enrich` 会把图片写成 `![alt](mdgoasset://local/<base64url(绝对路径)>)`，
    /// 而 Markdown 分块的 chunk 文本取的是**源码行切片**（`document/markdown.rs` 的
    /// sourcepos 切片），不是 inline 纯文本。因此必须显式验证：base64url 载荷
    /// （内含**绝对路径**）不会进入 BM25 / embedding 文本。
    #[test]
    fn enriched_image_urls_do_not_leak_into_index_text() {
        let md = "# Chapter One\n\n正文段落一。\n\n\
                  ![figure one](mdgoasset://local/QUJDREVGRw)\n\n尾段。\n";
        let src = crate::core::document::loader::DocumentSource::for_test("a.epub", md);
        let chunks = crate::core::pipeline::chunk_document(&src, 800, 100, None);
        assert!(!chunks.is_empty(), "应产出 chunk");

        for c in &chunks {
            assert!(
                !c.text.contains(ASSET_URL_PREFIX),
                "图片 URL 泄漏进 chunk.text（会污染 BM25）: {:?}",
                c.text
            );
            if let Some(et) = &c.embedding_text {
                assert!(
                    !et.contains(ASSET_URL_PREFIX),
                    "图片 URL 泄漏进 chunk.embedding_text（会污染向量）: {et:?}"
                );
            }
        }
    }
}
