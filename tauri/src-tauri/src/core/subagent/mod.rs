//! 子代理：隔离上下文的只读深度调研执行器。
//!
//! 对齐 Reasonix 的 `read_subagent_result` 思想：子代理用独立的 request_id
//! 与独立上下文（独立 ActiveSkillState / 检索命中收集器）执行，只把有界摘要
//! 返回父链，完整输出存入 `AppState.subagent_results` 供
//! `read_subagent_result` 分页按需读取——父链上下文不被调研过程污染，
//! 大范围调研不会撑爆主对话窗口。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tauri::Manager;
use tokio_util::sync::CancellationToken;

use crate::core::agent::KbSearchConfig;
use crate::core::skill::activation::ActiveSkillState;

/// 子代理执行模式（P1-9：写型子代理 + 并行派发）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentMode {
    /// 只读调研：白名单 = 检索 + 读类工具，无写操作、无审批
    ReadOnly,
    /// 写型执行：白名单 = 只读集 + edit/delete，**强制挂载审批门**
    /// （每次写操作仍需用户确认，fail-closed；审批门未启用时回退只读，防无确认写）
    Write,
}

/// 子代理只读工具白名单：检索 + 读类工具。
///
/// 明确不含：
/// - `edit` / `delete`：写操作（只读子代理做调研，天然绕过审批弹窗）
/// - `remember` / `forget`：记忆写操作（子代理不得污染全局记忆）
/// - `activate_skill` / `deactivate_skill`：技能激活（子代理不共享父链技能态）
/// - `pomodoro`：前端交互工具（子代理无人机交互界面）
/// - `deep_research` / `read_subagent_result` / `spawn_subagent` / `parallel_research`：
///   防无限递归嵌套
pub fn read_only_tool_set() -> HashSet<String> {
    [
        "kb_search", "code_lookup", "read", "grep", "ls", "glob", "git_status", "git_diff", "webfetch",
        "search_memory",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 写型子代理工具白名单：只读集 + edit/delete/write。
///
/// 写操作经审批门确认；`remember`/`forget` 仍排除（记忆写入仅主链负责，
/// 避免子代理未经用户感知地污染全局记忆）。
pub fn write_tool_set() -> HashSet<String> {
    let mut set = read_only_tool_set();
    set.insert("edit".to_string());
    set.insert("delete".to_string());
    set.insert("write".to_string());
    set.insert("multi_edit".to_string());
    set.insert("git_commit".to_string());
    set.insert("git_checkout".to_string());
    set
}

// 子代理摘要字符预算见 crate::core::agent::limits（SUBAGENT_SUMMARY_CHARS）
pub use crate::core::agent::limits::SUBAGENT_SUMMARY_CHARS;

/// 子代理执行规格
pub struct SubagentSpec {
    /// 独立 request_id（工具调用轨迹/事件隔离的关键）
    pub request_id: String,
    /// 任务描述（作为子代理的唯一 user 消息）
    pub task: String,
    /// 轮次上限
    pub max_turns: usize,
    /// 返回父链的摘要字符预算
    pub summary_chars: usize,
    /// 执行模式：只读调研 / 写型执行（写型强制审批门）
    pub mode: SubagentMode,
}

/// 子代理执行结果
pub struct SubagentOutcome {
    /// 有界摘要（≤ summary_chars，父链直接消费）
    pub summary: String,
    /// 完整输出（存 AppState 供 read_subagent_result 分页读取）
    pub full_output: String,
    /// 流是否失败
    pub failed: bool,
}

/// RAII 清理：确保子代理结束后（无论正常返回还是被父链取消级联 drop）
/// 都清理工具总线，避免 sub-* request_id 的事件桶残留在全局 ToolCallBus
/// （总线虽有 MAX_TRACKED_REQUESTS=64 容量兜底，及时清理仍更干净）。
struct ToolBusGuard {
    request_id: String,
}

impl ToolBusGuard {
    fn new(request_id: &str) -> Self {
        Self {
            request_id: request_id.to_string(),
        }
    }
}

impl Drop for ToolBusGuard {
    fn drop(&mut self) {
        crate::core::agent::tools::tool_call_bus().clear(&self.request_id);
    }
}

/// 子代理执行器（无状态；单一职责：构造只读 Agent 并跑完一次流式调研）。
pub struct SubagentRunner;

impl SubagentRunner {
    /// 执行一次只读调研子代理（v3：自研 LoopAgent 内核，替代 rig）。
    ///
    /// `adapter` / `search_config` / `base_rules` 由调用方（deep_research 工具闭包）
    /// 从 AppState 组装；本类型不依赖任何命令层代码。
    ///
    /// 取消传播：LoopAgent::turn 内偏置 select! 优先响应取消，工具调度器同样感知
    /// cancel token；父链取消后子代理不会成为孤儿任务。
    pub async fn run(
        adapter: Arc<dyn crate::core::r#loop::LlmAdapter>,
        search_config: KbSearchConfig,
        base_rules: String,
        spec: &SubagentSpec,
    ) -> SubagentOutcome {
        // 独立上下文：新的 request_id、空技能激活态、独立检索命中收集器
        let mut sub_cfg = search_config;
        sub_cfg.request_id = spec.request_id.clone();
        sub_cfg.skill_state = Arc::new(ActiveSkillState::new());
        sub_cfg.search_sink = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        sub_cfg.skill_id = None;
        // P0-8：子代理关闭技能软门禁——工具白名单已在注册表层过滤（只读/写型集合），
        // 技能声明类工具（kb_search 等）无需技能激活即可执行；若继承主对话的
        // skill_gating=true，空技能态（allowed_tools()==None）会误判为「未激活」并
        // 返回引导，导致子代理丧失检索能力。
        sub_cfg.skill_gating = false;

        // 按模式选择工具白名单：只读（调研）或写型（编辑/删除，挂审批门）
        let whitelist = match spec.mode {
            SubagentMode::ReadOnly => read_only_tool_set(),
            SubagentMode::Write => write_tool_set(),
        };
        // 写型子代理强制审批门（来自 AppState）；审批门未启用时回退只读白名单，
        // 防止无用户确认的写操作（fail-safe：宁可少做，不可无确认地改文件）。
        let approval_gate: Option<Arc<crate::core::approval::ApprovalGate>> = match spec.mode {
            SubagentMode::Write => {
                let gate = sub_cfg
                    .app_handle
                    .state::<crate::AppState>()
                    .approval_gate
                    .clone();
                if gate.is_none() {
                    log::warn!(
                        "[subagent] 写型子代理但审批门未启用，回退只读白名单 request_id={}",
                        spec.request_id
                    );
                }
                gate
            }
            SubagentMode::ReadOnly => None,
        };
        let effective_whitelist = match spec.mode {
            SubagentMode::ReadOnly => whitelist,
            SubagentMode::Write => {
                if approval_gate.is_some() {
                    whitelist
                } else {
                    read_only_tool_set()
                }
            }
        };

        // 新内核执行：LoopAgent + 迁移工具注册表（按白名单过滤）+ 审批门（写型）+ 事件 sink。
        // 子代理不带技能 Hook（注册表白名单已过滤，对齐 rig 版 narrow_tools=false 语义）。
        let cancel = sub_cfg.cancel.clone().unwrap_or_else(CancellationToken::new);
        let full_registry = crate::core::agent::loop_tools::build_loop_tool_registry(sub_cfg.clone());
        let registry =
            crate::core::agent::loop_tools::filter_registry(&full_registry, &effective_whitelist);
        let config = crate::core::r#loop::LoopConfig::new(spec.max_turns, base_rules);
        let mut agent = crate::core::r#loop::LoopAgent::new(adapter, config, &spec.request_id);
        agent.set_tools(Arc::new(registry));
        agent.set_sink(Arc::new(crate::core::agent::loop_tools::BusToolEventSink::new(
            sub_cfg.clone(),
        )));
        if let Some(gate) = approval_gate {
            agent.add_hook(Arc::new(crate::core::agent::loop_hooks::ApprovalHook { gate }));
        }

        log::info!(
            "[subagent] 开始调研 request_id={} task_len={} max_turns={}",
            spec.request_id,
            spec.task.len(),
            spec.max_turns
        );
        let sub_start = std::time::Instant::now();
        crate::core::trace::stage_start(&spec.request_id, "subagent", &format!("task_len={}", spec.task.len()));

        // 注册清理 guard：正常路径 return 前手动 clear 一次（幂等），
        // 被父链取消 drop 时由 Drop 兜底清理。
        let _bus_guard = ToolBusGuard::new(&spec.request_id);

        let mut full = String::new();
        let mut failed = false;
        let outcome = agent
            .turn(
                &spec.request_id,
                crate::core::r#loop::LlmMessage::text(
                    crate::core::r#loop::LlmRole::User,
                    spec.task.clone(),
                ),
                cancel,
                &mut |ev| {
                    if let crate::core::r#loop::LoopEvent::Delta(t) = ev {
                        full.push_str(&t);
                    }
                },
            )
            .await;
        match &outcome {
            crate::core::r#loop::TurnOutcome::Failed { err, .. } => {
                failed = true;
                log::warn!("[subagent] 调研失败 request_id={} err={}", spec.request_id, err);
            }
            crate::core::r#loop::TurnOutcome::Cancelled { .. } => {
                failed = true;
                log::info!("[subagent] 调研被父链取消 request_id={}", spec.request_id);
            }
            _ => {}
        }

        // 正常路径手动清理（幂等；被取消 drop 时由 ToolBusGuard::drop 兜底）
        crate::core::agent::tools::tool_call_bus().clear(&spec.request_id);

        let summary = if full.trim().is_empty() {
            if failed {
                "调研未能完成（模型调用失败或被取消），请重试或检查 LLM 配置。".to_string()
            } else {
                "(子代理未产生输出)".to_string()
            }
        } else if failed {
            // 部分输出 + 流中断：明确标注，避免与返回头部 failed=true 语义矛盾
            format!(
                "（调研在生成过程中中断，以下为已产出的部分内容）\n\n{}",
                truncate_chars(&full, spec.summary_chars)
            )
        } else {
            truncate_chars(&full, spec.summary_chars)
        };
        // P1-13：子代理输出（可能源自不可信文档）回传父链前做提示注入防护，
        // 命中可疑指令时包裹并提示忽略（不裁剪，可审计）
        let summary = crate::core::security::wrap_suspicious(&summary);
        crate::core::trace::stage_end(
            &spec.request_id,
            "subagent",
            if failed { "error" } else { "ok" },
            sub_start.elapsed().as_millis() as u64,
            &format!("full_chars={} summary_chars={}", full.len(), summary.len()),
        );
        // 子代理 trace 事件不转发前端（主链按主 request_id drain），及时清理防残留；
        // 被父链取消 drop 时由 TraceBus 容量治理（MAX_TRACKED_TRACES）兜底。
        crate::core::trace::trace_bus().clear(&spec.request_id);
        log::info!(
            "[subagent] 调研完成 request_id={} full_chars={} summary_chars={} failed={}",
            spec.request_id,
            full.len(),
            summary.len(),
            failed
        );

        SubagentOutcome {
            summary,
            full_output: full,
            failed,
        }
    }
}

