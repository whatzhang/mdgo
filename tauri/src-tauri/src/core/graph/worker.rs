//! 后台 Graph AI Worker（Phase 3 完整形态：构建后异步智能抽取）。
//!
//! 职责：轮询各知识库目录的 `graph_ai_queue`，对高重要度文档执行
//! 1) Level 1 规则抽取（免费，外部链接 host → 实体）——兜底 build_file 增量漏抽；
//! 2) Level 3 LLM 关系抽取（已配置 LLM 时；未配置自动降级）——候选状态机输出。
//!
//! 节流（成本控制 PRD §75-76）：
//! - 每轮每目录批量 ≤ [`BATCH_LIMIT`] 条；
//! - 其中 LLM 抽取每目录每轮 ≤ [`LLM_PER_DIR_CYCLE`] 条；
//! - 有界重试（[`MAX_ATTEMPTS`]），LLM 不可用不阻塞队列（规则抽取照常完成）。
//!
//! 可观测性：成功/失败计数写入 `graph_metrics`（worker_processed / worker_failed），
//! 经 `graph_metrics` 命令随 GraphMetrics 返回（前端可展示）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tauri::{AppHandle, Manager};

use super::ai::{GraphAiService, GraphLlm, NullGraphLlm};
use super::extractor::EntityExtractor;
use super::merger::EntityMerger;
use super::GraphEngine;
use crate::services::llm::LLMClient;
use crate::{AppState, ModelRole};

/// 每轮每目录最多处理的队列项数
const BATCH_LIMIT: u32 = 3;
/// 每轮每目录 LLM 抽取条数上限（规则抽取不受限）
const LLM_PER_DIR_CYCLE: usize = 2;
/// 单条队列项最大重试次数（超过 → failed）
const MAX_ATTEMPTS: u32 = 3;
/// 轮询周期
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// 启动后首轮延迟（等应用初始化与首次构建完成）
const STARTUP_DELAY: Duration = Duration::from_secs(20);

/// LLM 适配器（services LLMClient → core GraphLlm）。
/// 原 commands/graph.rs 私有实现集中于此，命令层与 worker 共用。
pub(crate) struct ServicesGraphLlm {
    client: LLMClient,
}

#[async_trait]
impl GraphLlm for ServicesGraphLlm {
    async fn json(&self, system: &str, user: &str) -> Option<serde_json::Value> {
        self.client
            .complete_json(system, user, tokio_util::sync::CancellationToken::new())
            .await
    }
    async fn text(&self, system: &str, user: &str, max_tokens: u32) -> Option<String> {
        self.client
            .complete_text(system, user, max_tokens, tokio_util::sync::CancellationToken::new())
            .await
    }
}

/// 获取 LLM 适配器（未配置/失败 → NullGraphLlm 降级；命令层与 worker 共用）。
pub async fn build_graph_llm(app: &AppHandle) -> Box<dyn GraphLlm> {
    let state = app.state::<AppState>();
    let cfg = match state.llm_config.read() {
        Ok(c) => c.clone(),
        Err(_) => return Box::new(NullGraphLlm),
    };
    if cfg.endpoint.trim().is_empty() || cfg.model.trim().is_empty() {
        return Box::new(NullGraphLlm);
    }
    match state.llm_client_for_role(&cfg, ModelRole::Summary).await {
        Ok(client) => Box::new(ServicesGraphLlm { client }),
        Err(e) => {
            log::warn!("[graph] LLM 适配器构建失败，AI 操作降级: {}", e);
            Box::new(NullGraphLlm)
        }
    }
}

/// 是否已配置 LLM（worker 用它决定是否执行 LLM 抽取；避免为 Null 适配器空跑）。
pub fn graph_llm_configured(app: &AppHandle) -> bool {
    match app.state::<AppState>().llm_config.read() {
        Ok(c) => !c.endpoint.trim().is_empty() && !c.model.trim().is_empty(),
        Err(_) => false,
    }
}

/// 启动后台 AI worker（随 app 生命周期运行；由 lib.rs setup 调用一次）。
pub fn spawn_ai_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(e) = run_once(&app).await {
                log::warn!("[graph-worker] 本轮处理失败: {}", e);
            }
        }
    });
}

