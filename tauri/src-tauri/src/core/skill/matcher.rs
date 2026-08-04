//! Skill 意图匹配模块：分层匹配算法（L1 关键词 / L2 语义 / L3 兜底）
//!
//! 匹配优先级：L1 > L2 > L3，命中即返回，不再继续下层匹配。

use crate::core::skill::Skill;

/// 匹配层级
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchLevel {
    /// L1: 关键词精准匹配
    #[serde(rename = "L1")]
    L1,
    /// L2: 语义相似度匹配
    #[serde(rename = "L2")]
    L2,
    /// L3: 兜底模糊匹配
    #[serde(rename = "L3")]
    L3,
    /// 会话显式挂载
    #[serde(rename = "attached")]
    Attached,
    /// 手动指定（/技能名）
    #[serde(rename = "manual")]
    Manual,
}

impl MatchLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchLevel::L1 => "L1",
            MatchLevel::L2 => "L2",
            MatchLevel::L3 => "L3",
            MatchLevel::Attached => "attached",
            MatchLevel::Manual => "manual",
        }
    }
}

/// 匹配结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchResult {
    pub skill: Skill,
    pub level: MatchLevel,
    pub score: f32,
}

/// L1: 关键词精准匹配
///
/// 用户消息对 `trigger_rules.keywords` 做词边界匹配（大小写不敏感）。
/// 命中即入选，得分最高（1.0）。
pub fn match_l1_keywords(query: &str, skills: &[Skill]) -> Vec<MatchResult> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    for skill in skills {
        if !skill.enabled {
            continue;
        }

        let keywords = &skill.trigger_rules.keywords;
        if keywords.is_empty() {
            continue;
        }

        // 检查是否命中任一关键词（词边界匹配，避免子串误判）
        let matched = keywords.iter().any(|kw| {
            let kw_lower = kw.to_lowercase();
            // 使用词边界匹配：关键词前后必须是非字母数字字符或字符串边界
            if let Some(pos) = query_lower.find(&kw_lower) {
                let before_ok = pos == 0 || !query_lower.as_bytes()[pos - 1].is_ascii_alphanumeric();
                let after_pos = pos + kw_lower.len();
                let after_ok = after_pos >= query_lower.len() || !query_lower.as_bytes()[after_pos].is_ascii_alphanumeric();
                before_ok && after_ok
            } else {
                false
            }
        });

        if matched {
            results.push(MatchResult {
                skill: skill.clone(),
                level: MatchLevel::L1,
                score: 1.0,
            });
        }
    }

    // 按 priority 降序排序
    results.sort_by(|a, b| b.skill.priority.cmp(&a.skill.priority));
    results
}

/// L2: 语义相似度匹配
///
/// 复用本地嵌入模型对消息与各 Skill 的 `description`+`keywords` 向量化，
/// 余弦相似度打分。`similarity_threshold` 过滤。
///
/// `call_embedding` 为同步批量嵌入函数（一次调用返回所有文本向量），
/// 由调用方负责在 `spawn_blocking` 中调度，避免阻塞异步运行时。
pub fn match_l2_semantic(
    query: &str,
    skills: &[Skill],
    call_embedding: impl Fn(&[String]) -> Result<Vec<Vec<f32>>, String>,
) -> Result<Vec<MatchResult>, String> {
    if skills.is_empty() {
        return Ok(Vec::new());
    }

    // 构造待向量化的文本列表：[query, skill1_text, skill2_text, ...]
    // 同时记录启用技能的索引映射
    let mut texts = vec![query.to_string()];
    let mut enabled_skill_indices = Vec::new(); // 记录启用技能在 texts 中的索引

    for (i, skill) in skills.iter().enumerate() {
        if !skill.enabled {
            continue;
        }
        // 组合 description + keywords
        let keywords_text = skill.trigger_rules.keywords.join(" ");
        let text = format!("{} {}", skill.description, keywords_text);
        texts.push(text);
        enabled_skill_indices.push(i);
    }

    if enabled_skill_indices.is_empty() {
        return Ok(Vec::new());
    }

    // 调用嵌入模型
    let embeddings = call_embedding(&texts)?;
    if embeddings.len() != texts.len() {
        return Err(format!(
            "嵌入模型返回数量不匹配：期望 {}，实际 {}",
            texts.len(),
            embeddings.len()
        ));
    }

    let query_embedding = &embeddings[0];
    let mut results = Vec::new();

    // 计算余弦相似度（使用正确的索引映射）
    for (text_idx, skill_idx) in enabled_skill_indices.iter().enumerate() {
        let skill = &skills[*skill_idx];
        let embedding_idx = text_idx + 1; // texts[0] 是 query，技能从 1 开始

        if embedding_idx >= embeddings.len() {
            break;
        }

        let skill_embedding = &embeddings[embedding_idx];
        let score = cosine_similarity(query_embedding, skill_embedding);

        // 按 threshold 过滤
        if score >= skill.trigger_rules.similarity_threshold {
            results.push(MatchResult {
                skill: skill.clone(),
                level: MatchLevel::L2,
                score,
            });
        }
    }

    // 按 score 降序排序
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    Ok(results)
}

