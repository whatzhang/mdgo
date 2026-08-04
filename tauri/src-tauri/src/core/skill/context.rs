//! Skill 执行上下文解析：意图匹配的唯一入口（单一职责原则）
//!
//! 收敛三种技能激活方式的解析逻辑，避免散落在各命令中的重复实现：
//! 1. 手动触发（`/技能名`）：剥离前缀得到 `cleaned_query`，标记 `MatchLevel::Manual`
//! 2. 会话挂载（chat_session_skills 快照）：标记 `MatchLevel::Attached`
//! 3. 自动意图匹配（L1 关键词 / L2 语义 / L3 兜底）
//!
//! 全部技能参数（指令 / 工具白名单 / 检索覆盖）由 [`SkillExecutionContext::from_skills`]
//! 统一聚合。本模块为纯逻辑层（core），会话挂载数据由调用方提取后以
//! `(scope, skill_id)` 列表注入，不直接依赖 services 层（依赖倒置）。

use crate::core::skill::matcher::{match_skills, MatchLevel};
use crate::core::skill::{Skill, SkillRegistry, SkillScope};

/// 自动匹配最多入选的技能数（只取最优单个技能，避免多技能指令互相干扰）
const MAX_MATCHED_SKILLS: usize = 1;

/// 单条技能匹配明细（供指标埋点与日志追踪）
#[derive(Debug, Clone)]
pub struct SkillMatchInfo {
    /// 技能 ID（不含 scope 前缀）
    pub skill_id: String,
    /// 作用域（system / global / project）
    pub scope: String,
    /// 匹配层级
    pub match_level: MatchLevel,
    /// 匹配分数
    pub match_score: f32,
}

/// 技能执行上下文（供 Agent 集成使用）
///
/// 包含已匹配技能的聚合配置，用于：
/// - 指令注入（合并所有技能的 body）
/// - 工具白名单（取并集）
/// - 检索参数覆盖（取最保守值）
#[derive(Debug, Clone, Default)]
pub struct SkillExecutionContext {
    /// 合并后的指令正文（多技能按优先级拼接）
    pub instructions: String,
    /// 允许的工具白名单（取并集）
    pub allowed_tools: Vec<String>,
    /// 检索参数覆盖（取最保守值）
    pub top_k: Option<u32>,
    pub min_score: Option<f32>,
    pub max_docs: Option<usize>,
    pub max_chunks_per_doc: Option<usize>,
    /// 参与的技能 ID 列表（scope:skill_id，用于日志追踪与指标埋点）
    pub skill_ids: Vec<String>,
    /// 匹配明细（每个技能对应的层级/分数，供指标埋点）
    pub matches: Vec<SkillMatchInfo>,
}

impl SkillExecutionContext {
    /// 从匹配结果构建执行上下文
    pub fn from_skills(skills: &[(Skill, MatchLevel, f32)]) -> Self {
        if skills.is_empty() {
            return Self::default();
        }

        let mut instructions = String::new();
        let mut allowed_tools: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut top_k: Option<u32> = None;
        let mut min_score: Option<f32> = None;
        let mut max_docs: Option<usize> = None;
        let mut max_chunks_per_doc: Option<usize> = None;
        let mut skill_ids = Vec::new();
        let mut matches = Vec::new();

        // 按优先级排序（高优先级在前）
        let mut sorted_skills = skills.to_vec();
        sorted_skills.sort_by(|a, b| b.0.priority.cmp(&a.0.priority));

        for (skill, level, score) in &sorted_skills {
            skill_ids.push(format!("{}:{}", skill.scope.as_str(), skill.id));
            matches.push(SkillMatchInfo {
                skill_id: skill.id.clone(),
                scope: skill.scope.as_str().into(),
                match_level: *level,
                match_score: *score,
            });

            // 合并指令（用分隔符区分不同技能）
            if !skill.body.trim().is_empty() {
                if !instructions.is_empty() {
                    instructions.push_str("\n\n---\n\n");
                }
                instructions.push_str(&format!("## {}\n\n{}", skill.name, skill.body.trim()));
            }

            // 工具白名单取并集
            for tool in &skill.tools {
                allowed_tools.insert(tool.clone());
            }

            // 检索参数取最保守值（最小 top_k、最大 min_score、最小 max_docs）
            if let Some(v) = skill.top_k {
                merge_min(&mut top_k, v);
            }
            if let Some(v) = skill.min_score {
                merge_max(&mut min_score, v);
            }
            if let Some(v) = skill.max_docs {
                merge_min(&mut max_docs, v);
            }
            if let Some(v) = skill.max_chunks_per_doc {
                merge_min(&mut max_chunks_per_doc, v);
            }
        }

        Self {
            instructions,
            allowed_tools: allowed_tools.into_iter().collect(),
            top_k,
            min_score,
            max_docs,
            max_chunks_per_doc,
            skill_ids,
            matches,
        }
    }
}

