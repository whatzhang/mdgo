//! AI 用量统计（dashboard 热力图数据源）
//!
//! 业务：展示 AI 调用情况、token 使用情况、对话次数、会话数（默认最近一年）。
//! 性能：内存缓存 30s（key = dir|days），命中直接返回；未命中走 SQLite 聚合
//! （created_at 有索引，一年数据 COUNT/SUM/GROUP BY 毫秒级，满足 <1s）。
//! 高可用：缓存锁失败/聚合失败均返回明确错误，前端降级显示；聚合在
//! spawn_blocking 线程执行，不阻塞 tokio。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use tauri::State;

use crate::AppState;

/// 缓存 TTL（秒）
const STATS_CACHE_TTL_SECS: u64 = 60 * 60;
/// 默认统计范围（天）
const DEFAULT_DAYS: u32 = 365;
/// 一天毫秒数
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Serialize)]
pub struct AiUsageDaily {
    pub date: String,
    pub ai_calls: u32,
    pub messages: u32,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiUsageSummary {
    pub ai_calls: u64,
    pub messages: u64,
    pub sessions: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AiUsageStats {
    pub days: u32,
    pub daily: Vec<AiUsageDaily>,
    pub summary: AiUsageSummary,
}

/// AI 统计缓存（内存，惰性过期）
#[derive(Default)]
pub struct AiStatsCache {
    pub inner: Mutex<HashMap<String, (Instant, AiUsageStats)>>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 获取 AI 用量统计（按天聚合，最近 N 天）。
#[tauri::command]
pub async fn stats_ai_usage(
    state: State<'_, AppState>,
    dir_path: String,
    days: Option<u32>,
) -> Result<AiUsageStats, String> {
    let days = days.unwrap_or(DEFAULT_DAYS).clamp(1, 3650);
    let cache_key = format!("{}|{}", dir_path, days);

    // 1. 缓存命中（30s TTL）
    if let Ok(guard) = state.ai_stats_cache.inner.lock() {
        if let Some(cached) = guard.get(&cache_key) {
            if cached.0.elapsed().as_secs() < STATS_CACHE_TTL_SECS {
                return Ok(cached.1.clone());
            }
        }
    }

    // 2. 聚合（spawn_blocking 避免阻塞异步运行时）
    let ai_store = state.get_ai_history_store(&dir_path)?;
    let chat_store = state.get_chat_store(&dir_path)?;
    let ts_ms = now_ms().saturating_sub(i64::from(days) * DAY_MS);

    let (ai_daily, msg_daily, sessions) = tokio::task::spawn_blocking(move || {
        let a = ai_store.daily_usage_since(ts_ms)?;
        let m = chat_store.daily_messages_since(ts_ms)?;
        let s = chat_store.session_count_since(ts_ms)?;
        Ok::<_, String>((a, m, s))
    })
    .await
    .map_err(|e| format!("统计任务执行失败: {}", e))??;

    // 3. 合并为按天数据 + 汇总
    let mut by_date: HashMap<String, AiUsageDaily> = HashMap::new();
    let mut summary = AiUsageSummary {
        ai_calls: 0,
        messages: 0,
        sessions,
        tokens: 0,
    };
    for (date, cnt, tk) in ai_daily {
        let e = by_date
            .entry(date.clone())
            .or_insert(AiUsageDaily { date, ai_calls: 0, messages: 0, tokens: 0 });
        e.ai_calls += cnt;
        e.tokens += tk;
        summary.ai_calls += u64::from(cnt);
        summary.tokens += tk;
    }
    for (date, cnt, tk) in msg_daily {
        let e = by_date
            .entry(date.clone())
            .or_insert(AiUsageDaily { date, ai_calls: 0, messages: 0, tokens: 0 });
        e.messages += cnt;
        e.tokens += tk;
        summary.messages += u64::from(cnt);
        summary.tokens += tk;
    }
    let mut daily: Vec<AiUsageDaily> = by_date.into_values().collect();
    daily.sort_by(|a, b| a.date.cmp(&b.date));

    let stats = AiUsageStats {
        days,
        daily,
        summary,
    };

    // 4. 写缓存（失败不影响返回）
    if let Ok(mut guard) = state.ai_stats_cache.inner.lock() {
        guard.insert(cache_key, (Instant::now(), stats.clone()));
    }

    Ok(stats)
}
