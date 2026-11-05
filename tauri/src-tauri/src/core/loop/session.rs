//! 事件溯源会话（第一天地基）——对齐 DeepSeek Harness `Session`（`docs/.../architecture-report.md` §4.1）。
//!
//! 会话是**仅追加**的事件日志；LLM 历史由 `derive_history` **派生**（增量缓存），
//! 从不单独存储。不变式：**"模型可见即已记录"**——任何进入模型请求的消息必须来自
//! `derive_history` 的投影，由 loop 运行时保证。
//!
//! 本模块为纯逻辑（可单测）；SQLite `session_events` 持久化在 Phase 3 接入（`commands/llm.rs`
//! 改造时），本层只暴露 `append`/`events`/`derive_history`，不感知存储。

use std::collections::{HashMap, HashSet};

use super::types::{ContentBlock, LlmMessage, LlmRole, TokenUsage, ToolCall};

/// 单条工具结果进入**模型历史**的字符上限（UI/日志仍保留完整结果）。
///
/// 一次 `grep` 最多 60k 字符、`webfetch` 50k、`read_document` 无局部上限；若原样进历史，
/// 两三条就能把上下文撑爆。截断保留首尾（尾部常含结论/错误），中间用标记说明。
pub const MAX_TOOL_RESULT_CHARS: usize = 24_000;

/// 单次请求历史中**所有**工具结果的累计字符预算。
///
/// 超出后从最旧的结果开始清理为占位符（保留 tool_call 配对与 `is_error`），
/// 既保住协议合法性，又让"最近几步"始终完整可见。
pub const MAX_HISTORY_TOOL_RESULT_CHARS: usize = 120_000;

/// turn 结束原因。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TurnEndReason {
    Completed,
    Blocked,
    Aborted,
    Error,
    MaxTokens,
    Interrupted,
}

/// 会话事件（持久事实的最小词汇表，对齐 DSH SessionEventMap 子集）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionEvent {
    TurnStart { turn: u32 },
    TurnEnd { turn: u32, reason: TurnEndReason },
    StepStart { turn: u32, step: u32 },
    StepEnd { turn: u32, step: u32 },
    /// 用户/注入上下文消息
    UserMessage { id: String, content: String, source: String },
    /// 组装后的助手消息（含工具调用与用量）
    AssistantMessage {
        content: String,
        tool_calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
        #[serde(default)]
        interrupted: bool,
    },
    /// 一次工具执行（记录模型发起的调用）
    ToolCall { call_id: String, name: String, arguments: String },
    /// 工具执行结果（`call_id` 与 AssistantMessage.tool_calls 配对）
    ToolResult { call_id: String, content: String, is_error: bool },
    /// 压缩摘要（shadowed 为被遮蔽的事件 seq，供回放/统计）
    CompactionSummary { summary: String, shadowed_seqs: Vec<u64> },
}

impl SessionEvent {
    /// 事件类型判别（持久化索引/查询用，与变体名一一对应）。
    pub fn type_name(&self) -> &'static str {
        match self {
            SessionEvent::TurnStart { .. } => "turn_start",
            SessionEvent::TurnEnd { .. } => "turn_end",
            SessionEvent::StepStart { .. } => "step_start",
            SessionEvent::StepEnd { .. } => "step_end",
            SessionEvent::UserMessage { .. } => "user_message",
            SessionEvent::AssistantMessage { .. } => "assistant_message",
            SessionEvent::ToolCall { .. } => "tool_call",
            SessionEvent::ToolResult { .. } => "tool_result",
            SessionEvent::CompactionSummary { .. } => "compaction_summary",
        }
    }
}

/// 一条带序号的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedEvent {
    pub seq: u64,
    pub event: SessionEvent,
}