/// 保守合并：数值型参数取更小值（top_k / max_docs / max_chunks_per_doc）
/// `PartialOrd` 兼容 f32（min_score 场景），避免依赖 `Ord` 的整数限定
fn merge_min<T: PartialOrd + Copy>(acc: &mut Option<T>, v: T) {
    *acc = Some(match acc {
        Some(cur) if *cur <= v => *cur,
        _ => v,
    });
}

/// 保守合并：阈值型参数取更大值（min_score）
fn merge_max<T: PartialOrd + Copy>(acc: &mut Option<T>, v: T) {
    *acc = Some(match acc {
        Some(cur) if *cur >= v => *cur,
        _ => v,
    });
}

/// 技能解析结果（含清理后的查询）
#[derive(Debug, Clone)]
pub struct ResolvedSkillContext {
    pub context: SkillExecutionContext,
    /// 清理后的查询：手动触发时剥离 `/技能名` 前缀，其余场景为原查询
    pub cleaned_query: String,
    /// 是否为手动触发（/技能名）
    pub is_manual: bool,
}

/// 解析技能上下文（唯一入口，同步函数）。
///
/// `attached_skills` 为会话挂载的技能 `(scope, skill_id)` 列表（由调用方从
/// ChatStore 提取后注入，避免 core 依赖 services 层）。
///
/// `call_embedding` 为同步批量嵌入闭包（内部做 ONNX 批处理推理），
/// 由调用方负责在 `spawn_blocking` 中调度，避免阻塞异步运行时。
pub fn resolve_skill_context(
    query: &str,
    registry: &SkillRegistry,
    attached_skills: &[(String, String)],
    call_embedding: impl Fn(&[String]) -> Result<Vec<Vec<f32>>, String>,
) -> Result<Option<ResolvedSkillContext>, String> {
    // 1. 手动触发（/技能名）：最高优先级，跳过挂载与自动匹配
    if let Some((skill, cleaned)) = resolve_manual_trigger(query, registry) {
        log::info!(
            "[skill_context] 手动触发: {}:{} cleaned_query={:?}",
            skill.scope.as_str(),
            skill.id,
            cleaned
        );
        let selected = vec![(skill, MatchLevel::Manual, 1.0)];
        return Ok(Some(ResolvedSkillContext {
            context: SkillExecutionContext::from_skills(&selected),
            cleaned_query: cleaned,
            is_manual: true,
        }));
    }

    let mut selected_skills: Vec<(Skill, MatchLevel, f32)> = Vec::new();

    // 2. 会话挂载（直接入选，不参与匹配）
    for (scope_str, skill_id) in attached_skills {
        if let Some(sc) = SkillScope::from_str(scope_str) {
            if let Some(skill) = registry.get(sc, skill_id) {
                if skill.enabled {
                    selected_skills.push((skill, MatchLevel::Attached, 1.0));
                }
            }
        }
    }

    // 3. 无显式技能时执行自动匹配（L1/L2/L3）
    if selected_skills.is_empty() {
        let enabled_skills: Vec<Skill> = registry
            .list(None)
            .into_iter()
            .filter(|s| s.enabled)
            .collect();

        if !enabled_skills.is_empty() {
            match match_skills(query, &enabled_skills, call_embedding)? {
                results if !results.is_empty() => {
                    selected_skills.extend(
                        results
                            .into_iter()
                            .take(MAX_MATCHED_SKILLS)
                            .map(|r| (r.skill, r.level, r.score)),
                    );
                }
                _ => return Ok(None),
            }
        }
    }

    if selected_skills.is_empty() {
        return Ok(None);
    }

    Ok(Some(ResolvedSkillContext {
        context: SkillExecutionContext::from_skills(&selected_skills),
        cleaned_query: query.to_string(),
        is_manual: false,
    }))
}

