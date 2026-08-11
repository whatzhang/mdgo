//! 对话历史上下文压缩:超预算时降级为「摘要 + 滑窗」,而不是拒绝请求。
//!
//! # 设计(SOLID)
//!
//! - [`ContextCompressor`]:压缩策略抽象(依赖倒置:命令层只依赖此 trait,不感知具体策略)
//! - [`SlidingWindowCompressor`]:滑窗策略,纯内存、无外部依赖,任何情况下可用(兜底地基)
//! - [`SummarizeThenWindowCompressor`]:摘要 + 滑窗组合策略,LLM 摘要失败自动降级滑窗(安全可用)
//! - [`HistorySummarizer`]:摘要引擎抽象(依赖倒置:组合策略依赖抽象,不依赖 LLM 客户端具体类型)
//!
//! 新增压缩策略 = 实现 [`ContextCompressor`] 并注册,无需改动现有代码(开闭原则)。

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// 一条对话轮次(轻量视图)。
///
/// 与 `services::llm::ChatMessage` 解耦,避免 core → services 的架构倒置;
/// 由命令层在边界处做一行转换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
    /// 助手消息携带的工具调用（仅 `role == "assistant"` 时可能有值）
    pub tool_calls: Option<Vec<crate::core::ToolCallDto>>,
    /// 工具结果消息关联的调用 ID（仅 `role == "tool"` 时可能有值）
    pub tool_call_id: Option<String>,
}

impl ChatTurn {
    /// 是否为工具结果消息（须与其配对的 assistant 工具调用同侧切分）
    pub fn is_tool_message(&self) -> bool {
        self.role == "tool"
    }
}

/// 将历史按「工具调用单元」分组：一条 assistant（带 `tool_calls`）与其
/// 紧随其后的连续 tool 结果消息同组，其余消息各自成组。
///
/// 压缩切分必须以单元为单位，否则会把 assistant 的 tool_call 与 tool 结果
/// 切到不同侧，产生「孤儿 tool 消息」导致 OpenAI 协议拒绝请求。
fn group_turns(history: &[ChatTurn]) -> Vec<Vec<ChatTurn>> {
    let mut units: Vec<Vec<ChatTurn>> = Vec::new();
    for turn in history {
        if turn.is_tool_message() {
            // 并入当前组（防御：若没有当前组（孤儿 tool 消息）则自成一组）
            match units.last_mut() {
                Some(last) => last.push(turn.clone()),
                None => units.push(vec![turn.clone()]),
            }
        } else {
            units.push(vec![turn.clone()]);
        }
    }
    units
}

/// 压缩结果
#[derive(Debug, Clone)]
pub struct CompressedHistory {
    /// 压缩后的历史轮次(不含当前问题;调用方负责转模型消息)
    pub turns: Vec<ChatTurn>,
    /// 被压缩掉的字符数(用于日志/前端提示)
    pub dropped_chars: usize,
    /// 实际使用的策略名(观测用)
    pub strategy: &'static str,
}

/// 压缩策略抽象:将历史压缩到不超过 `budget` 字符。
///
/// 未超预算时必须原样返回(零副作用);实现不得 panic,失败路径必须可降级。
#[async_trait]
pub trait ContextCompressor: Send + Sync {
    /// `cancel` 用于中断压缩过程中的异步步骤(如 LLM 摘要),压缩实现应在取消时尽快返回。
    async fn compress(
        &self,
        history: &[ChatTurn],
        budget: usize,
        cancel: CancellationToken,
    ) -> CompressedHistory;
}

/// 摘要引擎抽象:把一段历史压缩为要点摘要文本。
///
/// 返回 `None` 表示摘要不可用(LLM 未配置/调用失败),由组合策略降级。
#[async_trait]
pub trait HistorySummarizer: Send + Sync {
    async fn summarize(
        &self,
        turns: &[ChatTurn],
        max_chars: usize,
        cancel: CancellationToken,
    ) -> Option<String>;
}

/// 滑窗策略:从最新往旧累积,预算耗尽即停;始终保留最近消息。
///
/// - 不超预算:原样返回(`strategy = "none"`,`dropped_chars = 0`)
/// - 超预算:丢弃最旧消息,保证「至少保留一条」且总长不超预算
///
/// 纯内存、无外部依赖,是压缩链路的兜底地基(任何组合策略的最终防线)。
pub struct SlidingWindowCompressor;

