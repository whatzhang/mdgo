//! 纯文本切分工具（与文件类型无关）。
//!
//! 位于 `document` 层（基础层），供 `document::chunk_engine` 与 `db::utils` 共用，
//! 避免 `document` 反向依赖 `db`（分层颠倒，A3 修复）。
//!
//! P0-2：除字符切分（`split_text_with_separators`）外，新增 token 感知切分
//! （[`split_text_token_aware`]）——一次 tokenize + 按 token 预算定位切分点，
//! 消除"字符预算 × 中英文 token 密度差异"导致的资源浪费/超限。

use crate::core::document::token_budget::TokenCounter;

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

    // 🟠 L1：max_size == 0 直接整体返回——否则空窗口 `end == start` 导致
    // `next_start <= start → start = end` 原地踏步死循环（生产调用方均保证 ≥16，
    // 但这是公共 API，须自守卫）。
    if max_size == 0 {
        return vec![text.to_string()];
    }
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
                    // 🟠 L2：移除死条件 `candidate - start < max_size*1.5`——
                    // rfind 只在窗口内找分隔符，candidate ≤ start+max_size 恒成立，
                    // 该条件永远为真（评审遗留）。
                    if candidate > best_sep_pos {
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

/// Token 感知切分（P0-2）：一次 tokenize + 按 token 预算定位切分点。
///
/// 与 [`split_text_with_separators`] 的区别：
/// - 窗口大小按 **token 预算**（`max_tokens`）而非字符数；每个窗口在
///   [起始, 起始+max_tokens) 的**字符范围**内寻找最高优先级分隔符；
/// - overlap 以 token 计（`overlap_tokens`）：下一窗口起点 = 切点 token − overlap。
///
/// 返回 `None` 表示 counter 不可用（调用方降级字符切分）。
pub fn split_text_token_aware(
    text: &str,
    max_tokens: usize,
    overlap_tokens: usize,
    separators: &[&str],
    counter: &dyn TokenCounter,
) -> Option<Vec<String>> {
    let boundaries = counter.token_char_boundaries(text)?;
    if boundaries.len() <= 1 || max_tokens == 0 {
        return Some(vec![text.to_string()]);
    }
    let total_tokens = boundaries.len() - 1;
    if total_tokens <= max_tokens {
        return Some(vec![text.to_string()]);
    }

    let total_chars = text.chars().count();
    // 字符偏移 → 字节偏移（切分点定位用）
    let mut byte_of_char = vec![0usize; total_chars + 1];
    for (ci, (b, _)) in text.char_indices().enumerate() {
        byte_of_char[ci] = b;
    }
    byte_of_char[total_chars] = text.len();
    // 字节偏移 → 字符偏移（分隔符定位用）。
    // 🟠 L3：稀疏表（只记录字符边界，二分查找）——旧实现按**字节**建稠密数组
    // （8×字节数），10MB 中文文件峰值 ~160MB+；稀疏表为 16×字符数，内存减半以上。
    let mut char_of_byte: Vec<(usize, usize)> = Vec::with_capacity(total_chars + 1);
    for (ci, (b, _)) in text.char_indices().enumerate() {
        char_of_byte.push((b, ci));
    }
    char_of_byte.push((text.len(), total_chars));

    // 字符偏移 → token 下标（二分；boundaries 单调不减）
    let token_of_char = |char_pos: usize| -> usize {
        match boundaries.binary_search(&char_pos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    };

    let mut chunks: Vec<String> = Vec::new();
    let mut start_token = 0usize;
    while start_token < total_tokens {
        let end_token = (start_token + max_tokens).min(total_tokens);
        let window_end_char = boundaries[end_token];
        let mut cut_char = window_end_char;

        if end_token < total_tokens {
            let start_char = boundaries[start_token];
            let window_byte_start = byte_of_char[start_char];
            let window_byte_end = byte_of_char[window_end_char];
            // 防御：真实 tokenizer 的边界可能含零宽/重复值（[CLS]/[SEP] 等 special token），
            // 极端组合下 window 可能为空或反向——此时放弃分隔符搜索，整窗切分
            // （窗口本身仍是合法文本区间，只是切点不优先落在分隔符上）。
            if window_byte_start < window_byte_end {
                let window = &text[window_byte_start..window_byte_end];
                let mut best_char = start_char;
                for sep in separators {
                    if let Some(rel_byte) = window.rfind(sep) {
                        let cand_byte = window_byte_start + rel_byte + sep.len();
                        // 🟠 L3：稀疏表二分查找（分隔符为 ASCII，cand_byte 落在字符边界）
                        let cand_char = match char_of_byte
                            .binary_search_by_key(&cand_byte, |&(b, _)| b)
                        {
                            Ok(i) => char_of_byte[i].1,
                            Err(i) => char_of_byte[i.saturating_sub(1)].1,
                        };
                        if cand_char > best_char && cand_char < window_end_char {
                            best_char = cand_char;
                        }
                    }
                }
                if best_char > start_char {
                    cut_char = best_char;
                }
            }
        }

        let chunk_start_byte = byte_of_char[boundaries[start_token]];
        let cut_byte = byte_of_char[cut_char];
        if cut_char > boundaries[start_token] {
            chunks.push(text[chunk_start_byte..cut_byte].to_string());
        }

        // 下一窗口起点 = 切点 token − overlap（防原地踏步：不前进则跳到窗口尾）
        let cut_token = token_of_char(cut_char);
        let mut next_token = cut_token.saturating_sub(overlap_tokens);
        if next_token <= start_token {
            next_token = end_token;
        }
        start_token = next_token.min(total_tokens);
    }

    chunks.retain(|c| !c.trim().is_empty());
    Some(chunks)
}

// ─── P0-3 测试：token 感知切分不变量 ───

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::document::token_budget::test_util::FixedRateCounter;

    #[test]
    fn token_aware_respects_budget_no_overlap() {
        let counter = FixedRateCounter { chars_per_token: 1 };
        let text = "甲。".repeat(200); // 400 字符 = 400 token
        let pieces = split_text_token_aware(&text, 30, 0, GENERIC_TEXT_SEPARATORS, &counter)
            .unwrap();
        assert!(pieces.len() >= 2);
        for p in &pieces {
            assert!(p.chars().count() <= 30, "分片超预算: {}", p.chars().count());
        }
        // 无 overlap → 拼接应完整恢复原文
        assert_eq!(pieces.concat(), text, "无 overlap 时拼接应等于原文");
    }

    #[test]
    fn token_aware_unicode_no_panic_no_split() {
        let counter = FixedRateCounter { chars_per_token: 1 };
        let text = "😀🎉中文混合émoji🙏🏽 测试。".repeat(50);
        let pieces = split_text_token_aware(&text, 20, 0, GENERIC_TEXT_SEPARATORS, &counter)
            .unwrap();
        assert!(pieces.iter().all(|p| !p.is_empty()));
        // 拼接恢复原文（不得在字符中间切断，不得丢内容）
        assert_eq!(pieces.concat(), text, "Unicode 文本拼接应恢复原文");
    }

    #[test]
    fn token_aware_overlap_shares_tail() {
        let counter = FixedRateCounter { chars_per_token: 1 };
        let text = "句子内容反复出现测试。".repeat(80);
        let pieces = split_text_token_aware(&text, 30, 8, GENERIC_TEXT_SEPARATORS, &counter)
            .unwrap();
        assert!(pieces.len() >= 2);
        let t0: String = pieces[0]
            .chars()
            .rev()
            .take(8)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(pieces[1].contains(&t0), "后块应含前块尾部（overlap）");
    }

    #[test]
    fn token_aware_small_input_single_chunk() {
        let counter = FixedRateCounter { chars_per_token: 1 };
        let pieces = split_text_token_aware("短文本", 100, 0, GENERIC_TEXT_SEPARATORS, &counter)
            .unwrap();
        assert_eq!(pieces, vec!["短文本".to_string()]);
        // 空输入
        let empty = split_text_token_aware("", 100, 0, GENERIC_TEXT_SEPARATORS, &counter).unwrap();
        assert_eq!(empty, vec!["".to_string()]);
    }

    #[test]
    fn token_aware_fallback_none_without_boundaries() {
        // 不实现 boundaries 的 counter → 返回 None（调用方降级字符切分）
        struct NoBoundaries;
        impl TokenCounter for NoBoundaries {
            fn count(&self, _t: &str) -> Option<usize> {
                Some(1)
            }
            fn token_char_boundaries(&self, _t: &str) -> Option<Vec<usize>> {
                None
            }
        }
        let r = split_text_token_aware("内容", 10, 0, GENERIC_TEXT_SEPARATORS, &NoBoundaries);
        assert!(r.is_none());
    }

    /// 真实 tokenizer 的边界含零宽/重复值（[CLS] 开头重复 0、[SEP] 尾部重复 n）——
    /// 不得 panic、不得丢内容（回归：benchmark 全量索引时 text_split.rs 切片越界）
    #[test]
    fn token_aware_handles_duplicate_zero_width_boundaries() {
        struct DupCounter;
        impl TokenCounter for DupCounter {
            fn count(&self, text: &str) -> Option<usize> {
                Some(text.chars().count() + 2)
            }
            fn token_char_boundaries(&self, text: &str) -> Option<Vec<usize>> {
                let n = text.chars().count();
                let mut b = vec![0usize]; // [CLS]
                b.push(0usize); // 零宽重复
                for i in 1..=n {
                    b.push(i);
                }
                b.push(n); // [SEP]
                b.push(n); // 零宽重复
                Some(b)
            }
        }
        let text = "句子内容反复出现测试。".repeat(60);
        let pieces =
            split_text_token_aware(&text, 24, 6, GENERIC_TEXT_SEPARATORS, &DupCounter).unwrap();
        assert!(!pieces.is_empty());
        // 内容覆盖（含 overlap 尾部，允许重复；不允许缺失）
        let merged = pieces.concat();
        assert!(merged.contains("句子内容反复出现测试"), "内容不应丢失");
        assert!(pieces.iter().all(|p| !p.is_empty() || p.trim().is_empty()));
    }
}
