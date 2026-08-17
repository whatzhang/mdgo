use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use rig_agent::agent::MultiTurnStreamItem;
use rig_agent::streaming::StreamingChat;
use rig_core::completion::message::{ToolCall, ToolFunction};
use rig_core::completion::{AssistantContent, Message};
use rig_core::streaming::StreamedAssistantContent;
use rig_core::OneOrMany;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::core::agent::{
    KbSearchConfig, aggregate_hits, build_chat_agent, build_context_text, build_rag_agent,
    load_agent_rules,
};
use crate::core::agent::tools::tool_call_bus;
use crate::core::context::{
    ChatTurn, ContextCompressor, SummarizeThenWindowCompressor, tokens_to_chars_budget,
};
use crate::core::skill::activation::{ActivationSource, ActiveSkillState};
use crate::core::skill::context::{SkillExecutionContext, build_skill_catalog, resolve_preactivated};
use crate::core::skill::SkillStore;
use crate::core::{call_embedding_query, SearchHit};
use crate::services::llm::{LLMClient, UsageInfo, usage_to_info};

// ─── 后端消息长度预算（集中定义见 crate::core::agent::limits） ───
use crate::core::agent::limits::{MAX_MESSAGE_CHARS, MAX_MESSAGE_TOKENS, SUMMARY_MAX_CHARS};

// ─── 事件类型 ───

#[derive(Clone, Serialize)]
pub struct RagStatus {
    pub request_id: String,
    pub stage: String,
    pub message: String,
}

#[derive(Clone, Serialize)]
pub struct RagDelta {
    pub request_id: String,
    pub content: String,
}

#[derive(Clone, Serialize)]
pub struct RagSource {
    pub doc_name: String,
    pub score: f32,
    pub text: String,
    /// OPML 节点路径 JSON 数组（仅 OPML 文件有值）
    pub path_json: Option<String>,
    /// 代码符号名（仅代码文件有值），前端可用于高亮匹配
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    /// 代码符号类型（仅代码文件有值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct RagDone {
    pub request_id: String,
    pub content: String,
    pub sources: Vec<RagSource>,
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    /// 本次请求命中 provider 缓存的输入 token 数（缓存命中率 = cached / prompt）
    #[serde(default)]
    pub cached_input_tokens: u32,
    /// 本次请求写入 provider 缓存的输入 token 数
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
}

#[derive(Clone, Serialize)]
pub struct LlmDelta {
    pub request_id: String,
    pub content: String,
}

#[derive(Clone, Serialize)]
pub struct LlmDone {
    pub request_id: String,
    pub content: String,
}

#[derive(Clone, Serialize)]
pub struct CommandError {
    pub request_id: String,
    pub message: String,
}

// ─── AppState 扩展 ───

/// 可取消的任务注册表
pub struct TaskRegistry {
    pub tasks: Mutex<HashMap<String, CancellationToken>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// 注册一个可取消任务，返回 CancellationToken
    pub async fn register(&self, request_id: &str) -> CancellationToken {
        let token = CancellationToken::new();
        let mut map = self.tasks.lock().await;
        map.insert(request_id.to_string(), token.clone());
        token
    }

    /// 取消指定任务
    pub async fn cancel(&self, request_id: &str) {
        let mut map = self.tasks.lock().await;
        if let Some(token) = map.remove(request_id) {
            token.cancel();
        }
    }

    /// 任务完成后注销
    pub async fn unregister(&self, request_id: &str) {
        let mut map = self.tasks.lock().await;
        map.remove(request_id);
    }
}

// ─── 辅助函数 ───

/// 将消息历史（去掉最后一条当前问题）压缩到预算内。
///
/// 超预算时按策略压缩（摘要+滑窗，或纯滑窗兜底），压缩永不失败；
/// 返回压缩结果供调用方决定是否提示前端。
/// 应用会话压缩检查点（P0-5）：若检查点存在且历史中包含 cutoff 消息，
/// 则用摘要 system 消息替换 cutoff 之前的消息，避免每次请求对全部历史重算。
///
/// - 检查点不存在 / cutoff 消息已被前端裁剪 → 原样返回（安全降级为全量压缩）
/// - `cutoff_msg_id` 为 `None`（旧数据）→ 原样返回
fn apply_compaction_checkpoint(
    messages: &[crate::services::llm::ChatMessage],
    checkpoint: Option<&crate::core::context::CompactionState>,
) -> Vec<crate::services::llm::ChatMessage> {
    let Some(cp) = checkpoint else {
        return messages.to_vec();
    };
    let Some(cutoff_id) = &cp.cutoff_msg_id else {
        return messages.to_vec();
    };
    let Some(idx) = messages.iter().position(|m| m.id.as_deref() == Some(cutoff_id.as_str()))
    else {
        return messages.to_vec();
    };
    let mut out = Vec::with_capacity(messages.len() - idx + 1);
    out.push(crate::services::llm::ChatMessage {
        id: None,
        role: "system".into(),
        content: cp.summary.clone(),
        tool_calls: None,
        tool_call_id: None,
    });
    out.extend(messages[idx..].iter().cloned());
    out
}

async fn prepare_history(
    messages: &[crate::services::llm::ChatMessage],
    compressor: &dyn ContextCompressor,
    budget_tokens: usize,
    cancel: CancellationToken,
) -> crate::core::context::CompressedHistory {
    let turns: Vec<ChatTurn> = messages[..messages.len().saturating_sub(1)]
        .iter()
        .map(|m| ChatTurn {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_calls: m.tool_calls.clone(),
            tool_call_id: m.tool_call_id.clone(),
        })
        .collect();
    compressor
        .compress(&turns, tokens_to_chars_budget(budget_tokens), cancel)
        .await
}

/// 历史压缩预算（token）：默认按模型上下文窗口的 **80%** 计算（用户确认的口径），
/// 随模型窗口缩放，避免固定阈值在大窗口模型上过早压缩；
/// - 未配置上下文窗口（context_length=0）→ 回退固定 `MAX_MESSAGE_TOKENS`（旧行为兜底）；
/// - 极小窗口受下限保护（防预算过小导致每次请求都压缩）。
fn compression_budget_tokens(context_length: u32) -> usize {
    const MIN_BUDGET_TOKENS: usize = 2_000;
    if context_length > 0 {
        (context_length as usize * 8 / 10).max(MIN_BUDGET_TOKENS)
    } else {
        MAX_MESSAGE_TOKENS
    }
}

/// 将压缩后的历史轮次转为 Rig history
fn chat_turns_to_history(turns: &[ChatTurn]) -> Vec<Message> {
    // 统计历史中实际存在的 tool 结果 id：过滤「孤儿 tool_call」
    // （成功但空输出的工具其 result 为空串，前端不生成 tool 消息），
    // 否则 OpenAI 协议会因 tool_call 无配对结果而拒绝请求（review 修复）。
    let tool_result_ids: std::collections::HashSet<&str> = turns
        .iter()
        .filter(|t| t.role == "tool")
        .filter_map(|t| t.tool_call_id.as_deref())
        .collect();
    turns
        .iter()
        .map(|t| match t.role.as_str() {
            "system" => Message::system(&t.content),
            "assistant" => {
                let has_tools = t.tool_calls.as_ref().is_some_and(|c| !c.is_empty());
                if !has_tools {
                    return Message::assistant(&t.content);
                }
                let mut contents: Vec<AssistantContent> = Vec::new();
                if !t.content.is_empty() {
                    contents.push(AssistantContent::text(&t.content));
                }
                for tc in t.tool_calls.iter().flatten() {
                    // 仅保留历史中有对应 tool 结果消息的调用（孤儿调用剔除）
                    if !tool_result_ids.contains(tc.id.as_str()) {
                        continue;
                    }
                    // 参数为模型原始 JSON 字符串：解析失败时降级为空对象（防御，不阻断请求）
                    let args = serde_json::from_str(&tc.arguments)
                        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
                    contents.push(AssistantContent::ToolCall(ToolCall::new(
                        tc.id.clone(),
                        ToolFunction::new(tc.name.clone(), args),
                    )));
                }
                // 全部被过滤且无文本：补占位文本，避免空 assistant 消息（协议同样会拒绝）
                if contents.is_empty() {
                    contents.push(AssistantContent::text("（此前发起的部分工具调用结果为空，已省略）"));
                }
                Message::Assistant {
                    id: None,
                    content: OneOrMany::many(contents)
                        .expect("contents 至少含一个占位文本，expect 安全"),
                }
            }
            "tool" => Message::tool_result_with_call_id(
                t.tool_call_id.clone().unwrap_or_default(),
                t.tool_call_id.clone(),
                &t.content,
            ),
            _ => Message::user(&t.content),
        })
        .collect()
}

/// 流式消费循环的"下一个事件或取消"等待器。
///
/// 用 `tokio::select!` 同时等待流事件与取消信号:
/// - `Ok(Some(item))`:正常事件
/// - `Ok(None)`:流正常结束
/// - `Err(())`:取消已触发 —— 调用方应立即 return,select 会丢弃挂起中的
///   stream future;rig 的流是惰性驱动的,drop 会尽力断开底层 reqwest 连接
///   (连接可能被连接池复用,但取消不再依赖下一个 SSE chunk 到达)。
async fn next_or_cancel<T>(
    stream: &mut (impl futures_util::Stream<Item = T> + Unpin),
    cancel: &CancellationToken,
) -> Result<Option<T>, ()> {
    tokio::select! {
        biased; // 取消与流事件同时就绪时取消优先(严格"立即断开")
        _ = cancel.cancelled() => Err(()),
        item = stream.next() => Ok(item),
    }
}

/// 计算各作用域技能基础目录（供 read 工具按需读取已激活技能的参考文档，渐进式披露 L3）。
///
/// - system：应用资源目录下的 `skills`（开发期资源未同步时回退到源码资源目录）
/// - global：用户全局技能目录 `{appdata}/com.mdgo/skills`
/// - project：`{打开目录}/.mdgo/skills`
///
/// read 工具按「激活技能 → 作用域匹配 → 基础目录/skill_id」定位，仅限已激活技能。
fn resolve_skill_bases(app: &AppHandle, dir_path: &str) -> Vec<(String, String)> {
    let mut bases = Vec::new();
    let sys = app
        .path()
        .resource_dir()
        .map(|r| r.join("skills"))
        .unwrap_or_default();
    let sys = if sys.exists() {
        sys
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("skills")
    };
    bases.push(("system".to_string(), sys.to_string_lossy().to_string()));
    bases.push((
        "global".to_string(),
        SkillStore::global_skills_dir().to_string_lossy().to_string(),
    ));
    bases.push((
        "project".to_string(),
        SkillStore::project_skills_dir(dir_path)
            .to_string_lossy()
            .to_string(),
    ));
    bases
}

/// 将检索命中构建为引用来源列表（按 doc_name 去重，合并文本与 path_json，取最高分）。
///
/// 预检索与 kb_search / code_lookup 工具命中共用此逻辑，保证引用格式一致。
fn build_sources(selected: &[(SearchHit, f32)]) -> Vec<RagSource> {
    let mut source_dedup: std::collections::HashMap<String, RagSource> = std::collections::HashMap::new();
    for (hit, _) in selected {
        let doc_name = hit.doc_name.clone();
        let text = hit.text.clone();
        let path_json = hit.path_json.clone();
        let score = hit.score;
        match source_dedup.entry(doc_name.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                // 合并文本：仅当新文本未包含在已有文本中时才追加
                if !existing.text.contains(&text) && !text.contains(&existing.text) {
                    existing.text.push('\n');
                    existing.text.push_str(&text);
                }
                // 取最高分
                if score > existing.score {
                    existing.score = score;
                }
                // 合并 path_json（OPML/FreeMind 路径追加）
                if let Some(ref pj) = path_json {
                    match existing.path_json {
                        Some(ref mut existing_path) => {
                            if !existing_path.contains(pj) {
                                existing_path.push(',');
                                existing_path.push_str(pj);
                            }
                        }
                        None => {
                            existing.path_json = Some(pj.clone());
                        }
                    }
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RagSource {
                    doc_name,
                    score,
                    text,
                    path_json,
                    symbol_name: hit.symbol_name.clone(),
                    symbol_kind: hit.symbol_kind.clone(),
                });
            }
        }
    }
    let mut sources: Vec<RagSource> = source_dedup.into_values().collect();
    sources.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    sources
}

