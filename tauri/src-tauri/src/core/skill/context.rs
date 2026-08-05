//! Skill 预激活上下文解析（渐进式披露 L1/L2 的预激活入口）。
//!
//! 技能激活决策已完全交由 LLM：本模块不做任何本地匹配（关键词 / embedding /
//! 语义 / 模糊），仅处理查询启动时的两类显式预激活：
//! 1. 手动触发（`/技能名`）：剥离前缀得到 `cleaned_query`，来源 [`ActivationSource::Manual`]
//! 2. 会话挂载（chat_session_skills 快照）：来源 [`ActivationSource::Attached`]
//!
//! 预激活技能写入共享的 [`ActiveSkillState`]，由 Agent 钩子（L2 指令动态注入）与
//! 技能工具（activate_skill / deactivate_skill）统一读写；本模块只负责「解析 + 写入」。
//! 会话挂载数据由调用方提取后以 `(scope, skill_id)` 列表注入，不直接依赖
//! services 层（依赖倒置）。

use crate::core::skill::activation::{ActivationSource, ActiveSkillState};
use crate::core::skill::{Skill, SkillRegistry, SkillScope};

/// 单条技能激活明细（供指标埋点与日志追踪）
#[derive(Debug, Clone)]
pub struct SkillMatchInfo {
    /// 技能 ID（不含 scope 前缀）
    pub skill_id: String,
    /// 作用域（system / global / project）
    pub scope: String,
    /// 激活来源（attached / manual / llm）
    pub source: ActivationSource,
    /// 置信度（显式激活固定为 1.0）
    pub match_score: f32,
}

/// 预激活技能的执行参数（供预检索与工具默认参数使用）
///
/// 仅聚合预激活技能的检索参数覆盖（取最保守值）；指令与工具白名单
/// 由 [`ActiveSkillState`] 在请求期间动态提供，不在此重复。
#[derive(Debug, Clone, Default)]
pub struct SkillExecutionContext {
    pub top_k: Option<u32>,
    pub min_score: Option<f32>,
    pub max_docs: Option<usize>,
    pub max_chunks_per_doc: Option<usize>,
    /// 参与的技能 ID 列表（scope:skill_id，用于日志追踪与指标埋点）
    pub skill_ids: Vec<String>,
    /// 激活明细（每个技能对应的来源/分数，供指标埋点）
    pub matches: Vec<SkillMatchInfo>,
}

