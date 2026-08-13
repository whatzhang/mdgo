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

use crate::core::skill::activation::{ActivationSource, ActiveSkillState, SkillLifetime};
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
    /// 需注入正文的技能完整定义（当前仅手动触发 /技能名 + active 挂载技能；warm 挂载
    /// 不注入，由 LLM 后续 activate_skill 加载）。供请求入口一次性注入 history（不随每轮注入）
    pub skills: Vec<Skill>,
    /// 会话挂载中 warm=自动准备 的技能 ID（仅预热检索，正文/工具由 LLM 按需激活）
    pub mounted_warm: Vec<String>,
    /// 会话挂载中 active=立即生效 的技能 ID（正文已注入、工具已解锁）
    pub mounted_active: Vec<String>,
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
    attached_skills: &[(String, String, String)],
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
        state.activate(&skill, SkillLifetime::Turn, ActivationSource::Manual, true);
        let selected = vec![(skill, ActivationSource::Manual, 1.0)];
        return Ok(Some(ResolvedSkillContext {
            context: SkillExecutionContext::from_skills(&selected),
            cleaned_query: cleaned,
            is_manual: true,
            skills: selected.into_iter().map(|(s, _, _)| s).collect(),
            mounted_warm: Vec::new(),
            mounted_active: Vec::new(),
        }));
    }

    // 2. 会话挂载（直接入选，不参与任何匹配）
    //    mode: warm=自动准备（默认）/ active=立即生效
    let mut selected_skills: Vec<(Skill, ActivationSource, f32)> = Vec::new();
    let mut mounted_warm: Vec<String> = Vec::new();
    let mut mounted_active: Vec<Skill> = Vec::new();
    for (scope_str, skill_id, mode) in attached_skills {
        if let Some(sc) = SkillScope::from_str(scope_str) {
            if let Some(skill) = registry.get(sc, skill_id) {
                if skill.enabled {
                    log::info!(
                        "[skill_context] 会话挂载预激活: {}:{} mode={}",
                        skill.scope.as_str(),
                        skill.id,
                        mode
                    );
                    if mode == "active" {
                        // active=立即生效：正文注入（history 首条）+ 工具解锁
                        state.activate(
                            &skill,
                            SkillLifetime::Session,
                            ActivationSource::Attached,
                            true,
                        );
                        mounted_active.push(skill.clone());
                    } else {
                        // warm=自动准备（默认）：检索预热，正文/工具由 LLM 按需激活
                        state.activate_warm(&skill);
                        mounted_warm.push(skill.id.clone());
                    }
                    selected_skills.push((skill, ActivationSource::Attached, 1.0));
                }
            }
        }
    }

    if selected_skills.is_empty() {
        // 3. 意图匹配：用户消息命中技能 triggers 关键词 → 自动激活（LLM 决策前的可靠兜底）。
        //    恢复旧 matcher 的关键词匹配能力，但由 SKILL.md frontmatter 的 triggers 字段
        //    显式声明，比废弃前的全量描述匹配更可控、可扩展（50+ 技能 = 每个技能配关键词）。
        if let Some((skill, hits)) = resolve_intent_match(query, registry) {
            log::info!(
                "[skill_context] 意图匹配自动激活: {}:{} hits={} query={:?}",
                skill.scope.as_str(),
                skill.id,
                hits,
                query
            );
            state.activate(&skill, SkillLifetime::Turn, ActivationSource::Manual, true);
            let selected = vec![(skill.clone(), ActivationSource::Manual, hits as f32)];
            return Ok(Some(ResolvedSkillContext {
                context: SkillExecutionContext::from_skills(&selected),
                cleaned_query: query.to_string(),
                is_manual: false,
                skills: vec![skill],
                mounted_warm: Vec::new(),
                mounted_active: Vec::new(),
            }));
        }
        return Ok(None);
    }

    let mounted_active_ids: Vec<String> = mounted_active.iter().map(|s| s.id.clone()).collect();
    Ok(Some(ResolvedSkillContext {
        context: SkillExecutionContext::from_skills(&selected_skills),
        cleaned_query: query.to_string(),
        is_manual: false,
        // active 挂载技能正文由请求入口注入（skills）；warm 挂载不注入
        skills: mounted_active,
        mounted_warm,
        mounted_active: mounted_active_ids,
    }))
}