/// 有界 LRU 结果存储：子代理完整输出按"最近访问"淘汰（最多保留 `max` 条）。
///
/// 用单调递增访问序号实现 LRU（纯内存、无时间依赖、可单测）：
/// - `insert`：写入并记访问序；新 id 且已达上限时淘汰访问序最旧的一条
/// - `get`：读取并刷新访问序
/// - 相比"满则清空"，LRU 保留最近使用的结果，`read_subagent_result` 更不易失效
pub struct LruResultStore {
    map: Mutex<HashMap<String, (u64, String)>>,
    seq: AtomicU64,
    max: usize,
}

impl LruResultStore {
    pub fn new(max: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            max,
        }
    }

    /// 写入结果；新 id 且已达上限时淘汰最久未访问的一条。
    pub fn insert(&self, id: String, text: String) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            if !map.contains_key(&id) && map.len() >= self.max {
                if let Some(oldest) = map
                    .iter()
                    .min_by_key(|(_, (s, _))| *s)
                    .map(|(k, _)| k.clone())
                {
                    map.remove(&oldest);
                }
            }
            map.insert(id, (seq, text));
        }
    }

    /// 读取并刷新访问序（LRU 语义）。
    pub fn get(&self, id: &str) -> Option<String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.map.lock() {
            match map.get_mut(id) {
                Some(entry) => {
                    entry.0 = seq;
                    Some(entry.1.clone())
                }
                None => None,
            }
        } else {
            None
        }
    }

    /// 当前条目数（测试/观测用）。
    pub fn len(&self) -> usize {
        self.map.lock().map(|m| m.len()).unwrap_or(0)
    }
}

