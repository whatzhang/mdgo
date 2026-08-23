//! 证据 / 接地校验（C2，默认关）：轻量规则版。
//!
//! 从回答中提取"证据型断言"（含数字/专名/术语的句子），检查检索上下文中
//! 是否有足够证据。零 LLM 成本、确定性、可测试——适合本地单机。
//!
//! 定位：普通查询默认不开启（不增加延迟）；开启时在回答尾部标注无证据断言，
//! 供用户判断可信度。LLM 深度校验（Claim Extraction → Verify）留作后续增强。

/// 校验回答的接地性，返回未获得足够证据支撑的断言列表。
///
/// 规则：
/// - 候选断言 = 按句子边界切分、含数字或 ≥2 个拉丁词（专名/术语/代码符号）的句子；
/// - 证据判定 = 断言中的关键 token（≥4 字符的词/数字）在检索上下文中的命中比例
///   ≥ `min_hit_ratio` 视为有证据；
/// - 返回未通过判定的断言（上限 `max_claims`，避免刷屏）。
pub fn verify_grounding(
    answer: &str,
    context: &str,
    min_hit_ratio: f64,
    min_hits: usize,
    max_claims: usize,
) -> Vec<String> {
    if answer.trim().is_empty() || context.trim().is_empty() {
        return Vec::new();
    }
    let context_lower = context.to_lowercase();
    let mut unsupported = Vec::new();

    for sentence in split_claim_sentences(answer) {
        if unsupported.len() >= max_claims {
            break;
        }
        let s = sentence.trim();
        if s.len() < 8 || s.len() > 200 {
            continue;
        }
        // 仅检查"证据型"断言：含数字，或含 ≥2 个 ≥3 字符的拉丁词（专名/术语）
        let has_digit = s.chars().any(|c| c.is_ascii_digit());
        let latin_words = s
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| w.len() >= 3)
            .count();
        if !has_digit && latin_words < 2 {
            continue;
        }
        // 关键 token：≥4 字符的拉丁词/数字（含下划线，覆盖代码符号）；
        // 🟠 M15 修复：CJK 段按 2-gram 切分并小写——旧实现把整句中文当一个
        // 字节长度 token（6 字节/2 字），与上下文做整串子串匹配，中文改写断言
        // （同义表述）0 命中即被误标"无证据"。
        let mut tokens: Vec<String> = Vec::new();
        for seg in s.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if seg.is_empty() {
                continue;
            }
            let has_cjk = seg.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
            if has_cjk {
                let chars: Vec<char> = seg.chars().collect();
                if chars.len() >= 2 {
                    for w in chars.windows(2) {
                        tokens.push(w.iter().collect::<String>().to_lowercase());
                    }
                } else {
                    tokens.push(seg.to_lowercase());
                }
            } else if seg.chars().count() >= 4 {
                tokens.push(seg.to_lowercase());
            }
        }
        if tokens.is_empty() {
            continue;
        }
        let hits = tokens
            .iter()
            .filter(|t| context_lower.contains(t.as_str()))
            .count();
        if hits < min_hits && (hits as f64 / tokens.len() as f64) < min_hit_ratio {
            unsupported.push(s.to_string());
        }
    }
    unsupported
}

/// 按句子边界切分（。！？\n；英文 . ! ?）。
///
/// 🟠 L21 修复：英文 `.` 仅当后随空白或结尾时才视为句界——`3.14`、`v2.0`、
/// `Redis 6.2.0`、`example.com` 中的句点不切分，数字/版本/URL 断言不再被拆散。
fn split_claim_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        current.push(c);
        let is_boundary = match c {
            '。' | '！' | '？' | '\n' | '!' | '?' => true,
            '.' => {
                // 句点：后随空白/结尾才算句界（排除 3.14 / v2.0 / example.com）
                chars.get(i + 1).map_or(true, |n| n.is_whitespace())
            }
            _ => false,
        };
        if is_boundary {
            let t = current.trim();
            if !t.is_empty() {
                sentences.push(t.to_string());
            }
            current.clear();
        }
        i += 1;
    }
    let t = current.trim();
    if !t.is_empty() {
        sentences.push(t.to_string());
    }
    sentences
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_claim_passes() {
        let answer = "Redis 连接池默认超时是 60 秒。";
        let context = "Redis 连接池默认超时时间为 60 秒。";
        let claims = verify_grounding(answer, context, 0.5, 1, 5);
        assert!(claims.is_empty(), "有证据的断言不应标记: {:?}", claims);
    }

    #[test]
    fn unsupported_claim_flagged() {
        let answer = "该系统的延迟是 42 毫秒。";
        let context = "我们讨论了分词器与 BM25 的关系。";
        let claims = verify_grounding(answer, context, 0.5, 1, 5);
        assert_eq!(claims.len(), 1, "无证据断言应被标记");
        assert!(claims[0].contains("42"));
    }

    #[test]
    fn short_or_vague_sentences_skipped() {
        let answer = "你好。这很重要。Redis 的持久化有 RDB 和 AOF 两种方式。";
        let context = "完全无关的内容。";
        let claims = verify_grounding(answer, context, 0.5, 1, 5);
        // "你好"/"这很重要" 非证据型跳过；"Redis 的持久化有 RDB 和 AOF 两种方式"
        // 拉丁词 ≥2 → 被检查 → 无证据 → 标记
        assert_eq!(claims.len(), 1, "{:?}", claims);
        assert!(claims[0].contains("Redis"));
    }

    #[test]
    fn empty_inputs_noop() {
        assert!(verify_grounding("", "ctx", 0.5, 1, 5).is_empty());
        assert!(verify_grounding("回答", "", 0.5, 1, 5).is_empty());
    }

    /// 🟠 M15 回归：中文改写断言（同义表述）不得被误标"无证据"——
    /// CJK 2-gram 使 "默认超时是" 能命中上下文中的 "默认超时时间"。
    #[test]
    fn cjk_paraphrase_not_flagged() {
        let answer = "该系统的连接池默认超时是 60 秒。";
        let context = "该系统的连接池默认超时时间为 60 秒，超时后自动重连。";
        let claims = verify_grounding(answer, context, 0.5, 1, 5);
        assert!(claims.is_empty(), "同义改写断言不应被标记: {:?}", claims);
    }

    /// 🟠 L21 回归：句点不得拆散数字/版本——3.14 保持单句，命中上下文不误标。
    #[test]
    fn decimal_and_version_not_split() {
        let answer = "延迟 3.14 毫秒，Redis 6.2.0 支持该特性。";
        let context = "延迟 3.14 毫秒，Redis 6.2.0 支持该特性。";
        let claims = verify_grounding(answer, context, 0.5, 1, 5);
        assert!(claims.is_empty(), "数字/版本断言不应被拆散误标: {:?}", claims);
    }

    /// 🟠 L21：句点后随空白仍按句界切分（英文句子保持可切）。
    #[test]
    fn english_sentence_still_splits_on_period_space() {
        let answer = "First claim is true. Second claim is false.";
        let context = "First claim is true. Second claim is false.";
        let claims = verify_grounding(answer, context, 0.5, 1, 5);
        assert!(claims.is_empty(), "有证据的英文断言不应被标记: {:?}", claims);
    }
}
