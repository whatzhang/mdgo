//! AI 总结面板命令层：把前端采集的各分类文本（行为 / 知识库）逐类交给 LLM 推理，
//! 产出「总结 + 建议」，供 dashboard 手风琴面板展示。
//!
//! SOLID 设计：
//! - 单一职责：本模块只做「分类文本 → LLM 总结」，数据采集在 `ai-summay.js`（前端）；
//! - 依赖倒置：复用 `AppState::llm_client_for_role`（按角色路由轻量摘要模型）+ 重试链，
//!   不重复实现退避/协议适配；
//! - 开闭原则：新增分类只需前端新增 entry，命令签名不变。
//!
//! 并发：各分类 LLM 调用并行执行（`futures::future::join_all` + 信号量限流，
//! 默认同时最多 `MAX_CONCURRENT` 个请求，兼顾速度与本地模型负载）。

use std::sync::Arc;

use futures::future::join_all;
use tauri::{AppHandle, Manager, State};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::AppState;

/// 最大并发 LLM 调用数。行为 11 + 知识库 3 = 14 个分类，全并发一次发出，
/// 总耗时 ≈ 最慢单分类耗时（消除批数等待）。若本地小模型并发 OOM/排队，
/// 可调低该值（如 4/8）以串行化。
const MAX_CONCURRENT: usize = 14;

/// 一个待分析分类（前端采集的原始文本 + 该分类的分析指令）。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SummarySection {
    /// 分类键（如 operation / deleted / bookmarks / kb_skill ...）
    pub key: String,
    /// 分类展示名（如「操作记录」「Skill 使用」）
    pub label: String,
    /// 该分类的分析指令（system prompt，指示模型如何总结/给建议）
    #[serde(default)]
    pub instruction: String,
    /// 采集到的原始文本数据
    pub text: String,
}

/// 单个分类的 LLM 总结结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SummarySectionResult {
    pub key: String,
    pub label: String,
    /// 模型输出；失败时为降级文案（命令不整体失败）
    pub result: String,
    /// 是否成功调用模型（false = 降级/未配置模型）
    pub ok: bool,
}

/// 对一组分类文本并发调用 LLM 总结。
///
/// - 未配置模型：快速返回降级结果（`ok=false`，不阻塞面板）；
/// - 并发执行（信号量限流 `MAX_CONCURRENT`），单类失败不影响其他分类；
/// - 单次调用有软超时（由 retry 链 + 取消令牌控制），保证后台任务可终止；
/// - 结果按输入顺序返回（join_all 保序）。
#[tauri::command]
pub async fn kb_ai_summary(
    app: AppHandle,
    state: State<'_, AppState>,
    sections: Vec<SummarySection>,
) -> Result<Vec<SummarySectionResult>, String> {
    log::info!(
        "[ai_summary] 收到 {} 个分类: {:?}",
        sections.len(),
        sections.iter().map(|s| s.key.as_str()).collect::<Vec<_>>()
    );
    let cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();
    if cfg.endpoint.trim().is_empty() || cfg.model.trim().is_empty() {
        log::warn!("[ai_summary] LLM 未配置（endpoint/model 为空），返回降级结果");
        return Ok(sections
            .into_iter()
            .map(|s| SummarySectionResult {
                key: s.key,
                label: s.label,
                result: "未配置 LLM 模型，请在设置中配置后重试。".into(),
                ok: false,
            })
            .collect());
    }

    // 摘要角色路由：优先 summary_model（轻量），未配置回退主模型
    let client = state
        .llm_client_for_role(&cfg, crate::ModelRole::Summary)
        .await
        .map_err(|e| format!("初始化 LLM 客户端失败: {}", e))?;

    // 按需加载基础分析指令（prompt/summay_analysis.md，读取逻辑与 skill 一致：
    // 运行时资源目录优先，源码 resources/prompt 回退；加载失败回退内置常量）
    let base_instruction = load_summary_instruction(&app);

    // 并发执行：信号量限流，避免同时打爆本地模型
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let futures: Vec<_> = sections
        .into_iter()
        .map(|s| {
            let client = client.clone();
            let semaphore = semaphore.clone();
            let base_instruction = base_instruction.clone();
            async move {
                let _permit = semaphore.acquire().await;
                let cancel = CancellationToken::new();
                let instruction = if s.instruction.trim().is_empty() {
                    base_instruction
                } else {
                    // 分类级指令优先，但以基础指令为前缀（基础指令约束通用分析原则与输出格式）
                    format!("{}\n\n{}", base_instruction, s.instruction)
                };
                let user_text = if s.text.trim().is_empty() {
                    "（该分类暂无数据）".to_string()
                } else {
                    s.text.clone()
                };
                log::info!(
                    "[ai_summary] 分类 {} 开始总结（输入 {} 字符）",
                    s.key,
                    user_text.chars().count()
                );
                // 软超时：单类最长 90s（含重试），超时降级该分类
                let timeout = tokio::time::Duration::from_secs(90);
                let outcome = tokio::time::timeout(timeout, async {
                    client
                        .complete_text(&instruction, &user_text, Some(1536), Some(0.3), cancel)
                        .await
                })
                .await;

                let (result, ok) = match outcome {
                    Ok(Ok(text)) => {
                        let t = text.trim().to_string();
                        if t.is_empty() {
                            ("（模型返回空内容）".to_string(), false)
                        } else {
                            (t, true)
                        }
                    }
                    Ok(Err(e)) => (format!("分析失败: {}", e), false),
                    Err(_) => ("分析超时（90s），已跳过该分类。".to_string(), false),
                };
                SummarySectionResult {
                    key: s.key,
                    label: s.label,
                    result,
                    ok,
                }
            }
        })
        .collect();

    let results = join_all(futures).await;
    Ok(results)
}