/// 事件溯源会话。
#[derive(Debug, Clone)]
pub struct Session {
    /// 会话 ID
    id: String,
    /// 仅追加日志（seq 单调递增）
    events: Vec<PersistedEvent>,
    /// 派生缓存（`dirty` 时失效）
    derived: Vec<LlmMessage>,
    dirty: bool,
    /// 当前 turn/step（供 loop 用）
    current_turn: u32,
    current_step: u32,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            events: Vec::new(),
            derived: Vec::new(),
            dirty: true,
            current_turn: 0,
            current_step: 0,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// 追加事件，返回其 seq。
    pub fn append(&mut self, ev: SessionEvent) -> u64 {
        let seq = self.events.len() as u64;
        if matches!(&ev, SessionEvent::StepStart { .. }) {
            self.current_step += 1;
        }
        if matches!(&ev, SessionEvent::TurnStart { .. }) {
            self.current_turn += 1;
            self.current_step = 0;
        }
        self.events.push(PersistedEvent { seq, event: ev });
        self.dirty = true;
        seq
    }

    pub fn events(&self) -> &[PersistedEvent] {
        &self.events
    }

    pub fn current_turn(&self) -> u32 {
        self.current_turn
    }

    pub fn current_step(&self) -> u32 {
        self.current_step
    }

    /// 派生模型可见历史（增量缓存；工具调用/结果配对，孤儿被剔除）。
    ///
    /// 配对规则（对齐 OpenAI 协议与 `core::chat_types::group_tool_units`）：
    /// 一个 `call_id` 只有同时出现在 `AssistantMessage.tool_calls` **和** `ToolResult` 事件中
    /// 才视为已配对——未配对的工具调用不重放（assistant 侧剔除）、孤儿 tool 结果不重放
    /// （tool 侧剔除），避免 OpenAI 协议因 tool_call 无配对结果而拒绝请求。
    pub fn derive_history(&mut self) -> Vec<LlmMessage> {
        if !self.dirty && !self.derived.is_empty() {
            return self.derived.clone();
        }
        // 0) 压缩遮蔽集（可能有多条 CompactionSummary 叠加）
        let mut shadowed: HashSet<u64> = HashSet::new();
        for ev in &self.events {
            if let SessionEvent::CompactionSummary { shadowed_seqs, .. } = &ev.event {
                shadowed.extend(shadowed_seqs.iter().copied());
            }
        }
        // 1) 工具结果累计预算：从最新往回保留，放不下的旧结果清成占位符
        //    （保留事件本身 → tool_call 配对不受影响，只压缩内容）
        let mut kept_results: HashSet<u64> = HashSet::new();
        let mut result_budget = 0usize;
        for ev in self.events.iter().rev() {
            if shadowed.contains(&ev.seq) {
                continue;
            }
            if let SessionEvent::ToolResult { content, .. } = &ev.event {
                let n = content.chars().count();
                if result_budget + n <= MAX_HISTORY_TOOL_RESULT_CHARS {
                    kept_results.insert(ev.seq);
                    result_budget += n;
                }
            }
        }
        // 2) 两遍：先收集配对关系（只统计未被遮蔽的事件），再投影。
        let mut has_result: HashMap<&str, ()> = HashMap::new(); // call_id -> 存在结果
        let mut assistant_call: HashMap<&str, ()> = HashMap::new(); // call_id -> 存在 assistant 调用
        for ev in &self.events {
            if shadowed.contains(&ev.seq) {
                continue;
            }
            match &ev.event {
                SessionEvent::AssistantMessage { tool_calls, .. } => {
                    for tc in tool_calls {
                        assistant_call.insert(tc.id.as_str(), ());
                    }
                }
                SessionEvent::ToolResult { call_id, .. } => {
                    has_result.insert(call_id.as_str(), ());
                }
                _ => {}
            }
        }

        let mut out: Vec<LlmMessage> = Vec::new();
        for ev in &self.events {
            if shadowed.contains(&ev.seq) {
                continue;
            }
            match &ev.event {
                SessionEvent::UserMessage { content, .. } => {
                    out.push(LlmMessage::text(LlmRole::User, content.clone()));
                }
                SessionEvent::AssistantMessage { content, tool_calls, .. } => {
                    let mut blocks: Vec<ContentBlock> = Vec::new();
                    if !content.is_empty() {
                        blocks.push(ContentBlock::Text(content.clone()));
                    }
                    // 只保留已配对（有结果）的工具调用
                    let kept: Vec<&ToolCall> =
                        tool_calls.iter().filter(|tc| has_result.contains_key(tc.id.as_str())).collect();
                    for tc in &kept {
                        blocks.push(ContentBlock::ToolCall((*tc).clone()));
                    }
                    // 空消息（无文本且无有效工具调用）不进入历史
                    if !blocks.is_empty() {
                        out.push(LlmMessage { role: LlmRole::Assistant, content: blocks });
                    }
                }
                SessionEvent::ToolResult { call_id, content, is_error } => {
                    // 只重放已配对（有 assistant 调用）的工具结果
                    if assistant_call.contains_key(call_id.as_str()) {
                        let body = if kept_results.contains(&ev.seq) {
                            content.clone()
                        } else {
                            format!(
                                "（该工具结果已清理以节省上下文：原 {} 字符。如需该内容，请重新调用工具并缩小范围）",
                                content.chars().count()
                            )
                        };
                        out.push(LlmMessage {
                            role: LlmRole::Tool,
                            content: vec![ContentBlock::ToolResult {
                                tool_call_id: call_id.clone(),
                                content: body,
                                is_error: *is_error,
                            }],
                        });
                    }
                }
                // 压缩摘要：作为一条 user 消息注入，保证模型仍知道"前面发生过什么"
                SessionEvent::CompactionSummary { summary, .. } => {
                    out.push(LlmMessage::text(
                        LlmRole::User,
                        format!("[历史摘要（上下文压缩）]{summary}"),
                    ));
                }
                // turn/step 等非模型可见事件不进入历史
                _ => {}
            }
        }
        self.derived = out.clone();
        self.dirty = false;
        out
    }

