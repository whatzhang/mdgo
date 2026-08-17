//! 非文件知识资产根模块（Knowledge Asset Layer）。
//!
//! 当前实现：`bookmark`（mdgo 第一个非文件 Knowledge Asset）。
//! 未来扩展：media / git / web 等并列于此（触发式，非计划式）。
//!
//! 定位原则：Bookmark 是 Knowledge Asset，不是管理对象；
//! Agent 只负责理解和查询（search/get），导入与管理由 UI 完成。

pub mod bookmark;
