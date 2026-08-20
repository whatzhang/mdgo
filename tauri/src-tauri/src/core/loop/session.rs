//! 事件溯源会话（第一天地基）——对齐 DeepSeek Harness `Session`（`docs/.../architecture-report.md` §4.1）。
//!
//! 会话是**仅追加**的事件日志；LLM 历史由 `derive_history` **派生**（增量缓存），
//! 从不单独存储。不变式：**"模型可见即已记录"**——任何进入模型请求的消息必须来自
//! `derive_history` 的投影，由 loop 运行时保证。
//!
//! 本模块为纯逻辑（可单测）；SQLite `session_events` 持久化在 Phase 3 接入（`commands/llm.rs`
//! 改造时），本层只暴露 `append`/`events`/`derive_history`，不感知存储。

use std::collections::HashMap;

use super::types::{ContentBlock, LlmMessage, LlmRole, TokenUsage, ToolCall};

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
        // 两遍：先收集配对关系，再投影。
        let mut has_result: HashMap<&str, ()> = HashMap::new(); // call_id -> 存在结果
        let mut assistant_call: HashMap<&str, ()> = HashMap::new(); // call_id -> 存在 assistant 调用
        for ev in &self.events {
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
                        out.push(LlmMessage {
                            role: LlmRole::Tool,
                            content: vec![ContentBlock::ToolResult {
                                tool_call_id: call_id.clone(),
                                content: content.clone(),
                                is_error: *is_error,
                            }],
                        });
                    }
                }
                // turn/step/compaction 等非模型可见事件不进入历史
                _ => {}
            }
        }
        self.derived = out.clone();
        self.dirty = false;
        out
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
}
