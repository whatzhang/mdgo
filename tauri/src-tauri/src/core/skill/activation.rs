//! 技能激活状态（渐进式披露 L2 加载的核心共享状态）。
//!
//! 单一职责：维护当前请求中已激活的技能集合，供 Agent 钩子（指令注入、
//! 工具白名单窄化）与技能工具（activate_skill / deactivate_skill）共享读写。
//!
//! 激活决策完全交由 LLM：模型根据技能目录（L1 元数据）自主调用
//! `activate_skill` 加载技能正文（L2）；查询启动时的显式预激活（会话挂载 /
//! `/技能名` 手动触发）也写入同一状态，两类来源统一处理。

use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use super::Skill;

/// 技能激活来源（替代旧 matcher 的匹配层级）。
///
/// 决策已交由 LLM：本地不再做关键词 / embedding / 模糊匹配，
/// 仅保留三类显式激活来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivationSource {
    /// 会话挂载（chat_session_skills 快照）
    #[serde(rename = "attached")]
    Attached,
    /// 手动触发（/技能名）
    #[serde(rename = "manual")]
    Manual,
    /// LLM 通过 activate_skill 工具按技能目录（L1）决策激活
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
}

/// 技能激活状态（`Arc` 共享，跨工具闭包与 Agent 钩子）。
///
/// 每次请求开始时新建实例，请求期间 LLM 可动态增删。
#[derive(Debug)]
pub struct ActiveSkillState {
    inner: RwLock<Vec<Skill>>,
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
        }
    }

    /// 激活一个技能（幂等：重复激活以最新版本覆盖）。
    pub fn activate(&self, skill: Skill) {
        let mut list = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = list
            .iter_mut()
            .find(|s| s.id == skill.id && s.scope == skill.scope)
        {
            *existing = skill;
        } else {
            list.push(skill);
        }
    }

    /// 停用一个技能（按 ID 匹配，任意作用域），返回是否找到并停用。
    pub fn deactivate(&self, skill_id: &str) -> bool {
        let mut list = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = list.iter().position(|s| s.id == skill_id) {
            list.remove(pos);
            true
        } else {
            false
        }
    }

    /// 当前激活的技能列表（按激活顺序）。
    pub fn activated(&self) -> Vec<Skill> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 合并后的 L2 指令正文（多技能按激活顺序拼接，空指令技能跳过）。
    pub fn instructions(&self) -> String {
        let list = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();
        for skill in list.iter() {
            if skill.body.trim().is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push_str("\n\n---\n\n");
            }
            out.push_str(&format!("## {}\n\n{}", skill.name, skill.body.trim()));
        }
        out
    }

    /// 工具白名单（激活技能声明工具的并集）。
    ///
    /// 语义（`Option` 区分三种状态）：
    /// - `None`：无技能激活，无工具约束，放行全部
    /// - `Some(空列表)`：激活技能均未声明工具，放行全部
    /// - `Some(list)`：仅放行声明工具
    pub fn allowed_tools(&self) -> Option<Vec<String>> {
        let list = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if list.is_empty() {
            return None;
        }
        let mut set = std::collections::HashSet::new();
        for skill in list.iter() {
            for t in &skill.tools {
                set.insert(t.clone());
            }
        }
        Some(set.into_iter().collect())
    }

    /// 是否声明了检索工具（kb_search / code_lookup）——预检索开关。
    pub fn retrieval_enabled(&self) -> bool {
        let list = self.inner.read().unwrap_or_else(|e| e.into_inner());
        list.iter()
            .any(|s| s.tools.iter().any(|t| t == "kb_search" || t == "code_lookup"))
    }
}
