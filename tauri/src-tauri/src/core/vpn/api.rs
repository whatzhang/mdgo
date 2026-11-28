//! mihomo RESTful API 客户端（仅 Rust 侧使用，secret 不下发前端）。

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Deserializer};
use serde_json::json;

use super::types::{
    ConnectionItem, ConnectionsSnapshot, DelayResult, Ports, ProxyGroup, ProxyNode, RuleItem,
};

/// mihomo 在集合为空时可能返回 `null` 而不是 `[]`（实测 `/connections`），
/// 统一按默认值处理，避免整页解析失败。
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    /// 长连接专用：不设总超时（/traffic、/logs 会持续不断读到数据）。
    stream_http: reqwest::Client,
    base: String,
    secret: String,
}

#[derive(Debug, Deserialize)]
struct VersionResp {
    version: String,
}

#[derive(Debug, Deserialize, Default)]
struct ProxyEntry {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default, deserialize_with = "null_to_default")]
    all: Vec<String>,
    #[serde(default)]
    now: String,
    #[serde(default, deserialize_with = "null_to_default")]
    history: Vec<HistoryItem>,
}

#[derive(Debug, Deserialize, Default)]
struct HistoryItem {
    #[serde(default)]
    delay: u64,
}

#[derive(Debug, Deserialize)]
struct ProxiesResp {
    #[serde(default, deserialize_with = "null_to_default")]
    proxies: HashMap<String, ProxyEntry>,
}

#[derive(Debug, Deserialize)]
struct RulesResp {
    #[serde(default, deserialize_with = "null_to_default")]
    rules: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    proxy: String,
}

#[derive(Debug, Deserialize)]
struct ConnResp {
    #[serde(default, deserialize_with = "null_to_default")]
    connections: Vec<RawConn>,
    #[serde(default, rename = "uploadTotal")]
    upload_total: u64,
    #[serde(default, rename = "downloadTotal")]
    download_total: u64,
}

#[derive(Debug, Deserialize, Default)]
struct RawConn {
    #[serde(default)]
    id: String,
    #[serde(default)]
    metadata: ConnMeta,
    #[serde(default)]
    chains: Vec<String>,
    #[serde(default)]
    rule: String,
    #[serde(default)]
    upload: u64,
    #[serde(default)]
    download: u64,
    #[serde(default)]
    start: String,
}

#[derive(Debug, Deserialize, Default)]
struct ConnMeta {
    #[serde(default)]
    host: String,
    #[serde(default, rename = "destinationIP")]
    destination_ip: String,
    #[serde(default, rename = "destinationPort")]
    destination_port: String,
    #[serde(default)]
    network: String,
    #[serde(default)]
    process: String,
}

