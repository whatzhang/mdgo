//! Agent Git 只读工具（status / log / show / diff / refs）。
//!
//! 设计约束：
//! - 只读、`concurrency_safe=true`、无需审批；
//! - 严格模式：`.mdgo` 与敏感文件的路径/统计/内容都不进入模型；
//! - 不自打点：`on_call/on_result` 由调度器统一发送，避免同一调用两张卡片；
//! - 尊重取消：`spawn_blocking` + `CancelFlag`，取消时 kill git 子进程。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::core::git::{
    CancelFlag, DiffQuery, GitError, GitReadService, LogQuery, RefKind, RefsQuery, ShowQuery,
    StatusQuery, render,
};
use crate::core::r#loop::{Tool, ToolError, ToolRunContext, ToolSpec};
use crate::core::security::wrap_suspicious;

use super::super::KbSearchConfig;

fn spec_with_timeout(mut spec: ToolSpec) -> ToolSpec {
    spec.concurrency_safe = true;
    spec.timeout_ms = Some(25_000);
    spec
}

/// 在阻塞线程执行 Git 服务，取消时置位并等待 kill。
async fn run_git_task<T, F>(ctx: &ToolRunContext<'_>, task: F) -> Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce(CancelFlag) -> Result<T, GitError> + Send + 'static,
{
    let flag = CancelFlag::new();
    let task_flag = flag.clone();
    let join = tokio::task::spawn_blocking(move || task(task_flag));
    tokio::pin!(join);
    tokio::select! {
        _ = ctx.cancel.cancelled() => {
            flag.cancel();
            let _ = join.await;
            Err(ToolError::Cancelled)
        }
        res = &mut join => match res {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(ToolError::Failed(e.to_user_message())),
            Err(e) => Err(ToolError::Failed(format!("Git 任务执行失败: {e}"))),
        },
    }
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn arg_u32(args: &Value, key: &str, default: u32) -> u32 {
    args.get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(default)
}

// ─────────────────────────── git_status ───────────────────────────

/// git_status：工作区状态 + 分支/上游。
pub struct GitStatusTool {
    cfg: KbSearchConfig,
}

impl GitStatusTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GitStatusTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            spec_with_timeout(ToolSpec::new(
                "git_status",
                "查看当前知识库 Git 工作区状态：分支、上游、领先/落后、已暂存/未暂存/未跟踪/冲突文件。只读；.mdgo 内部数据与敏感文件不返回。用户问“有什么改动/未提交/仓库状态”时先调用本工具。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "只看知识库内某个相对路径（可选）"
                        },
                        "max_entries": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 500,
                            "description": "最多展示条目数，默认 200"
                        }
                    },
                    "required": []
                }),
            ))
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let path = arg_str(&args, "path");
        let max_entries = arg_u32(&args, "max_entries", 200).clamp(1, 500) as usize;
        let cfg = self.cfg.clone();
        let service = GitReadService::new();
        let status = run_git_task(ctx, move |flag| {
            service.status(
                &cfg.dir_path,
                &StatusQuery {
                    path,
                    max_entries,
                    strict: true,
                },
                &flag,
            )
        })
        .await?;
        Ok(Value::String(wrap_suspicious(&render::render_status(
            &status,
        ))))
    }
}

// ─────────────────────────── git_log ───────────────────────────

/// git_log：提交历史（分页/路径/作者/时间/消息/改动搜索）。
pub struct GitLogTool {
    cfg: KbSearchConfig,
}

impl GitLogTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GitLogTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            spec_with_timeout(ToolSpec::new(
                "git_log",
                "查看当前知识库的 Git 提交历史。path 可查某个文件/目录的历史；grep 搜提交信息；pickaxe 搜“哪次提交新增/删除了该字符串”；author/since/until 可过滤；limit/skip 翻页。只读；不返回 .mdgo 与敏感文件内容。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "本页条数，默认 20" },
                        "skip": { "type": "integer", "minimum": 0, "maximum": 10000, "description": "跳过条数，默认 0" },
                        "rev": { "type": "string", "description": "起始引用（分支/标签/提交 SHA），默认 HEAD" },
                        "path": { "type": "string", "description": "只看涉及该路径的提交（知识库内相对路径）" },
                        "author": { "type": "string", "description": "作者过滤（git --author 正则）" },
                        "since": { "type": "string", "description": "起始时间（git 可解析，如 2025-11-01 / 2 weeks ago）" },
                        "until": { "type": "string", "description": "结束时间" },
                        "grep": { "type": "string", "description": "提交消息搜索（固定字符串）" },
                        "pickaxe": { "type": "string", "description": "搜索哪次提交新增/删除了该字符串（git -S）" },
                        "ignore_case": { "type": "boolean", "description": "grep 忽略大小写，默认 false" }
                    },
                    "required": []
                }),
            ))
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let q = LogQuery {
            limit: arg_u32(&args, "limit", 20).clamp(1, 100),
            skip: arg_u32(&args, "skip", 0).min(10_000),
            rev: arg_str(&args, "rev"),
            path: arg_str(&args, "path"),
            author: arg_str(&args, "author"),
            since: arg_str(&args, "since"),
            until: arg_str(&args, "until"),
            grep: arg_str(&args, "grep"),
            pickaxe: arg_str(&args, "pickaxe"),
            ignore_case: arg_bool(&args, "ignore_case"),
            strict: true,
        };
        let cfg = self.cfg.clone();
        let service = GitReadService::new();
        let result = run_git_task(ctx, move |flag| service.log(&cfg.dir_path, &q, &flag)).await?;
        Ok(Value::String(wrap_suspicious(&render::render_log(&result))))
    }
}

// ─────────────────────────── git_show ───────────────────────────

