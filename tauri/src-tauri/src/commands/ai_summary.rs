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

/// 实际使用的并发上限：允许用 `MDGO_SUMMARY_CONCURRENCY`（1..=32）覆盖。
///
/// 为什么要可配：本机 llama.cpp / Ollama 常以单槽（`--parallel 1`）运行，此时
/// 提高并发只会排队等待；把该值调到 2~4 反而更快出首段结果。远端 API 可保持默认。
fn max_concurrent() -> usize {
    std::env::var("MDGO_SUMMARY_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| (1..=32).contains(n))
        .unwrap_or(MAX_CONCURRENT)
}

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

    // 并发执行：信号量限流；并发数可用 MDGO_SUMMARY_CONCURRENCY 覆盖（本地单槽模型建议 2~4）
    let concurrency = max_concurrent();
    log::info!(
        "[ai_summary] 开始总结 {} 个分类，并发上限 {}",
        sections.len(),
        concurrency
    );
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let futures: Vec<_> = sections
        .into_iter()
        .map(|s| {
            let client = client.clone();
            let semaphore = semaphore.clone();
            let base_instruction = base_instruction.clone();
            async move {
                // 空数据分类直接跳过模型：旧实现仍会发一次「（该分类暂无数据）」，
                // 本地单槽模型下每次都是数秒级浪费。
                if s.text.trim().is_empty() {
                    log::info!("[ai_summary] 分类 {} 无数据，跳过模型调用", s.key);
                    return SummarySectionResult {
                        key: s.key,
                        label: s.label,
                        result: "（该分类暂无数据，已跳过模型调用）".to_string(),
                        ok: true,
                    };
                }
                let started = std::time::Instant::now();
                let _permit = semaphore.acquire().await;
                let cancel = CancellationToken::new();
                let instruction = if s.instruction.trim().is_empty() {
                    base_instruction
                } else {
                    // 分类级指令优先，但以基础指令为前缀（基础指令约束通用分析原则与输出格式）
                    format!("{}\n\n{}", base_instruction, s.instruction)
                };
                let user_text = s.text.clone();
                log::info!(
                    "[ai_summary] 分类 {} 开始总结（输入 {} 字符）",
                    s.key,
                    user_text.chars().count()
                );
                // 软超时：单类最长 90s（含重试），超时降级该分类
                let timeout = tokio::time::Duration::from_secs(90);
                let outcome = tokio::time::timeout(timeout, async {
                    client
                        .complete_text(&instruction, &user_text, Some(1024), Some(0.3), cancel)
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
                log::info!(
                    "[ai_summary] 分类 {} 完成（{} ms，ok={}，输出 {} 字符）",
                    s.key,
                    started.elapsed().as_millis(),
                    ok,
                    result.chars().count()
                );
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
    // 1) 打包资源目录（Tauri 保留 resources/ 前缀，两种布局都探测）；
    //    开发期优先源码目录，避免用到构建时拷贝的旧副本。
    let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join(SUMMARY_INSTRUCTION_FILE);
    if cfg!(debug_assertions) {
        if let Ok(content) = std::fs::read_to_string(&src) {
            let t = content.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    if let Ok(dir) = app.path().resource_dir() {
        for candidate in [
            dir.join(SUMMARY_INSTRUCTION_FILE),
            dir.join("resources").join(SUMMARY_INSTRUCTION_FILE),
        ] {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                let t = content.trim();
                if !t.is_empty() {
                    return t.to_string();
                }
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
    "1. 用 Markdown 输出，先给 2-4 条核心结论（每条一句话），再给 1-3 条可执行建议；正文尽量控制在 300 字以内（Mermaid 代码块不计）；\n",
    "2. 只基于给定数据推理，禁止臆造数据、文件内容或系统架构；\n",
    "3. 数字保留原值，不四舍五入编造；\n",
    "4. 结论要具体（点名文件/目录/指标），避免空泛套话；\n",
    "5. 全文中文，不用 emoji 开头，不输出「以下是总结」等引导语；\n",
    "6. **图表要求（必须遵守）**：当给定数据包含可比较的分类占比、趋势或排名时，必须在结论后插入至少一个 Mermaid 图表\n",
    "   用 ```mermaid 代码块包裹，可选用 `pie`（占比）、`xychart-beta`（柱状/折线趋势）或 `flowchart`（结构/流程）；\n",
    "   图中标题、标签、图例一律用中文，数值必须来自给定数据，禁止编造数据点或凑整；某个数据集为空时不要画该图；\n",
    "7. 每个图表后面紧跟一句「读图结论」（不超过 30 字），说明这张图说明了什么；\n",
    "8. 图表只做辅助，不要把全部数据都做成图（单段最多 2 张）。"
);

// ───────────────────────── 结果持久化（知识库级 .mdgo/mdgo.db） ─────────────────────────
//
// 与上面的「生成」职责分离：本组命令只做读写，不触发任何模型调用。
// 前端语义：进入页面 → `ai_summary_load`；「重新分析」成功 → `ai_summary_save`。

/// 读取某知识库上一次的分析结果（不存在返回 `null`）。
#[tauri::command]
pub async fn ai_summary_load(
    state: State<'_, AppState>,
    dir_path: String,
    slot: Option<String>,
) -> Result<Option<crate::services::ai_summary::AiSummaryRecord>, String> {
    let store = state.get_ai_summary_store(&dir_path)?;
    let slot = slot.unwrap_or_else(|| crate::services::ai_summary::SLOT_DOC_ANALYSIS.to_string());
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.load(&slot))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 覆盖写入分析结果（原子 upsert；校验失败时旧记录保持不变）。
#[tauri::command]
pub async fn ai_summary_save(
    state: State<'_, AppState>,
    dir_path: String,
    req: crate::services::ai_summary::AiSummarySaveRequest,
) -> Result<crate::services::ai_summary::AiSummaryRecord, String> {
    let store = state.get_ai_summary_store(&dir_path)?;
    let slot = req
        .slot
        .clone()
        .unwrap_or_else(|| crate::services::ai_summary::SLOT_DOC_ANALYSIS.to_string());
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.save(&req, &slot))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

/// 清除某知识库的分析结果（当前 UI 未暴露，供后续"清空分析"入口使用）。
#[tauri::command]
pub async fn ai_summary_clear(
    state: State<'_, AppState>,
    dir_path: String,
    slot: Option<String>,
) -> Result<bool, String> {
    let store = state.get_ai_summary_store(&dir_path)?;
    let slot = slot.unwrap_or_else(|| crate::services::ai_summary::SLOT_DOC_ANALYSIS.to_string());
    tokio::task::spawn_blocking(move || {
        crate::core::db::with_busy_retry(3, || store.clear(&slot))
    })
    .await
    .map_err(|e| format!("任务执行失败: {}", e))?
}

// ───────────────────────── 结论卡（一次轻量 LLM 汇总） ─────────────────────────

/// 一张结论卡
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConclusionCard {
    /// 核心结论 / 风险提示 / 行动建议
    pub tag: String,
    pub text: String,
}

/// 结论卡的输入：上面 14 段分析结果（只要 label + result，不需要原始采集文本）
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ConclusionInput {
    pub label: String,
    #[serde(default)]
    pub result: String,
}

/// 三张卡的固定标签与顺序
const CONCLUSION_TAGS: [&str; 3] = ["核心结论", "风险提示", "行动建议"];

const CONCLUSION_INSTRUCTION: &str = concat!(
    "你是知识库使用行为分析助手。下面给出上一轮已经完成的若干段分析结果（每段含标题与正文）。\n",
    "请把它们压缩成 3 张结论卡，严格按下面格式输出（共 3 行，不要多余内容，不要 markdown 标记）：\n",
    "核心结论：<一句话，不超过 46 字，必须是具体事实，带关键数字>\n",
    "风险提示：<一句话，不超过 46 字，指出最值得注意的问题；没有问题就写「暂无明显风险」>\n",
    "行动建议：<一句话，不超过 46 字，可执行、指向具体文件或目录>\n",
    "要求：只使用给定内容，禁止引入新数据；不要复述原文；不要用「建议用户」这类空话。"
);

/// 从 14 段分析结果提炼 3 张结论卡（单独一次 LLM 调用，失败时规则兜底）。
#[tauri::command]
pub async fn ai_summary_conclusions(
    app: AppHandle,
    state: State<'_, AppState>,
    items: Vec<ConclusionInput>,
) -> Result<Vec<ConclusionCard>, String> {
    if items.is_empty() {
        return Ok(rule_based_conclusions(&items));
    }

    // 输入压缩：每段最多取 600 字，避免把 14 段全文塞进上下文
    let mut user_text = String::new();
    for it in items.iter().filter(|it| !it.result.trim().is_empty()) {
        let body: String = it.result.trim().chars().take(600).collect();
        user_text.push_str(&format!("## {}\n{}\n\n", it.label, body));
    }
    if user_text.trim().is_empty() {
        return Ok(rule_based_conclusions(&items));
    }

    let cfg = state.llm_config.read().unwrap_or_else(|e| e.into_inner()).clone();
    if cfg.endpoint.trim().is_empty() || cfg.model.trim().is_empty() {
        log::warn!("[ai_summary] 结论卡：LLM 未配置，使用规则兜底");
        return Ok(rule_based_conclusions(&items));
    }

    let client = match state
        .llm_client_for_role(&cfg, crate::ModelRole::Summary)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[ai_summary] 结论卡：客户端不可用（{}），使用规则兜底", e);
            return Ok(rule_based_conclusions(&items));
        }
    };
    let _ = app; // 预留：后续可把模型名写回元信息

    let cancel = CancellationToken::new();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(60),
        client.complete_text(CONCLUSION_INSTRUCTION, &user_text, Some(320), Some(0.2), cancel),
    )
    .await;

    let text = match outcome {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            log::warn!("[ai_summary] 结论卡生成失败（{}），使用规则兜底", e);
            return Ok(rule_based_conclusions(&items));
        }
        Err(_) => {
            log::warn!("[ai_summary] 结论卡生成超时，使用规则兜底");
            return Ok(rule_based_conclusions(&items));
        }
    };

    let cards = parse_conclusion_text(&text);
    if cards.len() == 3 {
        Ok(cards)
    } else {
        log::warn!("[ai_summary] 结论卡输出格式不符，使用规则兜底：{:?}", text);
        Ok(rule_based_conclusions(&items))
    }
}

/// 解析「标签：内容」三行；容忍中英文冒号与少量前缀噪声
fn parse_conclusion_text(text: &str) -> Vec<ConclusionCard> {
    let mut out: Vec<ConclusionCard> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches(['-', '*', ' ', '#', '1', '2', '3', '.', '、']);
        if line.is_empty() {
            continue;
        }
        let Some((head, body)) = line
            .split_once('：')
            .or_else(|| line.split_once(':'))
        else {
            continue;
        };
        let tag = CONCLUSION_TAGS
            .iter()
            .find(|t| head.contains(**t))
            .copied();
        let Some(tag) = tag else { continue };
        let body = body.trim().trim_matches(['`', '"', '「', '」', '。']);
        if body.is_empty() {
            continue;
        }
        if out.iter().any(|c| c.tag == tag) {
            continue;
        }
        let text: String = body.chars().take(80).collect();
        out.push(ConclusionCard {
            tag: tag.to_string(),
            text,
        });
    }
    // 保持固定顺序
    out.sort_by_key(|c| {
        CONCLUSION_TAGS
            .iter()
            .position(|t| *t == c.tag)
            .unwrap_or(usize::MAX)
    });
    out
}

/// 规则兜底：从最相关段落的首句提取，保证 UI 永远有 3 张卡
fn rule_based_conclusions(items: &[ConclusionInput]) -> Vec<ConclusionCard> {
    fn first_sentence(result: &str) -> Option<String> {
        let line = result
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("```") && !l.starts_with('#'))?;
        let cleaned = line.trim_start_matches(&['-', '*', ' ', '>'][..]).trim();
        let sentence: String = cleaned.chars().take(60).collect();
        if sentence.is_empty() {
            None
        } else {
            Some(sentence)
        }
    }

    let pick = |labels: &[&str]| -> Option<String> {
        for want in labels {
            if let Some(it) = items.iter().find(|i| i.label.contains(want)) {
                if let Some(s) = first_sentence(&it.result) {
                    return Some(s);
                }
            }
        }
        None
    };

    let failed: Vec<&str> = items
        .iter()
        .filter(|i| i.result.trim().is_empty())
        .map(|i| i.label.as_str())
        .collect();

    vec![
        ConclusionCard {
            tag: "核心结论".into(),
            text: pick(&["操作记录", "文件与磁盘", "内容分布"])
                .unwrap_or_else(|| "本次分析暂无可用结论".into()),
        },
        ConclusionCard {
            tag: "风险提示".into(),
            text: if failed.is_empty() {
                pick(&["删除记录", "RAG 与生态", "Git"]).unwrap_or_else(|| "暂无明显风险".into())
            } else {
                format!("{} 段分析未生成结果，请重试", failed.len())
            },
        },
        ConclusionCard {
            tag: "行动建议".into(),
            text: pick(&["文件增改", "书签", "Skill 使用"])
                .unwrap_or_else(|| "先打开最近编辑的文件继续阅读".into()),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_tagged_lines() {
        let text = "核心结论：阅读集中在 AI 目录，Top5 占 68% 时长\n风险提示：52% 文件从未被打开\n行动建议：为 3 个冷目录补索引摘要";
        let cards = parse_conclusion_text(text);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].tag, "核心结论");
        assert!(cards[0].text.contains("68%"));
    }

    #[test]
    fn tolerates_prefixes_and_english_colon() {
        let text = "- 核心结论: 其一\n2. 风险提示：其二\n> 行动建议：其三";
        let cards = parse_conclusion_text(text);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[1].tag, "风险提示");
        assert_eq!(cards[2].text, "其三");
    }

    #[test]
    fn rule_fallback_always_returns_three_cards() {
        let items = vec![
            ConclusionInput { label: "操作记录".into(), result: "浏览高度集中，Top5 占 68%".into() },
            ConclusionInput { label: "删除记录".into(), result: "".into() },
        ];
        let cards = rule_based_conclusions(&items);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].tag, "核心结论");
        assert!(cards[0].text.contains("Top5"));
        assert!(cards[1].text.contains("未生成结果"));
    }
}