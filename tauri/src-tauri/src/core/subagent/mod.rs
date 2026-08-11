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

use futures_util::StreamExt;
use rig_agent::agent::MultiTurnStreamItem;
use rig_agent::streaming::StreamingChat;
use rig_core::completion::Message;
use rig_core::providers::openai;
use rig_core::streaming::StreamedAssistantContent;

use crate::core::agent::{build_rag_agent, KbSearchConfig};
use crate::core::skill::activation::ActiveSkillState;
use crate::core::skill::SkillRegistry;

/// 子代理只读工具白名单：检索 + 读类工具。
///
/// 明确不含：
/// - `edit` / `delete`：写操作（子代理只做调研，天然绕过审批弹窗）
/// - `activate_skill` / `deactivate_skill`：技能激活（子代理不共享父链技能态）
/// - `pomodoro`：前端交互工具（子代理无人机交互界面）
/// - `deep_research` / `read_subagent_result`：防无限递归嵌套
pub fn read_only_tool_set() -> HashSet<String> {
    [
        "kb_search", "code_lookup", "read", "grep", "list_files", "git_status",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 子代理默认轮次上限（深度调研需要比主对话 `DEFAULT_MAX_TURNS=6` 更大的预算）
pub const SUBAGENT_MAX_TURNS: usize = 12;

/// 子代理默认摘要字符预算（返回父链的有界结果）
pub const SUBAGENT_SUMMARY_CHARS: usize = 4_000;

/// 子代理执行规格
pub struct SubagentSpec {
    /// 独立 request_id（工具调用轨迹/事件隔离的关键）
    pub request_id: String,
    /// 调研任务描述（作为子代理的唯一 user 消息）
    pub task: String,
    /// 轮次上限
    pub max_turns: usize,
    /// 返回父链的摘要字符预算
    pub summary_chars: usize,
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
    /// 执行一次只读调研子代理。
    ///
    /// `model` / `search_config` / `skill_registry` / `base_rules` 由调用方
    /// （deep_research 工具闭包）从 AppState 组装；本类型不依赖任何命令层代码。
    ///
    /// 取消传播：rig 0.41 的 agent 工具在流式 poll 栈内顺序执行（不 spawn 独立
    /// task，`bg_handle` spawn 仅存在于测试代码），因此父链取消后 drop stream 会
    /// 级联 drop 正在 await 的本执行体，子代理不会成为孤儿任务。
    pub async fn run(
        model: openai::CompletionModel,
        search_config: KbSearchConfig,
        skill_registry: Arc<SkillRegistry>,
        base_rules: String,
        spec: &SubagentSpec,
    ) -> SubagentOutcome {
        // 独立上下文：新的 request_id、空技能激活态、独立检索命中收集器
        let cancel = search_config.cancel.clone();
        let mut sub_cfg = search_config;
        sub_cfg.request_id = spec.request_id.clone();
        sub_cfg.skill_state = Arc::new(ActiveSkillState::new());
        sub_cfg.search_sink = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        sub_cfg.skill_id = None;

        // 只读工具子集 + 无预检索上下文 + 无技能目录 + 无审批门 + 更大轮次预算
        let agent = build_rag_agent(
            model,
            "",
            sub_cfg,
            skill_registry,
            String::new(),
            base_rules,
            None,
            spec.max_turns,
            Some(&read_only_tool_set()),
            false, // 子代理不窄化：注册表已白名单过滤，模型可见全部只读工具
        );

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
        let mut stream = agent
            .stream_chat(Message::user(spec.task.clone()), Vec::<Message>::new())
            .into_future()
            .await;
        loop {
            // 父链取消（用户点"停止"）时立即中止：rig 工具在 poll 栈内执行，
            // 但本循环显式 select! 取消可在工具间间隙/等待时更早响应。
            let item = if let Some(cancel) = &cancel {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        failed = true;
                        log::info!("[subagent] 调研被父链取消 request_id={}", spec.request_id);
                        break;
                    }
                    item = stream.next() => item,
                }
            } else {
                stream.next().await
            };
            let Some(item) = item else { break };
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(
                    StreamedAssistantContent::Text(text),
                )) => full.push_str(&text.text),
                Ok(MultiTurnStreamItem::FinalResponse(_)) => {}
                Err(e) => {
                    failed = true;
                    log::warn!("[subagent] 调研流失败 request_id={} err={}", spec.request_id, e);
                    // 流失败后 rig 通常不再产出,显式 break 避免继续 poll
                    break;
                }
                _ => {}
            }
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
        assert!(set.contains("list_files"));
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