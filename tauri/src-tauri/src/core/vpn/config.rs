//! mihomo 运行配置生成与校验。
//!
//! 设计原则（评审 P0）：**原始订阅 + 最小受管覆盖**。
//! 只强制覆盖端口/控制器/secret/监听地址/模式/日志级别，删除 `tun`、`listeners`，
//! 其余字段（包括 `dns`、`proxy-providers`、`rule-providers`、`sniffer` 等）原样透传。

use std::path::Path;

use serde_yaml::{Mapping, Value};

use super::types::{Ports, Settings};


fn key(name: &str) -> Value {
    Value::String(name.to_string())
}

fn insert_str(map: &mut Mapping, name: &str, value: &str) {
    map.insert(key(name), Value::String(value.to_string()));
}

fn insert_bool(map: &mut Mapping, name: &str, value: bool) {
    map.insert(key(name), Value::Bool(value));
}

/// 从原始订阅生成运行配置。
pub fn build_runtime(raw: &str, settings: &Settings, ports: &Ports) -> Result<String, String> {
    let mut value: Value = serde_yaml::from_str(raw).map_err(|e| format!("YAML 解析失败: {e}"))?;
    let map = value
        .as_mapping_mut()
        .ok_or_else(|| "配置文件顶层必须是 YAML 映射".to_string())?;

    // ── 受管字段：强制覆盖 ──
    map.insert(key("mixed-port"), Value::Number(ports.mixed.into()));
    insert_str(
        map,
        "external-controller",
        &format!("127.0.0.1:{}", ports.controller),
    );
    insert_str(map, "secret", &settings.secret);
    insert_str(map, "bind-address", "127.0.0.1");
    insert_bool(map, "allow-lan", false);
    insert_str(map, "mode", normalize_mode(&settings.mode));
    insert_str(map, "log-level", normalize_log_level(&settings.log_level));
    insert_str(
        map,
        "find-process-mode",
        if settings.find_process { "always" } else { "off" },
    );

    // ── 安全策略：明确不做 TUN / 不开放订阅里的额外入站端口 ──
    let mut removed = Vec::new();
    for name in [
        "tun",
        "listeners",
        "port",
        "socks-port",
        "redir-port",
        "tproxy-port",
        "mixed-port-alt",
    ] {
        if map.remove(key(name)).is_some() {
            removed.push(name);
        }
    }
    if !removed.is_empty() {
        log::info!("[vpn] 已按安全策略移除配置块: {}", removed.join(", "));
    }

    // ── profile 段：只补 store-selected，不覆盖其它 ──
    let profile_entry = map
        .entry(key("profile"))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if let Some(profile_map) = profile_entry.as_mapping_mut() {
        profile_map.insert(key("store-selected"), Value::Bool(true));
        profile_map
            .entry(key("store-fake-ip"))
            .or_insert(Value::Bool(true));
    }

    // ── DNS：默认完全继承；仅当用户显式覆盖时动它 ──
    if let Some(enable) = settings.dns_enable {
        let dns_entry = map
            .entry(key("dns"))
            .or_insert_with(|| Value::Mapping(Mapping::new()));
        if let Some(dns_map) = dns_entry.as_mapping_mut() {
            insert_bool(dns_map, "enable", enable);
        }
    }
    if settings.dns_override && !settings.dns_nameservers.is_empty() {
        let dns_entry = map
            .entry(key("dns"))
            .or_insert_with(|| Value::Mapping(Mapping::new()));
        if let Some(dns_map) = dns_entry.as_mapping_mut() {
            let list: Vec<Value> = settings
                .dns_nameservers
                .iter()
                .map(|s| Value::String(s.trim().to_string()))
                .filter(|v| v.as_str().map(|s| !s.is_empty()).unwrap_or(false))
                .collect();
            dns_map.insert(key("nameserver"), Value::Sequence(list));
        }
    }

    serde_yaml::to_string(&value).map_err(|e| format!("配置序列化失败: {e}"))
}

pub fn normalize_mode(mode: &str) -> &'static str {
    match mode {
        "global" => "global",
        "direct" => "direct",
        _ => "rule",
    }
}

pub fn normalize_log_level(level: &str) -> &'static str {
    match level {
        "debug" => "debug",
        "warning" | "warn" => "warning",
        "error" => "error",
        "silent" => "silent",
        _ => "info",
    }
}

/// 统计节点 / 策略组 / 规则数量（用于订阅差异检测）。
pub fn stats(raw: &str) -> (u32, u32, u32) {
    let Ok(value) = serde_yaml::from_str::<Value>(raw) else {
        return (0, 0, 0);
    };
    let count = |name: &str| -> u32 {
        value
            .get(name)
            .and_then(|v| v.as_sequence())
            .map(|s| s.len() as u32)
            .unwrap_or(0)
    };
    (count("proxies"), count("proxy-groups"), count("rules"))
}