/// 单轮处理：遍历活跃目录，每目录处理一批队列项。
async fn run_once(app: &AppHandle) -> Result<(), String> {
    let engine = app.state::<AppState>().graph_engine.clone();
    let dirs = engine.active_dirs();
    if dirs.is_empty() {
        return Ok(());
    }
    // 空闲守卫：无待处理/处理中项时直接返回（避免每 30s 空转构建 LLM 客户端）
    let has_work = dirs.iter().any(|d| {
        engine
            .queue_stats(d)
            .map(|(pending, processing, _, _)| pending + processing > 0)
            .unwrap_or(false)
    });
    if !has_work {
        return Ok(());
    }
    let llm_ready = graph_llm_configured(app);
    // LLM 适配器每轮构建一次（配置可动态变化）；未配置 → None，仅规则抽取
    let llm = if llm_ready {
        Some(build_graph_llm(app).await)
    } else {
        None
    };
    let mut processed_total = 0usize;
    for dir in &dirs {
        match process_dir(&engine, dir, llm.as_deref(), llm_ready).await {
            Ok(n) => processed_total += n,
            Err(e) => log::warn!("[graph-worker] {} 处理异常: {}", dir, e),
        }
    }
    // D5：每轮摘要日志（成功路径可观测）
    log::info!(
        "[graph-worker] 本轮完成: active_dirs={} processed={} llm_ready={}",
        dirs.len(),
        processed_total,
        llm_ready
    );
    Ok(())
}

/// 处理单目录：取一批 → 规则抽取（全部）+ LLM 抽取（前 N 条）→ 逐条收尾。
/// 返回本批处理的项数（供轮次摘要日志）。
async fn process_dir(
    engine: &Arc<GraphEngine>,
    dir_path: &str,
    llm: Option<&dyn GraphLlm>,
    llm_ready: bool,
) -> Result<usize, String> {
    let batch = engine.next_ai_batch(dir_path, BATCH_LIMIT)?;
    if batch.is_empty() {
        return Ok(0);
    }
    let service = GraphAiService::new(Arc::clone(engine));
    for (idx, item) in batch.iter().enumerate() {
        // 文件已不存在（删除/移动）：标记完成并跳过（避免僵尸重试）
        let abs = std::path::Path::new(dir_path).join(&item.rel_path);
        let content = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => {
                log::info!("[graph-worker] 文件不存在，跳过: {}", item.rel_path);
                engine.finish_ai_item(dir_path, item.id, true, MAX_ATTEMPTS)?;
                continue;
            }
        };
        // 1) Level 1 规则抽取（同步；best-effort，失败仅告警）
        {
            let store = engine.store(dir_path)?;
            let guard = store.lock().unwrap_or_else(|e| e.into_inner());
            let extractor = EntityExtractor::new(
                &guard,
                None::<&dyn super::extractor::EntityLlmExtractor>,
            );
            let mut merger = EntityMerger::new(&guard);
            let source_id = super::storage::node_id_for(super::model::NodeType::Doc, &item.rel_path);
            for (name, aliases) in extractor.rule_candidates(&item.rel_path, &content) {
                if let Err(e) = merger.upsert_entity(&name, &aliases, Some(&source_id)) {
                    log::warn!("[graph-worker] 规则实体落库失败: {}", e);
                }
            }
        }
        // 2) Level 3 LLM 抽取（节流：每目录每轮 ≤ LLM_PER_DIR_CYCLE 条）
        let mut ok = true;
        if llm_ready && idx < LLM_PER_DIR_CYCLE {
            if let Some(llm) = llm {
                if let Err(e) = service
                    .extract_relations(dir_path, &item.rel_path, &content, llm)
                    .await
                {
                    log::warn!("[graph-worker] LLM 抽取失败（将重试）: {}", e);
                    ok = false;
                }
            }
        }
        // 3) 收尾（ok=false → 重试计数，超限 failed）
        engine.finish_ai_item(dir_path, item.id, ok, MAX_ATTEMPTS)?;
    }
    Ok(batch.len())
}