/// 合并 kb_search / code_lookup 工具的检索命中到引用来源（按 doc_name 去重、保留最高分）。
///
/// 请求期间 Agent 调用的检索工具命中累积在 `search_sink`，rag:done 发射前
/// 与预检索来源合并，保证 LLM 驱动的检索同样出现在前端"引用"列表。
async fn merge_search_sink(
    sources: Vec<RagSource>,
    sink: &tokio::sync::Mutex<Vec<(SearchHit, f32)>>,
) -> Vec<RagSource> {
    let hits = {
        let mut guard = sink.lock().await;
        std::mem::take(&mut *guard)
    };
    if hits.is_empty() {
        return sources;
    }
    let mut map: std::collections::HashMap<String, RagSource> = sources
        .into_iter()
        .map(|s| (s.doc_name.clone(), s))
        .collect();
    for s in build_sources(&hits) {
        match map.entry(s.doc_name.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let existing = entry.get_mut();
                if !existing.text.contains(&s.text) && !s.text.contains(&existing.text) {
                    existing.text.push('\n');
                    existing.text.push_str(&s.text);
                }
                if s.score > existing.score {
                    existing.score = s.score;
                }
                if let Some(ref pj) = s.path_json {
                    match existing.path_json {
                        Some(ref mut ep) => {
                            if !ep.contains(pj) {
                                ep.push(',');
                                ep.push_str(pj);
                            }
                        }
                        None => {
                            existing.path_json = Some(pj.clone());
                        }
                    }
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(s);
            }
        }
    }
    let mut out: Vec<RagSource> = map.into_values().collect();
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// 发送错误事件（rag:error / llm:error）
fn emit_command_error(app: &AppHandle, channel: &str, request_id: &str, message: String) {
    let _ = app.emit(
        channel,
        CommandError {
            request_id: request_id.to_string(),
            message,
        },
    );
}

/// 转发该请求挂起的工具调用事件（消费式），供前端渲染工具调用轨迹。
///
/// 工具闭包在 Rig 流式内部执行，无法直接 emit Tauri 事件，故先写入
/// [`crate::core::agent::tools::ToolCallBus`]，由流式循环在此处统一转发。
fn emit_pending_tool_events(app: &AppHandle, request_id: &str) {
    for event in tool_call_bus().drain(request_id) {
        // 同步写入任务状态中心（后台任务快照：切回页面恢复工具卡片，与页面解耦）。
        // call 事件用 event.seq 建档，result 事件按 call_seq 回填 ok/summary。
        if let Some(state) = app.try_state::<crate::AppState>() {
            let tasks = state.agent_tasks.clone();
            if event.kind == "call" {
                tasks.add_tool_call(
                    request_id,
                    event.seq,
                    &event.tool,
                    &event.args_preview,
                    event.skill_id.clone(),
                );
            } else {
                tasks.update_tool_result(request_id, event.call_seq, event.ok, &event.summary);
            }
        }
        // 直接序列化 ToolCallEvent：skill_id 经 skip_serializing_if 仅在有值时输出，
        // 避免手动重建 JSON 丢失字段（如技能来源 skill_id）。
        let mut payload = match serde_json::to_value(&event) {
            Ok(v) => v,
            Err(_) => continue,
        };
        payload["request_id"] = serde_json::Value::String(request_id.to_string());
        let channel = if event.kind == "call" {
            "agent:tool_call"
        } else {
            "agent:tool_result"
        };
        let _ = app.emit(channel, payload);
    }
}

/// 消费式转发该请求的 trace 事件（`trace:event`，前端按 request_id 过滤渲染）。
fn emit_pending_trace_events(app: &AppHandle, request_id: &str) {
    let events = crate::core::trace::trace_bus().drain(request_id);
    if !events.is_empty() {
        // 同步写入任务状态中心（后台任务快照：切回页面恢复阶段面板）。
        if let Some(state) = app.try_state::<crate::AppState>() {
            let tasks = state.agent_tasks.clone();
            for ev in &events {
                if let Ok(v) = serde_json::to_value(ev) {
                    tasks.add_trace_event(request_id, v);
                }
            }
        }
        let _ = app.emit(
            "trace:event",
            serde_json::json!({
                "request_id": request_id,
                "events": events,
            }),
        );
    }
}

/// P0 防幻觉一致性校验（Action Claim Guard，声明表驱动）：模型在最终回答中声称
/// "已完成某操作"，但本请求**未调用对应工具**时，向回答追加一致性提醒——
/// 封死「声称已执行但实际未执行」的失败模式（典型：日程/文件写操作未调用时
/// LLM 编造"已创建/已保存"）。
///
/// 规则由 [`crate::core::agent::ACTION_CLAIMS`] 声明表定义（verbs × objects ×
/// required_tools），替代硬编码关键词：新增动作只需在表中增加一行。
///
/// 判定刻意收窄以降低误报：
/// - 声称词：完成式（已创建/创建了/已添加…），不含"尝试/计划/将"
/// - 对象词：明确动作对象（日程/文件/文档…）
/// 两者同时出现且请求内未调用该动作声明的工具时才追加提醒。
fn apply_anti_hallucination_guard(content: &mut String, tools_called: &[String]) {
    for claim in crate::core::agent::ACTION_CLAIMS {
        let verb_hit = claim.verbs.iter().any(|w| content.contains(w));
        if !verb_hit {
            continue;
        }
        let object_hit = claim.objects.iter().any(|o| content.contains(o));
        if !object_hit {
            continue;
        }
        let tool_hit = claim
            .required_tools
            .iter()
            .any(|t| tools_called.iter().any(|c| c == t))
            || claim
                .observe_tools
                .iter()
                .any(|t| tools_called.iter().any(|c| c == t));
        if tool_hit {
            continue;
        }
        content.push_str(&format!(
            "\n\n> ⚠️ 一致性提醒：本次回复声称已完成「{}」相关操作，但本次请求未实际调用 {} 工具，**该操作可能未执行**。请确认对应功能已生效，或重新发起操作。",
            claim.id,
            claim.required_tools.join(" / ")
        ));
        log::warn!(
            "[anti-hallucination] 声称 {} 但未调用对应工具（tools_called={:?}）",
            claim.id,
            tools_called
        );
        // P2-9：守卫触发计数（Hallucination Rate 分子）
        crate::core::agent::agent_quality()
            .hallucination_warnings
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 首个命中即生效，避免多动作声称堆叠多条提醒
        break;
    }
}

/// P0-2 Grounding Validator（后置兜底）：本地知识回答必须有依据。
///
/// 前置约束为主（rag_agent.md 的 Grounding Policy：本地事实断言必须引用来源），
/// 本函数只做兜底：当最终回答出现**本地文件引用信号**（文件名/路径后缀等），
/// 但本次请求既未检索知识库（未调用 kb_search/code_lookup 或无命中来源）、
/// 也未调用任何本地文件观察工具（read/ls/glob/grep）时，追加"依据不足"提醒——
/// 封死「引用不存在的文件 / 凭记忆断言本地内容」的引用幻觉。
///
/// 判定刻意收窄以降低误报：
/// - 只认文件引用信号（.md/.rs/docs/ 等具体后缀与路径前缀），不认宽泛的"项目/配置"词；
/// - 只要调用过本地观察工具（哪怕未检索）即视为有依据，不提醒。
fn apply_grounding_validator(content: &mut String, has_sources: bool, tools_called: &[String]) {
    const FILE_REF_SIGNALS: &[&str] = &[
        ".md", ".rs", ".json", ".yaml", ".yml", ".toml", ".js", ".ts", "docs/", "src/",
    ];
    let has_file_ref = FILE_REF_SIGNALS.iter().any(|s| content.contains(s));
    if !has_file_ref {
        return;
    }
    if has_sources {
        return;
    }
    // 调用过本地文件观察/操作工具即视为有依据（含写工具：写完引用刚写的文件是合理行为；
    // 含 git 观察工具：复述 git 状态不算凭空断言）
    let observed_local = tools_called.iter().any(|t| {
        matches!(
            t.as_str(),
            "read" | "ls" | "glob" | "grep" | "write" | "edit" | "multi_edit" | "delete"
                | "git_status" | "git_diff" | "git_commit" | "git_checkout"
        )
    });
    if observed_local {
        return;
    }
    content.push_str(
        "\n\n> ⚠️ 依据提醒：本次回答引用了本地文件/路径，但本次请求未检索知识库、也未读取本地文件，上述引用可能缺少依据，请核查重要信息。如确有依据，请补充引用来源。",
    );
    log::warn!(
        "[grounding] 回答含本地文件引用但无检索/读取来源（tools_called={:?}）",
        tools_called
    );
    // P2-9：守卫触发计数（Hallucination Rate 分子）
    crate::core::agent::agent_quality()
        .hallucination_warnings
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// 收集本次请求的技能执行输入（预激活 ∪ LLM 动态激活 ∪ 中途停用，去重），供批量落库。
///
/// 耗时按技能独立计时：优先取该技能「激活时刻 → 请求结束」的实际时长
/// （`ActiveSkillState::activated_elapsed`），查不到时回退请求总时长。
/// 中途被停用的技能经 `deactivated_elapsed` 补录，避免激活后又停用的执行漏记。
fn collect_skill_exec_inputs(
    skill_ctx: Option<&SkillExecutionContext>,
    active_skills: &ActiveSkillState,
    fallback_duration_ms: u64,
) -> Vec<crate::core::skill::metrics::ExecInput> {
    use crate::core::skill::metrics::ExecInput;
    use std::collections::{HashMap, HashSet};

    let mut recorded: HashSet<(String, String)> = HashSet::new();
    let mut inputs: Vec<ExecInput> = Vec::new();

    // 各技能的实际耗时（scope:id → ms）：轻量读取一次（不克隆 Skill body）
    let active = active_skills.activated_elapsed();
    let mut elapsed_by_key: HashMap<String, u64> = HashMap::new();
    for (scope, id, elapsed) in &active {
        elapsed_by_key.insert(format!("{}:{}", scope, id), *elapsed);
    }

    // 预激活技能（手动触发 / 会话挂载）
    if let Some(ctx) = skill_ctx {
        for m in &ctx.matches {
            recorded.insert((m.scope.clone(), m.skill_id.clone()));
            let key = format!("{}:{}", m.scope, m.skill_id);
            let duration = elapsed_by_key
                .get(&key)
                .copied()
                .unwrap_or(fallback_duration_ms);
            inputs.push(ExecInput {
                skill_id: m.skill_id.clone(),
                scope: m.scope.clone(),
                source: m.source,
                match_score: m.match_score,
                duration_ms: duration,
            });
        }
    }
    // LLM 会话中激活的技能（当前激活 + 中途停用，不在预激活上下文内）：按 Llm 来源补录，避免重复
    let deactivated = active_skills.deactivated_elapsed();
    for (scope, id, elapsed) in active.iter().chain(deactivated.iter()) {
        let key = (scope.clone(), id.clone());
        if recorded.insert(key) {
            inputs.push(ExecInput {
                skill_id: id.clone(),
                scope: scope.clone(),
                source: ActivationSource::Llm,
                match_score: 1.0,
                duration_ms: *elapsed,
            });
        }
    }
    inputs
}

/// 批量记录技能执行结果（在 spawn_blocking 中调用，避免阻塞 async runtime）。
///
/// 记录范围 = 预激活技能（手动触发/会话挂载，`skill_ctx.matches`）
/// ∪ 请求期间 LLM 经 `activate_skill` 激活的技能（主路径，`active_skills`），
/// 保证 LLM 驱动的激活同样进入指标闭环，而不是只统计预激活。
fn record_skill_execution(
    metrics: &crate::core::skill::metrics::SkillMetrics,
    dir_path: &str,
    inputs: Vec<crate::core::skill::metrics::ExecInput>,
    success: bool,
    error_code: Option<&str>,
    request_id: &str,
) {
    metrics.record_execution_batch(dir_path, inputs, success, error_code, request_id);
}

/// 获取或创建 LLM 客户端。
///
/// 按配置指纹缓存，复用内部 reqwest 连接池；配置热更新后指纹变化，自动重建。
/// 构建失败（非法 api_key 等）返回 Err，由调用方转为错误事件。
async fn get_or_create_llm_client(
    state: &tauri::State<'_, crate::AppState>,
    endpoint: &str,
    model: &str,
    api_key: &str,
    reasoning_effort: Option<&str>,
) -> Result<LLMClient, String> {
    // 委托 AppState 的公共工厂:供 commands 层与工具闭包(子代理)共用；
    // reasoning_effort 参与客户端指纹缓存（P2-18：思考程度变化后自动重建）
    state
        .llm_client_for_cfg(endpoint, model, api_key, reasoning_effort)
        .await
}

// ─── Tauri 命令 ───

/// 取消正在运行的任务
#[tauri::command]
pub async fn kb_cancel_task(
    state: tauri::State<'_, TaskRegistry>,
    request_id: String,
) -> Result<(), String> {
    state.cancel(&request_id).await;
    Ok(())
}

/// Agent 后台任务概览（切回 Agent 页面恢复视图用）：列出全部任务快照（最新在前）。
///
/// 任务状态中心 `AppState.agent_tasks` 由 `agent_query` / `kb_llm_query` 写入，
/// 前端切出页面时任务继续运行，切回页面经本命令 + [`agent_task_get`] 重建视图。
#[tauri::command]
pub fn agent_task_list(
    state: tauri::State<'_, crate::AppState>,
) -> Vec<crate::core::agent::task_store::BackgroundAgentTask> {
    state.agent_tasks.list()
}

/// Agent 后台任务完整快照（按 request_id）。
#[tauri::command]
pub fn agent_task_get(
    state: tauri::State<'_, crate::AppState>,
    request_id: String,
) -> Option<crate::core::agent::task_store::BackgroundAgentTask> {
    state.agent_tasks.get(&request_id)
}

/// 收集已连接 MCP 服务器的工具为 DynamicTool 列表（v3：携带请求级配置以接轨迹记录）。
async fn build_mcp_agent_tools(
    state: &tauri::State<'_, crate::AppState>,
    search_config: &KbSearchConfig,
) -> Vec<rig_agent::tool::DynamicTool> {
    let mut tools = Vec::new();
    let infos = state.mcp.list().await;
    
    // 检测工具列表是否有更新（用于调试/监控）
    let mut has_updates = false;
    
    for info in infos {
        if info.status != crate::core::mcp::STATUS_CONNECTED {
            continue;
        }
        if let Some(detail) = state.mcp.get(&info.name).await {
            // 检查工具列表更新时间（用于监控，不影响构建逻辑）
            if detail.tools.iter().any(|t| t.name.contains('_')) {
                has_updates = true;
            }
            
            for def in detail.tools {
                tools.push(crate::core::mcp::build_mcp_tool(
                    info.name.clone(),
                    def,
                    state.mcp.clone(),
                    search_config.clone(),
                ));
            }
        }
    }
    
    if !tools.is_empty() {
        log::info!("[mcp] Agent 已挂载 {} 个 MCP 工具{}", tools.len(), 
            if has_updates { "（检测到工具列表可能已更新）" } else { "" });
    }
    tools
}

/// RAG 查询：技能解析 → 查询扩展 → 混合检索 → 文档聚合 → RAG Agent 生成（全流式）
#[tauri::command]
pub async fn agent_query(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
    task_registry: tauri::State<'_, TaskRegistry>,
    dir_path: String,
    query: String,
    messages: Vec<crate::services::llm::ChatMessage>,
    request_id: String,
    top_k: u32,
    session_id: Option<String>,
) -> Result<(), String> {
    let cancel = task_registry.register(&request_id).await;
    // Phase 1：后台任务状态中心注册（切出页面任务继续、切回恢复视图）。
    // 必须在所有可能提前 return 的分支之前，保证任何路径下任务都可查询/收尾。
    state
        .agent_tasks
        .register(&request_id, session_id.clone(), &dir_path, "rag", &query);
    // Phase 2：取消来源收敛——同会话新请求替换旧任务（防同会话并发 Agent 任务堆积；
    // 显式取消三入口之一：停止按钮 / 同会话替换 / 应用退出）。前端发送新消息时
    // 也会先确认中断，此处为后端兜底。
    if let Some(sid) = &session_id {
        use crate::core::agent::task_store::AgentTaskStatus;
        let tasks = state.agent_tasks.clone();
        for t in tasks.list() {
            if t.status == AgentTaskStatus::Running
                && t.session_id.as_deref() == Some(sid.as_str())
                && t.request_id != request_id
            {
                task_registry.cancel(&t.request_id).await;
                tasks.finish(&t.request_id, AgentTaskStatus::Cancelled);
                log::info!(
                    "[agent_query] [0]: 同会话新请求替换旧任务 request_id={} old={}",
                    request_id,
                    t.request_id
                );
            }
        }
    }
    // 后端防御：限制 top_k 范围（前端 UI 为 1-50），防止异常参数触发全量检索/重排
    let top_k = top_k.clamp(1, 50);
    // P2-9：请求计数（质量指标）
    crate::core::agent::agent_quality()
        .requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // 请求级任务清单（todo_write 工具）隔离：新请求开始时清空上次残留
    crate::core::agent::tools::reset_todo(&request_id);

    log::info!("[agent_query] [0]: 开始 agent: request_id={} dir_path={} query_len={} msg_count={} top_k={}",
        request_id, dir_path, query.len(), messages.len(), top_k);

    // 从中央化内存配置读取 LLM 配置
    let llm_cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();

    // v2：Agent/RAG 模式的工具编排基于 rig OpenAI 通道，暂不支持 Anthropic 协议。
    // 明确报错而非发错格式请求（避免 OpenAI 语义的误导性错误）。
    if llm_cfg.protocol == "anthropic" {
        log::warn!("[agent_query] [0]: Anthropic 协议暂不支持 Agent 模式: request_id={}", request_id);
        emit_command_error(
            &app,
            "rag:error",
            &request_id,
            "Agent/RAG 模式暂不支持 Anthropic 模型，请在设置中切换到 OpenAI 兼容模型".into(),
        );
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 构建 LLM 客户端（失败转为错误事件，避免 panic 与注册表泄漏）
    let llm = match get_or_create_llm_client(
        &state,
        &llm_cfg.endpoint,
        &llm_cfg.model,
        &llm_cfg.api_key,
        llm_cfg.reasoning_effort.as_deref(),
    )
    .await
    {
        Ok(llm) => llm,
        Err(e) => {
            log::error!("[agent_query] [0]: LLMClient 初始化失败: request_id={} err={}", request_id, e);
            emit_command_error(&app, "rag:error", &request_id, format!("LLM 客户端初始化失败: {}", e));
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    };

    if !llm.is_configured() {
        log::warn!("[agent_query] [0]: LLM 未配置: request_id={}", request_id);
        emit_command_error(&app, "rag:error", &request_id, "LLM 未配置，请在设置中填写端点地址和模型名称".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 历史上下文压缩器：优先「摘要+滑窗」（依赖 LLM），否则纯滑窗兜底（压缩永不失败）
    // 单一模型：压缩器直接用主模型（不存在独立摘要小模型，避免跨模型分叉）
    let summary_llm = llm.clone();
    let summarizer: Arc<dyn crate::core::context::HistorySummarizer> = Arc::new(summary_llm);
    let compressor: Arc<dyn ContextCompressor> = Arc::new(SummarizeThenWindowCompressor::new(
        summarizer,
        SUMMARY_MAX_CHARS,
    ));

    // ── Stage 0: 技能预激活（手动触发 / 会话挂载）──
    // 激活决策已交由 LLM（渐进式披露 L1/L2）：此处不做任何本地匹配，
    // 仅处理两类显式预激活并写入共享激活状态 active_skills，供 Agent 钩子
    // （L2 指令注入）与技能工具（activate_skill / deactivate_skill）后续使用。
    let active_skills = Arc::new(ActiveSkillState::new());
    // 闭包用 session_id 副本，避免 move 后原值不可用（检查点读写仍需 session_id）
    let session_id_for_closure = session_id.clone();
    let skill_resolved = {
        let registry = state.skill_registry.clone();
        // 会话挂载查询（rusqlite I/O）与技能解析同为阻塞操作，
        // 一并移入 spawn_blocking 调度，避免阻塞异步运行时
        let chat_store = match &session_id {
            Some(_) => state.get_chat_store(&dir_path).ok(),
            None => None,
        };
        let query_for_skill = query.clone();
        let request_id_for_log = request_id.clone();
        let dir_for_registry = dir_path.clone();
        let active = active_skills.clone();
        match tokio::task::spawn_blocking(move || {
            // 注册表未加载过时先重建（幂等；对话前前端已调用 skill_list，此处兜底）
            let _ = registry.ensure_loaded(&dir_for_registry);
            let attached_skills: Vec<(String, String, String)> = match (&chat_store, &session_id_for_closure) {
                (Some(store), Some(sid)) => match store.get_attached_skills(sid) {
                    Ok(list) => list
                        .into_iter()
                        .map(|(s, id, _v, mode)| (s, id, mode))
                        .collect(),
                    Err(e) => {
                        log::warn!(
                            "[agent_query] [0]: 读取会话挂载技能失败（技能挂载将不生效）: {} sid={:?} request_id={}",
                            e,
                            sid,
                            request_id_for_log
                        );
                        Vec::new()
                    }
                },
                _ => Vec::new(),
            };
            resolve_preactivated(&query_for_skill, &registry, &attached_skills, &active)
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                log::warn!("[agent_query] [0]: 技能预激活失败 request_id={} err={}", request_id, e);
                None
            }
            Err(e) => {
                log::warn!("[agent_query] [0]: 技能预激活任务失败 request_id={} err={}", request_id, e);
                None
            }
        }
    };
    let skill_ctx = skill_resolved.as_ref().map(|r| &r.context);
    // 手动触发时使用清理后的查询（剥离 /技能名 前缀），其余场景保持原查询；
    // filter 守卫：清理结果为空字符串时回退到原查询，避免空查询进入检索
    let query = skill_resolved
        .as_ref()
        .map(|r| r.cleaned_query.clone())
        .filter(|q| !q.trim().is_empty())
        .unwrap_or(query);
    // 防御：单条超长问题截断到预算上限（边缘 case；原"全量超限拒绝"已由历史压缩替代，
    // 当前问题不参与压缩预算，这里兜底避免超预算请求直接打 API）
    let query = if query.chars().count() > MAX_MESSAGE_CHARS {
        log::warn!("[agent_query] [0]: 当前问题超长({} 字符)，截断到 {} 字符 request_id={}",
            query.chars().count(), MAX_MESSAGE_CHARS, request_id);
        query.chars().take(MAX_MESSAGE_CHARS).collect()
    } else {
        query
    };
    // 调度计数：总数在请求起始计入（仅自增 total，不阻塞请求主链路）；
    // 是否命中由请求结束时按实际激活情况补记（见 4 个终态点，覆盖预激活 ∪ LLM 动态激活）。
    {
        let metrics = state.skill_metrics.clone();
        let dir = dir_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            metrics.record_dispatch(&dir, false);
        });
    }
    if let Some(ctx) = skill_ctx {
        log::info!(
            "[agent_query] [0]: skills 手动触发 request_id={} skills={:?} manual={}",
            request_id,
            ctx.skill_ids,
            skill_resolved.as_ref().map(|r| r.is_manual).unwrap_or(false)
        );
    } else {
        log::info!(
            "[agent_query] [0]: 自动触发技能（技能激活交由 LLM 决策）request_id={}",
            request_id
        );
    }

    // 技能检索参数覆盖（技能优先：技能显式配置时以技能为准，可放宽全局限制；
    // 未配置时回退全局配置兜底），应用于主预检索（Stage 2/3）与 kb_search 工具（Stage 4）。
    // 多技能同时命中时，context 内部仍按最保守值合并（见 SkillExecutionContext::from_skills）
    let kb_cfg = state.config_store.read();
    // P4：检索策略收拢到 policy.rs（技能声明优先 → 请求级/全局兜底 → Security clamp）
    let policy = crate::core::skill::policy::resolve_retrieval_policy(skill_ctx, top_k, &kb_cfg);
    let effective_top_k = policy.top_k;
    let effective_min_score = policy.min_score;
    // 精排 sigmoid 阈值：与 pipeline 内精排阈值同语义，供下游聚合按分数域裁决
    let effective_rerank_min_score = policy.rerank_min_score;
    let effective_max_docs = policy.max_docs;
    let effective_max_chunks = policy.max_chunks_per_doc;

    // 是否执行预检索（Stage1-3）：仅当预激活技能声明了检索工具（kb_search/code_lookup）时执行。
    // 无预激活技能或技能未声明检索时跳过预检索，由 Agent 按需调用检索工具（agentic 模式），
    // 避免无关消息触发昂贵的查询扩展与向量检索（RAG 预检索与 Agent 解耦）。
    let retrieval_enabled = active_skills.retrieval_enabled();

    // ── Stage 0.5: 轻量规划（仅复杂任务，规则路由判定；单模型 plan-then-execute）──
    // 规划是一次独立非流式调用（不占 DEFAULT_MAX_TURNS 执行预算）；
    // 失败/取消降级为"不规划"继续原流程（fail-open）。
    let mut task_plan: Option<crate::core::agent::planner::Plan> = None;
    if crate::core::agent::planner::should_plan(&query) {
        let planning_start = std::time::Instant::now();
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "planning".into(),
                message: "正在规划任务...".into(),
            },
        );
        crate::core::trace::stage_start(&request_id, "planning", &format!("query_len={}", query.len()));
        emit_pending_trace_events(&app, &request_id);
        // P0-6：规划可用独立轻量模型（planner_model），缺省回退主模型
        let plan_llm = match &llm_cfg.planner_model {
            Some(_) => match state
                .llm_client_for_role(&llm_cfg, crate::ModelRole::Planner)
                .await
            {
                Ok(client) => client,
                Err(e) => {
                    log::warn!("[agent_query] [0.5]: 规划模型不可用，回退主模型: {}", e);
                    llm.clone()
                }
            },
            None => llm.clone(),
        };
        // P0-3：结构化输出校验 + 修正重试（最多 3 次尝试：1 次原始 + 2 次修正）。
        // 校验失败用可读错误构造修正提示引导模型重发；全部失败 fail-open 不规划。
        const PLAN_JSON_MAX_ATTEMPTS: usize = 3;
        let mut plan: Option<crate::core::agent::planner::Plan> = None;
        let mut correction: Option<String> = None;
        for attempt in 0..PLAN_JSON_MAX_ATTEMPTS {
            let Some(plan_json) = plan_llm
                .generate_plan_json(&query, &messages, cancel.clone(), correction.as_deref())
                .await
            else {
                break; // 生成失败/取消：fail-open 不规划
            };
            if let Some(p) = crate::core::agent::planner::parse_plan(&plan_json) {
                plan = Some(p);
                break;
            }
            if attempt + 1 < PLAN_JSON_MAX_ATTEMPTS {
                let errors = crate::core::agent::planner::validate_plan_json(&plan_json)
                    .map(|_| Vec::new())
                    .unwrap_or_else(|e| e);
                correction = Some(crate::core::validation::build_fix_prompt(
                    &errors,
                    "请重新输出符合要求的计划 JSON（goal 目标、steps 步骤、acceptance 验收均必填且类型正确）。",
                ));
                log::warn!(
                    "[agent_query] [0.5]: 计划 JSON 校验失败，第 {} 次修正重试 request_id={}",
                    attempt + 1, request_id
                );
            }
        }
        if let Some(plan) = plan {
            log::info!(
                "[agent_query] [0.5]: 任务已规划，等待用户确认 request_id={} goal_len={} steps={}",
                request_id, plan.goal.len(), plan.steps.len()
            );
            // 请求用户确认：plan:request → 前端计划卡片 → plan_respond 回传；
            // 超时 60s fail-closed 按拒绝处理（与审批通道同构）。
            let plan_id = uuid::Uuid::new_v4().to_string();
            let (tx, rx) =
                tokio::sync::oneshot::channel::<crate::core::agent::planner::PlanDecision>();
            {
                let mut pending = state.plan_pending.lock().unwrap_or_else(|e| e.into_inner());
                pending.insert(plan_id.clone(), tx);
            }
            let _ = app.emit(
                "plan:request",
                serde_json::json!({
                    "plan_id": plan_id,
                    "request_id": request_id,
                    "plan": {
                        "goal": plan.goal,
                        "steps": plan.steps,
                        "acceptance": plan.acceptance,
                        "risks": plan.risks,
                        "touchpoints": plan.touchpoints,
                        "non_goals": plan.non_goals,
                        "rollback": plan.rollback,
                    }
                }),
            );
            // 等待用户确认:同时监听取消信号(点"停止"立即中止,不必等满 60s)
            let decision = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = state
                        .plan_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&plan_id);
                    crate::core::trace::stage_end(
                        &request_id,
                        "planning",
                        "cancelled",
                        planning_start.elapsed().as_millis() as u64,
                        "等待确认时取消",
                    );
                    emit_pending_trace_events(&app, &request_id);
                    crate::core::trace::trace_bus().clear(&request_id);
                    log::info!("[agent_query] [0.5]: 等待计划确认时被取消 request_id={}", request_id);
                    // 问题3修复：等待计划确认时被取消（用户点停止）→ 任务状态中心收尾，
                    // 否则 task_store 停留 running，全局状态条不消失且停止按钮失效
                    state
                        .agent_tasks
                        .finish(&request_id, crate::core::agent::task_store::AgentTaskStatus::Cancelled);
                    let _ = app.emit(
                        "rag:done",
                        RagDone {
                            request_id: request_id.clone(),
                            content: String::new(),
                            sources: Vec::new(),
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            cached_input_tokens: 0,
                            cache_creation_input_tokens: 0,
                        },
                    );
                    task_registry.unregister(&request_id).await;
                    return Ok(());
                }
                res = tokio::time::timeout(std::time::Duration::from_secs(60), rx) => res,
            };
            match decision {
                Ok(Ok(crate::core::agent::planner::PlanDecision::Approved)) => {
                    task_plan = Some(plan);
                    crate::core::trace::stage_end(
                        &request_id,
                        "planning",
                        "ok",
                        planning_start.elapsed().as_millis() as u64,
                        "用户已批准计划",
                    );
                    emit_pending_trace_events(&app, &request_id);
                    log::info!("[agent_query] [0.5]: 用户已批准计划 request_id={}", request_id);
                }
                outcome => {
                    // 拒绝/通道异常/超时：清理挂起表并按拒绝中止
                    let _ = state
                        .plan_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&plan_id);
                    let reason = match &outcome {
                        Ok(Ok(crate::core::agent::planner::PlanDecision::Denied(r))) => {
                            format!("原因：{}", r)
                        }
                        Ok(Ok(crate::core::agent::planner::PlanDecision::Approved)) => {
                            "计划状态异常".to_string()
                        }
                        Ok(Err(_)) => "确认通道异常".to_string(),
                        Err(_) => "未在 60 秒内确认，已按拒绝处理".to_string(),
                    };
                    crate::core::trace::stage_end(
                        &request_id,
                        "planning",
                        "denied",
                        planning_start.elapsed().as_millis() as u64,
                        &reason,
                    );
                    emit_pending_trace_events(&app, &request_id);
                    log::info!(
                        "[agent_query] [0.5]: 计划未获批准，中止执行 request_id={} reason={}",
                        request_id, reason
                    );
                    // 非用户主动拒绝（超时/通道异常）：前端右下角 sticky 提醒，用户自行点叉号关闭；
                    // 用户主动点「拒绝」时用户已知情，不重复打扰
                    if !matches!(
                        outcome,
                        Ok(Ok(crate::core::agent::planner::PlanDecision::Denied(_)))
                    ) {
                        let _ = app.emit(
                            "plan:rejected",
                            serde_json::json!({
                                "request_id": request_id.clone(),
                                "reason": reason,
                            }),
                        );
                    }
                    // content 置空：拒绝原因经日志/前端计划卡片传达，空内容使前端
                    // `if (fullContent)` 跳过 push 与落库，避免污染对话历史
                    // 问题3修复：计划被拒绝/超时 → 任务状态中心收尾（cancelled），
                    // 否则 task_store 停留 running，全局状态条不消失、停止按钮失效
                    state
                        .agent_tasks
                        .finish(&request_id, crate::core::agent::task_store::AgentTaskStatus::Cancelled);
                    let _ = app.emit(
                        "rag:done",
                        RagDone {
                            request_id: request_id.clone(),
                            content: String::new(),
                            sources: Vec::new(),
                            prompt_tokens: 0,
                            completion_tokens: 0,
                            cached_input_tokens: 0,
                            cache_creation_input_tokens: 0,
                        },
                    );
                    task_registry.unregister(&request_id).await;
                    return Ok(());
                }
            }
        } else {
            // review 修复 A3：规划失败不再静默——发 rag:status 提示降级，避免前端无反馈
            log::warn!("[agent_query] [0.5]: 规划解析失败，降级为不规划 request_id={}", request_id);
            let _ = app.emit(
                "rag:status",
                RagStatus {
                    request_id: request_id.clone(),
                    stage: "planning".into(),
                    message: "规划生成失败，已降级为直接执行".into(),
                },
            );
        }
        // 检查取消（规划阶段同样可取消；补 rag:done 避免前端滞留 planning 状态）
        if cancel.is_cancelled() {
            log::info!("[agent_query] [0.5]: 规划阶段取消 request_id={}", request_id);
            crate::core::trace::stage_end(
                &request_id,
                "planning",
                "cancelled",
                planning_start.elapsed().as_millis() as u64,
                "规划阶段取消",
            );
            emit_pending_trace_events(&app, &request_id);
            crate::core::trace::trace_bus().clear(&request_id);
            let _ = app.emit(
                "rag:done",
                RagDone {
                    request_id: request_id.clone(),
                    content: String::new(),
                    sources: Vec::new(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
            );
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    }

    // ── Stage 1-3: 预检索（仅技能触发时执行）──
    let (context, sources, selected_count) = if retrieval_enabled {
        // ── Stage 1: 查询扩展 ──
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "expanding".into(),
                message: "正在扩展查询...".into(),
            },
        );
        let expanding_start = std::time::Instant::now();
        crate::core::trace::stage_start(&request_id, "expanding", "查询扩展");
        emit_pending_trace_events(&app, &request_id);

        let expanded = llm.expand_queries(&query, &messages, cancel.clone()).await;
        let mut queries = vec![query.clone()];
        queries.extend(expanded);
        log::info!("[agent_query] [1]: 查询扩展完成 request_id={} total_queries={} queries={:?}", request_id, queries.len(), queries);
        crate::core::trace::stage_end(
            &request_id,
            "expanding",
            "ok",
            expanding_start.elapsed().as_millis() as u64,
            &format!("queries={}", queries.len()),
        );
        emit_pending_trace_events(&app, &request_id);

        // 检查取消
        if cancel.is_cancelled() {
            log::info!("[agent_query] [1]: 对话取消，直接结束 request_id={}", request_id);
            task_registry.unregister(&request_id).await;
            return Ok(());
        }

        // ── Stage 2: 多查询混合检索（并行）──
        log::info!("[agent_query] [2]: 混合检索开始 request_id={} 语义扩展数量={}",  request_id, queries.len());
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "searching".into(),
                message: format!("正在检索知识库... ({} 组查询)", queries.len()),
            },
        );
        let searching_start = std::time::Instant::now();
        crate::core::trace::stage_start(&request_id, "searching", &format!("queries={}", queries.len()));
        emit_pending_trace_events(&app, &request_id);

        // 对每个查询：嵌入 → 混合检索
        let search_start = std::time::Instant::now();
        let search_futures: Vec<_> = queries
            .iter()
            .map(|q| {
                let dir = dir_path.clone();
                let state = state.clone();
                let q = q.clone();
                async move {
                    let q_for_embed = q.clone();
                    let embed_start = std::time::Instant::now();
                    let embedding = tokio::task::spawn_blocking(move || {
                        call_embedding_query(&q_for_embed)
                    })
                    .await
                    .ok()
                    .and_then(|e| e.ok())
                    .and_then(|v| v.into_iter().next());

                    log::info!("[agent_query] [2]: 语义扩展query向量化 query={} 耗时={:?} success={}",
                        &q, embed_start.elapsed(), embedding.is_some());

                    if let Some(vec) = embedding {
                        let start = std::time::Instant::now();
                        let hits = state
                            .indexer
                            .hybrid_search(&dir, &vec, &q, effective_top_k)
                            .await
                            .unwrap_or_default();

                        log::info!("[agent_query] [2]: 语义扩展query混合检索， query={} 命中 {} 条文档耗时={:?}",
                            &q, hits.len(), start.elapsed());

                        hits
                    } else {
                        log::warn!("[agent_query] [2]: 语义扩展query向量化失败 query={} skipping", &q);
                        Vec::new()
                    }
                }
            })
            .collect();

        let all_results: Vec<Vec<SearchHit>> = {
            // 可取消的并行检索（最多并发 4 个）：取消信号到达后停止消费新结果，
            // 已启动的检索会自然完成，不会拖住取消响应。
            let cancel_fut = {
                let cancel = cancel.clone();
                async move {
                    cancel.cancelled().await;
                }
            };
            futures::stream::iter(search_futures)
                .buffer_unordered(4)
                .take_until(cancel_fut)
                .collect()
                .await
        };

        // 展平所有结果
        let all_hits: Vec<SearchHit> = all_results.into_iter().flatten().collect();
        log::info!("[agent_query] [2]: 语义扩展query混合检索最终结果， request_id={} 命中 {} 条文档, 耗时={:?}", request_id, all_hits.len(), search_start.elapsed());
        crate::core::trace::stage_end(
            &request_id,
            "searching",
            "ok",
            searching_start.elapsed().as_millis() as u64,
            &format!("hits={}", all_hits.len()),
        );
        emit_pending_trace_events(&app, &request_id);

        if cancel.is_cancelled() {
            log::info!("[agent_query] [2]: 对话取消，直接结束 request_id={}", request_id);
            task_registry.unregister(&request_id).await;
            return Ok(());
        }

        // 预检索结果提取：无命中时降级为空上下文，交由 Agent 按需使用工具
        'retrieval: {
            if all_hits.is_empty() {
                log::warn!("[agent_query] [3]: 预检索降级为空上下文（agentic 模式）request_id={}", request_id);
                break 'retrieval (String::new(), Vec::new(), 0usize);
            }

            // ── Stage 3: 文档级聚合 + 绝对阈值（core::agent::aggregate_hits）──
            let aggregating_start = std::time::Instant::now();
            crate::core::trace::stage_start(&request_id, "aggregating", "文档级聚合");
            emit_pending_trace_events(&app, &request_id);
            let selected: Vec<(SearchHit, f32)> = aggregate_hits(
                all_hits,
                effective_min_score,
                effective_rerank_min_score,
                effective_max_docs,
                effective_max_chunks,
            );
            if log::log_enabled!(log::Level::Debug) {
                // 打印每个进入引用的命中的完整分数域（doc_name / score / score_rerank / symbol / vec / bm25），
                // 用于核对"代码文件混入引用"的根因：意图路由结果 + 精排 sigmoid 分数是否恰好通过阈值。
                log::info!("[agent_query] [3]: 文档聚合结果， request_id={} 命中 {} 条文档, effective_min_score={}， effective_max_docs={}, effective_max_chunks={}， doc=\n{:?}",
                 request_id, selected.len(), effective_min_score, effective_max_docs, effective_max_chunks,
                  selected.iter()
                    .map(|(hit, score)| {
                        format!(
                            "{} : {:.3} (rerank={:?} symbol={:?} vec={:.3} bm25={:.3})",
                            hit.doc_name,
                            score,
                            hit.score_rerank,
                            hit.symbol_name,
                            hit.score_vec,
                            hit.score_bm25
                        )
                    })
                    .collect::<Vec<_>>()
                );
            }
 
            if selected.is_empty() {
                log::info!("[agent_query] [3]: 没有文档符合阈值，预检索降级为空上下文（agentic 模式）request_id={}", request_id);
                break 'retrieval (String::new(), Vec::new(), 0usize);
            }

            // 按文档分组构建上下文：文档按分数降序、文档内按阅读顺序（chunk_index），
            // 优先使用 sentence_window（包含检索句子前后的上下文），fallback 到 chunk text，
            // 总字符数受 agent 模块的 MAX_CONTEXT_CHARS 限制避免超出模型窗口。
            let context = build_context_text(&selected, crate::core::agent::MAX_CONTEXT_CHARS);
            // P1-13：检索上下文提示注入防护——命中可疑指令时包裹并追加显式
            // 安全提示（不裁剪原文，可审计），引导模型忽略指令性内容
            let context = crate::core::security::wrap_suspicious(&context);
            log::info!( "[agent_query] [3]: 上下文构建结果， request_id={} 命中 {} 条文档, char_len={} preview={:?}",
                request_id, selected.len(), context.len(), context
            );

            // 构建引用来源（按 doc_name 去重，合并文本/path_json，取最高分；
            // 对 OPML/FreeMind 合并 path_json 层级路径展示）
            let sources = build_sources(&selected);
            log::info!("[agent_query] [3]: 引用来源去重结果， request_id={} 命中 {} 条文档, count={}", request_id, selected.len(), sources.len());
            crate::core::trace::stage_end(
                &request_id,
                "aggregating",
                "ok",
                aggregating_start.elapsed().as_millis() as u64,
                &format!("docs={} chars={}", selected.len(), context.len()),
            );
            emit_pending_trace_events(&app, &request_id);

            (context, sources, selected.len())
        }
    } else {
        log::info!("[agent_query] [3]: 未命中检索技能，跳过预检索（agentic 模式）request_id={}", request_id);
        (String::new(), Vec::new(), 0usize)
    };
    let sources_clone = sources.clone();

    // ── Stage 4: 构建 context → RAG Agent 生成（技能解析与参数覆盖已在 Stage 0 完成）──
    log::info!("[agent_query] [4]: 构建 context → Agent 生成 request_id={}", request_id);
    let status_msg = match selected_count {
        0 => "正在生成回答...".to_string(),
        n => format!("正在生成回答（基于 {} 个相关片段）...", n),
    };
    // Phase 1：任务状态中心同步运行状态文本
    state.agent_tasks.set_status_message(&request_id, &status_msg);
    let _ = app.emit(
        "rag:status",
        RagStatus {
            request_id: request_id.clone(),
            stage: "generating".into(),
            message: status_msg,
        },
    );
    let generating_start = std::time::Instant::now();
    crate::core::trace::stage_start(&request_id, "generating", &format!("docs={}", selected_count));
    emit_pending_trace_events(&app, &request_id);

    if cancel.is_cancelled() {
        log::info!("[agent_query] [4]: 对话取消，直接结束 request_id={}", request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 将任务计划注入 preamble（每轮可见，约束最强）；P3-2 起改为"经用户确认后注入"
    let context = if let Some(plan) = &task_plan {
        format!("{}\n\n{}", plan.to_preamble_text(), context)
    } else {
        context
    };

    // P0-2/O1：注入相关长期记忆（关键词 ∪ 向量融合检索，RRF；embedding
    // 不可用时 search_hybrid 内部降级纯关键词；检索失败/无命中不注入）。
    // 两级记忆（P0-3）：仅注入「当前知识库 ∪ 全局」的记忆，切换目录后自动隔离。
    let memory_block = match crate::core::memory::search_hybrid(
        state.memory_store.clone(),
        state.memory_vectors.clone(),
        &query,
        3,
        &dir_path,
    )
    .await
    {
        Ok(items) if !items.is_empty() => {
            let mut s = String::from("\n\n【长期记忆（与本问题相关，供参考）】\n");
            for it in &items {
                s.push_str(&format!("- {}：{}\n", it.title, it.body));
            }
            s
        }
        _ => String::new(),
    };
    let context = format!("{}{}", context, memory_block);

    // 构建 RAG Agent：预载检索上下文 + 检索/文件/技能工具（模型可补充检索、按需激活技能）
    let model = llm.completion_model().clone();
    // 取第一个预激活技能的 ID 作为工具轨迹标注来源
    let primary_skill_id = skill_ctx.and_then(|c| c.skill_ids.first().cloned());
    // L1 技能目录（id + description，常驻 preamble，模型始终知道自己有哪些技能）
    let mut catalog = build_skill_catalog(&state.skill_registry);
    // P3/MountPreference：会话挂载技能标注——warm（自动准备）正文不注入、工具不解锁
    // （检索已预热），提示模型任务相关时先 activate_skill；active（立即生效）已加载。
    if let Some(resolved) = &skill_resolved {
        let mut mount_parts: Vec<String> = Vec::new();
        if !resolved.mounted_active.is_empty() {
            mount_parts.push(format!(
                "立即生效（指令与工具已加载）：{}",
                resolved.mounted_active.join("、")
            ));
        }
        if !resolved.mounted_warm.is_empty() {
            mount_parts.push(format!(
                "自动准备（检索已预热，需要时先 activate_skill 激活完整规则）：{}",
                resolved.mounted_warm.join("、")
            ));
        }
        if !mount_parts.is_empty() {
            catalog.push_str(&format!(
                "\n\n【会话挂载技能：{}】",
                mount_parts.join("；")
            ));
        }
    }
    // P1-4：任务路由注入——把预分析得出的技能路由显式告知执行模型，
    // 避免模型自行猜测「该激活哪个技能、该用哪些工具」（防激活错/漏激活导致的编造执行）。
    // 与上方"会话挂载技能"（说明挂载状态）互补：此处是本次任务的执行建议。
    if let Some(resolved) = &skill_resolved {
        if !resolved.skills.is_empty() {
            let route = resolved
                .skills
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>()
                .join("、");
            catalog.push_str(&format!(
                "\n\n【任务路由（系统预分析建议，供参考）：已预激活技能 {}，正文已注入，优先遵循其流程与工具；若与本任务不相关可忽略】",
                route
            ));
        }
    }
    // 各作用域技能基础目录（供 read 工具按需读取已激活技能的参考文档，L3）
    let skill_bases = resolve_skill_bases(&app, &dir_path);
    // 检索命中收集器：kb_search / code_lookup 工具的命中经此回传，合并进 rag:done 引用来源
    let search_sink: Arc<tokio::sync::Mutex<Vec<(SearchHit, f32)>>> = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    // 目录/文件黑名单（来自 ConfigStore，与 kb_index 索引一致）：ls/glob/grep 文件枚举工具按此过滤
    let indexer_cfg = state.config_store.read();
    let search_config = KbSearchConfig {
        dir_path: dir_path.clone(),
        dir_blacklist: indexer_cfg.dir_blacklist.clone(),
        file_blacklist: indexer_cfg.file_blacklist.clone(),
        indexer: state.indexer.clone(),
        default_top_k: effective_top_k,
        request_id: request_id.clone(),
        min_score: effective_min_score,
        rerank_min_score: effective_rerank_min_score,
        max_context_docs: effective_max_docs,
        max_chunks_per_doc: effective_max_chunks,
        skill_id: primary_skill_id,
        skill_state: active_skills.clone(),
        skill_bases,
        search_sink: search_sink.clone(),
        app_handle: app.clone(),
        cancel: Some(cancel.clone()),
    };
    // Agent 规约（角色/语言/安全边界）从资源目录加载，打包后跟随安装包
    let agent_rules = load_agent_rules(&app, "rag_agent.md");
    let mcp_tools = build_mcp_agent_tools(&state, &search_config).await;
    let agent = build_rag_agent(
        model,
        &context,
        search_config,
        state.skill_registry.clone(),
        catalog,
        agent_rules,
        state.approval_gate.clone(),
        crate::core::agent::DEFAULT_MAX_TURNS,
        None, // 主对话全量工具
        true, // 主对话启用技能体系的工具窄化与门禁
        mcp_tools, // v2：MCP 工具
        llm_cfg.reasoning_effort.clone(), // P2-18：思考程度透传流式请求
        llm_cfg.max_tokens, // P3：最大输出 token（None/0 = 服务器默认）
    );
    log::info!("[agent_query] [4]: 构建 Agent 完成 request_id={}", request_id);

    // 技能执行计时起点（进入生成阶段即视为执行开始）
    let skill_exec_start = std::time::Instant::now();

    // 当前问题作为 prompt，历史消息（去掉最后一条当前问题）压缩后作为 history
    // P0-5：先应用会话压缩检查点（摘要 + cutoff 之后的增量消息），压缩后写回新检查点
    let checkpoint: Option<crate::core::context::CompactionState> = match (&session_id, &dir_path) {
        (Some(sid), _) => {
            let sid = sid.clone();
            let store = state.get_chat_store(&dir_path).ok();
            match store {
                Some(store) => tokio::task::spawn_blocking(move || {
                    store
                        .get_compaction_state(&sid)
                        .ok()
                        .flatten()
                        .and_then(|raw| crate::core::context::CompactionState::from_json(&raw))
                })
                .await
                .ok()
                .flatten(),
                None => None,
            }
        }
        _ => None,
    };
    let hist_messages = apply_compaction_checkpoint(&messages, checkpoint.as_ref());
    let compressed = prepare_history(
        &hist_messages,
        compressor.as_ref(),
        compression_budget_tokens(llm_cfg.context_length),
        cancel.clone(),
    )
    .await;
    // 写回新检查点：仅摘要策略成功且消息带 id 时（可定位 cutoff），
    // 失败静默（检查点缺失只是失去增量优化，不影响正确性）
    if let (Some(sid), Some(store)) = (&session_id, state.get_chat_store(&dir_path).ok()) {
        if compressed.strategy == "summarize+window" {
            let summary = compressed
                .turns
                .iter()
                .find(|t| t.role == "system")
                .map(|t| t.content.clone())
                .unwrap_or_default();
            if !summary.is_empty() {
                if let Some(first_kept_id) = hist_messages
                    .get(compressed.kept_from)
                    .and_then(|m| m.id.clone())
                {
                    // P5：记录 Session 生命周期激活技能（跨请求恢复引用，含版本校验）。
                    // 注意：仅当本请求触发 summarize+window 压缩时才更新检查点；
                    // 未压缩时保留上一检查点的引用（技能更新由恢复时版本校验兜底）。
                    let session_skills: Vec<crate::core::context::SessionSkillRef> = active_skills
                        .activated()
                        .iter()
                        .filter(|a| {
                            a.lifetime
                                == crate::core::skill::activation::SkillLifetime::Session
                                && a.status
                                    == crate::core::skill::activation::ActivationStatus::Active
                        })
                        .map(|a| crate::core::context::SessionSkillRef {
                            skill_id: a.skill_id.clone(),
                            scope: a.scope.as_str().to_string(),
                            version: a.version,
                        })
                        .collect();
                    let new_state = crate::core::context::CompactionState {
                        summary,
                        cutoff_msg_id: Some(first_kept_id),
                        tokens_before: 0,
                        session_skills,
                    };
                    let sid = sid.clone();
                    let store = store.clone();
                    let json = new_state.to_json();
                    let _ = tokio::task::spawn_blocking(move || {
                        store.set_compaction_state(&sid, &json)
                    })
                    .await;
                }
            }
        }
    }
    if compressed.dropped_chars > 0 {
        log::info!(
            "[agent_query] [4]: 对话历史已压缩 request_id={} dropped={} strategy={}",
            request_id, compressed.dropped_chars, compressed.strategy
        );
        let _ = app.emit(
            "rag:status",
            RagStatus {
                request_id: request_id.clone(),
                stage: "generating".into(),
                message: format!(
                    "对话历史较长，已自动压缩旧消息（节省约 {} 字符）",
                    compressed.dropped_chars
                ),
            },
        );
    }
    // 压缩阶段取消只中断压缩，此处快速检查避免取消后再发起一次 HTTP 请求
    if cancel.is_cancelled() {
        log::info!("[agent_query] [4]: 对话在压缩后取消，不发起请求 request_id={}", request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }
    let mut history = chat_turns_to_history(&compressed.turns);
    // Session 技能激活状态由挂载 mode 持久化驱动（P5 检查点 session_skills 保留写回，
    // 供未来多代理/MCP 扩展；恢复注入已移除——active 挂载由 resolve_preactivated 每请求
    // 激活并注入正文，warm/已移除技能不应被自动恢复推翻，符合用户当前挂载配置）。
    // 预激活技能正文一次性注入：作为 history 首条 system 消息。
    // 不随每轮 preamble 注入（避免常驻消耗 token），随历史受压缩机制管理。
    // 回退模式（PERSISTENT_INJECTION=true）下跳过：正文由每轮 Hook 注入，避免双份。
    if !crate::core::agent::PERSISTENT_INJECTION {
        if let Some(resolved) = &skill_resolved {
            if !resolved.skills.is_empty() {
                let instructions = crate::core::skill::context::format_skill_instructions(
                    &resolved.skills,
                    crate::core::skill::activation::MAX_SKILL_INJECTION_CHARS,
                );
                if !instructions.is_empty() {
                    let mut injected = vec![Message::system(format!(
                        "【已激活技能指令（仅提供一次，请遵循）】\n{}",
                        instructions
                    ))];
                    injected.extend(history);
                    history = injected;
                    log::info!(
                        "[agent_query] [4]: 预激活技能正文一次性注入 history: chars={} skills={:?} request_id={}",
                        instructions.chars().count(),
                        resolved.skills.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                        request_id
                    );
                }
            }
        }
    }
    let mut stream = agent
        .stream_chat(Message::user(query.clone()), history)
        .into_future()
        .await;

    // 流式生成
    let llm_start = std::time::Instant::now();
    let mut full_content = String::new();
    let mut final_usage: Option<UsageInfo> = None;
    let mut delta_count = 0u64;
    let mut stream_failed = false;
    let mut last_tool_summary: Option<String> = None;
    // 本请求实际调用过的工具名（防幻觉校验用：声称写操作但未调用对应工具时追加提醒）
    let mut tools_called: Vec<String> = Vec::new();
    loop {
        let item = match next_or_cancel(&mut stream, &cancel).await {
            Err(()) => {
                log::info!("[agent_query] [4]: 对话取消，立即断开请求 request_id={} accumulated={}",
                    request_id, full_content.len());
                // 取消时保留已生成的部分内容：通过 rag:done 交给前端落库
                if !full_content.is_empty() {
                    let (prompt_tokens, completion_tokens, cached_input_tokens, cache_creation_input_tokens) = final_usage
                        .as_ref()
                        .map(|u| (u.prompt_tokens, u.completion_tokens, u.cached_input_tokens, u.cache_creation_input_tokens))
                        .unwrap_or((0, 0, 0, 0));
                    let _ = app.emit(
                        "rag:done",
                        RagDone {
                            request_id: request_id.clone(),
                            content: full_content.clone(),
                            sources: merge_search_sink(sources_clone.clone(), &search_sink).await,
                            prompt_tokens,
                            completion_tokens,
                            cached_input_tokens,
                            cache_creation_input_tokens,
                        },
                    );
                }
                // 取消时补发残留工具事件并清理总线
                crate::core::trace::stage_end(
                    &request_id,
                    "generating",
                    "cancelled",
                    generating_start.elapsed().as_millis() as u64,
                    &format!("chars={}", full_content.len()),
                );
                emit_pending_trace_events(&app, &request_id);
                emit_pending_tool_events(&app, &request_id);
                tool_call_bus().clear(&request_id);
                // Phase 1：任务状态中心收尾（cancelled）——保留部分内容快照
                state
                    .agent_tasks
                    .finish(&request_id, crate::core::agent::task_store::AgentTaskStatus::Cancelled);
                {
                    let inputs = collect_skill_exec_inputs(skill_ctx, &active_skills, skill_exec_start.elapsed().as_millis() as u64);
                    let matched = !inputs.is_empty();
                    let metrics = state.skill_metrics.clone();
                    let dir = dir_path.clone();
                    let rid = request_id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if matched {
                            metrics.record_dispatch_matched(&dir);
                        }
                        record_skill_execution(&metrics, &dir, inputs, false, Some("cancelled"), &rid);
                    })
                    .await;
                }
                task_registry.unregister(&request_id).await;
                return Ok(());
            }
            Ok(None) => break,
            Ok(Some(item)) => item,
        };
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall { tool_call, .. })) => {
                log::info!("[agent_query] [4]: 工具调用: name={} arguments={}",
                    tool_call.function.name, tool_call.function.arguments);
                if !tools_called.iter().any(|t| t == &tool_call.function.name) {
                    tools_called.push(tool_call.function.name.clone());
                }
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                if text.text.is_empty() {
                    continue;
                }
                full_content.push_str(&text.text);
                delta_count += 1;
                // Phase 1：任务状态中心累积文本（切回页面恢复部分回复）
                state.agent_tasks.append_text(&request_id, &text.text);
                let _ = app.emit(
                    "rag:delta",
                    RagDelta {
                        request_id: request_id.clone(),
                        content: text.text,
                    },
                );
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                let usage = res.usage();
                if usage.has_values() {
                    log::info!("[agent_query] [4]: Agent 最终 token 使用: request_id={} input_tokens={} output_tokens={}",
                        request_id, usage.input_tokens, usage.output_tokens);
                    final_usage = Some(usage_to_info(&usage));
                }
            }
            Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                if call.usage.has_values() {
                    final_usage = Some(usage_to_info(&call.usage));
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("[agent_query] [4]: Agent 流式响应错误: request_id={} err={}", request_id, e);
                stream_failed = true;
                break;
            }
        }
        // 捕获最后一个成功的工具调用结果内容（用于兜底：模型调用工具成功但未生成文本时）
        if let Some(result) = tool_call_bus().peek_last_success_result(&request_id) {
            last_tool_summary = Some(result);
        }
        // 转发工具调用轨迹（工具在 Rig 流式内部执行，结果已写入总线）
        emit_pending_tool_events(&app, &request_id);
    }
    
     log::info!("[agent_query] [4]: Agent 流式响应完成: request_id={} took={:?} delta_count={} content_len={}",
        request_id, llm_start.elapsed(), delta_count, full_content.len());
    crate::core::trace::stage_end(
        &request_id,
        "generating",
        "ok",
        generating_start.elapsed().as_millis() as u64,
        &format!("chars={} delta={}", full_content.len(), delta_count),
    );
    emit_pending_trace_events(&app, &request_id);

    // 流式失败且无任何内容 → 显式报错，避免静默失败或空消息污染前端
    if stream_failed && full_content.is_empty() && !cancel.is_cancelled() {
        log::info!("[agent_query] [4]: 流式响应失败 request_id={}", request_id);
        crate::core::trace::stage_end(
            &request_id,
            "generating",
            "error",
            generating_start.elapsed().as_millis() as u64,
            "llm_stream_failed",
        );
        emit_pending_trace_events(&app, &request_id);
        emit_pending_tool_events(&app, &request_id);
        tool_call_bus().clear(&request_id);
        {
            let inputs = collect_skill_exec_inputs(skill_ctx, &active_skills, skill_exec_start.elapsed().as_millis() as u64);
            let matched = !inputs.is_empty();
            let metrics = state.skill_metrics.clone();
            let dir = dir_path.clone();
            let rid = request_id.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if matched {
                    metrics.record_dispatch_matched(&dir);
                }
                record_skill_execution(&metrics, &dir, inputs, false, Some("llm_stream_failed"), &rid);
            })
            .await;
        }
        // Phase 1：任务状态中心收尾（failed）
        state
            .agent_tasks
            .finish(&request_id, crate::core::agent::task_store::AgentTaskStatus::Failed);
        emit_command_error(&app, "rag:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // ── Done ──
    // 流式正常结束但内容为空：若工具调用成功，以工具结果兜底；否则报错。
    if full_content.trim().is_empty() {
        if let Some(summary) = last_tool_summary.take() {
            log::info!("[agent_query] [4]: 模型未生成文本但工具调用成功，以工具结果兜底 request_id={} summary={}",
                request_id, summary);
            full_content = summary;
            // 继续走到 rag:done 正常发射
        } else {
            log::warn!("[agent_query] [4]: 响应完成但内容为空 request_id={}", request_id);
            emit_pending_tool_events(&app, &request_id);
            tool_call_bus().clear(&request_id);
            {
                let inputs = collect_skill_exec_inputs(skill_ctx, &active_skills, skill_exec_start.elapsed().as_millis() as u64);
                let matched = !inputs.is_empty();
                let metrics = state.skill_metrics.clone();
                let dir = dir_path.clone();
                let rid = request_id.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if matched {
                        metrics.record_dispatch_matched(&dir);
                    }
                    record_skill_execution(&metrics, &dir, inputs, false, Some("llm_empty_output"), &rid);
                })
                .await;
            }
            emit_command_error(&app, "rag:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    }
    let (prompt_tokens, completion_tokens, cached_input_tokens, cache_creation_input_tokens) = final_usage
        .map(|u| (u.prompt_tokens, u.completion_tokens, u.cached_input_tokens, u.cache_creation_input_tokens))
        .unwrap_or((0, 0, 0, 0));

    log::info!("[agent_query] [4]: 响应完成: request_id={} content_len={} sources={} tokens_in={} tokens_out={} cached_in={}",
        request_id, full_content.len(), sources_clone.len(), prompt_tokens, completion_tokens, cached_input_tokens);

    // P0 防幻觉：声称完成写操作但本请求未调用对应工具 → 追加一致性提醒
    // （在 rag:done 之前修改 full_content，前端最终渲染与落库均包含提醒）
    apply_anti_hallucination_guard(&mut full_content, &tools_called);
    // P0-2 Grounding 后置兜底：本地文件引用但无检索/读取来源 → 追加依据提醒
    let merged_sources = merge_search_sink(sources_clone, &search_sink).await;
    apply_grounding_validator(&mut full_content, !merged_sources.is_empty(), &tools_called);

    // Phase 1：任务状态中心收尾（done）——保留最终内容/来源/用量快照，供切回页面恢复。
    // 先固化（引用 merged_sources），随后 emit 再 move。
    {
        let tasks = state.agent_tasks.clone();
        use crate::core::agent::task_store::AgentTaskStatus;
        tasks.set_sources(
            &request_id,
            merged_sources
                .iter()
                .map(|s| serde_json::to_value(s).unwrap_or_default())
                .collect(),
        );
        tasks.set_usage(&request_id, prompt_tokens, completion_tokens);
        tasks.finish(&request_id, AgentTaskStatus::Done);
    }
    let _ = app.emit(
        "rag:done",
        RagDone {
            request_id: request_id.clone(),
            content: full_content,
            sources: merged_sources,
            prompt_tokens,
            completion_tokens,
            cached_input_tokens,
            cache_creation_input_tokens,
        },
    );

    // 收尾：补发残留工具事件并清理总线
    emit_pending_tool_events(&app, &request_id);
    tool_call_bus().clear(&request_id);
    {
        let inputs = collect_skill_exec_inputs(skill_ctx, &active_skills, skill_exec_start.elapsed().as_millis() as u64);
        let matched = !inputs.is_empty();
        let metrics = state.skill_metrics.clone();
        let dir = dir_path.clone();
        let rid = request_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if matched {
                metrics.record_dispatch_matched(&dir);
            }
            record_skill_execution(&metrics, &dir, inputs, true, None, &rid);
        })
        .await;
    }
    task_registry.unregister(&request_id).await;
    Ok(())
}

