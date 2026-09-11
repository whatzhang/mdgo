//! Plan B v2 端到端**验收 harness**（方案 §8.4 两层指标）。
//!
//! 与 `#[cfg(test)]` 单测的区别：单测用合成数据锁**契约**，本 harness 用**真实样本**
//! 验证「注册表在册 → 真的能转出内容 → 真的能分块 → 页码 provenance 真的对得上」。
//! 两者互补：合成夹具保证可复现，真实样本才能暴露解析器在真实文件上的行为。
//!
//! **只在设置了 `MDGO_ACCEPT_DIR` 时运行**——真实样本（几十 MB 的 docx/pptx/pdf）
//! 不进仓库，未设置时直接跳过并打印原因，保证日常 `cargo test` 与 CI 不受影响。
//!
//! ```text
//! $env:MDGO_ACCEPT_DIR='C:\samples'
//! cargo test --lib acceptance -- --nocapture
//! ```
//!
//! 为什么是 in-crate 模块而不是 `tests/` 集成测试：`lib.rs` 的 L31 决策让 `core`
//! 保持私有（只对 `bench` feature 开窄门面），集成测试拿不到 `load_document` /
//! `chunk_document`。放 crate 内既保住该边界，又拿到真实调用链。

use std::path::{Path, PathBuf};

use crate::core::db::conversion_cache::ConversionCache;
use crate::core::document::filekind;
use crate::core::document::loader::{self, ConverterInfo, DocStatus};
use crate::core::pipeline::chunk_document;

/// 验收用分块参数（与索引默认同量级：1000 字符 / 100 重叠）
const CHUNK_SIZE: usize = 1000;
const CHUNK_OVERLAP: usize = 100;

fn accept_dir() -> Option<PathBuf> {
    match std::env::var_os("MDGO_ACCEPT_DIR") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// 递归收集可索引文件（深度上限 3，避免误扫巨大目录树）
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_files(&p, out, depth + 1);
        } else if p.is_file() {
            let rel = p
                .strip_prefix(dir)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if filekind::is_indexable(&rel) {
                out.push(p);
            }
        }
    }
}

fn rel_of(dir: &Path, p: &Path) -> String {
    p.strip_prefix(dir)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
}

/// `MDGO_ACCEPT_DUMP=N`：打印每个样本转换后正文的前 N 个字符。
/// 转换保真度（§8.4 第一层）最终要人眼核对，机器只能查乱码率与结构；
/// 这个开关让 harness 同时充当"人工复核取数"工具。
fn dump_limit() -> usize {
    std::env::var("MDGO_ACCEPT_DUMP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
}

/// 替换字符占比。UTF-8 误读二进制会大量产生 U+FFFD，是「乱码入库」最直接的量化信号。
fn mojibake_ratio(text: &str) -> f64 {
    let total = text.chars().count();
    if total == 0 {
        return 0.0;
    }
    let bad = text.chars().filter(|c| *c == '\u{FFFD}').count();
    bad as f64 / total as f64
}

fn count_headings(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("# ") || t.starts_with("## ") || t.starts_with("### ")
        })
        .count()
}

/// 单文件验收结果（一行 = 一个样本）
struct Row {
    rel: String,
    bytes: u64,
    outcome: String,
    converter: String,
    source_kind: String,
    form: String,
    text_chars: usize,
    headings: usize,
    pages: usize,
    chunks: usize,
    heading_path_rate: f64,
    page_covered: usize,
    mojibake: f64,
    extra: String,
}

