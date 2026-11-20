//! Git 提交信息生成（Skill 驱动的单轮执行）
//!
//! 设计要点：
//! - **Skill 是唯一事实源**：system prompt 取自系统技能 `git-commit-message` 的正文；
//!   技能缺失/被停用时回退到内置 Prompt（保证按钮永远可用）。
//! - **只读**：仅调用 `git_status` / `git_diff` / `git_parse_refs`，不提交、不写文件。
//! - **确定性上下文**：后端按预算裁剪暂存 diff（不依赖历史提交，符合产品要求）。
//! - **输出契约**：模型只返回 JSON `{subject, body, footer, notes}`，解析失败时回退首行，
//!   并按《阿里巴巴 Java 开发手册》的 header 规则做校验/修复。

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio_util::sync::CancellationToken;

use crate::core::skill::activation::ActivationSource;
use crate::core::skill::metrics::ExecInput;
use crate::AppState;

/// 技能 ID（与 SKILL.md frontmatter 一致）
const SKILL_ID: &str = "git-commit-message";
const SKILL_SCOPE: &str = "system";

/// 暂存 diff 送入模型的字符预算
const MAX_DIFF_CHARS: usize = 24_000;
/// system prompt（技能正文）字符预算，防御异常超长技能
const MAX_SYSTEM_PROMPT_CHARS: usize = 12_000;
/// 提交信息 header 硬上限（阿里巴巴规范：第一行 ≤70 字符）
const MAX_HEADER_CHARS: usize = 70;
/// body 字符预算
const MAX_BODY_CHARS: usize = 2_000;
/// 单次生成超时（本地模型首次冷启动可能 >30s，给足 120s）
const LLM_TIMEOUT_SECS: u64 = 120;
/// 单次生成的输出 token 预算（原 300 会把 JSON 截断，触发解析失败）
const MAX_OUTPUT_TOKENS: u32 = 800;

/// 阿里巴巴规范允许的 type（自动生成不包含 to/merge/sync/build/ci）
const ALLOWED_TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "chore", "revert",
];

/// 技能缺失/停用时的兜底 Prompt（与 SKILL.md 规则等价，保持精简）
const DEFAULT_SYSTEM_PROMPT: &str = r#"你是严格遵循《阿里巴巴 Java 开发手册》Git 提交规范的工程师。
根据给定的暂存区文件列表与 diff，生成一条提交信息。

只输出一个 JSON 对象，不要代码块、不要解释：
{"subject":"type(scope): 做了什么","body":"可选正文","footer":"可选","notes":"给用户看的提示，可为空"}

规范：
- 格式 `<type>(<scope>): <subject>`；header 整行 ≤70 字符，第二行必须为空，body 每行 ≤80 字符。
- type 只允许：feat, fix, docs, style, refactor, perf, test, chore, revert（全小写）；不使用 build/ci/to/merge/sync。
- scope 可选、小写；嵌套用 `/`，多模块用 `,` 不加空格；不确定就省略，禁止编造。
- subject 动宾、一般现在时、祈使句、不加句号；建议 ≤50 字符；禁止“更新代码”“修复问题”这类空泛描述。
- body 说明为什么改/影响面/注意事项，最多 5 行；footer 仅在输入明确给出 issue 或 BREAKING CHANGE 时填写。
- 语言默认中文；不要编造 issue/PR/作者/未出现的改动；diff 截断时只概括可见部分；不输出 emoji 与多方案。
"#;

/// 生成结果（前端直接用于填充提交消息框）
#[derive(Debug, Clone, Serialize)]
pub struct CommitSuggestion {
    /// 完整 header，如 `feat(git): 新增提交信息自动生成命令`
    pub subject: String,
    pub body: String,
    pub footer: String,
    /// 给用户看的提示（不写入提交信息）
    pub notes: String,
    /// subject + body + footer 已拼好的完整提交消息
    pub message: String,
    pub files: usize,
    pub additions: u64,
    pub deletions: u64,
    /// diff 是否被预算截断
    pub truncated: bool,
    /// 是否走了“首行兜底”（模型输出不是合法 JSON）
    pub fallback: bool,
    /// system prompt 来源：skill / builtin
    pub prompt_source: String,
    pub model: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Default, Deserialize)]
struct RawSuggestion {
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    footer: String,
    #[serde(default)]
    notes: String,
}

