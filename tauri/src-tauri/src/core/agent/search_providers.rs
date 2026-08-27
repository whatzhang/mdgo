//! Web 搜索提供商适配层（`web_search` 工具的数据源）——
//! 支持 Tavily / Brave / Exa 三家搜索 API，统一为 [`SearchResult`] 输出。
//!
//! # 设计（SOLID）
//!
//! - 单一职责：本模块只做「配置解析 + 搜索 API 调用 + 结果 markdown 化」；
//!   工具注册/审批/轨迹在 `loop_tools.rs`（WebSearchTool）。
//! - 开闭原则：新增提供商 = 新增 [`SearchProvider`] 分支，不改调用方。
//! - 依赖倒置：调用方只依赖 [`query`] 与 [`format_results`]，不感知具体 API。
//!
//! 配置来源：`%APPDATA%/com.mdgo/web_search.json`（经 `web_search_config_get/set`
//! 命令读写；API key 存本侧配置，**不暴露给模型**——模型调用时只传 query）。

use serde::{Deserialize, Serialize};

use crate::core::agent::limits::EXTERNAL_TIMEOUT_SECS;

/// 单条搜索结果。
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    /// 摘要（提供商给出；可能为空）
    pub snippet: String,
}

/// 支持的外部搜索提供商。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchProvider {
    Tavily,
    Brave,
    Exa,
}

impl SearchProvider {
    pub fn label(&self) -> &'static str {
        match self {
            SearchProvider::Tavily => "Tavily",
            SearchProvider::Brave => "Brave Search",
            SearchProvider::Exa => "Exa",
        }
    }
}

/// Web 搜索配置（持久化到 `web_search.json`）。
///
/// 三个提供商（Tavily / Brave / Exa）**各自独立存储 API key**（`keys` map），
/// 可同时配置并存；`provider` 字段记录 Agent 当前使用的提供商（唯一生效者，
/// 无自动优先级/回退——未配置 key 的提供商不可用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 当前生效的提供商（Agent 调用时使用；三个 key 可并存）
    #[serde(default)]
    pub provider: Option<SearchProvider>,
    /// 各提供商的 API key（按提供商独立存储，互不覆盖）
    #[serde(default)]
    pub keys: std::collections::HashMap<SearchProvider, String>,
    /// 默认返回条数（模型未指定时使用；1-10）
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: None,
            keys: std::collections::HashMap::new(),
            max_results: default_max_results(),
        }
    }
}

fn default_max_results() -> usize {
    5
}

impl WebSearchConfig {
    /// 配置是否可用（启用 + 已选提供商 + 该提供商有 key）
    pub fn is_ready(&self) -> bool {
        self.enabled
            && self.provider.is_some()
            && self.key_for(self.provider.unwrap()).is_some()
            && self.max_results.clamp(1, 10) > 0
    }

    /// 取指定提供商的 key（未配置返回 None）
    pub fn key_for(&self, p: SearchProvider) -> Option<&str> {
        self.keys
            .get(&p)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// 当前生效提供商的 key（is_ready 时必为 Some）
    pub fn active_key(&self) -> Option<&str> {
        self.provider.and_then(|p| self.key_for(p))
    }

    /// 当前提供商显示名（未配置返回空串）
    pub fn provider_label(&self) -> String {
        self.provider.map(|p| p.label().to_string()).unwrap_or_default()
    }

    /// 设置某提供商的 key（保存明文；空串 = 清除该提供商 key）
    pub fn set_key(&mut self, p: SearchProvider, key: String) {
        let key = key.trim().to_string();
        if key.is_empty() {
            self.keys.remove(&p);
        } else {
            self.keys.insert(p, key);
        }
    }
}

/// 默认配置路径：`%APPDATA%/com.mdgo/web_search.json`。
pub fn default_config_path() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    let base = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    #[cfg(target_os = "macos")]
    let base = std::path::PathBuf::from(
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
    )
    .join("Library")
    .join("Application Support");
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let base = std::path::PathBuf::from(
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
    )
    .join(".local")
    .join("share");
    base.join("com.mdgo").join("web_search.json")
}

/// 读取配置（文件缺失/解析失败 → 默认（禁用态），不阻断启动）。
pub fn load_config() -> WebSearchConfig {
    match std::fs::read_to_string(default_config_path()) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => WebSearchConfig::default(),
    }
}

/// 保存配置（父目录不存在则创建；写失败返回 Err）。
pub fn save_config(cfg: &WebSearchConfig) -> Result<(), String> {
    let path = default_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败: {}", e))?;
    }
    let raw = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化配置失败: {}", e))?;
    std::fs::write(&path, raw).map_err(|e| format!("写入配置失败: {}", e))
}

