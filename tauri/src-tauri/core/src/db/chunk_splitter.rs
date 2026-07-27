use std::collections::HashMap;

use regex::Regex;

use super::utils;

// ─── ChunkSplitter 特质（策略模式） ───

/// ChunkSplitter 特质：定义文本分割的统一接口。
///
/// 每种文件类型（Markdown、纯文本等）实现该特质，提供不同分割策略。
pub trait ChunkSplitter: Send + Sync {
    /// 将输入文本分割为若干文本块
    fn split(&self, text: &str, max_size: usize, overlap: usize) -> Vec<String>;
}

// ─── 纯文本文档分割器 ───

/// 纯文本文档分割器
///
/// 按句子边界（。！？等）切分，适合代码、配置、普通文本等文件。
pub struct PlainTextChunkSplitter;

impl ChunkSplitter for PlainTextChunkSplitter {
    fn split(&self, text: &str, max_size: usize, overlap: usize) -> Vec<String> {
        utils::split_text(text, max_size, overlap)
    }
}

// ─── Markdown 文档分割器 ───

/// Markdown 文档分割器
///
/// 按标题层级（# ~ ######）划分段落，每个 chunk 注入父级标题路径作为前缀。
/// 超过 max_size 的长段落回退到 split_text 切分。
pub struct MarkdownChunkSplitter;

impl MarkdownChunkSplitter {
    /// 标题行匹配正则
    fn heading_re() -> &'static Regex {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.+)").unwrap())
    }
}

impl ChunkSplitter for MarkdownChunkSplitter {
    fn split(&self, text: &str, max_size: usize, overlap: usize) -> Vec<String> {
        // 快速判断是否包含 Markdown 标题语法；不含时降级为纯文本分割
        if !text.contains('#') {
            return utils::split_text(text, max_size, overlap);
        }

        let heading_re = Self::heading_re();
        let lines: Vec<&str> = text.lines().collect();
        let mut result: Vec<String> = Vec::new();

        // 标题栈：维护当前所在的标题层级链
        struct Heading {
            level: usize,
            text: String,
        }
        let mut stack: Vec<Heading> = Vec::new();
        let mut section_start = 0usize;

        let push_section = |result: &mut Vec<String>,
                            stack: &[Heading],
                            lines: &[&str],
                            start: usize,
                            end: usize,
                            max_size: usize,
                            overlap: usize| {
            if end <= start {
                return;
            }
            // 构建标题前缀
            let prefix: String = stack
                .iter()
                .map(|h| {
                    let tag = "#".repeat(h.level);
                    format!("{} {}\n", tag, h.text)
                })
                .collect();
            let body = lines[start..end].join("\n");
            let combined = format!("{}{}", prefix, body);

            if combined.len() <= max_size * 3 / 2 {
                result.push(combined);
            } else {
                // 长段落：用前缀 + split_text 再切分
                let body_chunks = utils::split_text(&body, max_size - prefix.len().max(50), overlap);
                for chunk in body_chunks {
                    result.push(format!("{}{}", prefix, chunk));
                }
            }
        };

        for (i, line) in lines.iter().enumerate() {
            if let Some(caps) = heading_re.captures(line) {
                let level = caps.get(1).unwrap().as_str().len();
                let heading_text = caps.get(2).unwrap().as_str().trim().to_string();

                // 遇到新标题时，关闭上一段落
                if !stack.is_empty() {
                    push_section(
                        &mut result,
                        &stack,
                        &lines,
                        section_start,
                        i,
                        max_size,
                        overlap,
                    );
                }

                // 弹出级别 ≥ 当前标题的栈顶元素
                while let Some(top) = stack.last() {
                    if top.level >= level {
                        stack.pop();
                    } else {
                        break;
                    }
                }

                section_start = i + 1;
                stack.push(Heading {
                    level,
                    text: heading_text,
                });
            }
        }

        // 处理最后一段
        if !stack.is_empty() {
            push_section(
                &mut result,
                &stack,
                &lines,
                section_start,
                lines.len(),
                max_size,
                overlap,
            );
        }

        // 如果没有识别到任何标题结构，回退到 split_text
        if result.is_empty() {
            return utils::split_text(text, max_size, overlap);
        }

        result
    }
}

// ─── ChunkSplitterFactory（工厂模式） ───

/// 文件扩展名到分割器的映射注册表
type ExtensionMap = HashMap<&'static str, &'static (dyn ChunkSplitter + Sync)>;

/// ChunkSplitter 工厂
///
/// 根据文件扩展名返回对应的 ChunkSplitter 实现。
/// 支持运行时注册新的扩展名-分割器对，遵循开闭原则。 
pub struct ChunkSplitterFactory {
    /// 精确扩展名匹配
    exact: ExtensionMap,
    /// 后缀匹配（如 .md 匹配 .mdx）
    suffix: Vec<(&'static str, &'static (dyn ChunkSplitter + Sync))>,
}

impl ChunkSplitterFactory {
    /// 创建默认工厂，注册所有内置分割器
    pub fn new() -> Self {
        let mut factory = Self {
            exact: HashMap::new(),
            suffix: Vec::new(),
        };

        // 注册内置分割器
        let md: &'static (dyn ChunkSplitter + Sync) = &MarkdownChunkSplitter;
        let text: &'static (dyn ChunkSplitter + Sync) = &PlainTextChunkSplitter;

        // Markdown 类型
        factory.exact.insert("md", md);
        factory.exact.insert("mdx", md);

        // 后缀匹配：.md 也匹配 .mdx
        factory.suffix.push(("md", md));

        // 纯文本类型（默认）
        // 代码、配置文件等所有非 Markdown 类型都使用纯文本分割
        for ext in utils::KB_SUPPORTED_EXTS {
            if ext != &"md" && ext != &"mdx" {
                factory.exact.insert(ext, text);
            }
        }

        factory
    }

    /// 根据文件扩展名获取对应的分割器
    ///
    /// 匹配规则：
    /// 1. 精确匹配 `exact` 表
    /// 2. 后缀匹配 `suffix` 表（如 "md" 匹配 "mdx"）
    /// 3. 都不匹配则返回纯文本分割器
    pub fn get_splitter(&self, extension: &str) -> &'static (dyn ChunkSplitter + Sync) {
        let ext_lower = extension.to_lowercase();

        // 精确匹配
        if let Some(splitter) = self.exact.get(ext_lower.as_str()) {
            return *splitter;
        }

        // 后缀匹配
        for (suffix, splitter) in &self.suffix {
            if ext_lower.ends_with(suffix) {
                return *splitter;
            }
        }

        // 默认：纯文本分割器
        &PLAIN_TEXT_SPLITTER
    }

    /// 注册自定义扩展名到分割器的映射（对外扩展入口）
    pub fn register(
        &mut self,
        extension: &'static str,
        splitter: &'static (dyn ChunkSplitter + Sync),
    ) {
        self.exact.insert(extension, splitter);
    }
}

impl Default for ChunkSplitterFactory {
    fn default() -> Self {
        Self::new()
    }
}

// 确保 PlainTextChunkSplitter 可作静态变量
static PLAIN_TEXT_SPLITTER: PlainTextChunkSplitter = PlainTextChunkSplitter;