impl ApiClient {
    pub fn new(ports: &Ports, secret: &str, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let stream_http = reqwest::Client::builder()
            .connect_timeout(timeout.min(Duration::from_secs(5)))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            stream_http,
            base: format!("http://127.0.0.1:{}", ports.controller),
            secret: secret.to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, self.url(path));
        if !self.secret.is_empty() {
            rb = rb.bearer_auth(&self.secret);
        }
        rb
    }

    /// 长连接请求：使用无总超时的 client，避免 `/traffic`、`/logs` 被
    /// 普通 HTTP 超时周期性掐断。
    fn stream_req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self.stream_http.request(method, self.url(path));
        if !self.secret.is_empty() {
            rb = rb.bearer_auth(&self.secret);
        }
        rb
    }


    pub async fn version(&self) -> Result<String, String> {
        let resp = self
            .req(reqwest::Method::GET, "/version")
            .send()
            .await
            .map_err(|e| format!("内核 API 不可用: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("内核 API 返回 {}", resp.status()));
        }
        let body: VersionResp = resp.json().await.map_err(|e| e.to_string())?;
        Ok(body.version)
    }

    pub async fn proxies(&self) -> Result<Vec<ProxyGroup>, String> {
        let resp = self
            .req(reqwest::Method::GET, "/proxies")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("读取代理列表失败: {}", resp.status()));
        }
        let body: ProxiesResp = resp.json().await.map_err(|e| e.to_string())?;
        let proxies = body.proxies;

        let mut groups: Vec<ProxyGroup> = Vec::new();
        for (name, entry) in proxies.iter() {
            if !is_group(&entry.kind) || entry.all.is_empty() {
                continue;
            }
            let mut nodes = Vec::with_capacity(entry.all.len());
            for node_name in &entry.all {
                let (kind, delay) = proxies
                    .get(node_name)
                    .map(|p| (p.kind.clone(), p.history.last().map(|h| h.delay)))
                    .unwrap_or_else(|| ("unknown".into(), None));
                nodes.push(ProxyNode {
                    name: node_name.clone(),
                    kind,
                    delay,
                    delay_state: delay_state(delay),
                    tested_at: None,
                });
            }
            groups.push(ProxyGroup {
                name: name.clone(),
                kind: entry.kind.clone(),
                now: entry.now.clone(),
                nodes,
            });
        }
        // 稳定排序：select 优先，其余按名称
        groups.sort_by(|a, b| {
            let rank = |k: &str| match k {
                "Selector" => 0,
                "URLTest" => 1,
                "Fallback" => 2,
                _ => 3,
            };
            rank(&a.kind).cmp(&rank(&b.kind)).then(a.name.cmp(&b.name))
        });
        Ok(groups)
    }

    pub async fn select(&self, group: &str, name: &str) -> Result<(), String> {
        let path = format!("/proxies/{}", urlencode(group));
        let resp = self
            .req(reqwest::Method::PUT, &path)
            .json(&json!({ "name": name }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("切换节点失败: {}", resp.status()))
        }
    }

    pub async fn delay(&self, name: &str, url: &str, timeout_ms: u64) -> DelayResult {
        let path = format!(
            "/proxies/{}/delay?url={}&timeout={}",
            urlencode(name),
            urlencode(url),
            timeout_ms
        );
        match self
            .req(reqwest::Method::GET, &path)
            .timeout(Duration::from_millis(timeout_ms + 2000))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => match resp.json::<HashMap<String, u64>>().await
            {
                Ok(map) => {
                    let delay = map.get("delay").copied().unwrap_or(0);
                    DelayResult {
                        ok: delay > 0,
                        delay_ms: if delay > 0 { Some(delay) } else { None },
                        message: if delay > 0 {
                            String::new()
                        } else {
                            "超时".into()
                        },
                    }
                }
                Err(e) => DelayResult {
                    ok: false,
                    delay_ms: None,
                    message: e.to_string(),
                },
            },
            Ok(resp) => DelayResult {
                ok: false,
                delay_ms: None,
                message: format!("HTTP {}", resp.status()),
            },
            Err(e) => DelayResult {
                ok: false,
                delay_ms: None,
                message: e.to_string(),
            },
        }
    }

    pub async fn group_delay(
        &self,
        group: &str,
        url: &str,
        timeout_ms: u64,
    ) -> Result<HashMap<String, u64>, String> {
        let path = format!(
            "/group/{}/delay?url={}&timeout={}",
            urlencode(group),
            urlencode(url),
            timeout_ms
        );
        let resp = self
            .req(reqwest::Method::GET, &path)
            .timeout(Duration::from_millis(timeout_ms + 3000))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("整组测速失败: {}", resp.status()));
        }
        resp.json::<HashMap<String, u64>>()
            .await
            .map_err(|e| e.to_string())
    }


    pub async fn set_mode(&self, mode: &str) -> Result<(), String> {
        let resp = self
            .req(reqwest::Method::PATCH, "/configs")
            .json(&json!({ "mode": mode }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("切换模式失败: {}", resp.status()))
        }
    }

    pub async fn reload_config(&self) -> Result<(), String> {
        let resp = self
            .req(reqwest::Method::PUT, "/configs?force=true")
            .json(&json!({ "path": "" }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("重载配置失败: HTTP {}", resp.status()))
        }
    }

    pub async fn rules(&self) -> Result<Vec<RuleItem>, String> {
        let resp = self
            .req(reqwest::Method::GET, "/rules")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let body: RulesResp = resp.json().await.map_err(|e| e.to_string())?;
        let total = body.rules.len();
        Ok(body
            .rules
            .into_iter()
            .enumerate()
            .map(|(i, r)| RuleItem {
                index: i + 1,
                is_final: r.kind.eq_ignore_ascii_case("match") || (i + 1 == total),
                kind: r.kind,
                payload: r.payload,
                proxy: r.proxy,
                provider: String::new(),
            })
            .collect())
    }

    pub async fn connections(&self) -> Result<ConnectionsSnapshot, String> {
        let resp = self
            .req(reqwest::Method::GET, "/connections")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let body: ConnResp = resp.json().await.map_err(|e| e.to_string())?;
        let connections = body
            .connections
            .into_iter()
            .map(|c| {
                let host = if !c.metadata.host.is_empty() {
                    if c.metadata.destination_port.is_empty() {
                        c.metadata.host.clone()
                    } else {
                        format!("{}:{}", c.metadata.host, c.metadata.destination_port)
                    }
                } else if !c.metadata.destination_ip.is_empty() {
                    format!(
                        "{}:{}",
                        c.metadata.destination_ip, c.metadata.destination_port
                    )
                } else {
                    "—".into()
                };
                ConnectionItem {
                    id: c.id,
                    host,
                    protocol: if c.metadata.network.is_empty() {
                        "TCP".into()
                    } else {
                        c.metadata.network.to_uppercase()
                    },
                    chains: c.chains,
                    process: c.metadata.process,
                    upload: c.upload,
                    download: c.download,
                    start: c.start,
                    rule: c.rule,
                }
            })
            .collect();
        Ok(ConnectionsSnapshot {
            connections,
            upload: body.upload_total,
            download: body.download_total,
        })
    }

    pub async fn kill_connection(&self, id: &str) -> Result<(), String> {
        let resp = self
            .req(
                reqwest::Method::DELETE,
                &format!("/connections/{}", urlencode(id)),
            )
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("断开连接失败: {}", resp.status()))
        }
    }

    pub async fn kill_all(&self) -> Result<(), String> {
        let resp = self
            .req(reqwest::Method::DELETE, "/connections")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("断开全部连接失败: {}", resp.status()))
        }
    }

    /// 读取 `GET /traffic` 的 SSE 流（每行 `data: {"up":..,"down":..}`）。
    pub async fn stream_traffic(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<(u64, u64)>,
    ) -> Result<(), String> {
        let mut resp = self
            .stream_req(reqwest::Method::GET, "/traffic")
            .send()
            .await
            .map_err(|e| format!("流量流不可用: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("流量流 HTTP {}", resp.status()));
        }
        // 按字节缓冲：chunk 可能从多字节 UTF-8 字符中间切开，先拼完整行再解码
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            buf.extend_from_slice(&chunk);
            // 防御：异常流长时间无换行时不能无限增长内存
            if buf.len() > 1_048_576 {
                buf.clear();
            }
            while let Some(line) = take_line(&mut buf) {
                let data = line.strip_prefix("data:").unwrap_or(&line).trim();
                if data.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                let up = v.get("up").and_then(|x| x.as_u64()).unwrap_or(0);
                let down = v.get("down").and_then(|x| x.as_u64()).unwrap_or(0);
                if tx.send((up, down)).is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// 读取 `GET /logs?level=` 的流（JSON Lines）。
    pub async fn stream_logs(
        &self,
        level: &str,
        tx: tokio::sync::mpsc::UnboundedSender<(String, String)>,
    ) -> Result<(), String> {
        let mut resp = self
            .stream_req(
                reqwest::Method::GET,
                &format!("/logs?level={}", urlencode(level)),
            )
            .send()
            .await
            .map_err(|e| format!("日志流不可用: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("日志流 HTTP {}", resp.status()));
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
            buf.extend_from_slice(&chunk);
            // 防御：异常流长时间无换行时不能无限增长内存
            if buf.len() > 1_048_576 {
                buf.clear();
            }
            while let Some(line) = take_line(&mut buf) {
                let data = line.strip_prefix("data:").unwrap_or(&line).trim();
                if data.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                let lv = v
                    .get("type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("info")
                    .to_string();
                let payload = v
                    .get("payload")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if payload.is_empty() {
                    continue;
                }
                if tx.send((lv, payload)).is_err() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// 取出一行（以 `\n` 结尾）。
///
/// mihomo 的 `/traffic`、`/logs` 实际是 **JSON Lines**（每行一个 JSON，
/// 没有 SSE 的 `data:` 前缀与空行分隔）；这里同时兼容 SSE 风格。
fn take_line(buf: &mut Vec<u8>) -> Option<String> {
    let pos = buf.iter().position(|b| *b == b'\n')?;
    let line = buf.drain(..=pos).collect::<Vec<u8>>();
    Some(String::from_utf8_lossy(&line).trim().to_string())
}

fn is_group(kind: &str) -> bool {
    matches!(
        kind,
        "Selector" | "URLTest" | "Fallback" | "LoadBalance" | "Relay" | "Smart"
    )
}

fn delay_state(delay: Option<u64>) -> String {
    match delay {
        Some(0) => "timeout".into(),
        Some(d) if d < 150 => "ok".into(),
        Some(_) => "slow".into(),
        None => "untested".into(),
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_state_mapping() {
        assert_eq!(delay_state(Some(0)), "timeout");
        assert_eq!(delay_state(Some(88)), "ok");
        assert_eq!(delay_state(Some(400)), "slow");
        assert_eq!(delay_state(None), "untested");
    }

    #[test]
    fn group_detection() {
        assert!(is_group("Selector"));
        assert!(is_group("URLTest"));
        assert!(!is_group("Shadowsocks"));
        assert!(!is_group("Direct"));
    }

    #[test]
    fn urlencode_escapes_specials() {
        assert_eq!(urlencode("香港 01"), "%E9%A6%99%E6%B8%AF%2001");
    }

    /// mihomo 在集合为空时返回 `null`（实测 `/connections`），必须按空集合处理。
    #[test]
    fn null_collections_parse_as_empty() {
        let conns: ConnResp = serde_json::from_str(
            r#"{"connections":null,"uploadTotal":0,"downloadTotal":0,"memory":0}"#,
        )
        .expect("connections: null 应可解析");
        assert!(conns.connections.is_empty());

        let proxies: ProxiesResp =
            serde_json::from_str(r#"{"proxies":null}"#).expect("proxies: null 应可解析");
        assert!(proxies.proxies.is_empty());

        let rules: RulesResp =
            serde_json::from_str(r#"{"rules":null}"#).expect("rules: null 应可解析");
        assert!(rules.rules.is_empty());

        let entry: ProxyEntry = serde_json::from_str(
            r#"{"type":"Selector","all":null,"now":"","history":null}"#,
        )
        .expect("all/history: null 应可解析");
        assert!(entry.all.is_empty());
        assert!(entry.history.is_empty());
    }

    /// 回归：mihomo `/traffic`、`/logs` 是 JSON Lines（每行一个 JSON），
    /// 不是 SSE `data:` + 空行；同时兼容两种格式。
    #[test]
    fn take_line_accepts_json_lines_and_sse() {
        let mut buf = b"{\"up\":1,\"down\":2}\n{\"up\":3,\"down\":4}\n".to_vec();
        assert_eq!(
            take_line(&mut buf).as_deref(),
            Some("{\"up\":1,\"down\":2}")
        );
        assert_eq!(
            take_line(&mut buf).as_deref(),
            Some("{\"up\":3,\"down\":4}")
        );
        assert!(take_line(&mut buf).is_none());

        // 兼容 SSE：data: {...}\n\n
        let mut sse = b"data: {\"up\":5,\"down\":6}\n\n".to_vec();
        assert_eq!(
            take_line(&mut sse).as_deref(),
            Some("data: {\"up\":5,\"down\":6}")
        );
        assert_eq!(take_line(&mut sse).as_deref(), Some(""));
    }

    /// 真实内核回归：`/traffic` 每秒都会推一行（空闲时为 0），
    /// 解析器必须能收到事件，否则顶部速率会永远是 0、曲线是横线。
    #[tokio::test]
    #[ignore = "需要本机 mihomo 二进制"]
    async fn real_core_traffic_stream_yields_events() {
        let bin = crate::core::vpn::paths::bin_dir().join(if cfg!(windows) {
            "mihomo-windows-x64.exe"
        } else {
            "mihomo-darwin-arm64"
        });
        if !bin.is_file() {
            eprintln!("跳过：未找到 mihomo 二进制");
            return;
        }
        let dir = std::env::temp_dir().join("mdgo-vpn-traffic-test");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            "mixed-port: 17893\nexternal-controller: 127.0.0.1:19093\nproxies: []\nproxy-groups:\n  - {name: T, type: select, proxies: [DIRECT]}\nrules:\n  - MATCH,T\n",
        )
        .expect("写测试配置");
        let mut child = std::process::Command::new(&bin)
            .arg("-d")
            .arg(&dir)
            .arg("-f")
            .arg(&config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("启动测试内核");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let api = ApiClient::new(
            &Ports {
                mixed: 17893,
                controller: 19093,
            },
            "",
            Duration::from_secs(5),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stream = tokio::spawn(async move {
            let _ = api.stream_traffic(tx).await;
        });
        let got = tokio::time::timeout(Duration::from_secs(6), rx.recv()).await;
        assert!(matches!(got, Ok(Some(_))), "未收到流量事件，解析器仍不可用");
        stream.abort();
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
