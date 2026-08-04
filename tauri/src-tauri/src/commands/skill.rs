//! Skill 管理命令层（M1 基础管理 + M2 意图匹配 + M2 会话挂载）。
//!
//! 命令均基于 `SkillRegistry`（内存注册表）+ `SkillStore`（文件读写），
//! 写路径（创建/更新/删除/启停）先落盘 → 重建注册表 → 广播 `skill:changed`。

use tauri::{AppHandle, Emitter, Manager};

use crate::core::skill::matcher::{match_skills, MatchResult};
use crate::core::skill::{Skill, SkillFieldError, SkillInput, SkillScope, SkillStore, validate_skill};
use crate::AppState;

/// 将字段级错误列表格式化为可读错误串
fn format_skill_errors(errors: &[SkillFieldError]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {}", e.field, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_scope(scope: &str) -> Result<SkillScope, String> {
    SkillScope::from_str(scope).ok_or_else(|| format!("scope 非法: {}（应为 system/global/project）", scope))
}

/// 广播注册表变更事件（前端监听 `skill:changed` 自动刷新）
fn emit_changed(app: &AppHandle) {
    let _ = app.emit("skill:changed", ());
}

/// 技能列表（支持按作用域过滤）。
///
/// 首次访问时重建注册表；此后由写操作 / watcher 负责热更新。
#[tauri::command]
pub async fn skill_list(
    app: AppHandle,
    dir_path: String,
    scope: Option<String>,
) -> Result<Vec<Skill>, String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;
    state.skill_watcher.set_current_dir(&dir_path);
    let scope_filter = match scope {
        Some(s) => Some(parse_scope(&s)?),
        None => None,
    };
    Ok(state.skill_registry.list(scope_filter))
}

/// 技能详情（含正文）
#[tauri::command]
pub async fn skill_get(
    app: AppHandle,
    dir_path: String,
    scope: String,
    id: String,
) -> Result<Skill, String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;
    let sc = parse_scope(&scope)?;
    state
        .skill_registry
        .get(sc, &id)
        .ok_or_else(|| format!("技能不存在: {}:{}", scope, id))
}

/// 新建技能（仅 global / project 作用域）
#[tauri::command]
pub async fn skill_create(
    app: AppHandle,
    dir_path: String,
    scope: String,
    input: SkillInput,
) -> Result<Skill, String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;

    let sc = parse_scope(&scope)?;
    if !sc.is_writable() {
        return Err("系统内置技能不可创建".into());
    }
    let id = input
        .id
        .clone()
        .unwrap_or_default();
    if id.trim().is_empty() {
        return Err("id 不能为空".into());
    }
    if state.skill_registry.get(sc, &id).is_some() {
        return Err(format!("技能已存在: {}:{}", sc.as_str(), id));
    }

    let skill = input.to_new_skill(sc, &id);
    let errors = validate_skill(&skill);
    if !errors.is_empty() {
        return Err(format_skill_errors(&errors));
    }

    SkillStore::save_skill(&dir_path, &skill)?;
    state.skill_registry.reload(&dir_path)?;
    emit_changed(&app);
    Ok(skill)
}

/// 更新技能（系统内置拒绝；version 自增，updated_at 刷新）
#[tauri::command]
pub async fn skill_update(
    app: AppHandle,
    dir_path: String,
    scope: String,
    id: String,
    input: SkillInput,
) -> Result<Skill, String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;

    let sc = parse_scope(&scope)?;
    if !sc.is_writable() {
        return Err("系统内置技能不可修改".into());
    }
    let existing = state
        .skill_registry
        .get(sc, &id)
        .ok_or_else(|| format!("技能不存在: {}:{}", scope, id))?;

    let mut updated = input.merge_into(&existing);
    updated.version = updated.version.saturating_add(1);
    updated.updated_at = unix_timestamp_now();

    let errors = validate_skill(&updated);
    if !errors.is_empty() {
        return Err(format_skill_errors(&errors));
    }

    SkillStore::save_skill(&dir_path, &updated)?;
    state.skill_registry.reload(&dir_path)?;
    emit_changed(&app);
    Ok(updated)
}

/// 删除技能（仅用户级；系统内置拒绝）
#[tauri::command]
pub async fn skill_delete(
    app: AppHandle,
    dir_path: String,
    scope: String,
    id: String,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;
    let sc = parse_scope(&scope)?;
    SkillStore::delete_skill(&dir_path, sc, &id)?;
    state.skill_registry.reload(&dir_path)?;
    emit_changed(&app);
    Ok(())
}

