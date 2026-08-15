//! 工具注册表：按技能组织工具定义，统一管理工具的注册与构建。
//!
//! 方案一（技能感知的工具延迟注册）的核心基础设施：
//! - 每个工具通过 `ToolRegistry::register` 注册其构建函数
//! - `build_rag_agent` 通过 `build_all` 一次性构建所有已注册工具
//! - 工具可见性由 [`SkillInstructionHook`] 依据已激活技能动态窄化（`active_tools`）
//!
//! 200+ 工具场景下，每个工具只需一行 `register` 调用，无需逐个手写
//! `.dynamic_tool(...)`；工具按技能组织，维护成本低。

use rig_agent::tool::DynamicTool;

use super::KbSearchConfig;

/// 工具构建函数：接受 `KbSearchConfig`，返回 `DynamicTool`。
///
/// 大多数工具仅依赖 `KbSearchConfig`（含 `app_handle`、`skill_state` 等），
/// `activate_skill` / `deactivate_skill` 依赖 `Arc<SkillRegistry>` / `Arc<ActiveSkillState>`，
/// 由 `build_rag_agent` 直接注册，不走本注册表。
pub type ToolBuilder = Box<dyn Fn(KbSearchConfig) -> DynamicTool + Send + Sync>;

/// 工具注册条目
struct ToolEntry {
    /// 工具名称（与 `DynamicTool::name` 一致，供 `active_tools` 过滤）
    name: String,
    /// 工具构建函数
    builder: ToolBuilder,
}

/// 工具注册表（线程安全，构建期写入，运行期只读）。
pub struct ToolRegistry {
    entries: Vec<ToolEntry>,
}

impl ToolRegistry {
    /// 创建空注册表。
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 注册一个工具。
    ///
    /// `name` 必须与 `DynamicTool` 内部名称一致，否则 `active_tools` 过滤会失效。
    pub fn register(&mut self, name: &str, builder: ToolBuilder) {
        self.entries.push(ToolEntry {
            name: name.to_string(),
            builder,
        });
    }

    /// 构建所有已注册工具（消耗 `KbSearchConfig` 的克隆）。
    ///
    /// 返回的 `Vec<DynamicTool>` 可直接通过 `builder.dynamic_tool(t)` 逐项注册到
    /// `AgentBuilder`。
    pub fn build_all(&self, cfg: &KbSearchConfig) -> Vec<DynamicTool> {
        self.entries
            .iter()
            .map(|entry| (entry.builder)(cfg.clone()))
            .collect()
    }

    /// 获取所有已注册工具的名称列表（供日志/调试）。
    #[allow(dead_code)]
    pub fn tool_names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }
}

/// P2-7：静态工具分组元数据（filesystem / knowledge / git / schedule / memory / …）。
///
/// 定位：仅为「上下文组织」与「未来按需发现（tool search）」提供声明式分组，
/// **不参与执行逻辑**（可见性仍由 SkillInstructionHook 的 active_tools 控制）。
/// 当前工具规模（~28）未到动态发现阈值，先沉淀分组事实；超 50+ 工具时再基于
/// 此分组实现按需加载，无需改动注册流程。
#[allow(dead_code)] // P2-7 元数据：暂未消费，供未来按需发现使用（见函数文档）
pub fn tool_groups() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        (
            "filesystem",
            &["read", "write", "edit", "multi_edit", "delete", "ls", "glob", "grep"],
        ),
        ("knowledge", &["kb_search", "code_lookup"]),
        ("git", &["git_status", "git_diff", "git_commit", "git_checkout"]),
        ("schedule", &["schedule"]),
        ("memory", &["remember", "forget", "search_memory"]),
        ("productivity", &["pomodoro", "raw-parse", "todo_write"]),
        (
            "research",
            &[
                "deep_research",
                "read_subagent_result",
                "spawn_subagent",
                "parallel_research",
                "self_review",
                "webfetch",
            ],
        ),
    ]
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}