/// git_show：某次提交的元信息 + 文件清单 + 补丁。
pub struct GitShowTool {
    cfg: KbSearchConfig,
}

impl GitShowTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GitShowTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            spec_with_timeout(ToolSpec::new(
                "git_show",
                "查看某次提交的详情：完整消息、作者/时间、变更文件清单与增删统计、补丁。path 只看单个文件；stat_only=true 只看统计。补丁超预算会裁剪，可按 path 精读。只读；.mdgo 与敏感文件不返回。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "rev": { "type": "string", "description": "提交 SHA / 分支 / 标签（必填）" },
                        "path": { "type": "string", "description": "只看该路径的补丁（知识库内相对路径）" },
                        "stat_only": { "type": "boolean", "description": "只输出文件清单与增删统计，默认 false" }
                    },
                    "required": ["rev"]
                }),
            ))
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let rev = arg_str(&args, "rev")
            .ok_or_else(|| ToolError::InvalidArgs("rev 不能为空（提交 SHA/分支/标签）".into()))?;
        let q = ShowQuery {
            rev,
            path: arg_str(&args, "path"),
            stat_only: arg_bool(&args, "stat_only"),
            strict: true,
        };
        let cfg = self.cfg.clone();
        let service = GitReadService::new();
        let detail = run_git_task(ctx, move |flag| service.show(&cfg.dir_path, &q, &flag)).await?;
        Ok(Value::String(wrap_suspicious(&render::render_show(
            &detail,
        ))))
    }
}

// ─────────────────────────── git_diff ───────────────────────────

/// git_diff：工作区/暂存区/两个提交之间的差异。
pub struct GitDiffTool {
    cfg: KbSearchConfig,
}

impl GitDiffTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GitDiffTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            spec_with_timeout(ToolSpec::new(
                "git_diff",
                "查看当前知识库的 Git 差异。默认：工作区 vs 暂存区；staged=true：已暂存 vs HEAD；from/to：两个提交之间；只给 from：工作区 vs from。path 限定文件/目录，stat_only=true 只看统计，context 控制上下文行数。只读；.mdgo 与敏感文件不返回。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "staged": { "type": "boolean", "description": "查看已暂存（--cached）差异，默认 false" },
                        "from": { "type": "string", "description": "起始引用（与 to 组成范围；单独给 from 表示工作区 vs from）" },
                        "to": { "type": "string", "description": "结束引用（必须与 from 同时给）" },
                        "path": { "type": "string", "description": "只看知识库内某个相对路径" },
                        "stat_only": { "type": "boolean", "description": "只输出文件级增删统计，默认 false" },
                        "context": { "type": "integer", "minimum": 0, "maximum": 10, "description": "补丁上下文行数，默认 3" },
                        "ignore_whitespace": { "type": "boolean", "description": "忽略空白差异，默认 false" }
                    },
                    "required": []
                }),
            ))
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let q = DiffQuery {
            staged: arg_bool(&args, "staged"),
            from: arg_str(&args, "from"),
            to: arg_str(&args, "to"),
            path: arg_str(&args, "path"),
            stat_only: arg_bool(&args, "stat_only"),
            context: arg_u32(&args, "context", 3).min(10) as u8,
            ignore_whitespace: arg_bool(&args, "ignore_whitespace"),
            strict: true,
        };
        let cfg = self.cfg.clone();
        let service = GitReadService::new();
        let diff = run_git_task(ctx, move |flag| service.diff(&cfg.dir_path, &q, &flag)).await?;
        Ok(Value::String(wrap_suspicious(&render::render_diff(&diff))))
    }
}

// ─────────────────────────── git_refs ───────────────────────────

/// git_refs：本地/远程分支、标签、当前分支/上游。
pub struct GitRefsTool {
    cfg: KbSearchConfig,
}

impl GitRefsTool {
    pub fn new(cfg: KbSearchConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl Tool for GitRefsTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: std::sync::OnceLock<ToolSpec> = std::sync::OnceLock::new();
        SPEC.get_or_init(|| {
            spec_with_timeout(ToolSpec::new(
                "git_refs",
                "列出当前知识库 Git 仓库的本地分支、远程分支与标签，含当前分支、上游、领先/落后。kind 可只看 branches/tags/remotes；max 控制每组条数。只读。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["all", "branches", "tags", "remotes"],
                            "description": "默认 all"
                        },
                        "max": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 100,
                            "description": "每组最多展示条数，默认 50"
                        }
                    },
                    "required": []
                }),
            ))
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolRunContext<'_>) -> Result<Value, ToolError> {
        let kind = match arg_str(&args, "kind").as_deref() {
            Some("branches") => RefKind::Branches,
            Some("tags") => RefKind::Tags,
            Some("remotes") => RefKind::Remotes,
            _ => RefKind::All,
        };
        let q = RefsQuery {
            kind,
            max: arg_u32(&args, "max", 50).clamp(1, 100) as usize,
        };
        let cfg = self.cfg.clone();
        let service = GitReadService::new();
        let refs = run_git_task(ctx, move |flag| service.refs(&cfg.dir_path, &q, &flag)).await?;
        Ok(Value::String(wrap_suspicious(&render::render_refs(&refs))))
    }
}

/// 注册全部 Git 只读工具（供 `build_loop_tool_registry` 一行接入）。
pub fn register_git_read_tools(
    registry: &mut crate::core::r#loop::HashMapToolRegistry,
    cfg: KbSearchConfig,
) {
    registry.register(Arc::new(GitStatusTool::new(cfg.clone())));
    registry.register(Arc::new(GitLogTool::new(cfg.clone())));
    registry.register(Arc::new(GitShowTool::new(cfg.clone())));
    registry.register(Arc::new(GitDiffTool::new(cfg.clone())));
    registry.register(Arc::new(GitRefsTool::new(cfg)));
}
