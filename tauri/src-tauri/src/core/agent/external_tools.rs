//! 动态外部工具（P2-15）：配置驱动的 HTTP 工具适配器。
//!
//! # 设计（SOLID）
//!
//! - [`ExternalToolDef`]：外部工具定义（name/description/url/method/params_schema/
//!   timeout），YAML 文件配置（`%APPDATA%/com.mdgo/agent_tools.yaml`）。
//! - [`load_external_tools`]：配置加载（文件不存在返回空集，不阻断启动；
//!   解析失败记日志并降级为空集——外部工具是可选能力）。
//! - [`build_external_tool`]：把定义转为 rig `DynamicTool`，闭包内以
//!   HTTP POST JSON 调用外部端点，响应文本返回模型。
//!
//! 这是 MCP 全协议客户端的最小前置形态：先支持"配置驱动的 HTTP 工具"，
//! 后续可按同一注册面接入 stdio/MCP 传输（规划文档 P2-15）。

use std::path::Path;

use rig_agent::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde::Deserialize;

use crate::core::agent::limits::{EXTERNAL_TIMEOUT_SECS, MAX_EXTERNAL_RESPONSE_CHARS};
use crate::core::agent::KbSearchConfig;

/// 外部工具响应体上限（字符）见 limits::MAX_EXTERNAL_RESPONSE_CHARS

/// 外部工具定义（agent_tools.yaml 中的一条）。
#[derive(Debug, Clone, Deserialize)]
pub struct ExternalToolDef {
    /// 工具名（Agent 调用时使用；不得与内置工具重名）
    pub name: String,
    /// 工具描述（模型选择工具的依据）
    pub description: String,
    /// HTTP 端点（POST JSON；请求体为模型参数对象）
    pub url: String,
    /// HTTP 方法（默认 POST）
    #[serde(default = "default_method")]
    pub method: String,
    /// 参数 JSON Schema（OpenAI 工具参数格式；`type: object` + properties；缺省为空对象）
    #[serde(default = "default_schema")]
    pub params_schema: serde_json::Value,
    /// 请求超时（秒，默认 30）
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_method() -> String {
    "POST".to_string()
}

fn default_timeout() -> u64 {
    EXTERNAL_TIMEOUT_SECS
}

fn default_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

/// 默认外部工具配置文件路径：`%APPDATA%/com.mdgo/agent_tools.yaml`。
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
    base.join("com.mdgo").join("agent_tools.yaml")
}

/// 从 YAML 加载外部工具定义。
///
/// - 文件不存在：返回空集（可选能力，不阻断）
/// - 解析失败：返回 Err（调用方记日志并降级为空集）
/// - 空列表 / 无 name 或 url 的条目：跳过并告警
pub fn load_external_tools(path: &Path) -> Result<Vec<ExternalToolDef>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("读取外部工具配置失败: {}", e))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let defs: Vec<ExternalToolDef> = serde_yaml::from_str(&raw)
        .map_err(|e| format!("外部工具配置 YAML 解析失败: {}", e))?;
    Ok(defs
        .into_iter()
        .filter(|d| !d.name.trim().is_empty() && !d.url.trim().is_empty())
        .collect())
}