impl SkillExecutionContext {
    /// 从预激活技能列表聚合执行上下文
    pub fn from_skills(skills: &[(Skill, ActivationSource, f32)]) -> Self {
        if skills.is_empty() {
            return Self::default();
        }

        let mut top_k: Option<u32> = None;
        let mut min_score: Option<f32> = None;
        let mut max_docs: Option<usize> = None;
        let mut max_chunks_per_doc: Option<usize> = None;
        let mut skill_ids = Vec::new();
        let mut matches = Vec::new();

        // 按优先级排序（高优先级在前）
        let mut sorted_skills = skills.to_vec();
        sorted_skills.sort_by(|a, b| b.0.priority.cmp(&a.0.priority));

        for (skill, source, score) in &sorted_skills {
            skill_ids.push(format!("{}:{}", skill.scope.as_str(), skill.id));
            matches.push(SkillMatchInfo {
                skill_id: skill.id.clone(),
                scope: skill.scope.as_str().into(),
                source: *source,
                match_score: *score,
            });

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

/// 预激活解析结果（含清理后的查询）
#[derive(Debug, Clone)]
pub struct ResolvedSkillContext {
    pub context: SkillExecutionContext,
    /// 清理后的查询：手动触发时剥离 `/技能名` 前缀，其余场景为原查询
    pub cleaned_query: String,
    /// 是否为手动触发（/技能名）
    pub is_manual: bool,
}

/// 解析预激活技能（唯一入口，同步函数）。
///
/// 只处理两类显式预激活，命中后直接写入 `state`（LLM 决策前的显式用户意图）：
/// 1. 手动触发 `/技能名`（最高优先级）
/// 2. 会话挂载（`attached_skills` 由调用方从 ChatStore 提取后注入）
///
/// 未命中返回 `None`：表示本请求无预激活技能，技能是否激活完全交由 LLM
/// 依据 L1 技能目录自主决策。
pub fn resolve_preactivated(
    query: &str,
    registry: &SkillRegistry,
    attached_skills: &[(String, String)],
    state: &ActiveSkillState,
) -> Result<Option<ResolvedSkillContext>, String> {
    // 1. 手动触发（/技能名）：最高优先级，跳过挂载
    if let Some((skill, cleaned)) = resolve_manual_trigger(query, registry) {
        log::info!(
            "[skill_context] 手动预激活: {}:{} cleaned_query={:?}",
            skill.scope.as_str(),
            skill.id,
            cleaned
        );
        state.activate(skill.clone());
        let selected = vec![(skill, ActivationSource::Manual, 1.0)];
        return Ok(Some(ResolvedSkillContext {
            context: SkillExecutionContext::from_skills(&selected),
            cleaned_query: cleaned,
            is_manual: true,
        }));
    }

    // 2. 会话挂载（直接入选，不参与任何匹配）
    let mut selected_skills: Vec<(Skill, ActivationSource, f32)> = Vec::new();
    for (scope_str, skill_id) in attached_skills {
        if let Some(sc) = SkillScope::from_str(scope_str) {
            if let Some(skill) = registry.get(sc, skill_id) {
                if skill.enabled {
                    log::info!(
                        "[skill_context] 会话挂载预激活: {}:{}",
                        skill.scope.as_str(),
                        skill.id
                    );
                    state.activate(skill.clone());
                    selected_skills.push((skill, ActivationSource::Attached, 1.0));
                }
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

/// 构建 L1 技能目录（渐进式披露 L1 元数据，会话全程常驻）。
///
/// 列出全部启用技能（同名多作用域取优先级最高者），供模型自主决策是否
/// 调用 `activate_skill` 激活。由 Agent 钩子注入每次模型调用的 preamble。
pub fn build_skill_catalog(registry: &SkillRegistry) -> String {
    let mut best_by_id: std::collections::HashMap<String, Skill> = std::collections::HashMap::new();
    for skill in registry.list(None) {
        if !skill.enabled {
            continue;
        }
        match best_by_id.get(&skill.id) {
            Some(existing) if scope_rank(existing.scope) >= scope_rank(skill.scope) => {}
            _ => {
                best_by_id.insert(skill.id.clone(), skill);
            }
        }
    }

    let mut skills: Vec<Skill> = best_by_id.into_values().collect();
    skills.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| scope_rank(a.scope).cmp(&scope_rank(b.scope)))
    });

    if skills.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = skills
        .iter()
        .map(|s| {
            let desc = if s.description.trim().is_empty() {
                "（无描述）".to_string()
            } else {
                s.description.trim().to_string()
            };
            format!("- `{}`（{}）：{}", s.id, s.scope.as_str(), desc)
        })
        .collect();
    format!(
        "可用技能目录（skill_id 作为 activate_skill 的入参；当任务与某技能相关时先激活再执行）：\n{}",
        lines.join("\n")
    )
}

/// 作用域覆盖优先级（同名技能：项目 > 全局 > 系统）
fn scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::System => 0,
        SkillScope::Global => 1,
        SkillScope::Project => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::skill::SkillStore;

    /// 构建一个最小可用的注册表（含一个 project 作用域技能）。
    /// `tag` 用于区分每个测试的临时目录，避免并行测试互相覆盖文件。
    fn registry_with_skill(tag: &str, id: &str, enabled: bool) -> SkillRegistry {
        let dir = std::env::temp_dir()
            .join("mdgo-skill-context-test")
            .join(tag);
        let _ = std::fs::create_dir_all(dir.join(".mdgo").join("skills").join(id));
        let md = format!(
            "---\nid: {}\nscope: project\nname: test\npriority: 50\nenabled: {}\n---\n测试正文\n",
            id, enabled
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
    fn manual_trigger_strips_prefix_and_activates() {
        let registry = registry_with_skill("manual-trigger", "calc", true);
        let state = ActiveSkillState::new();
        let resolved = resolve_preactivated("/calc 1+1 等于几", &registry, &[], &state)
            .unwrap()
            .unwrap();
        assert!(resolved.is_manual);
        assert_eq!(resolved.cleaned_query, "1+1 等于几");
        assert_eq!(resolved.context.skill_ids, vec!["project:calc"]);
        // 预激活技能已写入共享状态（L2 指令由钩子读取）
        let activated_ids: Vec<String> = state
            .activated()
            .iter()
            .map(|s| format!("{}:{}", s.scope.as_str(), s.id))
            .collect();
        assert_eq!(activated_ids, vec!["project:calc"]);
    }

    #[test]
    fn manual_trigger_empty_remainder_falls_back_to_trigger() {
        // `/calc` 无剩余内容：cleaned_query 回退为完整触发语句，避免空查询进入检索
        let registry = registry_with_skill("manual-empty", "calc", true);
        let state = ActiveSkillState::new();
        let resolved = resolve_preactivated("/calc", &registry, &[], &state)
            .unwrap()
            .unwrap();
        assert!(resolved.is_manual);
        assert_eq!(resolved.cleaned_query, "/calc");
    }

    #[test]
    fn no_preactivation_returns_none() {
        let registry = registry_with_skill("no-match", "calc", true);
        let state = ActiveSkillState::new();
        let resolved = resolve_preactivated("今天天气怎么样", &registry, &[], &state).unwrap();
        assert!(resolved.is_none());
        assert!(state.activated().is_empty());
    }

    #[test]
    fn attached_skills_bypass_matching() {
        let registry = registry_with_skill("attached-bypass", "calc", true);
        let state = ActiveSkillState::new();
        let resolved = resolve_preactivated(
            "hello world",
            &registry,
            &[("project".to_string(), "calc".to_string())],
            &state,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resolved.context.skill_ids, vec!["project:calc"]);
        let activated_ids: Vec<String> = state
            .activated()
            .iter()
            .map(|s| format!("{}:{}", s.scope.as_str(), s.id))
            .collect();
        assert_eq!(activated_ids, vec!["project:calc"]);
    }

    #[test]
    fn disabled_skill_manual_is_rejected() {
        let registry = registry_with_skill("disabled-manual", "calc", false);
        let state = ActiveSkillState::new();
        let resolved = resolve_preactivated("/calc hi", &registry, &[], &state).unwrap();
        assert!(resolved.is_none());
        assert!(state.activated().is_empty());
    }

    #[test]
    fn skill_store_paths_exist() {
        // 确保 SkillStore 路径计算不 panic（Windows 路径分隔符）
        assert!(SkillStore::global_skills_dir().to_str().unwrap().len() > 0);
    }
}