/// 线上启停技能（动态生效，不重启服务）
#[tauri::command]
pub async fn skill_set_enabled(
    app: AppHandle,
    dir_path: String,
    scope: String,
    id: String,
    enabled: bool,
) -> Result<Skill, String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;
    let sc = parse_scope(&scope)?;
    if !sc.is_writable() {
        return Err("系统内置技能不可停用".into());
    }
    let mut skill = state
        .skill_registry
        .get(sc, &id)
        .ok_or_else(|| format!("技能不存在: {}:{}", scope, id))?;
    skill.enabled = enabled;
    skill.updated_at = unix_timestamp_now();

    SkillStore::save_skill(&dir_path, &skill)?;
    state.skill_registry.reload(&dir_path)?;
    emit_changed(&app);
    Ok(skill)
}

fn unix_timestamp_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 分层意图匹配（调试用）
///
/// 返回匹配结果列表，包含技能、匹配层级、得分。
#[tauri::command]
pub async fn skill_match(
    app: AppHandle,
    dir_path: String,
    query: String,
) -> Result<Vec<MatchResult>, String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;

    // 获取所有启用的技能
    let skills = state.skill_registry.list(None);
    let enabled_skills: Vec<Skill> = skills.into_iter().filter(|s| s.enabled).collect();

    if enabled_skills.is_empty() {
        return Ok(Vec::new());
    }

    // 同步批量嵌入（ONNX 推理）在 spawn_blocking 中调度，
    // 避免阻塞异步运行时（消除旧实现 Handle::block_on 在异步上下文的 panic 风险）
    let results = tokio::task::spawn_blocking(move || {
        match_skills(&query, &enabled_skills, crate::core::db::utils::embed_texts_batch)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {}", e))??;

    Ok(results)
}

/// 会话挂载技能（保存快照到 DB）
#[tauri::command]
pub async fn skill_attach(
    app: AppHandle,
    dir_path: String,
    session_id: String,
    scope: String,
    skill_id: String,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;

    let sc = parse_scope(&scope)?;
    let skill = state
        .skill_registry
        .get(sc, &skill_id)
        .ok_or_else(|| format!("技能不存在: {}:{}", scope, skill_id))?;

    if !skill.enabled {
        return Err("技能已停用，无法挂载".into());
    }

    // 打开会话数据库
    let chat_store = state.get_chat_store(&dir_path)?;
    chat_store.attach_skill(&session_id, &scope, &skill_id, skill.version)?;

    Ok(())
}

/// 会话卸载技能
#[tauri::command]
pub async fn skill_detach(
    app: AppHandle,
    dir_path: String,
    session_id: String,
    scope: String,
    skill_id: String,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let chat_store = state.get_chat_store(&dir_path)?;
    chat_store.detach_skill(&session_id, &scope, &skill_id)?;

    Ok(())
}

/// 获取会话挂载的技能列表（含版本校验）
#[tauri::command]
pub async fn skill_get_attached(
    app: AppHandle,
    dir_path: String,
    session_id: String,
) -> Result<Vec<Skill>, String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;

    let chat_store = state.get_chat_store(&dir_path)?;
    let attached = chat_store.get_attached_skills(&session_id)?;

    // 从注册表获取技能详情，并校验版本
    let mut skills = Vec::new();
    for (scope_str, skill_id, attached_version) in attached {
        if let Ok(sc) = parse_scope(&scope_str) {
            if let Some(skill) = state.skill_registry.get(sc, &skill_id) {
                // 已停用的技能从挂载列表过滤（与 context.rs 挂载解析逻辑一致）
                if !skill.enabled {
                    log::warn!(
                        "[skill] 会话 {} 挂载的技能 {}:{} 已停用，从挂载列表过滤",
                        session_id,
                        scope_str,
                        skill_id
                    );
                    continue;
                }
                // 版本漂移警告（技能已更新，但会话仍使用旧版本）
                if skill.version != attached_version {
                    log::warn!(
                        "[skill] 会话 {} 挂载的技能 {}:{} 版本漂移（挂载版本: {}, 当前版本: {}）",
                        session_id,
                        scope_str,
                        skill_id,
                        attached_version,
                        skill.version
                    );
                }
                skills.push(skill);
            } else {
                log::warn!(
                    "[skill] 会话 {} 挂载的技能 {}:{} 已不存在",
                    session_id,
                    scope_str,
                    skill_id
                );
            }
        }
    }

    Ok(skills)
}

/// 获取技能执行指标聚合数据
///
/// 返回全局或指定技能的执行统计，包括成功率、耗时分布、错误码等。
#[tauri::command]
pub async fn skill_metrics(
    app: AppHandle,
    dir_path: String,
    skill_id: Option<String>,
    since: Option<u64>,
) -> Result<crate::core::skill::metrics::GlobalMetricsSummary, String> {
    let state = app.state::<AppState>();
    state.skill_registry.ensure_loaded(&dir_path)?;
    
    let summary = state.skill_metrics.get_summary(
        skill_id.as_deref(),
        since,
    );
    
    Ok(summary)
}