/// 分析指令文件名（位于打包资源 `prompt/` 目录；源码期回退 `resources/prompt/`）。
const SUMMARY_INSTRUCTION_FILE: &str = "prompt/summay_analysis.md";

/// 按需加载基础分析指令（读取逻辑与 skill 一致）：
/// 1. 运行时资源目录优先（打包后 `resource_dir()/prompt/summay_analysis.md`）；
/// 2. 未打包环境回退源码 `resources/prompt/summay_analysis.md`；
/// 3. 均不存在/读取失败 → 回退内置常量（不阻塞功能）。
fn load_summary_instruction(app: &AppHandle) -> String {
    // 1) 打包资源目录
    if let Ok(dir) = app.path().resource_dir() {
        let p = dir.join(SUMMARY_INSTRUCTION_FILE);
        if let Ok(content) = std::fs::read_to_string(&p) {
            let t = content.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    // 2) 源码 resources/prompt 兜底（开发期）
    let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join(SUMMARY_INSTRUCTION_FILE);
    if let Ok(content) = std::fs::read_to_string(&src) {
        let t = content.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    log::warn!("[ai_summary] 未找到 {}, 使用内置默认指令", SUMMARY_INSTRUCTION_FILE);
    DEFAULT_SUMMARY_INSTRUCTION.to_string()
}

/// 内置兜底指令（外部 prompt 文件缺失时使用）：行为/知识库数据通用总结框架。
const DEFAULT_SUMMARY_INSTRUCTION: &str = concat!(
    "你是一个知识库用户行为与知识资产分析助手。下面提供的是 mdgo 知识库采集到的统计数据文本。\n",
    "请基于这些数据输出简洁、结构化、有洞察的总结与建议，要求：\n",
    "1. 用 Markdown 输出，先给 2-4 条核心结论（每条一句话），再给 1-3 条可执行建议；\n",
    "2. 只基于给定数据推理，禁止臆造数据、文件内容或系统架构；\n",
    "3. 数字保留原值，不四舍五入编造；\n",
    "4. 结论要具体（点名文件/目录/指标），避免空泛套话；\n",
    "5. 全文中文，不用 emoji 开头，不输出「以下是总结」等引导语。"
);
