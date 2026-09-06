//! Agent 模块：知识库检索助手 + 业务工具/业务 Hook（新内核 core/loop 承载）。
//!
//! - [`kb_search`]/[`code_search`]：检索/符号定位业务助手（rig-free，供 loop_tools 迁移工具调用）
//! - [`aggregate_hits`]：文档级聚合逻辑（与检索结果共享）
//! - [`loop_tools`]：迁移到 core/loop Tool trait 的业务工具（替代 rig DynamicTool）
//! - [`loop_hooks`]：迁移到 core/loop LoopHook 的业务 Hook（技能门禁/审批/技能指令）
//! - `BASE_TOOLS`/`SKILL_GATED_VISIBLE_TOOLS`：工具可见性语义（供 loop_hooks 使用）

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

use crate::core::skill::activation::ActiveSkillState;
use crate::core::skill::SkillRegistry;

pub mod planner;
/// AI Agent 指标参数集中配置（单一来源）
pub mod limits;
/// Agent 后台任务状态中心（切出页面任务继续、切回恢复视图）
pub mod task_store;

pub use limits::{
    AGREEMENT_BONUS_WEIGHT, DEFAULT_MAX_TURNS, MAX_CONTEXT_CHARS, QUERY_DIVERSITY_THRESHOLD,
};
use crate::core::{Indexer, SearchHit, QuerySource, call_embedding_query};

/// 规约文档缓存：(文件名 → (最后修改时间, 内容))。
///
/// 进程级缓存 + mtime 热重载：mtime 未变化时直接复用缓存内容，
/// 避免每条消息请求都在磁盘热路径上读文件；规约文件变更后下一次
/// 请求自动重读（无需重启应用）。对齐 OpenClaw 的缓存策略，
/// 同时借鉴 Codex「会话内加载一次」的语义（未变更时零读盘）。
static RULES_CACHE: OnceLock<Mutex<HashMap<String, (SystemTime, String)>>> = OnceLock::new();

fn rules_cache() -> &'static Mutex<HashMap<String, (SystemTime, String)>> {
    RULES_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 解析规约文件绝对路径：运行时资源目录（打包后）优先，源码资源目录（开发期）回退。
fn resolve_agent_rules_path(app: &AppHandle, name: &str) -> Option<PathBuf> {
    if let Ok(dir) = app.path().resource_dir() {
        let candidate = dir.join("agent").join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("agent")
        .join(name);
    fallback.exists().then_some(fallback)
}

/// 内置兜底规约（资源文件缺失或为空时），保证任何环境下 Agent 都有明确的中文回答约束。
fn builtin_agent_rules(name: &str) -> String {
    format!(
        "你是 mdgo 应用的{}助手。默认使用简体中文回答；若用户使用其他语言提问，则跟随用户语言。",
        if name.starts_with("rag") { "知识库" } else { "对话" }
    )
}

/// 加载 Agent 规约文档（resources/agent/{name}）。
///
/// 使用进程级缓存 + mtime 热重载：
/// - 缓存命中且文件 mtime 未变 → 直接返回缓存内容（零磁盘 I/O，请求热路径不受影响）
/// - mtime 变化或首次加载 → 读盘并更新缓存（规约修改后下一次请求自动生效）
/// - 文件缺失/为空 → 返回内置兜底规约
pub fn load_agent_rules(app: &AppHandle, name: &str) -> String {
    let path = match resolve_agent_rules_path(app, name) {
        Some(p) => p,
        None => return builtin_agent_rules(name),
    };

    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    {
        let cache = rules_cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_mtime, content)) = cache.get(name) {
            if let Some(actual) = mtime {
                if actual == *cached_mtime {
                    return content.clone();
                }
            }
        }
    }

    // mtime 变化或首次加载：读盘并更新缓存
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    if content.trim().is_empty() {
        rules_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        return builtin_agent_rules(name);
    }
    let mut cache = rules_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(actual) = mtime {
        cache.insert(name.to_string(), (actual, content.clone()));
    }
    log::info!("[agent_rules] 加载 Agent 规约: path={}", path.display());
    content
}

