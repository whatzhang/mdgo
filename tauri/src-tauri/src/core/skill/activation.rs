//! 技能激活状态（渐进式披露 L2 加载的核心共享状态）。
//!
//! 单一职责：维护当前请求中已激活的技能集合，供 Agent 钩子（指令注入、
//! 工具白名单窄化）与技能工具（activate_skill / deactivate_skill）共享读写。
//!
//! 激活决策完全交由 LLM：模型根据技能目录（L1 元数据）自主调用
//! `activate_skill` 加载技能正文（L2）；查询启动时的显式预激活（会话挂载 /
//! `/技能名` 手动触发）也写入同一状态，两类来源统一处理。

use std::collections::HashMap;
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

    /// 由字符串恢复（DB 读回时使用；未知值回退为 Llm）
    pub fn from_str(s: &str) -> Self {
        match s {
            "attached" => ActivationSource::Attached,
            "manual" => ActivationSource::Manual,
            _ => ActivationSource::Llm,
        }
    }
}

/// 技能激活状态（`Arc` 共享，跨工具闭包与 Agent 钩子）。
///
/// 每次请求开始时新建实例，请求期间 LLM 可动态增删。
#[derive(Debug)]
pub struct ActiveSkillState {
    inner: RwLock<Vec<Skill>>,
    /// 各技能激活时刻（key = "scope:id" → Instant），用于按技能独立统计执行耗时
    activated_at: RwLock<HashMap<String, std::time::Instant>>,
    /// 请求期间被停用的技能及停用时刻耗时（用于执行统计补录，避免中途停用的技能漏记）
    deactivated: RwLock<Vec<(Skill, u64)>>,
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
            activated_at: RwLock::new(HashMap::new()),
            deactivated: RwLock::new(Vec::new()),
        }
    }

    fn key(scope: &str, id: &str) -> String {
        format!("{}:{}", scope, id)
    }

    /// 激活一个技能（幂等：重复激活以最新版本覆盖，并刷新激活时刻）。
    pub fn activate(&self, skill: Skill) {
        let key = Self::key(skill.scope.as_str(), &skill.id);
        self.activated_at
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, std::time::Instant::now());
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
    ///
    /// 停用时记录「激活时刻 → 停用时刻」耗时到 `deactivated`，供执行统计补录；
    /// 计时清理按匹配到的技能精确 key 进行，避免 id 含冒号时误删其他技能。
    pub fn deactivate(&self, skill_id: &str) -> bool {
        let mut list = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = list.iter().position(|s| s.id == skill_id) {
            let skill = list.remove(pos);
            let key = Self::key(skill.scope.as_str(), &skill.id);
            let elapsed = self
                .activated_at
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);
            self.activated_at
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            self.deactivated
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .push((skill, elapsed));
            true
        } else {
            false
        }
    }

    /// 当前激活的技能列表（按激活顺序）。
    pub fn activated(&self) -> Vec<Skill> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 当前激活技能的精简计时信息（scope, id, 激活至今毫秒数）。
    ///
    /// 供技能执行统计按技能独立计时：LLM 动态激活的技能以其激活时间为起点，
    /// 预激活（会话挂载 / 手动触发）技能以请求早期激活时间为起点。
    /// 只返回 id/scope/耗时，不克隆 Skill body，避免指标路径搬运大文本。
    pub fn activated_elapsed(&self) -> Vec<(String, String, u64)> {
        let list = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let times = self.activated_at.read().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        list.iter()
            .map(|s| {
                let elapsed = times
                    .get(&Self::key(s.scope.as_str(), &s.id))
                    .map(|t| now.duration_since(*t).as_millis() as u64)
                    .unwrap_or(0);
                (s.scope.as_str().to_string(), s.id.clone(), elapsed)
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
            .map(|(s, elapsed)| (s.scope.as_str().to_string(), s.id.clone(), *elapsed))
            .collect()
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
