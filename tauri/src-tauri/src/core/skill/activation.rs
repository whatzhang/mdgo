//! 技能激活状态（渐进式披露 L2 加载的核心共享状态）。
//!
//! 单一职责：维护当前请求中已激活技能的**生命周期状态**（[`SkillActivation`]），
//! 供 Agent 钩子（工具白名单窄化）与技能工具（activate_skill / deactivate_skill）
//! 共享读写。
//!
//! 激活决策完全交由 LLM：模型根据技能目录（L1 元数据）自主调用
//! `activate_skill` 加载技能正文（L2）；查询启动时的显式预激活（会话挂载 /
//! `/技能名` 手动触发）也写入同一状态，两类来源统一处理。
//!
//! 一次性注入语义（对齐 Reasonix / Pi 的 progressive disclosure）：
//! 激活状态**不持有 Skill 正文**（正文是 SkillDefinition 的静态内容，
//! 不随运行时状态复制），正文由调用方负责一次性注入——
//! - LLM 动态激活：`activate_skill` 工具结果返回正文核心段
//! - 预激活（/技能名、会话挂载）：请求入口注入 history 首条消息
//! 本模块只维护生命周期、激活来源与工具声明，避免正文每轮重复注入消耗 token。

use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use super::{Skill, SkillScope};

/// 激活生命周期层（与存储层 [`SkillScope`] 正交，不合并：
/// `SkillScope` 描述 SKILL.md 存放在哪一层，本枚举描述激活持续多久）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // as_str 供后续 Phase 3/5（warm 态与 session summary）日志使用
pub enum SkillLifetime {
    /// 单次请求有效（`/技能名` 显式激活、LLM 动态激活）
    Turn,
    /// 会话生命周期有效（会话挂载）
    Session,
}

#[allow(dead_code)] // as_str 供后续 Phase 3/5（warm 态与 session summary）日志使用
impl SkillLifetime {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillLifetime::Turn => "turn",
            SkillLifetime::Session => "session",
        }
    }
}

/// 激活状态机。
///
/// 注意：mdgo 的 rig 工具在 poll 栈内顺序执行，无并发激活竞争，
/// 因此不引入 Warming / Activating 中间态，仅保留可观测终态与候选态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // as_str 供后续 Phase 3/5 日志与状态上报使用
pub enum ActivationStatus {
    /// 已进入激活流程但正文尚未注入（预算 defer / 等待加载等场景）
    Candidate,
    /// 已激活（正文已注入，或由请求入口一次性提供）
    Active,
    /// 激活失败（SKILL.md 缺失、超预算等）
    Failed,
    /// 生命周期结束（请求结束 / 会话销毁）
    Expired,
}

#[allow(dead_code)] // as_str 供后续 Phase 3/5 日志与状态上报使用
impl ActivationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActivationStatus::Candidate => "candidate",
            ActivationStatus::Active => "active",
            ActivationStatus::Failed => "failed",
            ActivationStatus::Expired => "expired",
        }
    }
}

/// 单技能正文注入上限（字符；≈2000 token，`ApproxTokenEstimator` chars/2）。
///
/// `activate_skill` 工具结果与请求入口一次性注入共用此上限；
/// 超限正文截断并引导用 read 读取完整 `{skill_id}/SKILL.md`（L3 参考路径）。
pub const MAX_SKILL_BODY_CHARS: usize = 4000;

/// 请求入口一次性注入的正文总量上限（字符；≈4000 token）。
///
/// 多技能预激活（/技能名 + 会话挂载）拼接时按此上限截断，避免 preamble 膨胀。
pub const MAX_SKILL_INJECTION_CHARS: usize = 8000;

/// 技能激活来源（替代旧 matcher 的匹配层级）。
///
/// 决策已交由 LLM：本地不再做关键词 / embedding / 模糊匹配，
/// 仅保留三类显式激活来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivationSource {
    /// 会话挂载（chat_session_skills 快照）≈ Mounted
    #[serde(rename = "attached")]
    Attached,
    /// 手动触发（/技能名）≈ Explicit
    #[serde(rename = "manual")]
    Manual,
    /// LLM 通过 activate_skill 工具按技能目录（L1）决策激活 ≈ Auto
    #[serde(rename = "llm")]
    Llm,
}