/// 内置工具集：文件只读、目录列举、Git 状态查询（含工具调用轨迹总线）
pub mod tools;
/// 业务工具的新内核实现（直接构建于 core/loop Tool trait，替代 rig DynamicTool；M1 并行验证期）
pub mod loop_tools;
/// 业务 Hook 的新内核实现（技能门禁/审批门 → core/loop LoopHook，替代 rig AgentHook）
pub mod loop_hooks;
/// 动态外部工具（P2-15，配置驱动）：定义加载 + mtime 缓存；执行由 loop_tools 承担
pub(crate) mod external_tools;
/// Web 搜索提供商适配层（Tavily / Brave / Exa）：配置 + API 调用 + 结果格式化
pub mod search_providers;
/// 工具注册表：按技能组织工具定义，统一管理工具的注册与构建

/// 始终可用的基础工具（不随技能白名单窄化，对齐主流 Agent：文件操作与技能管理常驻）。
///
/// 检索类工具（kb_search / code_lookup）**不在此列**（属于技能披露体系，激活
/// kb-search 技能后才可用）。其可见性由 [`SkillInstructionHook`] 显式补齐
/// （软门禁：始终可见可调，避免 rig active_tools 硬过滤产生 UnknownToolCall
/// 致命错误），可用性由 [`SkillGateHook`]（on_tool_call Skip 引导）与工具闭包
/// 内部守卫（未激活返回引导）双层拦截——模型未激活技能时收到温和引导并在
/// 下一轮激活后重试，而非整个流式请求失败。
pub const BASE_TOOLS: &[&str] = &[
    "activate_skill", "deactivate_skill", "read", "ls", "glob", "grep", "write", "edit", "multi_edit", "delete",
    "git_status", "git_diff", "git_commit", "git_checkout", "webfetch", "web_search", "deep_research", "read_subagent_result",
    "remember", "forget", "search_memory", "todo_write", "spawn_subagent", "parallel_research", "self_review",
    "doc_agent", "parallel_doc_agent", "ask_user_question",
];

/// 软门禁可见工具：始终出现在 `active_tools`（模型可见可调，不会 UnknownToolCall），
/// 但未激活声明技能时由 SkillGateHook Skip + 工具闭包守卫引导，不执行实际操作。
/// 覆盖**高频交互/查询类**技能声明工具（非 BASE_TOOLS）：
/// - 检索（kb_search/code_lookup）+ 日程（schedule）+ 番茄钟（pomodoro）+ RAW（raw-parse）
/// - 交互导航（open-ui）+ 书签查询（search_bookmarks/get_bookmark）
/// 与 BASE_TOOLS 分离，保持"基础工具"语义纯净（这些属技能披露体系）。
/// 注：Canvas 是知识文件格式（非工具），读写走通用 read/write，无需列入本清单。
pub const SKILL_GATED_VISIBLE_TOOLS: &[&str] = &[
    "kb_search", "code_lookup", "schedule", "pomodoro", "raw-parse",
    "open-ui", "search_bookmarks", "get_bookmark",
];

// ─── Action Claim Guard（P0-3）声明表 ───

/// 一条「动作声称 → 必须调用工具」约束（Action Claim Registry 项）。
///
/// 语义：当最终回答同时命中 `verbs`（完成式声称词）与 `objects`（动作对象词），
/// 但本次请求未调用 `required_tools` 中任一工具（也未调用 `observe_tools` 观察类
/// 工具）时，判定为「声称执行了未执行的操作」，由 [`apply_anti_hallucination_guard`]
/// 追加一致性提醒。
///
/// 设计约束（对照硬编码关键词守卫的升级）：
/// - 声明式驱动：新增动作只需在 [`ACTION_CLAIMS`] 增加一行，无需改守卫逻辑；
/// - 词集刻意收窄（完成式 + 具体对象）以降低误报，与既有 schedule 守卫口径一致；
/// - `observe_tools` 用于「观察即豁免」场景（如 git 状态复述）：调用过观察类工具
///   说明声称来自工具观察而非凭空断言，不判定为虚构执行；
/// - 提醒为追加而非拦截，偶发误报只会提示用户核实，不阻断回答。
#[derive(Debug)]
pub struct ActionClaim {
    pub id: &'static str,
    pub verbs: &'static [&'static str],
    pub objects: &'static [&'static str],
    pub required_tools: &'static [&'static str],
    /// 观察类工具：命中即豁免声称判定（声称来自工具观察，非虚构执行）
    pub observe_tools: &'static [&'static str],
}