#[test]
fn acceptance_real_samples() {
    let Some(dir) = accept_dir() else {
        eprintln!(
            "[skip] 未设置 MDGO_ACCEPT_DIR，真实样本验收未运行。\n\
             \t用法：$env:MDGO_ACCEPT_DIR='<样本目录>'; cargo test --lib acceptance -- --nocapture"
        );
        return;
    };
    assert!(dir.is_dir(), "MDGO_ACCEPT_DIR 不是目录: {}", dir.display());

    let mut files = Vec::new();
    collect_files(&dir, &mut files, 0);
    files.sort();
    assert!(
        !files.is_empty(),
        "{} 下没有可索引文件 —— 验收会假通过，请检查样本目录",
        dir.display()
    );

    let mut rows: Vec<Row> = Vec::new();
    // 断言用的累积量
    let mut checked_office = 0usize;
    let mut checked_pdf = 0usize;
    let mut total_chunks = 0usize;
    let dump = dump_limit();

    for abs in &files {
        let rel = rel_of(&dir, abs);
        let ext = filekind::ext_of(&rel).unwrap_or("").to_string();
        let meta_len = std::fs::metadata(abs).map(|m| m.len()).unwrap_or(0);

        match loader::load_document(abs, &rel) {
            Err(reason) => {
                rows.push(Row {
                    rel,
                    bytes: meta_len,
                    outcome: format!("SKIP[{}]", reason.code()),
                    converter: String::new(),
                    source_kind: String::new(),
                    form: String::new(),
                    text_chars: 0,
                    headings: 0,
                    pages: 0,
                    chunks: 0,
                    heading_path_rate: 0.0,
                    page_covered: 0,
                    mojibake: 0.0,
                    extra: reason.message(),
                });
            }
            Ok(src) => {
                let chunks = chunk_document(&src, CHUNK_SIZE, CHUNK_OVERLAP, None);
                let with_path = chunks.iter().filter(|c| c.path_json.as_deref().is_some_and(|s| !s.is_empty())).count();
                let page_covered = chunks.iter().filter(|c| c.page_start.is_some()).count();
                let mk = mojibake_ratio(&src.text);

                // ── 断言 1：转换器 / 失效粒度键必须与注册表能力位一致 ──
                let kind = filekind::lookup(&rel).expect("is_indexable 已保证在册");
                match kind.converter {
                    filekind::Converter::AnyDoc => {
                        checked_office += 1;
                        assert_eq!(
                            src.converter, ConverterInfo::ANYDOC,
                            "{} 应记为 anydoc 转换器",
                            rel
                        );
                        assert_eq!(src.source_kind, "office", "{} 的 source_kind 应为 office", rel);
                    }
                    filekind::Converter::PdfInspector => {
                        checked_pdf += 1;
                        assert_eq!(
                            src.converter, ConverterInfo::PDF_INSPECTOR,
                            "{} 应记为 pdf-inspector",
                            rel
                        );
                        assert_eq!(src.source_kind, "pdf", "{} 的 source_kind 应为 pdf", rel);
                        // ── 断言 2：PDF 必须有页 provenance（Phase 1 的核心交付）──
                        assert!(
                            !src.page_spans.is_empty(),
                            "{} 抽取成功却没有 page_spans（页码 provenance 是 Phase 1 核心交付）",
                            rel
                        );
                        // 并且必须**真的流到 chunk 上**。下面的 `if let (Some,Some)` 边界断言
                        // 在"一个块都没带页码"时会**空过**，所以这里显式要求覆盖率非零——
                        // 这正是 §8.4「定位可用性：命中块给出正确页码」的判据。
                        assert!(
                            page_covered > 0,
                            "{} 有 {} 个页区间，却没有任何 chunk 带 page_start —— \
                             行→页映射没有送达分块器（变换守卫或注入链断了）",
                            rel,
                            src.page_spans.len()
                        );
                        // ── 断言 2b：行→页映射必须与正文真值逐页对齐 ──
                        // 只查"页码落在 1..N 内"是**自洽性**检查，系统性错位的映射照样通过。
                        // 这里用 `page_spans` 的字节区间反推每页真实行区间（不依赖游标算术），
                        // 再与 `line_page_map` 比对 —— 这是唯一能抓住行号漂移的独立判据。
                        for (span, ls) in src.page_spans.iter().zip(src.line_page_map.iter()) {
                            let nl = |b: usize| src.text[..b].matches('\n').count();
                            let want_start = nl(span.byte_start) + 1;
                            let want_end = nl(span.byte_end.min(src.text.len())) + 2;
                            assert_eq!(
                                (ls.line_start, ls.line_end),
                                (want_start, want_end),
                                "{} 第 {} 页行区间错位：实现 [{},{}) vs 正文真值 [{},{})",
                                rel,
                                span.page,
                                ls.line_start,
                                ls.line_end,
                                want_start,
                                want_end
                            );
                            let by_lines: Vec<&str> = src
                                .text
                                .lines()
                                .skip(ls.line_start.saturating_sub(1))
                                .take(ls.line_end.saturating_sub(ls.line_start))
                                .collect();
                            assert_eq!(
                                by_lines.join("\n"),
                                src.text[span.byte_start..span.byte_end],
                                "{} 第 {} 页行区间取出的文本与该页正文不符（行号漂移）",
                                rel,
                                span.page
                            );
                        }
                    }
                    _ => {}
                }

                // ── 断言 3：乱码阈值（第一层：转换保真度）──
                assert!(
                    mk < 0.01,
                    "{} 替换字符占比 {:.3}% ≥ 1% —— 疑似二进制被按文本读入",
                    rel,
                    mk * 100.0
                );

                // ── 断言 4：分块产物必须存在且页码自洽 ──
                if !src.text.trim().is_empty() {
                    assert!(!chunks.is_empty(), "{} 有正文却分不出任何 chunk", rel);
                }
                let page_count = src.page_spans.len() as u32;
                for c in &chunks {
                    if let (Some(s), Some(e)) = (c.page_start, c.page_end) {
                        assert!(s <= e, "{} chunk#{} page_start({}) > page_end({})", rel, c.chunk_index, s, e);
                        assert!(
                            e <= page_count,
                            "{} chunk#{} page_end({}) 超出总页数({})",
                            rel,
                            c.chunk_index,
                            e,
                            page_count
                        );
                        assert!(s >= 1, "{} chunk#{} 页码非 1-indexed: {}", rel, c.chunk_index, s);
                    }
                }
                total_chunks += chunks.len();

                let status = match &src.doc_status {
                    DocStatus::Indexed => "Indexed".to_string(),
                    DocStatus::PartiallyIndexed { skipped_pages } => {
                        format!("PartiallyIndexed({:?})", skipped_pages)
                    }
                };
                let mut extra = status;
                if !src.page_diagnostics.is_empty() {
                    extra.push_str(&format!(" diag={}", src.page_diagnostics.len()));
                }
                if !src.warnings.is_empty() {
                    extra.push_str(&format!(" warn={}", src.warnings.len()));
                }

                if dump > 0 {
                    let preview: String = src.text.chars().take(dump).collect();
                    println!(
                        "\n----- DUMP {} [{} / {} / {}] -----\n{}\n----- END DUMP -----\n",
                        rel,
                        ext,
                        src.source_kind,
                        src.converter.label(),
                        preview.replace('\n', "\n")
                    );
                }

                rows.push(Row {
                    rel,
                    bytes: meta_len,
                    outcome: "OK".to_string(),
                    converter: src.converter.label(),
                    source_kind: src.source_kind.to_string(),
                    form: src.form.as_str().to_string(),
                    text_chars: src.text.chars().count(),
                    headings: count_headings(&src.text),
                    pages: src.page_spans.len(),
                    chunks: chunks.len(),
                    heading_path_rate: if chunks.is_empty() {
                        0.0
                    } else {
                        with_path as f64 / chunks.len() as f64
                    },
                    page_covered,
                    mojibake: mk,
                    extra,
                });
            }
        }
    }

    // ── 报告 ──
    println!("\n================ Plan B v2 真实样本验收 ================");
    println!(
        "{:<34} {:>9} {:>18} {:>9} {:>7} {:>7} {:>5} {:>4} {:>6} {:>7} {:>6} {:>6} {}",
        "rel", "bytes", "converter", "source_kind", "form", "chars", "head", "pg", "chunk", "hpath%", "pcover", "mk%", "status"
    );
    for r in &rows {
        println!(
            "{:<34} {:>9} {:>18} {:>9} {:>7} {:>7} {:>5} {:>4} {:>6} {:>7.1} {:>6} {:>6.2} {}",
            r.rel,
            r.bytes,
            if r.converter.is_empty() { r.outcome.as_str() } else { r.converter.as_str() },
            r.source_kind,
            r.form,
            r.text_chars,
            r.headings,
            r.pages,
            r.chunks,
            r.heading_path_rate * 100.0,
            r.page_covered,
            r.mojibake * 100.0,
            r.extra
        );
    }
    let ok = rows.iter().filter(|r| r.outcome == "OK").count();
    let skipped = rows.len() - ok;
    println!(
        "------------------------------------------------------\n\
         样本 {} 个：成功 {} / 跳过 {}；累计 chunk {}；anydoc 路 {}；pdf-inspector 路 {}\n\
         ======================================================\n",
        rows.len(),
        ok,
        skipped,
        total_chunks,
        checked_office,
        checked_pdf
    );

    // ── 全局守卫：避免「全都跳过了」这种假通过 ──
    assert!(
        checked_office > 0 || checked_pdf > 0,
        "没有任何样本走通 anydoc / pdf-inspector 路径，验收无意义（检查样本目录内容）"
    );
    assert!(total_chunks > 0, "所有样本都没分出 chunk");
}