/// 结构校验：至少包含 proxies / proxy-groups / rules 之一。
pub fn validate_structure(raw: &str) -> Result<(), String> {
    let value: Value = serde_yaml::from_str(raw).map_err(|e| format!("YAML 解析失败: {e}"))?;
    if !value.is_mapping() {
        return Err("配置文件顶层必须是 YAML 映射".into());
    }
    let has = |name: &str| {
        value
            .get(name)
            .and_then(|v| v.as_sequence())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    };
    if has("proxies") || has("proxy-groups") || has("rules") {
        Ok(())
    } else {
        Err("配置缺少 proxies / proxy-groups / rules".into())
    }
}


/// 调用 `mihomo -t` 做配置测试。
///
/// 返回 `Ok(None)` 表示通过；`Ok(Some(stderr))` 表示失败并带摘要。
pub async fn config_test(
    bin: &Path,
    config: &Path,
    cache: &Path,
) -> Result<Option<String>, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("-t")
        .arg("-f")
        .arg(config)
        .arg("-d")
        .arg(cache)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let fut = async {
        let output = cmd.output().await.map_err(|e| format!("无法执行内核: {e}"))?;
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if output.status.success() {
            Ok(None)
        } else {
            let mut detail = if stderr.trim().is_empty() { stdout } else { stderr };
            // 按字符边界截断（直接 truncate 可能切在 UTF-8 中间导致 panic）
            if detail.chars().count() > 4096 {
                detail = detail.chars().take(4096).collect::<String>();
                detail.push('…');
            }
            Ok(Some(detail))
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(15), fut).await {
        Ok(res) => res,
        Err(_) => Err("配置测试超时（15s）".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = r#"
proxies:
  - {name: a, type: ss, server: 1.2.3.4, port: 443, cipher: aes-128-gcm, password: x}
proxy-groups:
  - {name: sel, type: select, proxies: [a]}
rules:
  - MATCH,sel
"#;

    #[test]
    fn managed_fields_are_forced() {
        let settings = Settings {
            secret: "s3cr3t".into(),
            mode: "global".into(),
            ..Default::default()
        };
        let out = build_runtime(RAW, &settings, &Ports { mixed: 7891, controller: 9091 }).unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(v.get("mixed-port").and_then(|x| x.as_u64()), Some(7891));
        assert_eq!(
            v.get("external-controller").and_then(|x| x.as_str()),
            Some("127.0.0.1:9091")
        );
        assert_eq!(v.get("secret").and_then(|x| x.as_str()), Some("s3cr3t"));
        assert_eq!(v.get("mode").and_then(|x| x.as_str()), Some("global"));
        assert_eq!(v.get("allow-lan").and_then(|x| x.as_bool()), Some(false));
        assert_eq!(
            v.get("bind-address").and_then(|x| x.as_str()),
            Some("127.0.0.1")
        );
        assert_eq!(
            v.get("find-process-mode").and_then(|x| x.as_str()),
            Some("off")
        );
    }

    #[test]
    fn tun_and_listeners_are_stripped() {
        let raw = r#"
proxies: []
tun: {enable: true, stack: gvisor}
listeners:
  - {name: in, type: socks, port: 1080}
port: 7891
socks-port: 7892
redir-port: 7893
rules: []
"#;
        let out = build_runtime(raw, &Settings::default(), &Ports::default()).unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert!(v.get("tun").is_none());
        assert!(v.get("listeners").is_none());
        // 订阅里的额外入站端口不保留，只暴露 mixed-port
        assert!(v.get("port").is_none());
        assert!(v.get("socks-port").is_none());
        assert!(v.get("redir-port").is_none());
        assert_eq!(v.get("mixed-port").and_then(|x| x.as_u64()), Some(7890));
    }

    #[test]
    fn dns_is_preserved_by_default() {
        let raw = r#"
proxies: []
rules: []
dns: {enable: true, enhanced-mode: fake-ip, nameserver: [https://1.1.1.1/dns-query]}
"#;
        let out = build_runtime(raw, &Settings::default(), &Ports::default()).unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        let dns = v.get("dns").expect("dns 段应保留");
        assert_eq!(
            dns.get("enhanced-mode").and_then(|x| x.as_str()),
            Some("fake-ip")
        );
        assert_eq!(
            dns.get("nameserver")
                .and_then(|x| x.as_sequence())
                .and_then(|s| s.first())
                .and_then(|x| x.as_str()),
            Some("https://1.1.1.1/dns-query")
        );
    }

    #[test]
    fn dns_override_replaces_nameserver() {
        let settings = Settings {
            dns_override: true,
            dns_nameservers: vec!["https://223.5.5.5/dns-query".into()],
            ..Default::default()
        };
        let out = build_runtime(RAW, &settings, &Ports::default()).unwrap();
        let v: Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(
            v.get("dns")
                .and_then(|d| d.get("nameserver"))
                .and_then(|n| n.as_sequence())
                .and_then(|s| s.first())
                .and_then(|x| x.as_str()),
            Some("https://223.5.5.5/dns-query")
        );
    }

    #[test]
    fn stats_counts_entries() {
        assert_eq!(stats(RAW), (1, 1, 1));
        assert_eq!(stats("not: yaml: ]"), (0, 0, 0));
    }

    #[test]
    fn structure_validation() {
        assert!(validate_structure(RAW).is_ok());
        assert!(validate_structure("foo: bar").is_err());
        assert!(validate_structure("proxies: []").is_err());
    }
}