/// Action Claim Registry：全部「声称完成某操作」的约束声明（顺序即优先级，首个命中生效）。
pub const ACTION_CLAIMS: &[ActionClaim] = &[
    ActionClaim {
        id: "schedule_write",
        verbs: &[
            "已创建", "创建了", "已添加", "添加了", "已安排", "安排了", "已预约", "预约了",
            "已更新", "更新了", "已删除", "删除了", "已保存", "保存了", "已写入",
            "创建成功", "添加成功", "删除成功", "已设置", "设置了", "已配置", "配置好了",
            "已提醒", "提醒了",
        ],
        objects: &["日程", "日历", "会议", "预约", "专注时间块", "专注块"],
        required_tools: &["schedule"],
        observe_tools: &[],
    },
    ActionClaim {
        id: "file_write",
        verbs: &["已写入", "写入了", "保存了", "已保存", "创建了", "已创建", "写入成功", "保存成功", "创建成功"],
        objects: &["文件", "文档", "笔记"],
        required_tools: &["write", "edit", "multi_edit"],
        observe_tools: &[],
    },
    ActionClaim {
        id: "file_delete",
        verbs: &["已删除", "删除了", "删除成功", "清理了", "已清理", "移除了", "已移除"],
        objects: &["文件", "文档", "笔记"],
        required_tools: &["delete"],
        observe_tools: &[],
    },
    ActionClaim {
        id: "git_commit",
        verbs: &["已提交", "提交了", "提交成功", "已 commit", "commit 了"],
        objects: &["代码", "改动", "修改", "提交"],
        required_tools: &["git_commit"],
        // 复述 git 状态（git_status/git_diff 输出）不属于"声称执行提交"→ 观察即豁免
        observe_tools: &["git_status", "git_diff"],
    },
];

// ─── Agent Quality Metrics（P2-9）───

/// 进程内 Agent 质量计数（P2-9，轻量实现：内存计数 + 日志输出，不落库）。
///
/// 用途：迭代验证防幻觉守卫的实际触发率与工具执行可靠性——
/// - Hallucination Rate ≈ `hallucination_warnings / requests`（守卫触发率）
/// - 工具成功率 ≈ `tool_successes / (tool_successes + tool_failures)`
///
/// Recovery Rate（失败后换路成功）需 per-run 状态机，超出轻量范围；
/// 先以失败率暴露可靠性，后续可在 [`ToolCallBus`] 基础上升级。
pub struct AgentQualityCounters {
    /// 累计请求数（agent_query 入口 +1）
    pub requests: AtomicU64,
    /// 防幻觉守卫触发次数（Action Claim Guard + Grounding Validator 命中 +1）
    pub hallucination_warnings: AtomicU64,
    /// 工具执行成功次数（record_tool_result ok=true +1）
    pub tool_successes: AtomicU64,
    /// 工具执行失败次数（record_tool_result ok=false +1）
    pub tool_failures: AtomicU64,
}

impl AgentQualityCounters {
    pub const fn new() -> Self {
        Self {
            requests: AtomicU64::new(0),
            hallucination_warnings: AtomicU64::new(0),
            tool_successes: AtomicU64::new(0),
            tool_failures: AtomicU64::new(0),
        }
    }
}

static AGENT_QUALITY: OnceLock<AgentQualityCounters> = OnceLock::new();

/// 获取全局质量计数器（首次调用初始化）。
pub fn agent_quality() -> &'static AgentQualityCounters {
    AGENT_QUALITY.get_or_init(AgentQualityCounters::new)
}

