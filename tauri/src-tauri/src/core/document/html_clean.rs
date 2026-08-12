//! Markdown 内嵌 HTML 清洗（v2：Mark 标注/备注入库前剥离）。
//!
//! 在索引管线解析前调用：剥离用户自定义标注/备注生成的 HTML 标签
//! （`<mark style title>`、Obsidian `==text==` 转换的 `<span class="ob-highlight">`、
//! 历史内联样式 span 等），**仅保留标签内部文本**，避免标签污染向量文本与
//! LLM 检索上下文。代码块与行内代码内的 HTML 文本先占位保护，不误伤。
//!
//! 纯函数 + 正则实现（决策确认），前端渲染不做清洗。

use regex::Regex;
use std::sync::OnceLock;

/// 围栏代码块 / 行内代码占位前缀。
const CODE_PLACEHOLDER_PREFIX: &str = "\u{0}MDGO_CODE_";

/// 是否 Markdown 类文件（与 pipeline::chunk_document 扩展名判断一致）。
pub fn is_markdown_ext(ext: &str) -> bool {
    matches!(ext, "md" | "markdown" | "mdown" | "rst")
}

/// 剥离 Markdown 中自定义 HTML 标注/备注标签，保留标签内部文本。
///
/// 处理顺序：1) 保护代码块与行内代码 → 2) 特定标签剥离（mark / ob-highlight / 内联 span）
/// → 3) 通用标签对剥离 → 4) `<br>` 换行语义保留 → 5) 孤立标签/注释清理 → 6) 恢复代码块。
pub fn strip_custom_html_tags(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut protected: Vec<String> = Vec::new();

    // 1) 保护代码块（围栏）与行内代码，避免误伤示例中的 HTML 文本
    let step1 = CODE_BLOCK_RE().replace_all(input, |caps: &regex::Captures| {
        protected.push(caps[0].to_string());
        format!("{}{}", CODE_PLACEHOLDER_PREFIX, protected.len() - 1)
    });

    // 2) 特定自定义标签：整标签去除、仅保留内部文本（含属性，如 style / title）
    let step2 = MARK_TAG_RE().replace_all(&step1, "$1");
    let step2 = OBS_HIGHLIGHT_RE().replace_all(&step2, "$1");
    let step2 = INLINE_STYLE_SPAN_RE().replace_all(&step2, "$1");

    // 3) <br> 保留换行语义
    let step3 = BR_RE().replace_all(&step2, "\n");

    // 4) 通用标签剥离（开/闭/自闭合逐个删除，保留标签间文本）+ 注释清理
    //    注意：regex crate 不支持反向引用，故不配对匹配，逐个删除标签本身即可
    let step4 = COMMENT_RE().replace_all(&step3, "");
    let step4 = TAG_STRIP_RE().replace_all(&step4, "");

    // 6) 恢复代码块/行内代码
    restore_protected(&step4, &protected)
}

/// 恢复占位符为原始代码块内容。
///
/// 注意：占位符前缀相同（`MDGO_CODE_1` 是 `MDGO_CODE_10` 的前缀），
/// 必须**按编号降序**替换，否则 i=1 的 replace 会破坏第 10 个及以后的占位符。
fn restore_protected(input: &str, protected: &[String]) -> String {
    let mut out = input.to_string();
    for (i, code) in protected.iter().enumerate().rev() {
        out = out.replace(&format!("{}{}", CODE_PLACEHOLDER_PREFIX, i), code);
    }
    out
}