/// 手动触发解析：`/技能名 [其余内容]` → (技能, 清理后的查询)。
///
/// 按 system → global → project 优先级查找；先按技能 ID 精确匹配，
/// 未命中再按显示名匹配（前端 chips 展示的是 name，二者可能不一致）；
/// 未启用或不存在返回 None。
fn resolve_manual_trigger(query: &str, registry: &SkillRegistry) -> Option<(Skill, String)> {
    let trimmed = query.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let rest = &trimmed[1..];
    let (skill_name, remainder) = match rest.find(char::is_whitespace) {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    if skill_name.is_empty() {
        return None;
    }
    for scope in [SkillScope::System, SkillScope::Global, SkillScope::Project] {
        // 优先按 ID 匹配，其次按显示名匹配（同作用域内取优先级最高的同名技能）
        let skill = registry
            .get(scope, skill_name)
            .or_else(|| {
                registry
                    .list(Some(scope))
                    .into_iter()
                    .find(|s| s.name == skill_name)
            });
        if let Some(skill) = skill {
            if skill.enabled {
                let cleaned = remainder.trim();
                // 空查询守卫：`/技能名` 无剩余内容时回退为完整触发语句，
                // 避免下游以空字符串执行查询扩展与嵌入检索
                return Some((
                    skill,
                    if cleaned.is_empty() {
                        trimmed.to_string()
                    } else {
                        cleaned.to_string()
                    },
                ));
            }
        }
    }
    log::warn!(
        "[skill_context] 手动触发失败: 技能 '{}' 不存在或未启用",
        skill_name
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::skill::SkillStore;

    /// 构建一个最小可用的注册表（含一个 project 作用域技能）。
    /// `tag` 用于区分每个测试的临时目录，避免并行测试互相覆盖文件。
    fn registry_with_skill(tag: &str, id: &str, keywords: &[&str], enabled: bool) -> SkillRegistry {
        let dir = std::env::temp_dir()
            .join("mdgo-skill-context-test")
            .join(tag);
        let _ = std::fs::create_dir_all(dir.join(".mdgo").join("skills").join(id));
        let md = format!(
            "---\nid: {}\nscope: project\nname: test\npriority: 50\ntrigger_rules:\n  keywords: [{}]\nenabled: {}\n---\n测试正文\n",
            id,
            keywords
                .iter()
                .map(|k| format!("\"{}\"", k))
                .collect::<Vec<_>>()
                .join(", "),
            enabled
        );
        std::fs::write(
            dir.join(".mdgo").join("skills").join(id).join("SKILL.md"),
            md,
        )
        .unwrap();
        let registry = SkillRegistry::new();
        registry.reload(dir.to_str().unwrap()).unwrap();
        registry
    }

    #[test]
    fn manual_trigger_strips_prefix_and_marks_manual() {
        let registry = registry_with_skill("manual-trigger", "calc", &["计算"], true);
        let resolved = resolve_skill_context(
            "/calc 1+1 等于几",
            &registry,
            &[],
            |_| unreachable!("手动触发不应调用嵌入"),
        )
        .unwrap()
        .unwrap();
        assert!(resolved.is_manual);
        assert_eq!(resolved.cleaned_query, "1+1 等于几");
        assert_eq!(resolved.context.skill_ids, vec!["project:calc"]);
    }

    #[test]
    fn manual_trigger_empty_remainder_falls_back_to_trigger() {
        // `/calc` 无剩余内容：cleaned_query 回退为完整触发语句，避免空查询进入检索
        let registry = registry_with_skill("manual-empty", "calc", &["计算"], true);
        let resolved = resolve_skill_context(
            "/calc",
            &registry,
            &[],
            |_| unreachable!("手动触发不应调用嵌入"),
        )
        .unwrap()
        .unwrap();
        assert!(resolved.is_manual);
        assert_eq!(resolved.cleaned_query, "/calc");
    }

    #[test]
    fn l1_keyword_match_fires_without_embedding() {
        let registry = registry_with_skill("l1-keyword", "code-review", &["审查代码"], true);
        let resolved = resolve_skill_context(
            "请帮我审查代码",
            &registry,
            &[],
            |_| unreachable!("L1 命中不应调用嵌入"),
        )
        .unwrap()
        .unwrap();
        assert!(!resolved.is_manual);
        assert_eq!(resolved.cleaned_query, "请帮我审查代码");
    }

    #[test]
    fn no_match_returns_none() {
        let registry = registry_with_skill("no-match", "calc", &["计算"], true);
        let resolved = resolve_skill_context(
            "今天天气怎么样",
            &registry,
            &[],
            |texts| {
                // 模拟嵌入模型：返回与输入等量的零向量（低于阈值，不会命中）
                Ok(vec![vec![0.0f32; 64]; texts.len()])
            },
        )
        .unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn attached_skills_bypass_matching() {
        let registry = registry_with_skill("attached-bypass", "calc", &["完全不匹配"], true);
        let resolved = resolve_skill_context(
            "hello world",
            &registry,
            &[("project".to_string(), "calc".to_string())],
            |_| unreachable!("挂载技能不应调用嵌入"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved.context.skill_ids, vec!["project:calc"]);
    }

    #[test]
    fn disabled_skill_manual_falls_back_to_auto_match() {
        // 手动触发技能已停用 → 回落到自动匹配；零向量嵌入低于阈值 → 最终无命中
        let registry = registry_with_skill("disabled-manual", "calc", &["计算"], false);
        let resolved = resolve_skill_context(
            "/calc hi",
            &registry,
            &[],
            |texts| Ok(vec![vec![0.0f32; 64]; texts.len()]),
        )
        .unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn skill_store_paths_exist() {
        // 确保 SkillStore 路径计算不 panic（Windows 路径分隔符）
        assert!(SkillStore::global_skills_dir().to_str().unwrap().len() > 0);
    }
}