/// 输出当前质量指标摘要（日志，供调试与迭代观察）。
#[allow(dead_code)] // 调试入口：按需调用或在控制台手动观察，不参与业务路径
pub fn log_quality_summary() {
    let q = agent_quality();
    let requests = q.requests.load(Ordering::Relaxed);
    let warnings = q.hallucination_warnings.load(Ordering::Relaxed);
    let ok = q.tool_successes.load(Ordering::Relaxed);
    let fail = q.tool_failures.load(Ordering::Relaxed);
    let hallucination_rate = if requests > 0 {
        warnings as f64 / requests as f64
    } else {
        0.0
    };
    let tool_total = ok + fail;
    let fail_rate = if tool_total > 0 {
        fail as f64 / tool_total as f64
    } else {
        0.0
    };
    log::info!(
        "[agent-quality] requests={} hallucination_warnings={} (rate={:.2}) tool_ok={} tool_fail={} (fail_rate={:.2})",
        requests,
        warnings,
        hallucination_rate,
        ok,
        fail,
        fail_rate
    );
}

/// Agent 单次请求的模型调用总预算定义见 [`limits::DEFAULT_MAX_TURNS`]（集中配置）
///
/// 语义 = 模型调用次数上限（1-based）：第 1 次调用 turn=1，turn=10 是最后一次，
/// 第 11 次请求触发 MaxTurnsError。超出预算的流程由轮次预算预警 Hook 引导模型提前收尾。
// （常量定义已迁移至 limits.rs，经 pub use 再导出）


/// kb_search 工具允许的最大片段数（防止模型传入超大 top_k 触发全量检索/重排）
// MAX_TOP_K 定义已迁移至 limits.rs（pub use 再导出）

/// 聚合后送入模型上下文的总字符上限（约 3K token，避免超出模型窗口）。
/// kb_search 工具与 Agent 主链路共用此上限（单一来源）。
// MAX_CONTEXT_CHARS 定义已迁移至 limits.rs（pub use 再导出）

/// kb_search 工具的运行参数
#[derive(Clone)]
pub struct KbSearchConfig {
    /// 检索的知识库目录
    pub dir_path: String,
    /// 目录黑名单（gitignore 风格，如 `assets/`、`node_modules/`）：
    /// `ls`/`glob`/`grep` 文件枚举工具按此过滤目录
    pub dir_blacklist: Vec<String>,
    /// 文件黑名单（gitignore 风格，如 `*.log`）：
    /// `ls`/`glob`/`grep` 文件枚举工具按此过滤文件
    pub file_blacklist: Vec<String>,
    /// 索引器（混合检索）
    pub indexer: Arc<Indexer>,
    /// 默认返回的片段数量（模型未指定 top_k 时使用）
    pub default_top_k: u32,
    /// 当前请求 ID（用于工具调用轨迹的事件关联）
    pub request_id: String,
    /// 聚合绝对阈值（融合分数低于此值的命中不进入上下文；RRF 域）
    pub min_score: f32,
    /// 精排 sigmoid 阈值（`score_rerank` 非空的命中按此阈值裁决；与 pipeline 内精排阈值同语义）
    pub rerank_min_score: f32,
    /// 送入上下文的最大文档数
    pub max_context_docs: usize,
    /// 单文档最多保留的 chunk 数
    pub max_chunks_per_doc: usize,
    /// 触发本次请求的技能 ID（格式：scope:skill_id），用于工具调用轨迹标注
    pub skill_id: Option<String>,
    /// 当前请求的激活技能状态（`read` 工具按需读取 L3 references；钩子动态注入 L2 指令）
    pub skill_state: Arc<ActiveSkillState>,
    /// 技能软门禁开关（P0-8 修复）：主对话 true——技能声明类工具（kb_search/
    /// code_lookup/schedule/pomodoro/raw-parse/open-ui）仅当声明技能 Active 时执行，
    /// 无激活技能（`allowed_tools()==None`）时返回引导（与 SkillGateHook 语义一致）；
    /// 子代理等受限场景 false——工具白名单已在注册表层过滤，无需技能声明即可执行
    /// （对齐子代理 allow_all 语义，避免破坏只读/写型子代理的检索能力）。
    pub skill_gating: bool,
    /// 各作用域技能基础目录（(scope, 绝对路径)），`read` 工具据此定位已激活技能的 references
    pub skill_bases: Vec<(String, String)>,
    /// 技能注册表（内存常驻）：系统内置技能为编译期嵌入、启动时已解析进内存，
    /// `read` 工具读取已激活技能 SKILL.md 完整正文时直接从内存取用，不落盘
    pub skill_registry: Arc<SkillRegistry>,
    /// 检索命中收集器：kb_search / code_lookup 工具将聚合后的命中写入，
    /// 命令层在请求结束（rag:done）时合并进引用来源列表，供前端渲染"引用"。
    pub search_sink: Arc<tokio::sync::Mutex<Vec<(SearchHit, f32)>>>,
    /// 应用句柄：供前端通信桥工具（如 pomodoro）emit 事件并等待前端回传。
    pub app_handle: tauri::AppHandle,
    /// 当前请求的取消令牌：deep_research 等长耗时工具据此在父链取消时快速中止
    /// （无外部取消源的场景为 None）。
    pub cancel: Option<CancellationToken>,
}