#[allow(non_snake_case)]
fn CODE_BLOCK_RE() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```[\s\S]*?```|`[^`\n]+`").unwrap())
}

/// `<mark ...>text</mark>`：标注/备注统一标签（前端 L38673 生成，style 存颜色、title 存备注）。
#[allow(non_snake_case)]
fn MARK_TAG_RE() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<mark\b[^>]*>(.*?)</mark>").unwrap())
}

/// Obsidian `==text==` 高亮转换的 span（parseObsidianToHTML L28499）。
#[allow(non_snake_case)]
fn OBS_HIGHLIGHT_RE() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<span\b[^>]*class="[^"]*ob-highlight[^"]*"[^>]*>(.*?)</span>"#).unwrap()
    })
}

/// 历史版本内联样式 span 标注（旧数据兼容）。
#[allow(non_snake_case)]
fn INLINE_STYLE_SPAN_RE() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?is)<span\b[^>]*style="[^"]*"[^>]*>(.*?)</span>"#).unwrap()
    })
}

/// 通用标签剥离：开/闭/自闭合标签逐个删除（regex 不支持反向引用，不配对匹配）。
/// 顺序上先删除特定标签（mark/span，保留内部文本），此规则负责清理剩余通用标签。
#[allow(non_snake_case)]
fn TAG_STRIP_RE() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)</?[a-z][a-z0-9]*\b[^>]*>").unwrap())
}

/// `<br>` / `<br/>` → 换行。
#[allow(non_snake_case)]
fn BR_RE() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<br\s*/?>").unwrap())
}

/// HTML 注释。
#[allow(non_snake_case)]
fn COMMENT_RE() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<!--.*?-->").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_mark_with_attributes() {
        let input = r#"重点 <mark style="color:#dc2626;background-color:#fef08a;">划分</mark> 完成"#;
        assert_eq!(strip_custom_html_tags(input), "重点 划分 完成");
    }

    #[test]
    fn strips_mark_with_title_comment() {
        let input = r#"<mark style="color:#dc2626;" title="备注内容">认知</mark> 结束"#;
        assert_eq!(strip_custom_html_tags(input), "认知 结束");
    }

    #[test]
    fn strips_ob_highlight_span() {
        let input = r#"这是 <span class="ob-highlight">高亮</span> 文本"#;
        assert_eq!(strip_custom_html_tags(input), "这是 高亮 文本");
    }

    #[test]
    fn strips_inline_style_span_legacy() {
        let input = r#"旧 <span style="color:#dc2626;">标注</span> 数据"#;
        assert_eq!(strip_custom_html_tags(input), "旧 标注 数据");
    }

    #[test]
    fn keeps_code_block_html_untouched() {
        let input = "示例：\n```html\n<mark style=\"color:red;\">keep</mark>\n```\n完";
        let out = strip_custom_html_tags(input);
        assert!(out.contains("<mark style=\"color:red;\">keep</mark>"), "代码块被误伤: {}", out);
        assert!(out.contains("```html"));
    }

    #[test]
    fn keeps_inline_code_untouched() {
        let input = r#"用 `<mark>x</mark>` 表示标记"#;
        let out = strip_custom_html_tags(input);
        assert!(out.contains("<mark>x</mark>"), "行内代码被误伤: {}", out);
    }

    #[test]
    fn strips_generic_pairs_and_lone_tags() {
        let input = r#"<div class="x">段落</div> 与 <img src="a.png"> 与 <br/> 换行<!-- 注释 -->"#;
        let out = strip_custom_html_tags(input);
        assert_eq!(out, "段落 与  与 \n 换行");
    }

    #[test]
    fn nested_mark_keeps_inner_text() {
        let input = r#"外层<mark style="color:red;">内层<b>加粗</b>文字</mark>结尾"#;
        let out = strip_custom_html_tags(input);
        // mark 剥离后剩 <b>加粗</b>，再被通用规则剥离
        assert_eq!(out, "外层内层加粗文字结尾");
    }

    #[test]
    fn empty_input() {
        assert_eq!(strip_custom_html_tags(""), "");
    }

    #[test]
    fn many_code_blocks_restore_after_strip() {
        // 回归：占位符前缀冲突（MDGO_CODE_1 是 MDGO_CODE_10 的前缀），
        // ≥10 个代码块时第 10 个及以后必须完整还原
        let mut input = String::new();
        for n in 0..12 {
            input.push_str(&format!("块{n}：\n```html\n<mark>code{n}</mark>\n```\n"));
        }
        let out = strip_custom_html_tags(&input);
        for n in 0..12 {
            assert!(
                out.contains(&format!("<mark>code{n}</mark>")),
                "第 {} 个代码块未完整还原: {}",
                n,
                out
            );
        }
    }

    #[test]
    fn is_markdown_ext_cases() {
        assert!(is_markdown_ext("md"));
        assert!(is_markdown_ext("markdown"));
        assert!(!is_markdown_ext("html"));
        assert!(!is_markdown_ext("txt"));
    }
}