/// 按字符数安全截断（不切 UTF-8 边界）。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n\n…(已截断，完整结果可用 read_subagent_result 分页读取)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_tool_set_excludes_writes_and_recursion() {
        let set = read_only_tool_set();
        assert!(set.contains("kb_search"));
        assert!(set.contains("code_lookup"));
        assert!(set.contains("read"));
        assert!(set.contains("grep"));
        assert!(set.contains("ls"));
        assert!(set.contains("git_status"));
        assert!(!set.contains("edit"), "只读子代理不得包含 edit");
        assert!(!set.contains("delete"), "只读子代理不得包含 delete");
        assert!(!set.contains("activate_skill"), "不得包含技能激活");
        assert!(!set.contains("deactivate_skill"), "不得包含技能激活");
        assert!(!set.contains("pomodoro"), "不得包含前端交互工具");
        assert!(!set.contains("deep_research"), "防无限递归");
        assert!(!set.contains("read_subagent_result"), "防无限递归");
    }

    #[test]
    fn write_tool_set_includes_edits_and_excludes_recursion() {
        let set = write_tool_set();
        // 写型：只读集 + edit/delete/write/multi_edit/git_commit/git_checkout
        assert!(set.contains("edit"));
        assert!(set.contains("delete"));
        assert!(set.contains("write"));
        assert!(set.contains("multi_edit"));
        assert!(set.contains("git_commit"));
        assert!(set.contains("git_checkout"));
        assert!(set.contains("read"));
        assert!(set.contains("grep"));
        // 递归子代理与记忆写操作仍排除
        assert!(!set.contains("spawn_subagent"), "防无限递归");
        assert!(!set.contains("parallel_research"), "防无限递归");
        assert!(!set.contains("deep_research"), "防无限递归");
        assert!(!set.contains("read_subagent_result"), "防无限递归");
        assert!(!set.contains("remember"), "记忆写仅主链负责");
        assert!(!set.contains("forget"), "记忆写仅主链负责");
        // 写型 = 只读 ∪ {edit, delete, write, multi_edit, git_commit, git_checkout}
        let expected: HashSet<String> = {
            let mut s = read_only_tool_set();
            s.insert("edit".to_string());
            s.insert("delete".to_string());
            s.insert("write".to_string());
            s.insert("multi_edit".to_string());
            s.insert("git_commit".to_string());
            s.insert("git_checkout".to_string());
            s
        };
        assert_eq!(set, expected);
    }

    #[test]
    fn truncate_chars_safe_and_bounded() {
        let s = "你好".repeat(100);
        let t = truncate_chars(&s, 50);
        assert!(t.starts_with("你好"));
        assert!(t.ends_with("…(已截断，完整结果可用 read_subagent_result 分页读取)"));
        // 短文本不截断
        assert_eq!(truncate_chars("abc", 100), "abc");
    }

    #[test]
    fn lru_store_evicts_oldest_and_refreshes_on_get() {
        let s = LruResultStore::new(2);
        s.insert("a".into(), "A".into());
        s.insert("b".into(), "B".into());
        assert_eq!(s.len(), 2);
        // 读取 a 刷新其访问序
        assert_eq!(s.get("a"), Some("A".into()));
        // 插入 c 应淘汰 b（最久未访问）
        s.insert("c".into(), "C".into());
        assert_eq!(s.get("b"), None, "应淘汰最久未访问的 b");
        assert_eq!(s.get("a"), Some("A".into()));
        assert_eq!(s.get("c"), Some("C".into()));
        // 更新已存在 id 不淘汰
        s.insert("a".into(), "A2".into());
        assert_eq!(s.get("a"), Some("A2".into()));
        assert_eq!(s.get("c"), Some("C".into()));
        assert_eq!(s.len(), 2);
    }
}