/// 执行一次完整检索：嵌入 → 混合检索 → 文档级聚合 → 生成模型可读文本。
///
/// 返回的文本按文档分组，同文档的多个片段合并，供模型直接作为上下文；
/// 结构化输出（来源列表）供前端增强卡片渲染（P1-5）。
pub async fn kb_search(
    cfg: &KbSearchConfig,
    query: &str,
    top_k: u32,
) -> Result<(String, Option<serde_json::Value>), String> {
    log::info!("[skill] kb_search: query={}, top_k={}", query, top_k);
    let embedding = tokio::task::spawn_blocking({
        let query = query.to_string();
        move || call_embedding_query(&query)
    })
    .await
    .map_err(|e| format!("查询向量计算任务失败: {}", e))?
    .map_err(|e| e)?
    .into_iter()
    .next()
    .ok_or_else(|| "生成查询向量失败".to_string())?;

    let hits = cfg
        .indexer
        .hybrid_search(&cfg.dir_path, &embedding, query, top_k)
        .await?;
    if hits.is_empty() {
        return Ok(("知识库中未找到相关内容。".to_string(), None));
    }

    let hits_len = hits.len();
    let selected = aggregate_hits(
        hits,
        cfg.min_score,
        cfg.rerank_min_score,
        cfg.max_context_docs,
        cfg.max_chunks_per_doc,
        None, // kb_search 工具为单查询路径，无跨查询一致性统计
    );
    if selected.is_empty() {
        return Ok(("知识库中未找到足够相关的内容。".to_string(), None));
    }

    // 命中回传：合并进 rag:done 的引用来源（供前端渲染"引用"）
    cfg.search_sink
        .lock()
        .await
        .extend(selected.iter().cloned());
    log::info!("[skill] kb_search 结果: 选中={}， 命中={} ，min_score={}， max_context_docs={}， max_chunks_per_doc={}", selected.len(), hits_len, cfg.min_score, cfg.max_context_docs, cfg.max_chunks_per_doc);
    // P1-8：工具输出同样过注入防护——检索片段可能含恶意指令，包裹并提示模型忽略
    // （与主链路预检索的 wrap_suspicious 语义一致，见 commands/llm.rs）
    // P1-5：结构化输出（来源列表）供前端渲染增强卡片
    let text = crate::core::security::wrap_suspicious(&build_context_text(
        &selected,
        MAX_CONTEXT_CHARS,
    ));
    let structured = serde_json::json!({
        "sources": selected.iter().map(|(hit, score)| {
            serde_json::json!({
                "doc_name": hit.doc_name,
                "score": score,
            })
        }).collect::<Vec<_>>(),
    });
    Ok((text, Some(structured)))
}