impl ActivationSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActivationSource::Attached => "attached",
            ActivationSource::Manual => "manual",
            ActivationSource::Llm => "llm",
        }
    }

    /// 由字符串恢复（DB 读回时使用；未知值回退为 Llm）
    pub fn from_str(s: &str) -> Self {
        match s {
            "attached" => ActivationSource::Attached,
            "manual" => ActivationSource::Manual,
            _ => ActivationSource::Llm,
        }
    }
}

/// 运行时激活记录（不持有正文；正文一次性注入由调用方负责）。
///
/// `version`/`lifetime`/`status`/`mode`/`summary` 为 V4 生命周期字段，
/// 分别服务于 Session 技能跨请求恢复校验（P5）、warm 态（P3）、调试溯源与压缩恢复。
#[derive(Debug, Clone)]
#[allow(dead_code)] // 部分生命周期字段在后续 Phase 3/5 使用，先冻结结构
pub struct SkillActivation {
    pub skill_id: String,
    pub scope: SkillScope,
    /// SKILL.md frontmatter version（Session 生命周期技能跨请求恢复时校验）
    pub version: u32,
    pub lifetime: SkillLifetime,
    pub status: ActivationStatus,
    /// 激活来源（Manual=显式 /技能名、Attached=会话挂载、Llm=LLM 决策）
    pub mode: ActivationSource,
    /// 正文是否已注入过（幂等依据：重复激活不重复返回正文）
    pub loaded_once: bool,
    /// 声明的工具（allowed_tools 聚合、工具调用轨迹溯源）
    pub tools: Vec<String>,
    pub activated_at: std::time::Instant,
    /// Session 生命周期技能跨请求压缩恢复的约束摘要（预留，P5 使用）
    pub summary: String,
}

impl SkillActivation {
    /// 从 Skill 定义构造激活记录（仅提取元数据，不复制正文）。
    pub fn from_skill(
        skill: &Skill,
        lifetime: SkillLifetime,
        mode: ActivationSource,
        loaded_once: bool,
    ) -> Self {
        Self {
            skill_id: skill.id.clone(),
            scope: skill.scope,
            version: skill.version,
            lifetime,
            status: ActivationStatus::Active,
            mode,
            loaded_once,
            tools: skill.tools.clone(),
            activated_at: std::time::Instant::now(),
            summary: String::new(),
        }
    }
}

/// 技能激活状态（`Arc` 共享，跨工具闭包与 Agent 钩子）。
///
/// 每次请求开始时新建实例，请求期间 LLM 可动态增删。
#[derive(Debug)]
pub struct ActiveSkillState {
    inner: RwLock<Vec<SkillActivation>>,
    /// 请求期间被停用的技能及停用时刻耗时（用于执行统计补录，避免中途停用的技能漏记）
    deactivated: RwLock<Vec<(SkillActivation, u64)>>,
}

impl Default for ActiveSkillState {
    fn default() -> Self {
        Self::new()
    }
}