/// 根据暂存区改动生成提交信息（主模型，单轮）
#[tauri::command]
pub async fn git_suggest_commit_message(
    state: State<'_, AppState>,
    dir_path: String,
) -> Result<CommitSuggestion, String> {
    let started = Instant::now();
    let dir = dir_path.trim().to_string();
    if dir.is_empty() {
        return Err("NO_STAGED|仓库路径为空".into());
    }

    // ── 1. 暂存条目（复用已修正的 porcelain 语义）──
    let entries = crate::commands::git::git_status(dir.clone())?;
    let staged: Vec<_> = entries
        .into_iter()
        .filter(|e| e.x != " " && e.x != "?")
        .collect();
    if staged.is_empty() {
        return Err("NO_STAGED|没有暂存的更改，请先暂存文件".into());
    }
    let has_conflict = staged.iter().any(|e| {
        e.x == "U" || e.y == "U" || (e.x == "A" && e.y == "A") || (e.x == "D" && e.y == "D")
    });
    if has_conflict {
        return Err("HAS_CONFLICT|存在未解决的冲突，请先解决并暂存".into());
    }

    // ── 2. 文件统计（numstat，二进制用 - 表示）──
    let (numstat_text, _) = crate::core::agent::tools::git_diff(&dir, true, true).await?;
    let stats = parse_numstat(&numstat_text);
    let mut files: Vec<(String, String, Option<u64>, Option<u64>)> = Vec::new();
    let mut additions: u64 = 0;
    let mut deletions: u64 = 0;
    for e in &staged {
        let stat = stats.iter().find(|(p, _, _)| p == &e.path);
        let add = stat.and_then(|(_, a, _)| *a);
        let del = stat.and_then(|(_, _, d)| *d);
        additions += add.unwrap_or(0);
        deletions += del.unwrap_or(0);
        // 重命名显示为 `old -> new`，便于模型理解改动范围
        let display_path = match &e.from {
            Some(old) => format!("{} -> {}", old, e.path),
            None => e.path.clone(),
        };
        files.push((e.x.clone(), display_path, add, del));
    }

    // ── 3. 暂存 diff（按预算裁剪；跳过敏感/生成文件）──
    let (raw_patch, _) = crate::core::agent::tools::git_diff(&dir, true, false).await?;
    let patch = if raw_patch.trim().is_empty() || raw_patch.starts_with("工作区无差异") {
        String::new()
    } else {
        raw_patch
    };
    let (filtered_patch, skipped_files) = filter_patch(&patch);
    let (diff_text, mut diff_truncated) = truncate_chars(&filtered_patch, MAX_DIFF_CHARS);
    if patch.contains("差异过大已截断") {
        diff_truncated = true;
    }

    // ── 4. 分支 ──
    let branch = crate::commands::git::git_parse_refs(dir.clone())
        .map(|r| {
            if r.detached {
                "HEAD（游离）".to_string()
            } else {
                r.current_branch
            }
        })
        .unwrap_or_else(|_| "unknown".to_string());

    // ── 5. system prompt：技能正文优先，缺失/停用时用内置兜底 ──
    let registry = state.skill_registry.clone();
    let _ = registry.ensure_loaded(&dir);
    let skill_body = registry
        .find_enabled(SKILL_ID)
        .map(|s| s.body)
        .unwrap_or_default();
    let (system_prompt, prompt_source) = if skill_body.trim().is_empty() {
        (DEFAULT_SYSTEM_PROMPT.to_string(), "builtin".to_string())
    } else {
        (skill_body, "skill".to_string())
    };
    let (system_prompt, _) = truncate_chars(&system_prompt, MAX_SYSTEM_PROMPT_CHARS);
    if prompt_source == "builtin" {
        log::warn!("[git_ai] 技能 {} 不可用，使用内置 Prompt 兜底", SKILL_ID);
    }

    // ── 6. 模型调用（主模型；未配置直接给出可操作错误）──
    let cfg = state
        .llm_config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if cfg.endpoint.trim().is_empty() || cfg.model.trim().is_empty() {
        return Err("LLM_NOT_CONFIGURED|未配置 LLM 模型，请到设置中填写端点与模型".into());
    }
    let client = state
        .llm_client_for_role(&cfg, crate::ModelRole::Main)
        .await
        .map_err(|e| format!("LLM_INIT|初始化 LLM 客户端失败: {}", e))?;
    if !client.is_configured() {
        return Err("LLM_NOT_CONFIGURED|未配置 LLM 模型，请到设置中填写端点与模型".into());
    }

    let user_prompt = build_user_prompt(
        &branch,
        &files,
        &diff_text,
        diff_truncated,
        skipped_files,
    );
    log::info!(
        "[git_ai] 开始生成提交信息: dir={} staged={} diff_chars={} truncated={} prompt={}",
        dir,
        staged.len(),
        diff_text.chars().count(),
        diff_truncated,
        prompt_source
    );

    let cancel = CancellationToken::new();
    let raw = tokio::time::timeout(
        std::time::Duration::from_secs(LLM_TIMEOUT_SECS),
        client.complete_text(
            &system_prompt,
            &user_prompt,
            Some(MAX_OUTPUT_TOKENS),
            Some(0.2),
            cancel,
        ),
    )
    .await
    .map_err(|_| {
        format!(
            "TIMEOUT|生成超时（{} 秒），请检查模型服务后重试",
            LLM_TIMEOUT_SECS
        )
    })?
    .map_err(|e| format!("LLM_CALL|生成失败: {}", e))?;

    // ── 7. 解析 + 按阿里规范修复 header ──
    let (mut suggestion, parse_fallback) = parse_suggestion(&raw);
    if suggestion.subject.trim().is_empty() {
        return Err("EMPTY|模型未返回有效内容，请重试".into());
    }
    let (subject, repaired) = normalize_header(&suggestion.subject);
    suggestion.subject = subject;
    suggestion.body = sanitize_body(&suggestion.body);
    suggestion.footer = suggestion.footer.trim().to_string();

    let mut note_parts: Vec<String> = Vec::new();
    if !suggestion.notes.trim().is_empty() {
        note_parts.push(suggestion.notes.trim().to_string());
    }
    if parse_fallback {
        note_parts.push("模型输出非标准 JSON，已容错解析".to_string());
    }
    if repaired {
        note_parts.push("标题类型已按规范修正".to_string());
    }
    if diff_truncated {
        note_parts.push("暂存 diff 过大，已截断后生成".to_string());
    }
    if skipped_files > 0 {
        note_parts.push(format!("{} 个敏感/生成文件未纳入 diff", skipped_files));
    }
    let notes = note_parts.join("；");

    let mut parts: Vec<String> = vec![suggestion.subject.trim().to_string()];
    if !suggestion.body.trim().is_empty() {
        parts.push(suggestion.body.trim().to_string());
    }
    if !suggestion.footer.is_empty() {
        parts.push(suggestion.footer.clone());
    }
    let message = parts.join("\n\n");

    // ── 8. 技能指标（异步落库，不阻塞返回）──
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let metrics = state.skill_metrics.clone();
    let dir_for_metrics = dir.clone();
    let request_id = format!("git-suggest-{}", now_ms());
    let success = !parse_fallback && !repaired;
    let error_code = if success {
        None
    } else if parse_fallback {
        Some("PARSE_FALLBACK".to_string())
    } else {
        Some("HEADER_REPAIRED".to_string())
    };
    tokio::task::spawn_blocking(move || {
        metrics.record_dispatch(&dir_for_metrics, true);
        metrics.record_execution_batch(
            &dir_for_metrics,
            vec![ExecInput {
                skill_id: SKILL_ID.to_string(),
                scope: SKILL_SCOPE.to_string(),
                source: ActivationSource::Manual,
                match_score: 1.0,
                duration_ms: elapsed_ms,
            }],
            success,
            error_code.as_deref(),
            &request_id,
        );
    })
    .await
    .ok();

    Ok(CommitSuggestion {
        subject: suggestion.subject,
        body: suggestion.body,
        footer: suggestion.footer,
        notes,
        message,
        files: files.len(),
        additions,
        deletions,
        truncated: diff_truncated,
        fallback: parse_fallback,
        prompt_source,
        model: cfg.model,
        elapsed_ms,
    })
}