/// 执行代码符号检索：按符号名（函数/类/方法名等）精确或前缀匹配定位代码定义。
///
/// 返回的文本按文档分组，供模型直接作为上下文。与 `kb_search`（语义检索）互补；
/// 结构化输出（来源列表）供前端增强卡片渲染（P1-5）。
pub async fn code_search(
    cfg: &KbSearchConfig,
    symbol: &str,
    top_k: u32,
) -> Result<(String, Option<serde_json::Value>), String> {
    let hits = cfg.indexer.search_symbols(&cfg.dir_path, symbol, top_k).await?;
    if hits.is_empty() {
        return Ok((format!("知识库中未找到与符号 '{}' 相关的代码。", symbol), None));
    }

    let mut parts: Vec<String> = Vec::new();
    let mut last_doc = String::new();
    for hit in &hits {
        if hit.doc_name != last_doc {
            if !last_doc.is_empty() {
                parts.push(String::new());
            }
            parts.push(format!("--- {} ---", hit.doc_name));
            last_doc = hit.doc_name.clone();
        }
        let kind = hit.symbol_kind.as_deref().unwrap_or("symbol");
        let name = hit.symbol_name.as_deref().unwrap_or("");
        let text = hit.sentence_window.as_deref().unwrap_or(&hit.text);
        parts.push(format!("[{kind}] {name}\n{text}"));
    }

    // 命中回传：合并进 rag:done 的引用来源（供前端渲染"引用"）
    cfg.search_sink
        .lock()
        .await
        .extend(hits.iter().map(|h| (h.clone(), h.score)));

    // P1-8：与 kb_search 一致，工具输出过注入防护（代码片段同样可能含指令性内容）
    // P1-5：结构化输出（来源列表）供前端增强卡片渲染
    let structured = serde_json::json!({
        "sources": hits.iter().map(|h| {
            serde_json::json!({
                "doc_name": h.doc_name,
                "symbol_name": h.symbol_name,
                "symbol_kind": h.symbol_kind,
            })
        }).collect::<Vec<_>>(),
    });
    Ok((
        crate::core::security::wrap_suspicious(&parts.join("\n")),
        Some(structured),
    ))
}

/// # 分数域契约（与 `Indexer::hybrid_search` 输出对齐）
/// `SearchHit.score` 在不同阶段承载不同分数域，本函数按域选择阈值：
/// - `score_rerank: Some(_)`：sigmoid 域（精排激活）→ 用 `rerank_min_score`（模型判定门槛，
///   与 pipeline 内精排阈值同语义，幂等兜底）
/// - `symbol_name: Some(_)` 且未精排：符号强信号 → 完全放行（符号路召回已按匹配质量
///   分级截断，且其 RRF 归一化分无判别力，不受融合阈值约束）
/// - 其余：RRF 归一化域（精排未激活/失败回退）→ 用 `min_score`（融合兜底阈值）
pub fn aggregate_hits(
    all_hits: Vec<SearchHit>,
    min_score: f32,
    rerank_min_score: f32,
    max_docs: usize,
    max_chunks_per_doc: usize,
    query_vectors: Option<&HashMap<QuerySource, Vec<f32>>>,
) -> Vec<(SearchHit, f32)> {
    // 1. 按 doc_name + chunk_index 去重，保留最高分
    let mut seen: HashMap<(String, u32), (SearchHit, f32)> = HashMap::new();
    for hit in all_hits {
        let score = hit.score;
        let key = (hit.doc_name.clone(), hit.chunk_index);
        match seen.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if score > entry.get().1 {
                    entry.insert((hit, score));
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((hit, score));
            }
        }
    }

    // 2. 按 doc_name 分组，分数域感知的绝对阈值过滤（见函数文档的分数域契约）
    let mut doc_map: HashMap<String, Vec<(SearchHit, f32)>> = HashMap::new();
    for (hit, score) in seen.into_values() {
        let is_sigmoid = hit.score_rerank.is_some();
        let is_symbol = hit.symbol_name.is_some();
        let pass = if is_sigmoid {
            score >= rerank_min_score
        } else if is_symbol {
            true
        } else {
            score >= min_score
        };
        if !pass {
            continue;
        }
        let doc_name = hit.doc_name.clone();
        doc_map.entry(doc_name).or_default().push((hit, score));
    }

    // 3. 以每篇文档的最佳 chunk 分数作为文档代表分排序（同分按 doc_name 字典序保证确定性），
    //    每篇文档内按分数降序截断，再取 top max_docs 文档。
    //    P1：跨查询一致性加成——被多个"不同角度"查询命中的文档证据更强，
    //    文档代表分 = 最佳 chunk 分 + agreement × AGREEMENT_BONUS_WEIGHT（clamp ≤ 1.0）。
    let mut doc_scores: Vec<(String, f32, Vec<(SearchHit, f32)>)> = doc_map
        .into_values()
        .map(|mut chunks| {
            chunks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            chunks.truncate(max_chunks_per_doc.max(1));
            let best = chunks.first().map(|(_, s)| *s).unwrap_or(0.0);
            let doc = chunks
                .first()
                .map(|(h, _)| h.doc_name.clone())
                .unwrap_or_default();
            let rank_score = (best + agreement_bonus(&chunks, query_vectors)).min(1.0);
            (doc, rank_score, chunks)
        })
        .collect();
    doc_scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    doc_scores.truncate(max_docs.max(1));

    doc_scores
        .into_iter()
        .flat_map(|(_, _, chunks)| chunks)
        .collect()
}