impl ActiveSkillState {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Vec::new()),
            deactivated: RwLock::new(Vec::new()),
        }
    }

    /// 激活一个技能（幂等：同 id+scope 覆盖为最新定义，并刷新激活时刻）。
    ///
    /// `loaded_once` 由调用方决定（正文是否已注入）：
    /// - `activate_skill` 工具：返回正文 → true；预算 defer → false
    /// - 请求入口预激活（/技能名、会话挂载）：正文已入 history → true
    pub fn activate(
        &self,
        skill: &Skill,
        lifetime: SkillLifetime,
        mode: ActivationSource,
        loaded_once: bool,
    ) {
        let activation = SkillActivation::from_skill(skill, lifetime, mode, loaded_once);
        let mut list = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = list
            .iter_mut()
            .find(|a| a.skill_id == skill.id && a.scope == skill.scope)
        {
            *existing = activation;
        } else {
            list.push(activation);
        }
    }

    /// 幂等辅助：技能是否已激活且正文已注入（重复 activate 不再返回正文）。
    pub fn is_loaded(&self, skill_id: &str) -> bool {
        let list = self.inner.read().unwrap_or_else(|e| e.into_inner());
        list.iter()
            .any(|a| a.skill_id == skill_id && a.loaded_once)
    }

    /// 停用一个技能（按 ID 匹配，任意作用域），返回是否找到并停用。
    ///
    /// 停用时记录「激活时刻 → 停用时刻」耗时到 `deactivated`，供执行统计补录。
    pub fn deactivate(&self, skill_id: &str) -> bool {
        let mut list = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = list.iter().position(|a| a.skill_id == skill_id) {
            let activation = list.remove(pos);
            let elapsed = activation.activated_at.elapsed().as_millis() as u64;
            self.deactivated
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .push((activation, elapsed));
            true
        } else {
            false
        }
    }

    /// 挂载为 warm（会话级候选）：正文不注入、工具不解锁，
    /// 但检索参数参与合并、预检索开关生效（warm 技能声明 kb_search/code_lookup 时）。
    /// 幂等：同 id+scope 覆盖（保持 warm 状态，不因重复挂载升级为 Active）。
    pub fn activate_warm(&self, skill: &Skill) {
        let mut activation =
            SkillActivation::from_skill(skill, SkillLifetime::Session, ActivationSource::Attached, false);
        activation.status = ActivationStatus::Candidate;
        let mut list = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = list
            .iter_mut()
            .find(|a| a.skill_id == skill.id && a.scope == skill.scope)
        {
            *existing = activation;
        } else {
            list.push(activation);
        }
    }

    /// 当前激活（status=Active）的完整记录——工具解锁 / read L3 参考 / 工具轨迹溯源用。
    ///
    /// 与 [`Self::activated`] 的区别：warm（Candidate）技能不在此列，
    /// 其声明工具不可见、references 不可读、不参与正文注入。
    pub fn active_only(&self) -> Vec<SkillActivation> {
        let list = self.inner.read().unwrap_or_else(|e| e.into_inner());
        list.iter()
            .filter(|a| a.status == ActivationStatus::Active)
            .cloned()
            .collect()
    }

    /// 当前激活的完整记录（含 warm/Candidate，按激活顺序）。
    pub fn activated(&self) -> Vec<SkillActivation> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 当前激活技能的精简计时信息（scope, id, 激活至今毫秒数）。
    ///
    /// 供技能执行统计按技能独立计时：LLM 动态激活的技能以其激活时间为起点，
    /// 预激活（会话挂载 / 手动触发）技能以请求早期激活时间为起点。
    /// 只返回 id/scope/耗时，不克隆 SkillActivation，避免指标路径搬运大对象。
    pub fn activated_elapsed(&self) -> Vec<(String, String, u64)> {
        let list = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        list.iter()
            .map(|a| {
                let elapsed = now.duration_since(a.activated_at).as_millis() as u64;
                (a.scope.as_str().to_string(), a.skill_id.clone(), elapsed)
            })
            .collect()
    }

    /// 请求期间被停用技能的精简计时信息（scope, id, 停用时刻耗时）。
    ///
    /// 与 `activated_elapsed` 配合，保证 LLM 中途激活又停用的技能也进入执行统计。
    pub fn deactivated_elapsed(&self) -> Vec<(String, String, u64)> {
        let deactivated = self.deactivated.read().unwrap_or_else(|e| e.into_inner());
        deactivated
            .iter()
            .map(|(a, elapsed)| (a.scope.as_str().to_string(), a.skill_id.clone(), *elapsed))
            .collect()
    }

    /// 工具白名单（激活技能声明工具的并集）。
    ///
    /// 语义（`Option` 区分三种状态）：
    /// - `None`：无技能激活，无工具约束，放行全部
    /// - `Some(空列表)`：激活技能均未声明工具；实际仅放行 BASE_TOOLS（SkillGateHook 兜底拦截非基础工具）
    /// - `Some(list)`：仅放行声明工具（与 BASE_TOOLS 取并集）
    pub fn allowed_tools(&self) -> Option<Vec<String>> {
        let list = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if list.is_empty() {
            return None;
        }
        let mut set = std::collections::HashSet::new();
        for a in list.iter() {
            // 仅 Active 技能解锁工具；warm（Candidate）技能声明工具不可见
            if a.status != ActivationStatus::Active {
                continue;
            }
            for t in &a.tools {
                set.insert(t.clone());
            }
        }
        Some(set.into_iter().collect())
    }

    /// 是否声明了检索工具（kb_search / code_lookup）——预检索开关。
    ///
    /// 统计 Active + warm（Candidate）技能：warm 挂载技能即使未激活正文，
    /// 其检索声明仍驱动请求启动时的预检索（P3 设计）。
    pub fn retrieval_enabled(&self) -> bool {
        let list = self.inner.read().unwrap_or_else(|e| e.into_inner());
        list.iter()
            .any(|a| a.tools.iter().any(|t| t == "kb_search" || t == "code_lookup"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_skill(id: &str, tools: &[&str]) -> Skill {
        Skill {
            id: id.to_string(),
            scope: SkillScope::System,
            name: id.to_string(),
            description: String::new(),
            priority: 50,
            tools: tools.iter().map(|t| t.to_string()).collect(),
            triggers: Vec::new(),
            top_k: None,
            min_score: None,
            max_docs: None,
            max_chunks_per_doc: None,
            enabled: true,
            version: 1,
            body: "规则正文".to_string(),
            file_path: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn activate_then_loaded_once_flag() {
        let s = ActiveSkillState::new();
        s.activate(&sample_skill("kb-search", &["kb_search"]), SkillLifetime::Turn, ActivationSource::Llm, true);
        assert!(s.is_loaded("kb-search"));
        // 覆盖刷新不重复 push
        s.activate(&sample_skill("kb-search", &["kb_search"]), SkillLifetime::Turn, ActivationSource::Llm, true);
        assert_eq!(s.activated().len(), 1);
    }

    #[test]
    fn activate_defer_not_loaded() {
        let s = ActiveSkillState::new();
        s.activate(&sample_skill("mermaid", &["read"]), SkillLifetime::Turn, ActivationSource::Llm, false);
        assert!(!s.is_loaded("mermaid"));
    }

    #[test]
    fn allowed_tools_union_and_retrieval() {
        let s = ActiveSkillState::new();
        assert_eq!(s.allowed_tools(), None); // 无激活 → None（放行全部）
        s.activate(&sample_skill("kb-search", &["kb_search", "read"]), SkillLifetime::Turn, ActivationSource::Llm, true);
        s.activate(&sample_skill("mermaid", &["read"]), SkillLifetime::Turn, ActivationSource::Llm, true);
        let tools = s.allowed_tools().expect("有激活技能");
        assert!(tools.contains(&"kb_search".to_string()));
        assert!(tools.contains(&"read".to_string()));
        assert!(s.retrieval_enabled());
    }

    #[test]
    fn deactivate_records_elapsed_and_removes() {
        let s = ActiveSkillState::new();
        s.activate(&sample_skill("kb-search", &["kb_search"]), SkillLifetime::Turn, ActivationSource::Llm, true);
        assert!(s.deactivate("kb-search"));
        assert!(!s.deactivate("kb-search")); // 已移除，二次停用失败
        assert!(s.activated().is_empty());
        assert_eq!(s.deactivated_elapsed().len(), 1);
    }

    #[test]
    fn empty_declared_tools_means_only_base() {
        let s = ActiveSkillState::new();
        s.activate(&sample_skill("note-writing", &[]), SkillLifetime::Turn, ActivationSource::Manual, true);
        let tools = s.allowed_tools().expect("有激活技能");
        assert!(tools.is_empty(), "未声明工具 → Some(空)（仅 BASE_TOOLS）");
    }

    #[test]
    fn warm_mount_does_not_unlock_tools_but_enables_retrieval() {
        let s = ActiveSkillState::new();
        s.activate_warm(&sample_skill("kb-search", &["kb_search"]));
        // warm 技能工具不解锁（仅 BASE_TOOLS）
        let tools = s.allowed_tools().expect("有激活技能");
        assert!(!tools.contains(&"kb_search".to_string()), "warm 技能不解锁检索工具");
        // warm 技能声明检索 → 预检索开关仍开启
        assert!(s.retrieval_enabled(), "warm 技能声明 kb_search → 预检索生效");
        // warm 技能不算已激活（read L3 / 正文注入不可用）
        assert!(s.active_only().is_empty());
        // LLM 激活后升级为 Active：工具解锁
        let skill = sample_skill("kb-search", &["kb_search"]);
        s.activate(&skill, SkillLifetime::Session, ActivationSource::Llm, true);
        assert!(s
            .allowed_tools()
            .expect("有激活技能")
            .contains(&"kb_search".to_string()));
        assert_eq!(s.active_only().len(), 1);
    }
}