    /// 溢出恢复：把最旧的若干个 **step** 整体遮蔽为一条压缩摘要（确定性，不调用 LLM）。
    ///
    /// 为什么按 step 切：一个 step = 一次 assistant 消息 + 它的全部工具结果；按 step 遮蔽
    /// 才能保证 tool_call 与 tool_result 同进同退（否则会产生协议拒绝的孤儿消息）。
    /// 返回 `true` 表示发生了压缩（hook 可以据此决定重试）。
    pub fn emergency_compact(&mut self) -> bool {
        // 已遮蔽集合：重复溢出时只统计"仍可见"的内容，避免越压越切
        let mut already: HashSet<u64> = HashSet::new();
        for ev in &self.events {
            if let SessionEvent::CompactionSummary { shadowed_seqs, .. } = &ev.event {
                already.extend(shadowed_seqs.iter().copied());
            }
        }
        let total: usize = self
            .events
            .iter()
            .filter(|e| !already.contains(&e.seq))
            .map(event_chars)
            .sum();
        if total <= MAX_HISTORY_TOOL_RESULT_CHARS {
            return false;
        }
        let target = total / 2;
        // step 起点索引（保证切分在 step 边界）
        let boundaries: Vec<usize> = self
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.event, SessionEvent::StepStart { .. }))
            .map(|(i, _)| i)
            .collect();
        if boundaries.len() < 3 {
            return false;
        }
        // 至少保留最后 2 个 step（当前轮的最近上下文）
        let max_cut_steps = boundaries.len() - 2;
        let mut cut_steps = 0usize;
        let mut remaining = total;
        while cut_steps < max_cut_steps && remaining > target {
            let start = boundaries[cut_steps];
            let end = boundaries
                .get(cut_steps + 1)
                .copied()
                .unwrap_or(self.events.len());
            let cut: usize = self.events[start..end]
                .iter()
                .filter(|e| !already.contains(&e.seq))
                .map(event_chars)
                .sum();
            remaining = remaining.saturating_sub(cut);
            cut_steps += 1;
        }
        if cut_steps == 0 {
            return false;
        }
        let cutoff = boundaries[cut_steps];
        let shadowed_seqs: Vec<u64> = self.events[..cutoff]
            .iter()
            .filter(|e| !already.contains(&e.seq))
            .map(|e| e.seq)
            .collect();
        if shadowed_seqs.is_empty() {
            return false;
        }
        let summary = build_compact_summary(&self.events[..cutoff], &already, shadowed_seqs.len());
        self.append(SessionEvent::CompactionSummary {
            summary,
            shadowed_seqs,
        });
        true
    }

    /// 模型可见事件的总字符数（粗略预算；不追求 tokenizer 精度）。
    #[cfg(test)]
    fn visible_chars(&self) -> usize {
        self.events.iter().map(event_chars).sum()
    }

    /// 历史长度（token 计量用占位，Phase 6 接真实 tokenizer）。
    pub fn derived_chars(&self) -> usize {
        self.derived
            .iter()
            .map(|m| m.plain_text().len())
            .sum::<usize>()
            + self
                .derived
                .iter()
                .flat_map(|m| m.tool_calls())
                .map(|tc| tc.arguments.len() + tc.name.len())
                .sum::<usize>()
    }
}