/// P1：跨查询一致性加成——统计文档所有 chunk 命中的**不同来源查询对**，
/// 仅当来源查询向量差异足够大（cosine < `QUERY_DIVERSITY_THRESHOLD`，视为
/// "不同角度"）时才计分；无向量可用的来源对保守计分（不同来源即视为不同角度）。
///
/// 防止"虚假共识"：三个高度相似的查询命中同一文档不加分（如 `Redis 分布式锁` /
/// `Redis 分布式锁实现` / `Redis 分布式锁代码`），三个异构查询命中才加
/// （如 `Redis 分布式锁` / `Redisson RLock` / `Redis Lua 原子锁`）。
fn agreement_bonus(
    chunks: &[(SearchHit, f32)],
    query_vectors: Option<&HashMap<QuerySource, Vec<f32>>>,
) -> f32 {
    let mut srcs: Vec<QuerySource> = Vec::new();
    for (h, _) in chunks {
        for s in &h.query_sources {
            if !srcs.contains(s) {
                srcs.push(*s);
            }
        }
    }
    if srcs.len() < 2 {
        return 0.0;
    }
    let mut count = 0.0f32;
    for i in 0..srcs.len() {
        for j in (i + 1)..srcs.len() {
            let diverse = match (
                query_vectors.and_then(|m| m.get(&srcs[i])),
                query_vectors.and_then(|m| m.get(&srcs[j])),
            ) {
                (Some(a), Some(b)) => crate::core::db::utils::cosine_similarity(a, b)
                    < QUERY_DIVERSITY_THRESHOLD as f64,
                _ => true, // 无向量时保守按不同来源计
            };
            if diverse {
                count += 1.0;
            }
        }
    }
    count * AGREEMENT_BONUS_WEIGHT
}

/// 将聚合后的命中组装为模型可读的上下文文本。
///
/// 文档顺序（分数降序，即 `aggregate_hits` 输出顺序）保持不变；
/// 文档内按 `chunk_index` 阅读顺序重排，保证送入模型的片段连贯可读。
/// 总字符数受 `max_chars` 限制，超出部分截断。
pub fn build_context_text(selected: &[(SearchHit, f32)], max_chars: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0usize;

    let mut i = 0usize;
    while i < selected.len() {
        let doc = selected[i].0.doc_name.clone();
        // 收集同文档连续片段（aggregate_hits 保证同文档相邻），按阅读顺序重排
        let mut run: Vec<&SearchHit> = Vec::new();
        while i < selected.len() && selected[i].0.doc_name == doc {
            run.push(&selected[i].0);
            i += 1;
        }
        run.sort_by_key(|h| h.chunk_index);

        if !parts.is_empty() {
            parts.push(String::new());
        }
        let header = format!("--- {} ---", doc);
        if used + header.len() > max_chars {
            break;
        }
        used += header.len();
        parts.push(header);

        for hit in run {
            let text = hit.sentence_window.as_deref().unwrap_or(&hit.text);
            if used + text.len() + 1 > max_chars {
                break;
            }
            parts.push(text.to_string());
            used += text.len() + 1;
        }
    }

    parts.join("\n")
}

