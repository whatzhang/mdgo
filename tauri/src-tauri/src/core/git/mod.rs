//! Git 读能力核心模块（单一事实源）。
//!
//! 分层：
//! - [`runner`]：子进程执行（env、超时、kill、取消、输出上限）；
//! - [`scope`]：仓库根/知识库前缀、路径与引用校验；
//! - [`parse`]：porcelain v2 / log -z / name-status / numstat / refs 纯解析；
//! - [`filter`]：Agent 严格模式（`.mdgo`、敏感文件、生成物）；
//! - [`render`]：模型可读文本与预算提示；
//! - [`service`]：status/log/show/diff/refs 门面。
//!
//! 依赖约束：本模块不依赖 `core::agent`、Tauri 或 LLM；commands 与 Agent tools
//! 都只依赖这里的公开接口。

pub mod error;
pub mod filter;
pub mod limits;
pub mod model;
pub mod parse;
pub mod render;
pub mod runner;
pub mod scope;
pub mod service;

pub use error::GitError;
pub use limits::GitLimits;
pub use runner::CancelFlag;
pub use service::{
    DiffQuery, GitReadService, LogQuery, RefKind, RefsQuery, ShowQuery, StatusQuery,
};

#[cfg(test)]
mod integration_tests;