/// 单条工具结果进入历史前的截断（保留首尾，中间标注省略）。
///
/// UI/日志仍拿到完整结果（截断只发生在写入会话事件时），模型看到的是"首 + 尾 + 说明"，
/// 既保住开头上下文与结尾结论/错误，又不会让一条输出独占上下文。
pub fn truncate_tool_result(content: &str) -> String {
    let n = content.chars().count();
    if n <= MAX_TOOL_RESULT_CHARS {
        return content.to_string();
    }
    let head_len = MAX_TOOL_RESULT_CHARS * 4 / 5;
    let tail_len = MAX_TOOL_RESULT_CHARS - head_len;
    let head: String = content.chars().take(head_len).collect();
    let tail: String = content.chars().skip(n - tail_len).collect();
    format!(
        "{head}\n\n…[工具结果过长已截断：原 {n} 字符，仅保留首 {head_len} + 尾 {tail_len} 字符；如需完整内容请缩小参数范围后重新调用]\n\n{tail}"
    )
}

/// 估算一条事件在模型上下文中占用的字符数（压缩阈值判定用）。
fn event_chars(ev: &PersistedEvent) -> usize {
    match &ev.event {
        SessionEvent::UserMessage { content, .. } => content.chars().count(),
        SessionEvent::AssistantMessage { content, tool_calls, .. } => {
            content.chars().count()
                + tool_calls
                    .iter()
                    .map(|tc| tc.name.chars().count() + tc.arguments.chars().count())
                    .sum::<usize>()
        }
        SessionEvent::ToolCall { name, arguments, .. } => {
            name.chars().count() + arguments.chars().count()
        }
        SessionEvent::ToolResult { content, .. } => content.chars().count(),
        SessionEvent::CompactionSummary { summary, .. } => summary.chars().count(),
        _ => 0,
    }
}