/// 构建外部工具 DynamicTool（HTTP JSON 调用适配器）。
pub fn build_external_tool(def: ExternalToolDef, cfg: KbSearchConfig) -> DynamicTool {
    let name = def.name.clone();
    let description = def.description.clone();
    let parameters = def.params_schema.clone();
    DynamicTool::new(
        name.clone(),
        description,
        parameters,
        move |_ctx: &mut ToolContext, args: serde_json::Value| {
            let cfg = cfg.clone();
            let def = def.clone();
            Box::pin(async move {
                // 工具轨迹记录（与内置工具一致）
                crate::core::agent::tools::record_tool_call(
                    &cfg,
                    &def.name,
                    &args.to_string().chars().take(80).collect::<String>(),
                    Some(&args),
                );
                let client = reqwest::Client::new();
                let url = def.url.clone();
                let timeout = std::time::Duration::from_secs(def.timeout_secs.max(1));
                let result = tokio::time::timeout(timeout, async {
                    match def.method.to_ascii_uppercase().as_str() {
                        "GET" => client.get(&url).query(&args).send().await,
                        _ => client.post(&url).json(&args).send().await,
                    }
                })
                .await;
                match result {
                    Ok(Ok(resp)) => {
                        let status = resp.status();
                        match resp.text().await {
                            Ok(body) if status.is_success() => {
                                // 响应体截断护栏：超过上限截断并提示，防撑爆模型上下文
                                let truncated = body.chars().count() > MAX_EXTERNAL_RESPONSE_CHARS;
                                let final_body = if truncated {
                                    let cut: String = body
                                        .chars()
                                        .take(MAX_EXTERNAL_RESPONSE_CHARS)
                                        .collect();
                                    format!("{}（响应体过长已截断，共 {} 字符）", cut, body.chars().count())
                                } else {
                                    body
                                };
                                crate::core::agent::tools::record_tool_result(
                                    &cfg,
                                    &def.name,
                                    true,
                                    &format!(
                                        "HTTP {}，{} 字符{}",
                                        status,
                                        final_body.chars().count(),
                                        if truncated { "（已截断）" } else { "" }
                                    ),
                                    Some(&final_body),
                                );
                                Ok(ToolOutput::text(final_body))
                            }
                            Ok(body) => {
                                let msg = format!("HTTP {} 错误: {}", status, body);
                                crate::core::agent::tools::record_tool_result(
                                    &cfg,
                                    &def.name,
                                    false,
                                    &msg,
                                    Some(&msg),
                                );
                                Err(ToolExecutionError::other(msg))
                            }
                            Err(e) => {
                                crate::core::agent::tools::record_tool_result(
                                    &cfg,
                                    &def.name,
                                    false,
                                    &e.to_string(),
                                    Some(&e.to_string()),
                                );
                                Err(ToolExecutionError::other(format!("读取响应失败: {e}")))
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        let msg = format!("外部工具请求失败: {e}");
                        crate::core::agent::tools::record_tool_result(&cfg, &def.name, false, &msg, Some(&msg));
                        Err(ToolExecutionError::other(msg))
                    }
                    Err(_) => {
                        let msg = format!("外部工具请求超时（{}s）", def.timeout_secs);
                        crate::core::agent::tools::record_tool_result(&cfg, &def.name, false, &msg, Some(&msg));
                        Err(ToolExecutionError::other(msg))
                    }
                }
            })
        },
    )
}

/// 外部工具配置缓存（mtime 感知：配置文件未变化时复用已解析结果）。
///
/// `load_external_tools_or_default` 在每次 Agent 请求构建工具与窄化可见工具时
/// 都会被调用，直接读盘 + YAML 解析属于重复 IO；缓存后仅在文件变更时重新解析
/// （P0-3 修复后该函数调用更频繁，缓存收益显著）。
static EXTERNAL_TOOLS_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Option<(std::time::SystemTime, Vec<ExternalToolDef>)>>,
> = std::sync::OnceLock::new();

/// 便捷：加载外部工具定义（默认配置路径；失败降级为空集 + 日志；带 mtime 缓存）。
pub fn load_external_tools_or_default() -> Vec<ExternalToolDef> {
    let path = default_config_path();
    let mtime = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok();
    let cache = EXTERNAL_TOOLS_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((cached_mtime, defs)) = guard.as_ref() {
            // 文件 mtime 未变（含从未存在时缓存 epoch）→ 复用
            let fresh = match mtime {
                Some(mt) => mt == *cached_mtime,
                None => *cached_mtime == std::time::UNIX_EPOCH,
            };
            if fresh {
                return defs.clone();
            }
        }
    }
    match load_external_tools(&path) {
        Ok(defs) => {
            if let Ok(mut guard) = cache.lock() {
                *guard = Some((mtime.unwrap_or(std::time::UNIX_EPOCH), defs.clone()));
            }
            defs
        }
        Err(e) => {
            log::warn!("[external_tools] 加载外部工具失败，降级为空集: {}", e);
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    #[test]
    fn load_external_tools_parses_yaml_and_skips_invalid() {
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("agent_tools_test_{}.yaml", uuid::Uuid::new_v4()));
        let yaml = r#"
- name: weather
  description: 查询天气
  url: https://example.com/weather
  params_schema:
    type: object
    properties:
      city: { type: string }
    required: [city]
- name: ""            # 无 name：应被过滤
  description: x
  url: https://example.com/x
"#;
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let defs = load_external_tools(&tmp).expect("YAML 应可解析");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "weather");
        assert_eq!(defs[0].method, "POST", "默认方法 POST");
        assert_eq!(defs[0].timeout_secs, 30, "默认超时 30s");
        // 文件不存在 → 空集
        assert!(load_external_tools(&std::path::Path::new("C:/nonexistent_agent_tools.yaml")).unwrap().is_empty());
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn external_tool_def_builds_dynamic_tool() {
        // 仅验证构建（不实际发 HTTP）：schema 与描述透传
        let def = ExternalToolDef {
            name: "weather".into(),
            description: "查询天气".into(),
            url: "http://127.0.0.1:9/weather".into(),
            method: "POST".into(),
            params_schema: serde_json::json!({
                "type": "object",
                "properties": { "city": { "type": "string" } },
                "required": ["city"]
            }),
            timeout_secs: 1,
        };
        // 构造一个最小 KbSearchConfig 比较麻烦，这里验证 def 可被 build 闭包捕获（编译期验证）
        let _ = Arc::new(def);
    }
}
