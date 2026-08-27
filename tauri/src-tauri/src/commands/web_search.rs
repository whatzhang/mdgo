//! Web 搜索配置命令层：前端 Agent 设置页读写 `web_search.json`。
//!
//! - `web_search_config_get`：读取当前配置（enabled / provider / 各提供商 key 掩码 / max_results）
//! - `web_search_config_set`：保存配置（校验 provider 合法、max_results 范围；
//!   API key **按提供商独立存储**，设置 Tavily 不影响 Brave/Exa）
//! - `web_search_test`：用指定提供商 + key 发起一次搜索（不落库），前端「测试连接」用
//!
//! API key 安全：`get` 返回时掩码（`sk-****1234`），避免回显明文；`set` 时若
//! api_key 字段以掩码形式回传（前端只改了其他字段），保留该提供商原值不覆盖。

use tauri::State;

use crate::core::agent::search_providers::{self, SearchProvider, WebSearchConfig};

/// 单个提供商的 key 状态（前端渲染用）。
#[derive(Debug, serde::Serialize)]
pub struct ProviderKeyDto {
    /// 是否已配置 key
    pub configured: bool,
    /// 掩码（未配置为空串）
    pub masked: String,
}

/// 配置响应（各提供商 key 已掩码，供前端回显）。
#[derive(Debug, serde::Serialize)]
pub struct WebSearchConfigDto {
    pub enabled: bool,
    pub provider: Option<String>,
    pub provider_label: String,
    pub max_results: usize,
    /// 各提供商 key 状态（tavily / brave / exa）
    pub keys: std::collections::HashMap<String, ProviderKeyDto>,
}

fn provider_id(p: SearchProvider) -> &'static str {
    match p {
        SearchProvider::Tavily => "tavily",
        SearchProvider::Brave => "brave",
        SearchProvider::Exa => "exa",
    }
}

fn parse_provider(s: &str) -> Result<SearchProvider, String> {
    match s.trim().to_lowercase().as_str() {
        "tavily" => Ok(SearchProvider::Tavily),
        "brave" => Ok(SearchProvider::Brave),
        "exa" => Ok(SearchProvider::Exa),
        other => Err(format!("提供商非法: {}（应为 tavily / brave / exa）", other)),
    }
}

fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    if key.len() <= 8 {
        return "****".to_string();
    }
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("****{}", tail)
}

fn to_dto(cfg: &WebSearchConfig) -> WebSearchConfigDto {
    let mut keys = std::collections::HashMap::new();
    for p in [SearchProvider::Tavily, SearchProvider::Brave, SearchProvider::Exa] {
        let k = cfg.key_for(p).unwrap_or("");
        keys.insert(
            provider_id(p).to_string(),
            ProviderKeyDto {
                configured: !k.is_empty(),
                masked: mask_key(k),
            },
        );
    }
    WebSearchConfigDto {
        enabled: cfg.enabled,
        provider: cfg.provider.map(provider_id).map(|s| s.to_string()),
        provider_label: cfg.provider_label(),
        max_results: cfg.max_results.clamp(1, 10),
        keys,
    }
}

/// 读取当前配置（各提供商 key 掩码后返回）。
#[tauri::command]
pub fn web_search_config_get() -> WebSearchConfigDto {
    to_dto(&search_providers::load_config())
}

/// 保存配置。
///
/// - `provider`: "tavily" / "brave" / "exa"（空 = 不选）
/// - `api_key`: **该提供商的**明文 key；若以 `****` 开头（前端回显的掩码）则视为
///   未修改，保留该提供商原值。不同提供商 key 独立存储，互不覆盖。
#[tauri::command]
pub fn web_search_config_set(
    _state: State<'_, crate::AppState>,
    enabled: bool,
    provider: Option<String>,
    api_key: Option<String>,
    max_results: Option<usize>,
) -> Result<WebSearchConfigDto, String> {
    let mut cfg = search_providers::load_config();
    cfg.enabled = enabled;

    // provider 解析（空串 → None）
    if let Some(p) = provider {
        let p = p.trim().to_lowercase();
        if p.is_empty() {
            cfg.provider = None;
        } else {
            cfg.provider = Some(parse_provider(&p)?);
        }
    }

    // api_key：掩码回传则保留该提供商原值，否则覆盖该提供商的 key
    if let Some(k) = api_key {
        let k = k.trim().to_string();
        if let Some(p) = cfg.provider {
            // 掩码 = 未修改，保留原值；明文（含空串=清除）才写入
            if !k.starts_with("****") || k.is_empty() {
                cfg.set_key(p, k);
            }
        }
    }

    if let Some(m) = max_results {
        cfg.max_results = m.clamp(1, 10);
    }

    search_providers::save_config(&cfg)?;
    Ok(to_dto(&cfg))
}

/// 测试连接：用指定提供商 + key（未传则用该提供商已保存的 key）发起一次搜索，不落库。
///
/// 关键：前端「测试」按钮应在**保存前**即可验证表单里的 key——故不能只读
/// 已保存配置。`api_key` 为掩码或空时回退该提供商已保存的 key。
#[tauri::command]
pub async fn web_search_test(
    _state: State<'_, crate::AppState>,
    provider: Option<String>,
    api_key: Option<String>,
    max_results: Option<usize>,
    query: Option<String>,
) -> Result<serde_json::Value, String> {
    let saved = search_providers::load_config();

    // 解析提供商（传入优先，否则已保存；空串 → None）
    let provider = match provider {
        Some(p) if !p.trim().is_empty() => Some(parse_provider(&p)?),
        _ => saved.provider,
    };
    let provider = provider.ok_or_else(|| "请选择搜索提供商（Tavily / Brave / Exa）".to_string())?;

    // api_key：传入明文则用传入值；掩码或空则回退该提供商已保存 key
    let api_key = match api_key {
        Some(k) => {
            let k = k.trim().to_string();
            if k.is_empty() || k.starts_with("****") {
                saved.key_for(provider).unwrap_or("").to_string()
            } else {
                k
            }
        }
        None => saved.key_for(provider).unwrap_or("").to_string(),
    };
    if api_key.trim().is_empty() {
        return Err(format!("请填写搜索 API Key（{}）", provider.label()));
    }

    let max_results = max_results.unwrap_or(saved.max_results).clamp(1, 10);
    let q = query.unwrap_or_else(|| "mdgo".to_string());
    let results = search_providers::query(provider, &api_key, &q, max_results).await?;
    let first_title = results.first().map(|r| r.title.clone()).unwrap_or_default();
    Ok(serde_json::json!({
        "ok": true,
        "provider": provider.label(),
        "count": results.len(),
        "first_title": first_title,
    }))
}
