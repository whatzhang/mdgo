//! 订阅 / 本地配置管理（下载、校验、版本、差异、定时更新）。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;

use super::config;
use super::paths;
use super::types::{Profile, ProfileVersion, ProfilesFile};

/// 下载硬限制。
const MAX_SUB_BYTES: usize = 8 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;

#[derive(Debug, Default, Clone)]
pub struct SubInfo {
    pub upload: u64,
    pub download: u64,
    pub total: u64,
    pub expire: i64,
}

impl SubInfo {
    fn parse(header: &str) -> Self {
        let mut info = SubInfo::default();
        for part in header.split(';') {
            let mut kv = part.trim().splitn(2, '=');
            let key = kv.next().unwrap_or("").trim();
            let value = kv.next().unwrap_or("").trim().parse::<u64>().unwrap_or(0);
            match key {
                "upload" => info.upload = value,
                "download" => info.download = value,
                "total" => info.total = value,
                "expire" => info.expire = value as i64,
                _ => {}
            }
        }
        info
    }
}

fn http_client(timeout_secs: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .user_agent("clash.meta")
        .build()
        .map_err(|e| e.to_string())
}

/// 下载订阅并做内容检查（大小、类型、HTML 登录页）。
pub async fn fetch_subscription(url: &str) -> Result<(String, SubInfo), String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("订阅地址必须以 http:// 或 https:// 开头".into());
    }
    let client = http_client(20)?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("订阅下载失败: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let msg = match code {
            401 | 403 => "订阅被拒绝（403/401），请检查令牌或 UA",
            404 => "订阅地址不存在（404）",
            429 => "订阅请求过于频繁（429），稍后再试",
            _ => "订阅返回异常状态码",
        };
        return Err(format!("{msg}（HTTP {code}）"));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if content_type.contains("text/html") {
        return Err("订阅返回了 HTML 页面（可能是登录页/套餐过期页）".into());
    }
    let sub_info = resp
        .headers()
        .get("subscription-userinfo")
        .and_then(|v| v.to_str().ok())
        .map(SubInfo::parse)
        .unwrap_or_default();

    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("订阅读取失败: {e}"))?;
        if body.len() + chunk.len() > MAX_SUB_BYTES {
            return Err(format!(
                "订阅内容超过上限（{} MB）",
                MAX_SUB_BYTES / 1024 / 1024
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let raw = String::from_utf8(body).map_err(|_| "订阅内容不是有效 UTF-8".to_string())?;
    // 部分机场会在 YAML 前带 UTF-8 BOM，serde_yaml 会解析失败
    let raw = raw.trim_start_matches('\u{feff}').to_string();
    let head = raw.trim_start().to_ascii_lowercase();
    if head.starts_with("<!doctype") || head.starts_with("<html") || head.contains("<html") {
        return Err("订阅返回了 HTML 页面（可能是登录页/套餐过期页）".into());
    }
    config::validate_structure(&raw)?;
    Ok((raw, sub_info))
}

/// 读取本地 YAML 文件。
pub fn read_local(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if !p.is_file() {
        return Err(format!("文件不存在: {path}"));
    }
    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    if meta.len() as usize > MAX_SUB_BYTES {
        return Err("本地配置超过 8 MB".into());
    }
    let raw = std::fs::read_to_string(p).map_err(|e| format!("读取失败: {e}"))?;
    let raw = raw.trim_start_matches('\u{feff}').to_string();
    config::validate_structure(&raw)?;
    Ok(raw)
}

pub fn load_index() -> ProfilesFile {
    match std::fs::read(paths::profiles_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => ProfilesFile::default(),
    }
}

pub fn save_index(index: &ProfilesFile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(index).map_err(|e| e.to_string())?;
    paths::atomic_write(&paths::profiles_path(), &bytes).map_err(|e| e.to_string())
}

pub fn save_raw(id: &str, raw: &str) -> Result<(), String> {
    paths::atomic_write(&paths::profile_raw(id), raw.as_bytes()).map_err(|e| e.to_string())
}

pub fn read_raw(id: &str) -> Result<String, String> {
    std::fs::read_to_string(paths::profile_raw(id))
        .map_err(|e| format!("读取配置失败: {e}"))
}

/// 更新前备份旧版本（保留最近 3 个）。
pub fn backup_version(id: &str) {
    let raw_path = paths::profile_raw(id);
    if !raw_path.is_file() {
        return;
    }
    let dir = paths::profile_versions(id);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = paths::now_secs();
    let target = dir.join(format!("{ts}.yaml"));
    let _ = std::fs::copy(&raw_path, &target);

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "yaml").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    while files.len() > 3 {
        let oldest = files.remove(0);
        let _ = std::fs::remove_file(oldest);
    }
}

pub fn versions(id: &str) -> Vec<ProfileVersion> {
    let dir = paths::profile_versions(id);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|x| x != "yaml").unwrap_or(true) {
            continue;
        }
        let ts = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let (nodes, groups, rules) = std::fs::read_to_string(&path)
            .map(|raw| config::stats(&raw))
            .unwrap_or((0, 0, 0));
        out.push(ProfileVersion {
            ts,
            nodes,
            groups,
            rules,
        });
    }
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    out
}