// ==================== 纯函数（可单测） ====================

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// numstat 文本 → (path, additions, deletions)；二进制为 `-` → None
fn parse_numstat(text: &str) -> Vec<(String, Option<u64>, Option<u64>)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let add = parts.next()?;
            let del = parts.next()?;
            let path = parts.next()?.trim();
            if path.is_empty() {
                return None;
            }
            Some((
                path.to_string(),
                add.parse::<u64>().ok(),
                del.parse::<u64>().ok(),
            ))
        })
        .collect()
}

/// 跳过敏感/生成文件（既省 token 也避免把密钥发送给模型）
fn should_skip_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".lock")
        || p.ends_with("-lock.json")
        || p.ends_with(".min.js")
        || p.ends_with(".min.css")
        || p.starts_with("dist/")
        || p.starts_with("node_modules/")
        || p.starts_with("target/")
        || p.starts_with(".mdgo/")
        || p.ends_with(".pem")
        || p.ends_with(".key")
        || p.starts_with(".env")
        || p.contains("id_rsa")
}

/// 按 `diff --git` 分块保留非敏感文件，返回（保留的 patch, 跳过的文件块数）
fn filter_patch(patch: &str) -> (String, usize) {
    if patch.trim().is_empty() {
        return (String::new(), 0);
    }
    let mut out = String::new();
    let mut keep = true;
    let mut skipped = 0usize;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            let path = line.rsplit(" b/").next().unwrap_or("").trim();
            let skip = should_skip_path(path);
            if skip {
                skipped += 1;
            }
            keep = !skip;
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    (out, skipped)
}

fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n…（已截断）");
    (out, true)
}

/// 去掉 ``` 围栏
fn strip_fences(s: &str) -> String {
    let mut t = s.trim();
    if t.starts_with("```") {
        if let Some(pos) = t.find('\n') {
            t = &t[pos + 1..];
        }
        let trimmed = t.trim_end();
        if trimmed.ends_with("```") {
            t = trimmed[..trimmed.len() - 3].trim_end();
        }
    }
    t.to_string()
}

/// 取第一个 `{` 开始、花括号配平的 JSON 对象（字符串内的括号不算）。
/// 旧实现用「第一个 { 到最后一个 }」，模型输出多个对象或前后带解释时会切出非法片段。
fn extract_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in s.char_indices().skip_while(|(i, _)| *i < start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// 把字符串值里的裸换行/制表符转义成 JSON 合法转义（模型常直接输出多行 body）
fn escape_control_chars_in_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in s.chars() {
        if in_string {
            if escaped {
                out.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => {
                    out.push(ch);
                    escaped = true;
                }
                '"' => {
                    out.push(ch);
                    in_string = false;
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        } else {
            out.push(ch);
            if ch == '"' {
                in_string = true;
            }
        }
    }
    out
}

/// 去掉对象/数组结尾多余的逗号（字符串内的逗号不受影响）
fn remove_trailing_commas(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escaped = false;
    for i in 0..chars.len() {
        let ch = chars[i];
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }
        if ch == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// 从半截/非法 JSON 中提取一个字符串字段（用于输出被截断时仍可拿到 subject/body）。
fn extract_string_field(input: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let key_pos = input.find(&needle)?;
    let after = &input[key_pos + needle.len()..];
    let colon = after.find(':')?;
    let value = after[colon + 1..].trim_start();
    let chars: Vec<char> = value.chars().collect();
    if chars.first() != Some(&'"') {
        return None;
    }
    let mut out = String::new();
    let mut j = 1usize;
    while j < chars.len() {
        match chars[j] {
            '\\' => {
                if j + 1 >= chars.len() {
                    break;
                }
                match chars[j + 1] {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'u' => {
                        if j + 5 < chars.len() {
                            let hex: String = chars[j + 2..j + 6].iter().collect();
                            if let Ok(v) = u32::from_str_radix(&hex, 16) {
                                if let Some(c) = char::from_u32(v) {
                                    out.push(c);
                                }
                                j += 6;
                                continue;
                            }
                        }
                        out.push_str("\\u");
                        j += 2;
                        continue;
                    }
                    c => out.push(c),
                }
                j += 2;
            }
            '"' => return Some(out.trim().to_string()),
            c => {
                out.push(c);
                j += 1;
            }
        }
    }
    // 没有收尾引号（被 max_tokens 截断）：返回已解析部分
    Some(out.trim().trim_end_matches('\\').trim().to_string())
}

/// 解析模型输出；优先严格 JSON，失败后做容错修复/字段提取，绝不把 JSON 残片当标题。
fn parse_suggestion(raw: &str) -> (RawSuggestion, bool) {
    let unfenced = strip_fences(raw);
    let text = unfenced.replace("\r\n", "\n");

    let mut candidates: Vec<String> = Vec::new();
    if let Some(obj) = extract_object(&text) {
        let escaped = escape_control_chars_in_strings(obj);
        candidates.push(obj.to_string());
        if escaped != obj {
            candidates.push(escaped.clone());
        }
        let no_commas = remove_trailing_commas(&escaped);
        if no_commas != escaped {
            candidates.push(no_commas);
        }
    }
    // 兜底：整段文本（模型可能只给出一个对象，前后带空白/解释）
    let escaped_all = escape_control_chars_in_strings(&text);
    candidates.push(remove_trailing_commas(&escaped_all));

    for candidate in candidates {
        if let Ok(mut v) = serde_json::from_str::<RawSuggestion>(&candidate) {
            if v.subject.trim().is_empty() {
                continue;
            }
            v.subject = v.subject.trim().trim_matches('"').trim().to_string();
            v.body = v.body.trim().to_string();
            v.footer = v.footer.trim().to_string();
            v.notes = v.notes.trim().to_string();
            return (v, false);
        }
    }

    // 容错：JSON 被截断/含未转义字符时，逐字段提取（subject 在对象开头，通常完整）
    let subject = extract_string_field(&text, "subject").unwrap_or_default();
    if !subject.trim().is_empty() {
        let body = extract_string_field(&text, "body").unwrap_or_default();
        let footer = extract_string_field(&text, "footer").unwrap_or_default();
        let notes = extract_string_field(&text, "notes").unwrap_or_default();
        return (
            RawSuggestion {
                subject,
                body,
                footer,
                notes,
            },
            true,
        );
    }

    let line = text
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let cleaned = line.trim_start_matches(['-', '*', '1', '.', ' ']).trim();
    // 兜底行本身是 JSON 残片时不要当作提交标题（否则会出现 `chore: {"subject":...}`）
    if cleaned.starts_with('{') || cleaned.contains("\"subject\"") {
        return (RawSuggestion::default(), true);
    }
    (
        RawSuggestion {
            subject: cleaned.to_string(),
            ..Default::default()
        },
        true,
    )
}

/// 宽松解析 header：`type(scope): desc`、`type:desc`、含大写 scope 都接受，
/// 由调用方决定是否规范化（模型偶尔会漏空格或写大写）。
fn split_header_loose(s: &str) -> Option<(String, Option<String>, String)> {
    let (prefix, desc) = s.split_once(':')?;
    let desc = desc.trim();
    if desc.is_empty() {
        return None;
    }
    let prefix = prefix.trim();
    let (ty, scope) = match prefix.find('(') {
        Some(i) => {
            if !prefix.ends_with(')') || i == 0 {
                return None;
            }
            (
                prefix[..i].trim().to_ascii_lowercase(),
                Some(prefix[i + 1..prefix.len() - 1].trim().to_ascii_lowercase()),
            )
        }
        None => (prefix.to_ascii_lowercase(), None),
    };
    Some((ty, scope, desc.to_string()))
}

fn scope_is_valid(scope: &str) -> bool {
    !scope.is_empty()
        && scope.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '/' | ',' | '-' | '_')
        })
}

