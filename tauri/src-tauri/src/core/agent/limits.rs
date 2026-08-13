//! AI Agent 指标参数集中配置（单一来源，避免多文件双份维护）。
//!
//! 集中收纳所有「指标/上限类」参数：模型调用预算、检索阈值、文件工具上限、
//! 子代理预算、外部工具护栏、工具 schema 的 min/max/maxLength/maxItems 约束值。
//! 工具 schema 生成与代码层 clamp 均引用本模块常量，保证单一口径。

// ── 模型调用预算 ──

/// Agent 单次请求的模型调用总预算（1-based：turn=10 为最后一次，第 11 次触发 MaxTurnsError）
pub const DEFAULT_MAX_TURNS: usize = 10;
/// 技能正文注入模式回退开关：false=一次性注入（默认，对齐 Reasonix/Pi）；
/// true=恢复每轮注入（三拆后激活状态不持有正文，回退模式由
/// SkillInstructionHook 从 SkillRegistry 按激活记录重新查询正文，总量受预算截断）。
/// 回退模式下 llm.rs 跳过 history 一次性注入（正文统一由每轮 Hook 注入，避免双份）。
pub const PERSISTENT_INJECTION: bool = false;
/// 单条消息最大字符数（超长问题截断上限）
pub const MAX_MESSAGE_CHARS: usize = 30_000;
/// 对话历史压缩的 token 预算
pub const MAX_MESSAGE_TOKENS: usize = 15_000;
/// 历史摘要的最大字符数
pub const SUMMARY_MAX_CHARS: usize = 6_000;

// ── 检索 ──

/// kb_search / code_lookup 工具默认 top_k 上限
pub const MAX_TOP_K: u32 = 20;
/// 检索上下文注入模型的最大字符数
pub const MAX_CONTEXT_CHARS: usize = 12_000;
/// kb_search / code_lookup 工具 schema 的 top_k 上限（对齐前端 UI 1-50）
pub const KB_TOP_K_SCHEMA_MAX: u32 = 50;

// ── 文件工具 ──

/// read 单次读取最大字符数（分页续读单元）
pub const MAX_FILE_READ_CHARS: usize = 8192;
/// ls / glob 单次最多返回条目数
pub const MAX_LIST_ITEMS: usize = 60;
/// 引用来源片段截断上限
pub const MAX_SOURCE_SNIPPET_CHARS: usize = 800;
/// grep 单次最多返回命中文件数
pub const MAX_GREP_FILES: usize = 20;
/// grep 输出字符上限
pub const MAX_GREP_OUTPUT_CHARS: usize = 60_000;
/// git_diff 输出字符上限（截断续读提示）
pub const GIT_DIFF_MAX_CHARS: usize = 60_000;
/// multi_edit 单次最多编辑数
pub const MAX_MULTI_EDITS: usize = 10;
/// read 的 paths 数组上限（并行读取文件数）
pub const READ_PATHS_MAX: usize = 10;
/// grep context_lines 上限
pub const GREP_CONTEXT_MAX: usize = 5;
/// 编辑/写入单文件大小上限（字节）
pub const MAX_EDIT_FILE_BYTES: u64 = 1024 * 1024;

// ── 子代理 ──

/// 子代理默认轮次上限
pub const SUBAGENT_MAX_TURNS: usize = 12;
/// 子代理摘要字符预算
pub const SUBAGENT_SUMMARY_CHARS: usize = 4_000;
/// 子代理工具参数 max_turns 上限（deep_research / spawn_subagent）
pub const SUBAGENT_MAX_TURNS_LIMIT: u32 = 30;
/// parallel_research 任务数下限
pub const PARALLEL_TASKS_MIN: usize = 2;
/// parallel_research 任务数上限
pub const PARALLEL_TASKS_MAX: usize = 5;
/// read_subagent_result 单次读取最大字符数
pub const SUBAGENT_RESULT_MAX_CHARS: u32 = 60_000;

// ── 外部工具 ──

/// 外部工具响应体上限（字符）
pub const MAX_EXTERNAL_RESPONSE_CHARS: usize = 100_000;
/// 外部工具默认超时（秒）
pub const EXTERNAL_TIMEOUT_SECS: u64 = 30;
/// webfetch 响应体上限（字节）
pub const WEBFETCH_MAX_BODY_BYTES: usize = 200 * 1024;
/// webfetch 提取文本上限（字符）
pub const WEBFETCH_MAX_CHARS: usize = 50_000;
/// webfetch 请求超时（秒）
pub const WEBFETCH_TIMEOUT_SECS: u64 = 10;
/// webfetch 最大重定向跳数（每跳重新校验目标地址，防 SSRF 绕过）
pub const WEBFETCH_MAX_REDIRECTS: u32 = 5;

// ── 其他工具 ──

/// pomodoro 时长上限（分钟）
pub const POMODORO_MINUTES_MAX: u32 = 180;

// ── 输出 token ──

/// 最大输出 token 合法上限（防御性 clamp：防异常配置/恶意超大值直传服务端；
/// 主流推理模型输出上限一般不超过 128K，512K 已覆盖所有已知模型）
pub const MAX_OUTPUT_TOKENS: u32 = 512_000;

// ── MCP ──

/// MCP 工具单次输出字符上限（content/structuredContent 拼接后截断，防撑爆模型上下文）
pub const MCP_MAX_OUTPUT_CHARS: usize = 60_000;