#[async_trait]
impl ContextCompressor for SlidingWindowCompressor {
    async fn compress(
        &self,
        history: &[ChatTurn],
        budget: usize,
        _cancel: CancellationToken,
    ) -> CompressedHistory {
        let units = group_turns(history);
        let total: usize = units
            .iter()
            .map(|u| u.iter().map(|t| t.content.len()).sum::<usize>())
            .sum();
        if total <= budget {
            return CompressedHistory {
                turns: history.to_vec(),
                dropped_chars: 0,
                strategy: "none",
            };
        }
        let mut kept_units: Vec<Vec<ChatTurn>> = Vec::new();
        let mut used = 0usize;
        // 从后往前按「工具调用单元」保留（最新优先，保证 tool_call 与 tool 结果成对）
        for unit in units.iter().rev() {
            let len = unit.iter().map(|t| t.content.len()).sum::<usize>();
            if !kept_units.is_empty() && used + len > budget {
                break;
            }
            used += len;
            kept_units.push(unit.clone());
        }
        kept_units.reverse();
        let turns: Vec<ChatTurn> = kept_units.into_iter().flatten().collect();
        CompressedHistory {
            dropped_chars: total.saturating_sub(used),
            turns,
            strategy: "sliding-window",
        }
    }
}

/// 摘要 + 滑窗组合策略:历史过长时,先把最旧的一部分用摘要引擎压成一条
/// system 消息,再滑窗保证不超预算。
///
/// - 摘要失败(返回 `None`):降级为纯滑窗,压缩永不阻断主流程
/// - 摘要后仍超预算:滑窗兜底
pub struct SummarizeThenWindowCompressor {
    /// 摘要引擎(依赖注入:由组装点传入,本类型不负责创建)
    summarizer: Arc<dyn HistorySummarizer>,
    /// 摘要消息的最大字符数
    max_summary_chars: usize,
}

impl SummarizeThenWindowCompressor {
    pub fn new(summarizer: Arc<dyn HistorySummarizer>, max_summary_chars: usize) -> Self {
        Self {
            summarizer,
            max_summary_chars,
        }
    }
}

