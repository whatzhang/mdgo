//! 检索管线模块（SRP 拆分：查询理解 / 融合 / 精排）。
//!
//! 分层设计（SOLID）：
//! - [`query_plan`]：查询理解（QueryPlan + 规则路由）→ 单一职责：把字符串查询结构化为可执行计划
//! - [`rrf`]：多路召回融合（RRF）→ 开闭原则：支持任意多路输入，不感知具体检索器
//! - [`rerank`]：精排（Cross-Encoder）→ 依赖倒置：对外暴露 [`Reranker`] trait，本地实现可替换

pub mod query_plan;
pub mod rerank;
pub mod rrf;