/// 意图匹配：用户消息包含技能 `triggers` 关键词 → 返回命中数最多的技能。
///
/// 命中数并列时取 priority 高者；同名多作用域由注册表 list 去重（同名取高作用域）。
/// 仅在手动触发与会话挂载均未命中时由 [`resolve_preactivated`] 调用，
/// 作为 LLM 自主 activate_skill 决策前的可靠兜底。无命中返回 None。
fn resolve_intent_match(query: &str, registry: &SkillRegistry) -> Option<(Skill, usize)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    let mut best: Option<(Skill, usize)> = None;
    for skill in registry.list(None) {
        if !skill.enabled || skill.triggers.is_empty() {
            continue;
        }
        let hits = skill
            .triggers
            .iter()
            .filter(|t| {
                let t = t.trim().to_lowercase();
                !t.is_empty() && q.contains(&t)
            })
            .count();
        if hits == 0 {
            continue;
        }
        let replace = match &best {
            None => true,
            Some((_, b_hits)) => hits > *b_hits || (hits == *b_hits && skill.priority > best.as_ref().unwrap().0.priority),
        };
        if replace {
            best = Some((skill, hits));
        }
    }
    best
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
    let catalog = format!(
        "可用技能目录（skill_id 作为 activate_skill 的入参；当任务与某技能相关时先激活再执行）：\n{}",
        lines.join("\n")
    );
    catalog
}

/// 作用域覆盖优先级（同名技能：项目 > 全局 > 系统）
fn scope_rank(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::System => 0,
        SkillScope::Global => 1,
        SkillScope::Project => 2,
    }
}

/// 将技能正文拼接为一次注入文本（供请求入口一次性注入 history）。
///
/// 规则：
/// - 按 priority 降序拼接（高优先级技能优先进入）
/// - 超过 `max_chars` 时丢弃后续低优先级技能，并追加截断提示
/// - 空正文技能跳过；空输入返回空串
pub fn format_skill_instructions(skills: &[Skill], max_chars: usize) -> String {
    let mut sorted = skills.to_vec();
    sorted.sort_by(|a, b| b.priority.cmp(&a.priority));
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;
    let mut truncated_skill: Option<String> = None;
    for skill in &sorted {
        let body = skill.body.trim();
        if body.is_empty() {
            continue;
        }
        let block = format!("## {}\n\n{}", skill.name, body);
        let block_chars = block.chars().count();
        if used + block_chars > max_chars {
            truncated = true;
            truncated_skill = Some(skill.id.clone());
            break;
        }
        parts.push(block);
        used += block_chars;
    }
    let mut out = parts.join("\n\n---\n\n");
    if truncated {
        match truncated_skill {
            Some(id) => out.push_str(&format!(
                "\n\n[技能指令已按预算截断（{} 等技能正文超出预算）；如需完整内容请用 read 读取对应技能的 SKILL.md]",
                id
            )),
            None => out.push_str(
                "\n\n[技能指令已按预算截断，如需完整内容请用 read 读取对应技能的 SKILL.md]",
            ),
        }
    }
    out
}

#[cfg(test)]
mod skill_instruction_tests {
    use super::*;

    fn skill(id: &str, priority: u32, body: &str) -> Skill {
        Skill {
            id: id.to_string(),
            scope: SkillScope::System,
            name: id.to_string(),
            description: String::new(),
            priority,
            tools: Vec::new(),
            triggers: Vec::new(),
            top_k: None,
            min_score: None,
            max_docs: None,
            max_chunks_per_doc: None,
            enabled: true,
            version: 1,
            body: body.to_string(),
            file_path: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn joins_by_priority_desc() {
        let out = format_skill_instructions(
            &[
                skill("low", 40, "低优先级正文"),
                skill("high", 80, "高优先级正文"),
            ],
            100_000,
        );
        assert!(out.starts_with("## high"), "高优先级应在前: {}", out);
        assert!(out.contains("低优先级正文"));
    }

    #[test]
    fn truncates_drops_low_priority_and_hints() {
        let out = format_skill_instructions(
            &[
                skill("low", 40, "低优先级正文"),
                skill("high", 80, "高优先级正文"),
            ],
            15,
        );
        assert!(out.contains("高优先级正文"), "应保留高优先级");
        assert!(!out.contains("低优先级正文"), "低优先级应被丢弃");
        assert!(out.contains("已按预算截断"), "应含截断提示");
        assert!(out.contains("low"), "截断提示应指明被截断的技能");
    }

    #[test]
    fn empty_input_and_blank_body() {
        assert!(format_skill_instructions(&[], 1000).is_empty());
        let out = format_skill_instructions(&[skill("blank", 50, "   ")], 1000);
        assert!(out.is_empty(), "空正文应跳过");
    }
}
