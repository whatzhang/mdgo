//! Git 读预算（字符/条数/超时）。
//!
//! 与 Session 的 24k 截断线对齐：工具侧补丁预算 18k，避免被二次硬切。

/// status 默认展示条目数。
pub const STATUS_MAX_ENTRIES: usize = 300;
/// 历史默认/最大条数。
pub const LOG_DEFAULT_LIMIT: u32 = 20;
pub const LOG_MAX_LIMIT: u32 = 100;
/// 提交详情文件清单上限。
pub const SHOW_MAX_FILES: usize = 100;
/// 提交正文上限。
pub const META_BODY_MAX_CHARS: usize = 4_000;
/// 补丁/差异输出字符上限（低于 Session 的 24k）。
pub const PATCH_MAX_CHARS: usize = 18_000;
/// refs 每组展示上限。
pub const REFS_MAX_PER_GROUP: usize = 50;
/// 单个文件内容读取上限（旧版本文件）。
pub const BLOB_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// 可注入的读预算（工具/命令各自取默认）。
#[derive(Debug, Clone, Copy)]
pub struct GitLimits {
    pub log_max_limit: u32,
    pub show_max_files: usize,
    pub meta_body_max_chars: usize,
    pub patch_max_chars: usize,
}

impl Default for GitLimits {
    fn default() -> Self {
        Self {
            log_max_limit: LOG_MAX_LIMIT,
            show_max_files: SHOW_MAX_FILES,
            meta_body_max_chars: META_BODY_MAX_CHARS,
            patch_max_chars: PATCH_MAX_CHARS,
        }
    }
}