/// 生成确定性压缩摘要（不调用 LLM）：工具调用统计 + 涉及文件 + 最近一段输出。
///
/// 为什么不用 LLM 摘要：溢出重试是**错误恢复路径**，此时再发一次大请求可能再次溢出，
/// 而且压缩必须是同步的（`retry_prepare` 是同步回调）。确定性摘要保证"一定能成功"。
#[allow(clippy::collapsible_if)] // 保留逐键判断的可读性（`rel_path`/`path`/`file` 三个候选键）
fn build_compact_summary(events: &[PersistedEvent], already: &HashSet<u64>, shadowed: usize) -> String {
    use std::collections::BTreeMap;
    let mut tools: BTreeMap<String, usize> = BTreeMap::new();
    let mut files: Vec<String> = Vec::new();
    let mut last_assistant = String::new();
    for ev in events {
        if already.contains(&ev.seq) {
            continue;
        }
        match &ev.event {
            SessionEvent::ToolCall { name, arguments, .. } => {
                *tools.entry(name.clone()).or_default() += 1;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(arguments) {
                    for key in ["rel_path", "path", "file", "relPath"] {
                        if let Some(p) = v.get(key).and_then(|x| x.as_str()) {
                            if !files.iter().any(|f| f == p) {
                                files.push(p.to_string());
                            }
                        }
                    }
                }
            }
            SessionEvent::AssistantMessage { content, .. } if !content.trim().is_empty() => {
                let chars: Vec<char> = content.chars().collect();
                let start = chars.len().saturating_sub(400);
                last_assistant = chars[start..].iter().collect();
            }
            _ => {}
        }
    }
    let mut out = format!("（上下文压缩）已省略 {shadowed} 条较早事件。");
    if !tools.is_empty() {
        let summary = tools
            .iter()
            .map(|(k, v)| format!("{k}×{v}"))
            .collect::<Vec<_>>()
            .join("、");
        out.push_str(&format!("已执行工具：{summary}。"));
    }
    if !files.is_empty() {
        files.truncate(8);
        out.push_str(&format!("涉及文件：{}。", files.join("、")));
    }
    if !last_assistant.trim().is_empty() {
        out.push_str(&format!("最近输出：{}", last_assistant.trim()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> SessionEvent {
        SessionEvent::UserMessage { id: "u".into(), content: content.into(), source: "user".into() }
    }

    fn assistant(content: &str, calls: Vec<(&str, &str)>) -> SessionEvent {
        SessionEvent::AssistantMessage {
            content: content.into(),
            tool_calls: calls
                .into_iter()
                .map(|(id, name)| ToolCall { id: id.into(), name: name.into(), arguments: "{}".into() })
                .collect(),
            usage: None,
            interrupted: false,
        }
    }

    fn tool_result(id: &str, content: &str) -> SessionEvent {
        SessionEvent::ToolResult { call_id: id.into(), content: content.into(), is_error: false }
    }

    #[test]
    fn derives_paired_tool_round_trip() {
        let mut s = Session::new("s1");
        s.append(user("列出文件"));
        s.append(assistant("", vec![("c1", "read")]));
        s.append(tool_result("c1", "file content"));
        s.append(assistant("结果：file content", vec![]));
        let h = s.derive_history();
        assert_eq!(h.len(), 4);
        assert_eq!(h[0].role, LlmRole::User);
        // assistant with tool_call
        assert_eq!(h[1].role, LlmRole::Assistant);
        assert_eq!(h[1].tool_calls().len(), 1);
        assert_eq!(h[1].tool_calls()[0].id, "c1");
        // tool result
        assert_eq!(h[2].role, LlmRole::Tool);
        // final assistant text
        assert_eq!(h[3].plain_text(), "结果：file content");
    }

    #[test]
    fn drops_orphan_tool_result_and_unpaired_tool_call() {
        let mut s = Session::new("s1");
        // assistant 调用 c1，但无结果 → 工具调用被剔除（保留文本）
        s.append(user("q"));
        s.append(assistant("text", vec![("c1", "read")]));
        // 孤立的 tool 结果 c2（无 assistant 调用）→ 剔除
        s.append(tool_result("c2", "orphan"));
        let h = s.derive_history();
        assert_eq!(h.len(), 2); // user + assistant(text only)
        assert_eq!(h[1].tool_calls().len(), 0);
        assert_eq!(h[1].plain_text(), "text");
    }

    #[test]
    fn empty_assistant_skipped() {
        let mut s = Session::new("s1");
        s.append(user("q"));
        s.append(assistant("", vec![]));
        s.append(assistant("final", vec![]));
        let h = s.derive_history();
        assert_eq!(h.len(), 2); // user + final
        assert_eq!(h[1].plain_text(), "final");
    }

    #[test]
    fn seq_monotonic() {
        let mut s = Session::new("s1");
        assert_eq!(s.append(user("a")), 0);
        assert_eq!(s.append(user("b")), 1);
        assert_eq!(s.append(SessionEvent::TurnStart { turn: 1 }), 2);
        assert_eq!(s.events().len(), 3);
    }

    #[test]
    fn truncates_oversized_tool_result_keeping_head_and_tail() {
        let big = format!("HEAD{}TAIL", "x".repeat(MAX_TOOL_RESULT_CHARS + 500));
        let out = truncate_tool_result(&big);
        assert!(out.chars().count() < big.chars().count(), "必须变短");
        assert!(out.starts_with("HEAD"), "保留开头");
        assert!(out.ends_with("TAIL"), "保留结尾");
        assert!(out.contains("工具结果过长已截断"), "必须给出截断说明");
        // 小结果原样返回
        assert_eq!(truncate_tool_result("small"), "small");
    }

    /// 回填历史时，超出累计预算的**最旧**工具结果被清成占位符，最近的保持完整；
    /// 配对关系（assistant tool_call ↔ tool_result）必须不受影响。
    #[test]
    fn derive_history_clears_oldest_tool_results_beyond_budget() {
        let mut s = Session::new("s1");
        s.append(user("q"));
        let big = "y".repeat(MAX_HISTORY_TOOL_RESULT_CHARS / 2 + 1000);
        for (i, id) in ["c1", "c2", "c3"].iter().enumerate() {
            s.append(assistant("", vec![(*id, "read")]));
            s.append(tool_result(id, &big));
            let _ = i;
        }
        let h = s.derive_history();
        // 3 组配对，全部保留在历史里（结构不变）
        assert_eq!(h.iter().filter(|m| m.role == LlmRole::Tool).count(), 3);
        let bodies: Vec<String> = h
            .iter()
            .filter(|m| m.role == LlmRole::Tool)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert!(bodies[0].contains("已清理"), "最旧的结果应被清理：{}", bodies[0]);
        assert!(!bodies[2].contains("已清理"), "最新结果必须完整");
    }

    /// 溢出恢复：按 step 边界遮蔽旧事件，生成一条摘要事件；配对不产生孤儿。
    #[test]
    fn emergency_compact_shadows_old_steps_and_keeps_pairs() {
        let mut s = Session::new("s1");
        s.append(user("开始"));
        let ids: Vec<String> = (0..6).map(|i| format!("c{i}")).collect();
        for (i, id) in ids.iter().enumerate() {
            s.append(SessionEvent::StepStart { turn: 1, step: i as u32 + 1 });
            s.append(assistant("", vec![(id.as_str(), "read")]));
            s.append(tool_result(
                id,
                &format!("{}{}", "z".repeat(30_000), i),
            ));
        }
        assert!(s.emergency_compact(), "超出预算应触发压缩");
        let h = s.derive_history();
        // 摘要注入 + 剩余的 step
        assert!(
            h.iter().any(|m| m.plain_text().contains("历史摘要（上下文压缩）")),
            "必须注入压缩摘要"
        );
        // 工具结果与 tool_call 仍然一一配对（无孤儿）
        let calls: usize = h.iter().map(|m| m.tool_calls().len()).sum();
        let results = h.iter().filter(|m| m.role == LlmRole::Tool).count();
        assert_eq!(calls, results, "遮蔽后仍须保持 tool_call/result 配对");
        // 至少保留最后一步
        assert!(h.iter().any(|m| m.role == LlmRole::Tool), "最近结果必须可见");
    }
}