/// 按阿里规范修复 header，并保证 ≤70 字符；返回（修复后的 header, 是否修复过）
fn normalize_header(raw: &str) -> (String, bool) {
    let s = raw
        .trim()
        .trim_matches('"')
        .trim()
        .trim_end_matches(['。', '.'])
        .trim();
    if s.is_empty() {
        return (String::new(), false);
    }
    // 防御：JSON 残片绝不能当标题（否则会生成 `chore: {"subject":...}`）
    if s.starts_with('{') || s.contains("\"subject\"") {
        return (String::new(), false);
    }

    if let Some((ty, scope, desc)) = split_header_loose(s) {
        let scope_ok = scope.as_deref().map(scope_is_valid).unwrap_or(true);
        if ALLOWED_TYPES.contains(&ty.as_str()) && scope_ok {
            let header = match &scope {
                Some(sc) => format!("{}({}): {}", ty, sc, desc),
                None => format!("{}: {}", ty, desc),
            };
            let (truncated, cut) = truncate_header(&header);
            let repaired = header != s || cut;
            return (truncated, repaired);
        }
        // 已知但不在允许列表里的类型（Conventional Commits 扩展 / 人工场景）→ 归到 chore
        if matches!(ty.as_str(), "build" | "ci" | "to" | "merge" | "sync") {
            let (t, _) = truncate_header(&format!("chore: {}", desc));
            return (t, true);
        }
    }

    let (t, _) = truncate_header(&format!("chore: {}", s));
    (t, true)
}