/// L3 兜底模糊匹配的最低得分（0.0 - 1.0）。
///
/// 0.5 意味着允许关键词最多约一半字符被编辑（如 4 字关键词容忍 2 个错字）。
/// 阈值过低会让「短查询窗口与长关键词共享单个字符」这类弱匹配误命中，
/// 造成无关技能被错误激活（如 "hi" 与 "git" 得分 0.33）。
const L3_MIN_SCORE: f32 = 0.5;

/// L3: 兜底模糊匹配
///
/// 对关键词做模糊匹配（允许错别字、变体），仅在前两层均无命中时触发。
/// 策略（对中文/英文均有效）：
/// 1. 包含关系：关键词与查询互为子串时视为高置信命中；
/// 2. 模糊子串：在查询的滑动窗口内做字符级编辑距离，容忍少量错别字；
/// 3. 词重叠（Jaccard）：保留对空格分词语言（英文）的词语级模糊匹配。
pub fn match_l3_fuzzy(query: &str, skills: &[Skill]) -> Vec<MatchResult> {
    let mut results = Vec::new();

    for skill in skills {
        if !skill.enabled {
            continue;
        }

        let keywords = &skill.trigger_rules.keywords;
        if keywords.is_empty() {
            continue;
        }

        // 计算与所有关键词的最大模糊匹配分数
        let mut max_score = 0.0f32;
        for kw in keywords {
            max_score = max_score.max(fuzzy_keyword_score(query, kw));
        }

        // 模糊匹配阈值（允许一定程度的错别字，但拒绝弱匹配）
        if max_score >= L3_MIN_SCORE {
            results.push(MatchResult {
                skill: skill.clone(),
                level: MatchLevel::L3,
                score: max_score,
            });
        }
    }

    // 按 score 降序排序
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/// 计算查询与单个关键词的模糊匹配分数（0.0 - 1.0）
fn fuzzy_keyword_score(query: &str, keyword: &str) -> f32 {
    if keyword.is_empty() {
        return 0.0;
    }
    let q_lower = query.to_lowercase();
    let k_lower = keyword.to_lowercase();
    if q_lower.contains(&k_lower) || k_lower.contains(&q_lower) {
        return 0.95; // 包含关系：高置信命中
    }

    let q_chars: Vec<char> = q_lower.chars().collect();
    let k_chars: Vec<char> = k_lower.chars().collect();

    // 模糊子串：在查询中以 len±1 的窗口滑动，取最佳归一化编辑距离
    let mut best = 0.0f32;
    let min_len = k_chars.len().saturating_sub(1).max(1);
    let max_len = k_chars.len() + 1;
    for win_len in min_len..=max_len {
        if q_chars.len() < win_len {
            continue;
        }
        for start in 0..=(q_chars.len() - win_len) {
            let window = &q_chars[start..start + win_len];
            let dist = levenshtein_chars(window, &k_chars);
            let score = 1.0 - dist as f32 / win_len.max(k_chars.len()) as f32;
            if score > best {
                best = score;
            }
            if best >= 0.95 {
                return best;
            }
        }
    }

    // 词重叠（Jaccard）：对空格分词语言（英文）的补充
    let query_words: std::collections::HashSet<&str> = q_lower.split_whitespace().collect();
    let kw_words: std::collections::HashSet<&str> = k_lower.split_whitespace().collect();
    let jaccard = jaccard_similarity(&query_words, &kw_words);

    best.max(jaccard)
}

/// 字符级 Levenshtein 编辑距离
fn levenshtein_chars(s1: &[char], s2: &[char]) -> usize {
    let m = s1.len();
    let n = s2.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    let mut prev: Vec<usize> = (0..=n).collect();
    for i in 1..=m {
        let mut cur = vec![0usize; n + 1];
        cur[0] = i;
        for j in 1..=n {
            let cost = if s1[i - 1] == s2[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = cur;
    }
    prev[n]
}

/// 分层意图匹配主入口
///
/// 按 L1 → L2 → L3 顺序匹配，命中即返回。
///
/// 同步函数：`call_embedding` 为同步批量嵌入闭包（可能执行 ONNX 推理），
/// 由调用方负责在 `spawn_blocking` 中调度，避免阻塞异步运行时。
pub fn match_skills(
    query: &str,
    skills: &[Skill],
    call_embedding: impl Fn(&[String]) -> Result<Vec<Vec<f32>>, String>,
) -> Result<Vec<MatchResult>, String> {
    // L1: 关键词精准匹配
    let l1_results = match_l1_keywords(query, skills);
    if !l1_results.is_empty() {
        log::info!("[skill_match] L1 命中 {} 个技能", l1_results.len());
        return Ok(l1_results);
    }

    // L2: 语义相似度匹配
    let l2_results = match_l2_semantic(query, skills, call_embedding)?;
    if !l2_results.is_empty() {
        log::info!("[skill_match] L2 命中 {} 个技能", l2_results.len());
        return Ok(l2_results);
    }

    // L3: 兜底模糊匹配
    let l3_results = match_l3_fuzzy(query, skills);
    if !l3_results.is_empty() {
        log::info!("[skill_match] L3 命中 {} 个技能", l3_results.len());
        return Ok(l3_results);
    }

    log::debug!("[skill_match] 未命中任何技能");
    Ok(Vec::new())
}

// ─── 辅助函数 ───

/// 余弦相似度
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot_product / (norm_a * norm_b)
}

/// Jaccard 相似度（集合交集 / 集合并集）
fn jaccard_similarity<T: Eq + std::hash::Hash>(set_a: &std::collections::HashSet<T>, set_b: &std::collections::HashSet<T>) -> f32 {
    let intersection = set_a.intersection(set_b).count();
    let union = set_a.union(set_b).count();

    if union == 0 {
        return 0.0;
    }

    intersection as f32 / union as f32
}

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);

        let c = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &c).abs() < 1e-6);
    }

    #[test]
    fn test_levenshtein_chars() {
        let a: Vec<char> = "kitten".chars().collect();
        let b: Vec<char> = "sitting".chars().collect();
        assert_eq!(levenshtein_chars(&a, &b), 3);
        assert_eq!(levenshtein_chars(&[], &"abc".chars().collect::<Vec<_>>()), 3);
        let c: Vec<char> = "abc".chars().collect();
        assert_eq!(levenshtein_chars(&c, &c), 0);
    }

    #[test]
    fn test_jaccard_similarity() {
        let a: std::collections::HashSet<&str> = ["a", "b", "c"].iter().cloned().collect();
        let b: std::collections::HashSet<&str> = ["a", "b", "d"].iter().cloned().collect();
        let score = jaccard_similarity(&a, &b);
        assert!((score - 0.5).abs() < 1e-6); // 交集2，并集4
    }

    #[test]
    fn test_fuzzy_keyword_score_rejects_weak_short_window() {
        // 回归：短查询窗口与长关键词仅共享 1 个字符时（"hi" vs "git" 得 0.33），
        // 得分必须低于 L3 阈值，避免无关技能被错误激活。
        assert!(fuzzy_keyword_score("hi", "git") < L3_MIN_SCORE);
        // 单字符错别字仍应达到阈值
        assert!(fuzzy_keyword_score("总洁", "总结") >= L3_MIN_SCORE);
        // 缺一个字符的变体仍应命中
        assert!(fuzzy_keyword_score("summry", "summary") >= L3_MIN_SCORE);
    }
}
