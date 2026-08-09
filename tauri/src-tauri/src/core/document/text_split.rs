//! 纯文本切分工具（与文件类型无关）。
//!
//! 位于 `document` 层（基础层），供 `document::chunk_engine` 与 `db::utils` 共用，
//! 避免 `document` 反向依赖 `db`（分层颠倒，A3 修复）。

/// 统一字符计数：替代字节长度 `str::len()`，避免中英文混用导致分块大小不一致
/// （1 个中文字符 = 3 字节，字节计数会把中文 chunk 切到实际大小的 1/3）。
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// 通用文本分隔符（按优先级从高到低）
pub const GENERIC_TEXT_SEPARATORS: &[&str] = &[
    "\n\n", "\n", ". ", "。", "！", "？", "，", " ",
];

/// 按字符数（而非字节数）切分文本，中英文场景更一致。
///
/// 使用 `GENERIC_TEXT_SEPARATORS` 作为分隔符优先级列表。
/// 预先计算所有字符的字节偏移，避免重复遍历。
pub fn split_text(text: &str, max_size: usize, overlap: usize) -> Vec<String> {
    split_text_with_separators(text, max_size, overlap, GENERIC_TEXT_SEPARATORS)
}

/// 使用自定义分隔符优先级列表进行文本切分。
///
/// 按优先级从高到低尝试在每个窗口内寻找分隔符位置切分，
/// 保证块内语义完整性。max_size 为单块最大字符数，
/// overlap 为前后块重叠字符数。
pub fn split_text_with_separators(
    text: &str,
    max_size: usize,
    overlap: usize,
    separators: &[&str],
) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();

    if total <= max_size {
        return vec![text.to_string()];
    }

    // 预计算每个字符位置的字节偏移（一次性 O(n)，避免循环内重复计算）
    let byte_offsets: Vec<usize> = std::iter::once(0)
        .chain(text.char_indices().skip(1).map(|(i, _)| i))
        .chain(std::iter::once(text.len()))
        .collect();

    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < total {
        let mut end = (start + max_size).min(total);

        if end < total {
            let mut best_sep_pos = start;
            // 使用预计算的字节偏移取窗口子串
            let window_start_byte = byte_offsets[start];
            let window_end_byte = byte_offsets[end];
            let window = &text[window_start_byte..window_end_byte];

            for sep in separators {
                if let Some(rel_byte) = window.rfind(sep) {
                    let sep_end_byte = rel_byte + sep.len();
                    let sep_char_count = window[..sep_end_byte].chars().count();
                    let candidate = start + sep_char_count;
                    if candidate > best_sep_pos
                        && candidate - start < (max_size as f64 * 1.5) as usize
                    {
                        best_sep_pos = candidate;
                    }
                }
            }
            if best_sep_pos > start {
                end = best_sep_pos;
            }
        }

        let start_byte = byte_offsets[start];
        let end_byte = byte_offsets[end];
        chunks.push(text[start_byte..end_byte].to_string());

        let next_start = if end > overlap { end - overlap } else { end };
        // 防止无限循环：确保每次迭代都有进展
        if next_start <= start {
            start = end;
        } else {
            start = next_start;
        }
    }

    chunks.retain(|c| !c.trim().is_empty());
    chunks
}