pub fn rollback(id: &str, ts: i64) -> Result<(), String> {
    let path = paths::profile_versions(id).join(format!("{ts}.yaml"));
    if !path.is_file() {
        return Err("该版本不存在".into());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    config::validate_structure(&raw)?;
    save_raw(id, &raw)
}

/// 更新元数据（节点数/组数/规则数/差异说明）。
pub fn apply_stats(profile: &mut Profile, raw: &str) -> String {
    let (nodes, groups, rules) = config::stats(raw);
    let note = if profile.nodes > 0 && nodes == 0 {
        format!("本次更新节点数为 0（原 {}），已保留旧配置", profile.nodes)
    } else if profile.nodes > 0 && nodes * 5 < profile.nodes {
        format!("节点数变化较大：{} → {}", profile.nodes, nodes)
    } else if profile.nodes > 0 {
        format!("节点 {} → {}", profile.nodes, nodes)
    } else {
        String::new()
    };
    profile.nodes = nodes;
    profile.groups = groups;
    profile.rules = rules;
    profile.updated_note = note.clone();
    note
}

/// 判断哪些订阅到了更新时间。
pub fn due_ids(index: &ProfilesFile, now: i64) -> Vec<String> {
    index
        .profiles
        .iter()
        .filter(|p| p.kind == "url" && p.interval_hours > 0 && !p.url.is_empty())
        .filter(|p| now - p.updated_at >= (p.interval_hours as i64) * 3600)
        // 失败退避：连续失败时不再每 5 分钟重试（5min → 30min → 2h → 24h 封顶）
        .filter(|p| {
            if p.fail_count == 0 {
                return true;
            }
            let shift = (p.fail_count.min(6) - 1) as u32;
            let backoff = (1800i64 << shift).min(86_400);
            now - p.last_attempt_at >= backoff
        })
        .map(|p| p.id.clone())
        .collect()
}

/// 生成一份稳定的短 id。
pub fn new_id() -> String {
    format!("p_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

/// 统计各订阅文件占用（供设置页展示，暂未使用）。
#[allow(dead_code)]
pub fn disk_usage() -> HashMap<String, u64> {
    let mut map = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(paths::profiles_dir()) {
        for entry in rd.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            let size = std::fs::read_dir(entry.path())
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| e.metadata().ok())
                        .map(|m| m.len())
                        .sum()
                })
                .unwrap_or(0);
            map.insert(name, size);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_subscription_userinfo() {
        let info = SubInfo::parse("upload=1024; download=2048; total=10240; expire=1760000000");
        assert_eq!(info.upload, 1024);
        assert_eq!(info.download, 2048);
        assert_eq!(info.total, 10240);
        assert_eq!(info.expire, 1760000000);
    }

    #[test]
    fn html_is_rejected_by_structure() {
        assert!(config::validate_structure("<html><body>login</body></html>").is_err());
    }

    #[test]
    fn due_ids_only_url_profiles() {
        let index = ProfilesFile {
            active_id: "a".into(),
            profiles: vec![
                Profile {
                    id: "a".into(),
                    name: "a".into(),
                    kind: "url".into(),
                    url: "https://x".into(),
                    interval_hours: 1,
                    updated_at: 0,
                    ..empty_profile("a")
                },
                Profile {
                    id: "b".into(),
                    name: "b".into(),
                    kind: "file".into(),
                    path: "/tmp/x.yaml".into(),
                    interval_hours: 1,
                    updated_at: 0,
                    ..empty_profile("b")
                },
            ],
        };
        let due = due_ids(&index, 7200);
        assert_eq!(due, vec!["a".to_string()]);
    }

    #[test]
    fn failed_updates_back_off() {
        let mut p = Profile {
            id: "p_x".into(),
            name: "x".into(),
            kind: "url".into(),
            url: "https://x".into(),
            interval_hours: 1,
            updated_at: 0,
            last_attempt_at: 100_000,
            fail_count: 1,
            ..empty_profile("p_x")
        };
        let index = ProfilesFile {
            active_id: "p_x".into(),
            profiles: vec![p.clone()],
        };
        // 更新周期已到，但首次失败后 5 分钟内不再重试
        assert!(due_ids(&index, 100_000 + 1799).is_empty());
        assert_eq!(due_ids(&index, 100_000 + 1800).len(), 1);
        // 连续失败按指数退避：fail_count=3 → 2 小时
        p.fail_count = 3;
        p.last_attempt_at = 200_000;
        let index = ProfilesFile {
            active_id: "p_x".into(),
            profiles: vec![p],
        };
        assert!(due_ids(&index, 200_000 + 3600).is_empty());
        assert_eq!(due_ids(&index, 200_000 + 7200).len(), 1);
    }

    fn empty_profile(id: &str) -> Profile {
        Profile {
            id: id.to_string(),
            name: String::new(),
            kind: String::new(),
            url: String::new(),
            path: String::new(),
            interval_hours: 0,
            updated_at: 0,
            nodes: 0,
            groups: 0,
            rules: 0,
            upload: 0,
            download: 0,
            total: 0,
            expire_at: 0,
            last_error: String::new(),
            updated_note: String::new(),
            version_count: 0,
            last_attempt_at: 0,
            fail_count: 0,
        }
    }
}