// ─────────────────────────── 搜索调用 ───────────────────────────

/// 执行一次 Web 搜索（按配置的提供商分派）。
///
/// - 未配置/禁用：返回 Err（工具侧给出引导提示）
/// - 提供商请求失败/解析失败：返回 Err（含可读原因，模型可据此调整）
/// - `max_results` 自动 clamp 到 [1, 10]
pub async fn query(
    provider: SearchProvider,
    api_key: &str,
    query_text: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, String> {
    let max_results = max_results.clamp(1, 10);
    match provider {
        SearchProvider::Tavily => tavily_search(api_key, query_text, max_results).await,
        SearchProvider::Brave => brave_search(api_key, query_text, max_results).await,
        SearchProvider::Exa => exa_search(api_key, query_text, max_results).await,
    }
}

/// 统一的 HTTP 客户端（超时复用 EXTERNAL_TIMEOUT_SECS）。
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(EXTERNAL_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {}", e))
}

/// Tavily：POST /search，返回 `results[]: {title, url, content}`
async fn tavily_search(
    api_key: &str,
    query_text: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, String> {
    let client = http_client()?;
    let resp = client
        .post("https://api.tavily.com/search")
        .json(&serde_json::json!({
            "api_key": api_key,
            "query": query_text,
            "max_results": max_results,
            "search_depth": "basic",
        }))
        .send()
        .await
        .map_err(|e| format!("Tavily 请求失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Tavily 返回状态 {}", resp.status()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Tavily 响应解析失败: {}", e))?;
    let results = body
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(results
        .into_iter()
        .map(|r| SearchResult {
            title: r.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            url: r.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            snippet: r.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        })
        .filter(|r| !r.url.is_empty())
        .collect())
}

/// Brave：GET /res/v1/web/search?q=，返回 `web.results[]: {title, url, description}`
async fn brave_search(
    api_key: &str,
    query_text: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, String> {
    let client = http_client()?;
    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[
            ("q", query_text),
            ("count", &max_results.to_string()),
        ])
        .header("X-Subscription-Token", api_key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("Brave 请求失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Brave 返回状态 {}", resp.status()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Brave 响应解析失败: {}", e))?;
    let results = body
        .get("web")
        .and_then(|v| v.get("results"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(results
        .into_iter()
        .map(|r| SearchResult {
            title: r.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            url: r.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            snippet: r.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        })
        .filter(|r| !r.url.is_empty())
        .collect())
}

/// Exa：POST /search，返回 `results[]: {title, url, text}`
async fn exa_search(
    api_key: &str,
    query_text: &str,
    max_results: usize,
) -> Result<Vec<SearchResult>, String> {
    let client = http_client()?;
    let resp = client
        .post("https://api.exa.ai/search")
        .header("x-api-key", api_key)
        .json(&serde_json::json!({
            "query": query_text,
            "numResults": max_results,
            "contents": { "text": { "maxCharacters": 300 } },
        }))
        .send()
        .await
        .map_err(|e| format!("Exa 请求失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Exa 返回状态 {}", resp.status()));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Exa 响应解析失败: {}", e))?;
    let results = body
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(results
        .into_iter()
        .map(|r| SearchResult {
            title: r.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            url: r.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            snippet: r
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .filter(|r| !r.url.is_empty())
        .collect())
}

// ─────────────────────────── 结果格式化 ───────────────────────────

/// 单条摘要截断上限（字符）
const SNIPPET_MAX_CHARS: usize = 200;

/// 把搜索结果 markdown 化（标题 + URL + 摘要截断），供模型直接阅读。
/// 末尾提示可配合 `webfetch` 打开具体 URL——工具协同，避免模型误以为搜索即全文。
pub fn format_results(query_text: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("搜索「{query_text}」未返回结果。");
    }
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("搜索结果（\"{query_text}\"，共 {} 条）：", results.len()));
    for (i, r) in results.iter().enumerate() {
        let title = if r.title.is_empty() { "(无标题)".to_string() } else { r.title.clone() };
        lines.push(format!("{}. [{}]({})", i + 1, title, r.url));
        if !r.snippet.trim().is_empty() {
            let s = r.snippet.trim();
            let s = if s.chars().count() > SNIPPET_MAX_CHARS {
                format!("{}…", s.chars().take(SNIPPET_MAX_CHARS).collect::<String>())
            } else {
                s.to_string()
            };
            lines.push(format!("   摘要：{s}"));
        }
    }
    lines.push("如需某条完整内容，调用 webfetch 打开对应 URL。".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_disabled() {
        let cfg = WebSearchConfig::default();
        assert!(!cfg.is_ready());
        assert_eq!(cfg.max_results, 5);
    }

    #[test]
    fn config_ready_requires_all_fields() {
        let mut base = WebSearchConfig {
            enabled: true,
            provider: Some(SearchProvider::Tavily),
            keys: Default::default(),
            max_results: 3,
        };
        base.set_key(SearchProvider::Tavily, "k".into());
        assert!(base.is_ready());
        // 缺 key（当前提供商无 key）
        let mut no_key = base.clone();
        no_key.set_key(SearchProvider::Tavily, String::new());
        assert!(!no_key.is_ready());
        // 未启用
        let disabled = WebSearchConfig { enabled: false, ..base.clone() };
        assert!(!disabled.is_ready());
    }

    #[test]
    fn keys_are_per_provider_independent() {
        // 三个提供商 key 独立存储，互不覆盖（问题 2 的回归测试）
        let mut cfg = WebSearchConfig::default();
        cfg.set_key(SearchProvider::Tavily, "tavily-key".into());
        cfg.set_key(SearchProvider::Brave, "brave-key".into());
        assert_eq!(cfg.key_for(SearchProvider::Tavily), Some("tavily-key"));
        assert_eq!(cfg.key_for(SearchProvider::Brave), Some("brave-key"));
        assert_eq!(cfg.key_for(SearchProvider::Exa), None);
        // 覆盖 Tavily 不影响 Brave
        cfg.set_key(SearchProvider::Tavily, "tavily-key-2".into());
        assert_eq!(cfg.key_for(SearchProvider::Brave), Some("brave-key"));
        // 清除一个不影响其它
        cfg.set_key(SearchProvider::Tavily, String::new());
        assert_eq!(cfg.key_for(SearchProvider::Tavily), None);
        assert_eq!(cfg.key_for(SearchProvider::Brave), Some("brave-key"));
        // active_key 跟随当前 provider
        cfg.provider = Some(SearchProvider::Brave);
        cfg.enabled = true;
        assert_eq!(cfg.active_key(), Some("brave-key"));
        assert!(cfg.is_ready());
    }

    #[test]
    fn provider_label_maps() {
        assert_eq!(SearchProvider::Tavily.label(), "Tavily");
        assert_eq!(SearchProvider::Brave.label(), "Brave Search");
        assert_eq!(SearchProvider::Exa.label(), "Exa");
    }

    #[test]
    fn format_results_empty_and_normal() {
        assert!(format_results("x", &[]).contains("未返回结果"));
        let results = vec![
            SearchResult {
                title: "Rust 异步运行时".into(),
                url: "https://example.com/rust".into(),
                snippet: "tokio 仍是主流，多线程 work-stealing 调度器性能领先。".into(),
            },
            SearchResult {
                title: String::new(),
                url: "https://example.com/b".into(),
                snippet: String::new(),
            },
        ];
        let out = format_results("rust async", &results);
        assert!(out.contains("共 2 条"));
        assert!(out.contains("[Rust 异步运行时](https://example.com/rust)"));
        assert!(out.contains("(无标题)"));
        assert!(out.contains("webfetch"));
    }

    #[test]
    fn format_results_truncates_long_snippet() {
        let long = "x".repeat(500);
        let results = vec![SearchResult {
            title: "t".into(),
            url: "https://e.com".into(),
            snippet: long,
        }];
        let out = format_results("q", &results);
        assert!(out.contains("…"), "超长摘要应截断");
        assert!(out.chars().count() < 500, "输出不应包含完整超长摘要");
    }

    #[test]
    fn config_round_trip_via_temp_path() {
        // save_config/load_config 走默认路径，这里只验证序列化兼容（直接构造 JSON）
        let mut cfg = WebSearchConfig {
            enabled: true,
            provider: Some(SearchProvider::Exa),
            keys: Default::default(),
            max_results: 7,
        };
        cfg.set_key(SearchProvider::Exa, "sk-x".into());
        let raw = serde_json::to_string(&cfg).unwrap();
        let parsed: WebSearchConfig = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.provider, Some(SearchProvider::Exa));
        assert_eq!(parsed.max_results, 7);
        assert_eq!(parsed.key_for(SearchProvider::Exa), Some("sk-x"));
        // 旧配置缺字段 → 默认值兜底
        let legacy: WebSearchConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(legacy.provider.is_none());
        assert_eq!(legacy.max_results, 5);
    }
}