/// header 超长时优先截断描述部分，保留 `type(scope): ` 前缀
fn truncate_header(s: &str) -> (String, bool) {
    if s.chars().count() <= MAX_HEADER_CHARS {
        return (s.to_string(), false);
    }
    if let Some((prefix, desc)) = s.split_once(": ") {
        let prefix_chars = prefix.chars().count() + 2;
        if prefix_chars + 2 <= MAX_HEADER_CHARS {
            let take = MAX_HEADER_CHARS - prefix_chars - 1; // 预留省略号
            let desc: String = desc.chars().take(take).collect();
            return (format!("{}: {}…", prefix, desc.trim_end()), true);
        }
    }
    let mut out: String = s.chars().take(MAX_HEADER_CHARS - 1).collect();
    out.push('…');
    (out, true)
}

fn sanitize_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut lines: Vec<String> = trimmed
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect();
    while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    let joined = lines.join("\n");
    // body 直接截断，不追加“已截断”标记（避免写进提交信息）
    if joined.chars().count() > MAX_BODY_CHARS {
        joined.chars().take(MAX_BODY_CHARS).collect()
    } else {
        joined
    }
}

fn build_user_prompt(
    branch: &str,
    files: &[(String, String, Option<u64>, Option<u64>)],
    diff: &str,
    diff_truncated: bool,
    skipped_files: usize,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("当前分支：{}\n", branch));
    out.push_str(&format!("暂存文件（{} 个）：\n", files.len()));
    for (status, path, add, del) in files {
        let stat = match (add, del) {
            (Some(a), Some(d)) => format!(" (+{}/-{})", a, d),
            _ => String::new(),
        };
        out.push_str(&format!("- [{}] {}{}\n", status, path, stat));
    }
    if skipped_files > 0 {
        out.push_str(&format!(
            "（另有 {} 个敏感或生成文件未纳入 diff）\n",
            skipped_files
        ));
    }
    out.push_str("\n暂存区 diff");
    if diff_truncated {
        out.push_str("（已按预算截断，仅包含可见部分）");
    }
    out.push_str("：\n");
    if diff.trim().is_empty() {
        out.push_str("（没有可读文本差异，可能全为二进制或仅权限变更）\n");
    } else {
        out.push_str("```diff\n");
        out.push_str(diff);
        out.push_str("\n```\n");
    }
    out.push_str("\n请严格按技能要求只输出 JSON 对象。");
    out
}

