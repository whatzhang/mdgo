//! Skill 管理命令层（M1 基础管理）。
//!
//! 命令均基于 `SkillRegistry`（内存注册表）+ `SkillStore`（文件读写），
//! 写路径（创建/更新/删除/启停）先落盘 → 重建注册表 → 广播 `skill:changed`。

use tauri::{AppHandle, Emitter, Manager};

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