/// 转换缓存验收（§8.4「二次重建加速：转换缓存命中率 ≥90%」的机制验证）。
///
/// 这里验证**机制**（同内容第二次必命中、且还原结果与首次逐字节一致）；
/// 端到端命中率由真实重建场景度量，不在本测试硬编码。
#[test]
fn acceptance_conversion_cache_roundtrip() {
    let Some(dir) = accept_dir() else {
        eprintln!("[skip] 未设置 MDGO_ACCEPT_DIR —— 转换缓存验收未运行");
        return;
    };

    // 选一个「必须转换」的样本（pdf/office），直读类不参与缓存
    let mut files = Vec::new();
    collect_files(&dir, &mut files, 0);
    files.sort();
    let target = files.iter().find(|p| {
        let rel = rel_of(&dir, p);
        matches!(
            filekind::lookup(&rel).map(|k| k.converter),
            Some(filekind::Converter::PdfInspector) | Some(filekind::Converter::AnyDoc)
        )
    });
    let Some(target) = target else {
        eprintln!("[skip] 样本目录里没有需转换的文件（pdf/office），跳过缓存验收");
        return;
    };
    let rel = rel_of(&dir, target);

    let cache_dir = std::env::temp_dir().join("mdgo-accept-cache");
    let _ = std::fs::remove_dir_all(&cache_dir);
    let cache = ConversionCache::open(&cache_dir.to_string_lossy()).expect("打开转换缓存");

    let t0 = std::time::Instant::now();
    let first = cache.load_or_convert(target, &rel).expect("首次转换应成功");
    let d0 = t0.elapsed();
    let after_first = cache.len();

    let t1 = std::time::Instant::now();
    let second = cache.load_or_convert(target, &rel).expect("二次装载应成功");
    let d1 = t1.elapsed();
    let after_second = cache.len();

    println!(
        "\n[缓存验收] {} （{}）\n  首次（未命中→转换+写回）: {:?}，缓存条目 {}\n  二次（应命中缓存）      : {:?}，缓存条目 {}\n",
        rel,
        target.display(),
        d0,
        after_first,
        d1,
        after_second
    );

    assert_eq!(first.text, second.text, "缓存还原的正文与首次转换不一致（缓存不可信）");
    assert_eq!(
        first.source_kind, second.source_kind,
        "缓存还原的 source_kind 不一致"
    );
    assert_eq!(
        first.page_spans, second.page_spans,
        "缓存还原的 page_spans 不一致（页码 provenance 必须可缓存）"
    );
    assert_eq!(after_first, 1, "首次装载后缓存应有且仅有 1 条");
    assert_eq!(after_second, 1, "命中缓存不应新增条目（说明发生了重复转换）");
}