// ==================== 单测 ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_json() {
        let (s, fallback) = parse_suggestion(
            r#"{"subject":"feat(git): 新增命令","body":"说明","footer":"","notes":"提示"}"#,
        );
        assert!(!fallback);
        assert_eq!(s.subject, "feat(git): 新增命令");
        assert_eq!(s.body, "说明");
        assert_eq!(s.notes, "提示");
    }

    #[test]
    fn parse_fenced_json() {
        let (s, fallback) = parse_suggestion(
            "```json\n{\"subject\":\"fix: 修复问题\",\"body\":\"\",\"footer\":\"\",\"notes\":\"\"}\n```",
        );
        assert!(!fallback);
        assert_eq!(s.subject, "fix: 修复问题");
    }

    #[test]
    fn parse_prose_wrapped_json() {
        let (s, fallback) =
            parse_suggestion("好的：\n{\"subject\":\"docs: 更新文档\",\"body\":\"\",\"footer\":\"\",\"notes\":\"\"}\n以上。");
        assert!(!fallback);
        assert_eq!(s.subject, "docs: 更新文档");
    }

    #[test]
    fn parse_falls_back_to_first_line() {
        let (s, fallback) = parse_suggestion("feat: 直接给了一行\n多余解释");
        assert!(fallback);
        assert_eq!(s.subject, "feat: 直接给了一行");
    }

    #[test]
    fn parse_truncated_json_salvages_fields() {
        // 复现线上问题：输出被 max_tokens 截断，JSON 没有收尾
        let raw = r#"{"subject":"style: 调整图表提示字号与 diff 视图按钮文案","body":"放大行为分析图表"#;
        let (s, fallback) = parse_suggestion(raw);
        assert!(fallback);
        assert_eq!(s.subject, "style: 调整图表提示字号与 diff 视图按钮文案");
        assert!(s.body.starts_with("放大行为分析图表"));
        assert!(!s.subject.contains('{'));
    }

    #[test]
    fn parse_json_with_literal_newlines_in_body() {
        // 模型把多行 body 直接写成裸换行（非法 JSON）
        let raw = "{\"subject\":\"feat: 新增功能\",\"body\":\"第一行\n第二行\",\"footer\":\"\",\"notes\":\"\"}";
        let (s, fallback) = parse_suggestion(raw);
        assert!(!fallback);
        assert_eq!(s.subject, "feat: 新增功能");
        assert_eq!(s.body, "第一行\n第二行");
    }

    #[test]
    fn parse_json_with_trailing_comma() {
        let raw = r#"{"subject":"fix: 修复问题","body":"说明","footer":"","notes":"",}"#;
        let (s, fallback) = parse_suggestion(raw);
        assert!(!fallback);
        assert_eq!(s.subject, "fix: 修复问题");
        assert_eq!(s.body, "说明");
    }

    #[test]
    fn parse_multiple_objects_uses_first_balanced() {
        let raw = r#"{"subject":"docs: 更新文档","body":"a","footer":"","notes":""} 或者 {"subject":"x"}"#;
        let (s, fallback) = parse_suggestion(raw);
        assert!(!fallback);
        assert_eq!(s.subject, "docs: 更新文档");
    }

    #[test]
    fn fallback_never_uses_json_fragment_as_subject() {
        let (s, fallback) = parse_suggestion("{\"unexpected\":1}");
        assert!(fallback);
        assert!(s.subject.is_empty());
    }

    #[test]
    fn normalize_rejects_json_fragment() {
        let (h, repaired) = normalize_header(r#"{"subject":"x"}"#);
        assert!(h.is_empty());
        assert!(!repaired);
    }

    #[test]
    fn normalize_repairs_build_type() {
        let (h, repaired) = normalize_header("build: 升级依赖");
        assert!(repaired);
        assert_eq!(h, "chore: 升级依赖");
    }

    #[test]
    fn normalize_keeps_valid_header() {
        let (h, repaired) = normalize_header("fix(auth): 修复登录态刷新失败");
        assert!(!repaired);
        assert_eq!(h, "fix(auth): 修复登录态刷新失败");
    }

    #[test]
    fn normalize_fixes_missing_space_and_uppercase_scope() {
        let (h, repaired) = normalize_header("feat(Git):新增命令");
        assert!(repaired);
        assert_eq!(h, "feat(git): 新增命令");
    }

    #[test]
    fn normalize_wraps_invalid_header() {
        let (h, repaired) = normalize_header("更新代码");
        assert!(repaired);
        assert_eq!(h, "chore: 更新代码");
    }

    #[test]
    fn header_truncates_to_70() {
        let long = format!("feat(git): {}", "很长的描述".repeat(20));
        let (h, truncated) = truncate_header(&long);
        assert!(truncated);
        assert!(h.chars().count() <= MAX_HEADER_CHARS);
        assert!(h.starts_with("feat(git): "));
    }

    #[test]
    fn filter_patch_skips_sensitive_and_dist() {
        let patch = "diff --git a/src/main.ts b/src/main.ts\n+ok\n\
diff --git a/.env b/.env\n+SECRET=1\n\
diff --git a/dist/app.js b/dist/app.js\n+built\n\
diff --git a/src/a.ts b/src/a.ts\n+two\n";
        let (kept, skipped) = filter_patch(patch);
        assert_eq!(skipped, 2);
        assert!(kept.contains("src/main.ts"));
        assert!(kept.contains("src/a.ts"));
        assert!(!kept.contains("SECRET"));
        assert!(!kept.contains("dist/app.js"));
    }

    #[test]
    fn numstat_parses_binary_and_normal() {
        let rows = parse_numstat("3\t1\tsrc/a.ts\n-\t-\timg.png\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("src/a.ts".to_string(), Some(3), Some(1)));
        assert_eq!(rows[1].0, "img.png");
        assert!(rows[1].1.is_none());
    }

    #[test]
    fn truncate_marks_cut() {
        let (s, cut) = truncate_chars("abcdef", 3);
        assert!(cut);
        assert!(s.starts_with("abc"));
        assert!(s.contains("已截断"));
    }
}