#[async_trait]
impl ContextCompressor for SummarizeThenWindowCompressor {
    async fn compress(
        &self,
        history: &[ChatTurn],
        budget: usize,
        cancel: CancellationToken,
    ) -> CompressedHistory {
        let total: usize = history.iter().map(|t| t.content.len()).sum();
        if total <= budget {
            return CompressedHistory {
                turns: history.to_vec(),
                dropped_chars: 0,
                strategy: "none",
            };
        }

        // 1. 最旧的 2/3 用于摘要,最近的 1/3 原样保留；
        //    切分按「工具调用单元」边界进行，避免切断 tool_call 与 tool 结果配对
        let units = group_turns(history);
        let split_idx = units.len() * 2 / 3;
        if split_idx == 0 {
            // 历史过短无摘要价值,直接滑窗(避免空摘要空转)
            return SlidingWindowCompressor
                .compress(history, budget, CancellationToken::new())
                .await;
        }
        let old: Vec<ChatTurn> = units[..split_idx].iter().flatten().cloned().collect();
        let recent: Vec<ChatTurn> = units[split_idx..].iter().flatten().cloned().collect();

        // 2. 摘要(取消信号透传;失败时降级为纯滑窗)
        let Some(summary) = self
            .summarizer
            .summarize(&old, self.max_summary_chars, cancel)
            .await
        else {
            // 摘要已失败,滑窗无异步中断点,无需再携带取消信号
            return SlidingWindowCompressor
                .compress(history, budget, CancellationToken::new())
                .await;
        };

        // 3. 摘要恒保留(它是压缩的核心,若摘要本身超预算则降级纯滑窗),
        //    滑窗只作用于 recent 部分,避免摘要被当作"最旧消息"砍掉
        if summary.len() > budget {
            return SlidingWindowCompressor
                .compress(history, budget, CancellationToken::new())
                .await;
        }
        let recent_budget = budget.saturating_sub(summary.len());
        // recent 滑窗为纯内存操作(无异步中断点),无需携带取消信号
        let recent_result = SlidingWindowCompressor
            .compress(&recent, recent_budget, CancellationToken::new())
            .await;

        // 4. 摘要作为 system 消息置于最近消息之前
        let mut merged = Vec::with_capacity(recent_result.turns.len() + 1);
        merged.push(ChatTurn {
            role: "system".into(),
            content: summary,
            tool_calls: None,
            tool_call_id: None,
        });
        merged.extend(recent_result.turns);
        let kept_chars: usize = merged.iter().map(|t| t.content.len()).sum();
        CompressedHistory {
            dropped_chars: total.saturating_sub(kept_chars),
            turns: merged,
            strategy: "summarize+window",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: &str, content: &str) -> ChatTurn {
        ChatTurn {
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// 构造一条 assistant 工具调用消息 + 一条配对 tool 结果消息
    fn tool_pair(call_id: &str, call_content: &str, result: &str) -> Vec<ChatTurn> {
        vec![
            ChatTurn {
                role: "assistant".into(),
                content: call_content.into(),
                tool_calls: Some(vec![crate::core::ToolCallDto {
                    id: call_id.into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }]),
                tool_call_id: None,
            },
            ChatTurn {
                role: "tool".into(),
                content: result.into(),
                tool_calls: None,
                tool_call_id: Some(call_id.into()),
            },
        ]
    }

    fn long_history(n: usize, len: usize) -> Vec<ChatTurn> {
        (0..n)
            .map(|i| turn("user", &format!("消息{i}:{}", "x".repeat(len))))
            .collect()
    }

    #[tokio::test]
    async fn sliding_window_passthrough_within_budget() {
        let h = long_history(3, 10);
        let r = SlidingWindowCompressor.compress(&h, 10_000, CancellationToken::new()).await;
        assert_eq!(r.turns, h);
        assert_eq!(r.dropped_chars, 0);
        assert_eq!(r.strategy, "none");
    }

    #[tokio::test]
    async fn sliding_window_keeps_latest_within_budget() {
        let h = long_history(10, 100); // 1000 chars
        let r = SlidingWindowCompressor.compress(&h, 250, CancellationToken::new()).await;
        let used: usize = r.turns.iter().map(|t| t.content.len()).sum();
        assert!(used <= 250);
        // 最后一条消息(最新)必须保留
        assert_eq!(r.turns.last(), h.last());
        // 至少保留一条
        assert!(!r.turns.is_empty());
        assert!(r.dropped_chars > 0);
        assert_eq!(r.strategy, "sliding-window");
    }

    struct FakeSummarizer(&'static str);

    #[async_trait]
    impl HistorySummarizer for FakeSummarizer {
        async fn summarize(
            &self,
            _turns: &[ChatTurn],
            _max_chars: usize,
            _cancel: CancellationToken,
        ) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    struct FailingSummarizer;

    #[async_trait]
    impl HistorySummarizer for FailingSummarizer {
        async fn summarize(
            &self,
            _turns: &[ChatTurn],
            _max_chars: usize,
            _cancel: CancellationToken,
        ) -> Option<String> {
            None
        }
    }

    #[tokio::test]
    async fn summarize_then_window_replaces_old_with_summary() {
        let h = long_history(9, 100); // 900 chars
        let c = SummarizeThenWindowCompressor::new(Arc::new(FakeSummarizer("[摘要]")), 200);
        let r = c.compress(&h, 300, CancellationToken::new()).await;
        assert_eq!(r.strategy, "summarize+window");
        // 摘要 system 消息必须存在
        assert!(r
            .turns
            .iter()
            .any(|t| t.role == "system" && t.content == "[摘要]"));
        let used: usize = r.turns.iter().map(|t| t.content.len()).sum();
        assert!(used <= 300);
        // 最近消息(原始)必须保留
        assert_eq!(r.turns.last(), h.last());
    }

    #[tokio::test]
    async fn summarize_failure_falls_back_to_sliding_window() {
        let h = long_history(9, 100);
        let c = SummarizeThenWindowCompressor::new(Arc::new(FailingSummarizer), 200);
        let r = c.compress(&h, 300, CancellationToken::new()).await;
        assert_eq!(r.strategy, "sliding-window");
        let used: usize = r.turns.iter().map(|t| t.content.len()).sum();
        assert!(used <= 300);
    }

    #[tokio::test]
    async fn sliding_window_keeps_tool_pairs_together() {
        // 构造：旧文本消息 + 工具调用对（assistant + tool 结果），预算只够保留最新单元
        let mut h = long_history(5, 50); // 5 条旧消息（ASCII x，bytes≈字符数）
        h.extend(tool_pair("call_1", "", &"result text ".repeat(20))); // 工具对 240 bytes
        // 预算 300（content.len() 以字节计）：旧消息被部分丢弃，但工具对必须完整保留
        let r = SlidingWindowCompressor
            .compress(&h, 300, CancellationToken::new())
            .await;
        assert_eq!(r.strategy, "sliding-window");
        // 最新消息（tool 结果）必须保留
        assert_eq!(r.turns.last(), h.last());
        // 工具对必须完整保留（assistant + tool 成对，不能出现孤儿 tool 消息）
        assert!(r.turns.iter().any(|t| t.role == "tool"));
        assert_eq!(
            r.turns
                .iter()
                .filter(|t| t.role == "tool")
                .count(),
            r.turns
                .iter()
                .filter(|t| t.tool_calls.as_ref().is_some_and(|c| !c.is_empty()))
                .count(),
            "tool 结果消息数量必须等于 assistant 工具调用数量（成对）"
        );
        let used: usize = r.turns.iter().map(|t| t.content.len()).sum();
        assert!(used <= 300);
    }

    #[tokio::test]
    async fn group_turns_keeps_orphan_tool_safe() {
        // 防御：孤儿 tool 消息（无配对 assistant）不应 panic；
        // 并入前一组（可能被一起保留/丢弃，但不会单独留下造成协议孤儿）
        let mut h = long_history(2, 10);
        h.push(ChatTurn {
            role: "tool".into(),
            content: "孤儿结果".into(),
            tool_calls: None,
            tool_call_id: Some("call_x".into()),
        });
        let groups = group_turns(&h);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].len(), 2);
        assert_eq!(groups[1][1].role, "tool");
    }
}
