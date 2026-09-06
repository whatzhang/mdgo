//! AI Agent 指标参数集中配置（单一来源，避免多文件双份维护）。
//!
//! 集中收纳所有「指标/上限类」参数：模型调用预算、检索阈值、文件工具上限、
//! 子代理预算、外部工具护栏、工具 schema 的 min/max/maxLength/maxItems 约束值。
//! 工具 schema 生成与代码层 clamp 均引用本模块常量，保证单一口径。

// ── 模型调用预算 ──

/// Agent 单次请求的模型调用总预算（1-based：turn=20 为最后一次，第 21 次触发 MaxTurnsError）。
/// 20 轮对齐主流 Agent（Claude Code / Codex）的多步工具任务预算；
/// 预算预警 Hook（`SkillInstructionHook`）在剩余 3 轮时提前引导模型收尾。
pub const DEFAULT_MAX_TURNS: usize = 20;
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

// ── 预检索查询扩展（P0 预检索优化器）──

/// 扩展查询数量上限（商用默认：原始查询 + 至多 2 条扩展）
pub const MAX_EXPANDED_QUERIES: usize = 2;
/// 单次预检索总查询数上限（原始 + 扩展，防发散）
pub const MAX_TOTAL_QUERIES: usize = 3;
/// 查询扩展 LLM 调用的独立总时限（秒）。
///
/// 扩展与原始查询检索经 `tokio::join!` 并发，但流水线**必须等它到点才进入下一阶段**
/// （日志：慢端点 `查询扩展超时（>10s）` 后流水线才继续），处于首答关键路径上。
/// 慢模型端点上 10s 几乎必然超时、每次白等 10s 却拿不到扩展结果——收窄到 5s：
/// 慢端点少等 5s；正常端点（扩展输出短，通常 <5s）功能不受影响。超时 fail-open 回退为仅原始查询。
pub const QUERY_EXPANSION_TIMEOUT_SECS: u64 = 5;
/// 查询扩展调用的重试次数（预检索预算从紧：总尝试 = 重试次数 + 1）。
/// 🟠 L28 修复：5s 总时限下最多容纳 1 次重试（退避 2s 起步，第 2 次重试需再等
/// 4s，必然超时）——旧值 2 使重试形同虚设；1 次重试覆盖瞬时抖动且不挤占时限。
pub const QUERY_EXPANSION_RETRY_MAX: usize = 1;
/// 扩展查询去重的 embedding cosine 阈值（≥ 视为重复，丢弃后生成的）
pub const QUERY_DEDUP_SIMILARITY: f32 = 0.92;
/// 跨查询一致性加成的查询对多样性判定阈值：来源查询对 cosine 低于此值
/// 视为"不同角度"，才计入 agreement（防三个相似查询制造虚假共识）
pub const QUERY_DIVERSITY_THRESHOLD: f32 = 0.85;
/// 文档级一致性加成系数（叠加到文档代表分，仅影响排序、不改变通过/拒绝）
pub const AGREEMENT_BONUS_WEIGHT: f32 = 0.05;

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

/// 子代理摘要字符预算
pub const SUBAGENT_SUMMARY_CHARS: usize = 4_000;

// ── DocAgent（文档子 Agent）──

/// DocAgent 单次会话的最大模型轮次（轻量问答：低于主 Agent 的 20 轮预算）
pub const DOC_MAX_TURNS: usize = 12;
/// DocAgent 单文件上下文默认预算（token；调用方可按模型上下文窗口覆盖）
pub const DOC_DEFAULT_BUDGET_TOKENS: usize = 16_000;

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

/// ask_user_question 等待用户回答的超时（秒）；超时视为「未回答」
pub const ASK_USER_TIMEOUT_SECS: u64 = 120;

// ── 输出 token ──

/// 最大输出 token 合法上限（防御性 clamp：防异常配置/恶意超大值直传服务端；
/// 主流推理模型输出上限一般不超过 128K，512K 已覆盖所有已知模型）
pub const MAX_OUTPUT_TOKENS: u32 = 512_000;

// ── MCP ──

/// MCP 工具单次输出字符上限（content/structuredContent 拼接后截断，防撑爆模型上下文）
pub const MCP_MAX_OUTPUT_CHARS: usize = 60_000;