/// 纯 LLM 对话（Rig Agent，无工具）
#[tauri::command]
pub async fn kb_llm_query(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
    task_registry: tauri::State<'_, TaskRegistry>,
    messages: Vec<crate::services::llm::ChatMessage>,
    request_id: String,
) -> Result<(), String> {
    let cancel = task_registry.register(&request_id).await;
    // Phase 1：后台任务状态中心注册（chat 模式同样支持后台运行与切回恢复）
    let rag_prompt = messages.last().map(|m| m.content.clone()).unwrap_or_default();
    state
        .agent_tasks
        .register(&request_id, None, "", "chat", &rag_prompt);

    // 从中央化内存配置读取 LLM 配置
    let llm_cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();

    // v2：Anthropic Messages 协议走独立流式通道（普通对话，不含 Agent 工具编排）
    if llm_cfg.protocol == "anthropic" {
        return kb_llm_query_anthropic(&app, task_registry, messages, request_id, llm_cfg).await;
    }

    // 构建 LLM 客户端（失败转为错误事件，避免 panic 与注册表泄漏）
    let llm = match get_or_create_llm_client(
        &state,
        &llm_cfg.endpoint,
        &llm_cfg.model,
        &llm_cfg.api_key,
        llm_cfg.reasoning_effort.as_deref(),
    )
    .await
    {
        Ok(llm) => llm,
        Err(e) => {
            log::error!("[kb_llm_query] [0]: LLM 客户端初始化失败: request_id={} err={}", request_id, e);
            emit_command_error(&app, "llm:error", &request_id, format!("LLM 客户端初始化失败: {}", e));
            task_registry.unregister(&request_id).await;
            return Ok(());
        }
    };

    if !llm.is_configured() {
        emit_command_error(&app, "llm:error", &request_id, "LLM 未配置".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 历史上下文压缩器：无工具对话同样适用，避免长会话被直接拒绝
    // 单一模型：压缩器直接用主模型（不存在独立摘要小模型，避免跨模型分叉）
    let summary_llm = llm.clone();
    let summarizer: Arc<dyn crate::core::context::HistorySummarizer> = Arc::new(summary_llm);
    let compressor: Arc<dyn ContextCompressor> = Arc::new(SummarizeThenWindowCompressor::new(
        summarizer,
        SUMMARY_MAX_CHARS,
    ));

    if messages.is_empty() {
        emit_command_error(&app, "llm:error", &request_id, "消息不能为空".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    let prompt_content = messages.last().map(|m| m.content.clone()).unwrap_or_default();
    let compressed = prepare_history(
        &messages,
        compressor.as_ref(),
        compression_budget_tokens(llm_cfg.context_length),
        cancel.clone(),
    )
    .await;
    if compressed.dropped_chars > 0 {
        log::info!(
            "[kb_llm_query] [0]: 对话历史已压缩 request_id={} dropped={} strategy={}",
            request_id, compressed.dropped_chars, compressed.strategy
        );
    }
    // 压缩阶段取消只中断压缩，此处快速检查避免取消后再发起一次 HTTP 请求
    if cancel.is_cancelled() {
        log::info!("[kb_llm_query] [1]: 对话在压缩后取消，不发起请求 request_id={}", request_id);
        task_registry.unregister(&request_id).await;
        return Ok(());
    }
    let history = chat_turns_to_history(&compressed.turns);

    // Agent 规约（角色/语言/安全边界）从资源目录加载，打包后跟随安装包
    let agent_rules = load_agent_rules(&app, "chat_agent.md");
    let agent = build_chat_agent(
        llm.completion_model().clone(),
        agent_rules,
        llm_cfg.reasoning_effort.clone(), // P2-18：思考程度透传流式请求
        llm_cfg.max_tokens, // P3：最大输出 token（None/0 = 服务器默认）
    );
    let mut stream = agent
        .stream_chat(Message::user(prompt_content.clone()), history)
        .into_future()
        .await;

    let kb_gen_start = std::time::Instant::now();
    crate::core::trace::stage_start(&request_id, "generating", "kb_llm_query");
    emit_pending_trace_events(&app, &request_id);

    let mut full_content = String::new();
    let mut stream_failed = false;
    loop {
        let item = match next_or_cancel(&mut stream, &cancel).await {
            Err(()) => {
                log::info!("[kb_llm_query] [1]: 对话取消，立即断开请求 request_id={} accumulated={}",
                    request_id, full_content.len());
                crate::core::trace::stage_end(
                    &request_id,
                    "generating",
                    "cancelled",
                    kb_gen_start.elapsed().as_millis() as u64,
                    &format!("chars={}", full_content.len()),
                );
                emit_pending_trace_events(&app, &request_id);
                crate::core::trace::trace_bus().clear(&request_id);
                // 取消时保留已生成的部分内容：通过 llm:done 交给前端落库
                if !full_content.is_empty() {
                    let _ = app.emit(
                        "llm:done",
                        LlmDone {
                            request_id: request_id.clone(),
                            content: full_content.clone(),
                        },
                    );
                }
                // Phase 1：任务状态中心收尾（cancelled）——保留部分内容快照
                state
                    .agent_tasks
                    .finish(&request_id, crate::core::agent::task_store::AgentTaskStatus::Cancelled);
                task_registry.unregister(&request_id).await;
                return Ok(());
            }
            Ok(None) => break,
            Ok(Some(item)) => item,
        };
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall { tool_call, .. })) => {
                log::info!("[kb_llm_query] [1]: agent 工具调用: name={} arguments={}",
                    tool_call.function.name, tool_call.function.arguments);
            }
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                if text.text.is_empty() {
                    continue;
                }
                full_content.push_str(&text.text);
                // Phase 1：任务状态中心累积文本（chat 模式后台运行快照）
                state.agent_tasks.append_text(&request_id, &text.text);
                let _ = app.emit(
                    "llm:delta",
                    LlmDelta {
                        request_id: request_id.clone(),
                        content: text.text,
                    },
                );
            }
            Ok(MultiTurnStreamItem::FinalResponse(res)) => {
                let usage = res.usage();
                if usage.has_values() {
                    let _ = app.emit(
                        "llm:usage",
                        serde_json::json!({
                            "request_id": request_id,
                            "prompt_tokens": usage.input_tokens,
                            "completion_tokens": usage.output_tokens,
                            "cached_input_tokens": usage.cached_input_tokens,
                            "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                        }),
                    );
                }
            }
            Ok(MultiTurnStreamItem::CompletionCall(call)) => {
                if call.usage.has_values() {
                    let _ = app.emit(
                        "llm:usage",
                        serde_json::json!({
                            "request_id": request_id.clone(),
                            "prompt_tokens": call.usage.input_tokens,
                            "completion_tokens": call.usage.output_tokens,
                            "cached_input_tokens": call.usage.cached_input_tokens,
                            "cache_creation_input_tokens": call.usage.cache_creation_input_tokens,
                        }),
                    );
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("[kb_llm_query] [1]: agent 流式错误: request_id={} err={}", request_id, e);
                stream_failed = true;
                break;
            }
        }
    }

    // 流式失败且无任何内容 → 显式报错，避免静默失败或空消息污染前端
    if stream_failed && full_content.is_empty() && !cancel.is_cancelled() {
        log::warn!("[kb_llm_query] [1]: 流式响应失败: request_id={}", request_id);
        crate::core::trace::stage_end(
            &request_id,
            "generating",
            "error",
            kb_gen_start.elapsed().as_millis() as u64,
            "llm_stream_failed",
        );
        emit_pending_trace_events(&app, &request_id);
        crate::core::trace::trace_bus().clear(&request_id);
        // Phase 1：任务状态中心收尾（failed）
        state
            .agent_tasks
            .finish(&request_id, crate::core::agent::task_store::AgentTaskStatus::Failed);
        emit_command_error(&app, "llm:error", &request_id, "LLM 生成失败，请检查模型服务是否可用".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }
    crate::core::trace::stage_end(
        &request_id,
        "generating",
        "ok",
        kb_gen_start.elapsed().as_millis() as u64,
        &format!("chars={}", full_content.len()),
    );
    emit_pending_trace_events(&app, &request_id);
    crate::core::trace::trace_bus().clear(&request_id);

    let _ = app.emit(
        "llm:done",
        LlmDone {
            request_id: request_id.clone(),
            content: full_content,
        },
    );
    // Phase 1：任务状态中心收尾（done）
    state
        .agent_tasks
        .finish(&request_id, crate::core::agent::task_store::AgentTaskStatus::Done);

    task_registry.unregister(&request_id).await;
    Ok(())
}

// ─── Anthropic Messages 通道（v2） ───

/// Anthropic Messages 协议流式对话（普通 Chat 模式）。
///
/// 与 OpenAI 兼容通道完全隔离（开闭原则）：不经过 rig agent 工具编排，
/// 独立实现 /v1/messages + SSE 解析；历史按窗口截断（不做摘要压缩）。
/// 取消时保留已生成的部分内容（通过 llm:done 交给前端落库），与 openai 通道一致。
async fn kb_llm_query_anthropic(
    app: &AppHandle,
    task_registry: tauri::State<'_, TaskRegistry>,
    messages: Vec<crate::services::llm::ChatMessage>,
    request_id: String,
    cfg: crate::LlmConfig,
) -> Result<(), String> {
    use crate::services::anthropic::{
        AnthropicEvent, AnthropicMessage, AnthropicStreamClient, THINK_BUDGET_HIGH,
        THINK_BUDGET_LOW, THINK_BUDGET_MAX, THINK_BUDGET_STANDARD,
    };

    let cancel = task_registry.register(&request_id).await;

    // thinking 档位映射：reasoning_effort → Anthropic extended thinking token 预算
    // （对齐主流 Agent：low/medium/high/xhigh 逐档递增；auto/空串不启用）
    let thinking_budget = match cfg.reasoning_effort.as_deref() {
        Some("low") => Some(THINK_BUDGET_LOW),
        Some("medium") => Some(THINK_BUDGET_STANDARD),
        Some("high") => Some(THINK_BUDGET_HIGH),
        Some("xhigh") => Some(THINK_BUDGET_MAX),
        _ => None,
    };

    let client = AnthropicStreamClient::new(
        cfg.endpoint.clone(),
        cfg.api_key.clone(),
        cfg.model.clone(),
        // P3：最大输出 token（None/0 = 默认 4096，可配置覆盖）
        cfg.max_tokens.unwrap_or(4096),
        thinking_budget,
    );
    if !client.is_configured() {
        emit_command_error(app, "llm:error", &request_id, "Anthropic 未配置（缺少地址或模型）".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // system 提取到顶层；仅保留 user / assistant 进 body
    let mut system_parts: Vec<String> = Vec::new();
    let mut body: Vec<AnthropicMessage> = Vec::new();
    for m in messages {
        if m.role == "system" {
            if !m.content.trim().is_empty() {
                system_parts.push(m.content);
            }
        } else if m.role == "user" || m.role == "assistant" {
            body.push(AnthropicMessage { role: m.role, content: m.content });
        }
    }
    if body.is_empty() {
        emit_command_error(app, "llm:error", &request_id, "消息不能为空".into());
        task_registry.unregister(&request_id).await;
        return Ok(());
    }

    // 历史窗口截断（Anthropic 通道独立，不做摘要压缩）：
    // 保留最近 40 条，且累计字符超预算时从头部丢弃旧消息
    const MAX_BODY_MESSAGES: usize = 40;
    const MAX_BODY_CHARS: usize = 180_000;
    if body.len() > MAX_BODY_MESSAGES {
        body = body.split_off(body.len() - MAX_BODY_MESSAGES);
    }
    let mut total_chars: usize = body.iter().map(|m| m.content.chars().count()).sum();
    while body.len() > 1 && total_chars > MAX_BODY_CHARS {
        if let Some(removed) = body.first() {
            total_chars = total_chars.saturating_sub(removed.content.chars().count());
        }
        body.remove(0);
    }

    let system = if system_parts.is_empty() { None } else { Some(system_parts.join("\n\n")) };

    match client
        .stream_chat(system.as_deref(), &body, cancel.clone(), |ev| match ev {
            AnthropicEvent::Delta(t) => {
                let _ = app.emit(
                    "llm:delta",
                    LlmDelta {
                        request_id: request_id.clone(),
                        content: t,
                    },
                );
            }
            AnthropicEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
            } => {
                let _ = app.emit(
                    "llm:usage",
                    serde_json::json!({
                        "request_id": request_id,
                        "prompt_tokens": input_tokens,
                        "completion_tokens": output_tokens,
                        "cached_input_tokens": cache_read_input_tokens,
                        "cache_creation_input_tokens": cache_creation_input_tokens,
                    }),
                );
            }
        })
        .await
    {
        Ok(full_content) => {
            let _ = app.emit(
                "llm:done",
                LlmDone {
                    request_id: request_id.clone(),
                    content: full_content,
                },
            );
        }
        Err(e) => {
            log::warn!("[kb_llm_query_anthropic] 请求失败: request_id={} err={}", request_id, e);
            if !cancel.is_cancelled() {
                emit_command_error(app, "llm:error", &request_id, e);
            }
        }
    }
    task_registry.unregister(&request_id).await;
    Ok(())
}

#[cfg(test)]
mod anti_hallucination_tests {
    use super::{apply_anti_hallucination_guard, apply_grounding_validator};

    #[test]
    fn appends_warning_when_claims_schedule_without_call() {
        let mut content = "已创建：产品评审（2026-08-18 14:00~15:00）。当前日程汇总：...".to_string();
        apply_anti_hallucination_guard(&mut content, &[]);
        assert!(content.contains("一致性提醒"), "应追加提醒: {}", content);
        assert!(content.contains("schedule"), "应指明对应工具");
    }

    #[test]
    fn no_warning_when_schedule_called() {
        let mut content = "已创建日程：产品评审（2026-08-18 14:00~15:00）。".to_string();
        apply_anti_hallucination_guard(&mut content, &["schedule".to_string()]);
        assert!(!content.contains("一致性提醒"), "调用过 schedule 不应追加提醒");
    }

    #[test]
    fn appends_warning_when_claims_file_write_without_call() {
        // 声明表驱动：声称"已保存文件"但未调用 write/edit/multi_edit → 提醒
        let mut content = "已保存总结到文件 summary.md。".to_string();
        apply_anti_hallucination_guard(&mut content, &[]);
        assert!(content.contains("一致性提醒"), "应追加提醒: {}", content);
        assert!(content.contains("file_write"), "应指明动作 id");
    }

    #[test]
    fn no_warning_when_file_write_called() {
        let mut content = "已将总结保存到文件 summary.md。".to_string();
        apply_anti_hallucination_guard(&mut content, &["write".to_string()]);
        assert!(!content.contains("一致性提醒"), "调用过 write 不应追加提醒");
    }

    #[test]
    fn no_warning_when_git_claimed_but_status_observed() {
        // 复述 git 状态（调用过 git_status）不属于"声称执行提交"→ 观察豁免
        let mut content = "当前改动已提交，工作区干净。".to_string();
        apply_anti_hallucination_guard(&mut content, &["git_status".to_string()]);
        assert!(!content.contains("一致性提醒"), "git_status 观察应豁免声称判定");
    }

    #[test]
    fn appends_warning_when_git_claimed_without_any_git_tool() {
        // 声称"已提交"但既未调用 git_commit 也未调用 git_status/git_diff → 提醒
        let mut content = "本次改动已提交到仓库。".to_string();
        apply_anti_hallucination_guard(&mut content, &[]);
        assert!(content.contains("一致性提醒"), "应追加提醒: {}", content);
        assert!(content.contains("git_commit"), "应指明对应工具");
    }

    #[test]
    fn no_warning_without_claim_word() {
        // 无完成式声称词（"计划"不是完成式）→ 不拦截
        let mut content = "我可以帮你创建日程，请告诉我时间。".to_string();
        apply_anti_hallucination_guard(&mut content, &[]);
        assert!(!content.contains("一致性提醒"));
    }

    #[test]
    fn no_warning_for_unrelated_topic() {
        // 声称词存在但对象不在任何声明表中（"公式推导"非日程/文件/代码对象）→ 不拦截
        let mut content = "已创建：一条公式推导，过程如下：...".to_string();
        apply_anti_hallucination_guard(&mut content, &[]);
        assert!(!content.contains("一致性提醒"), "无关对象不应拦截");
    }

    #[test]
    fn no_warning_for_meeting_notes_without_claim() {
        // "会议"对象但无完成式声称词（"总结"不在声称词）→ 不拦截
        let mut content = "以下是会议纪要的要点总结。".to_string();
        apply_anti_hallucination_guard(&mut content, &[]);
        assert!(!content.contains("一致性提醒"));
    }

    #[test]
    fn multiple_warnings_not_duplicated() {
        let mut content = "已创建日程A，已创建日程B。".to_string();
        apply_anti_hallucination_guard(&mut content, &[]);
        assert_eq!(content.matches("一致性提醒").count(), 1, "只追加一次提醒");
    }

    #[test]
    fn grounding_validator_flags_file_ref_without_evidence() {
        // 引用本地文件但未检索、未读取 → 依据提醒
        let mut content = "项目的架构说明见 docs/architecture.md。".to_string();
        apply_grounding_validator(&mut content, false, &[]);
        assert!(content.contains("依据提醒"), "应追加依据提醒: {}", content);
    }

    #[test]
    fn grounding_validator_silent_with_sources() {
        // 有检索来源 → 不提醒
        let mut content = "项目的架构说明见 docs/architecture.md。".to_string();
        apply_grounding_validator(&mut content, true, &[]);
        assert!(!content.contains("依据提醒"));
    }

    #[test]
    fn grounding_validator_silent_when_local_tool_used() {
        // 调用过 read/ls/glob/grep → 视为有依据，不提醒
        let mut content = "项目的架构说明见 docs/architecture.md。".to_string();
        apply_grounding_validator(&mut content, false, &["read".to_string()]);
        assert!(!content.contains("依据提醒"));
    }

    #[test]
    fn grounding_validator_silent_without_file_ref() {
        // 无文件引用信号 → 不提醒
        let mut content = "根据现有资料无法确认该项目是否使用 Redis Cluster。".to_string();
        apply_grounding_validator(&mut content, false, &[]);
        assert!(!content.contains("依据提醒"));
    }
}

