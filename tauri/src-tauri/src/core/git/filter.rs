//! Agent 侧严格过滤：`.mdgo` 内部数据与敏感文件。
//!
//! 严格模式（D3=B）：
//! - `.mdgo` 任意层级：路径、统计、内容都不进入 Agent 输出；
//! - 敏感文件：文件名、统计、内容都不进入 Agent 输出；
//! - 显式 path 指向以上两类：直接报错，不降级为“隐藏”。
//!
//! UI 侧（commands）不走这里，用户仍能在应用里看到自己的文件。

/// 隐藏原因（用于计数与提示，不暴露路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenKind {
    Mdgo,
    Sensitive,
}

/// 路径是否位于 `.mdgo` 内部（任一路径组件等于 `.mdgo`，忽略大小写）。
pub fn is_mdgo_internal(path: &str) -> bool {
    path.replace('\\', "/")
        .split('/')
        .any(|c| c.eq_ignore_ascii_case(".mdgo"))
}

/// 路径是否为敏感文件（严格模式：保守名单，命中即完全隐藏）。
pub fn is_sensitive_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.is_empty() {
        return false;
    }

    // 凭据/密钥目录：整个目录都按敏感处理
    const SENSITIVE_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker"];
    if parts.iter().any(|c| SENSITIVE_DIRS.contains(c)) {
        return true;
    }

    let name = parts.last().copied().unwrap_or("");
    if name.is_empty() {
        return false;
    }

    // .env / .env.*（严格模式包含 .env.example）
    if name == ".env" || name.starts_with(".env.") {
        return true;
    }

    // 常见凭据文件名
    const SENSITIVE_NAMES: &[&str] = &[
        ".npmrc",
        ".yarnrc",
        ".netrc",
        "_netrc",
        ".git-credentials",
        ".pgpass",
        ".htpasswd",
        ".dockercfg",
        "credentials",
        "secrets",
        "auth.json",
        "kubeconfig",
    ];
    if SENSITIVE_NAMES.contains(&name) {
        return true;
    }
    if name.starts_with("credentials.") || name.starts_with("secrets.") {
        return true;
    }
    if name.starts_with("service-account") && name.ends_with(".json") {
        return true;
    }

    // SSH 私钥族
    const KEY_PREFIXES: &[&str] = &[
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        "id_ecdsa_sk",
        "id_ed25519_sk",
    ];
    if KEY_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }

    // 证书/密钥/加密容器/基础设施密钥
    let ext = name.rsplit('.').next().unwrap_or("");
    if matches!(
        ext,
        "pem"
            | "key"
            | "p12"
            | "pfx"
            | "jks"
            | "keystore"
            | "ppk"
            | "ovpn"
            | "kdbx"
            | "age"
            | "gpg"
            | "tfvars"
            | "tfstate"
            | "mobileconfig"
    ) {
        return true;
    }
    if name.contains(".tfstate.") {
        return true;
    }

    false
}

/// 生成物/依赖目录：内容通常不值得送模型（但文件名/统计可保留）。
pub fn is_generated_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let p = normalized.trim_start_matches("./");
    if p.starts_with("dist/")
        || p.starts_with("node_modules/")
        || p.starts_with("target/")
        || p.starts_with("build/")
        || p.starts_with(".next/")
        || p.starts_with(".cache/")
        || p.starts_with("out/")
    {
        return true;
    }
    let name = p.rsplit('/').next().unwrap_or(p);
    name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".map")
        || name.ends_with(".lock")
        || name.ends_with("-lock.json")
        || name == "package-lock.json"
        || name == "pnpm-lock.yaml"
        || name == "yarn.lock"
}

/// 严格模式下的隐藏判定（不含作用域外）。
pub fn hidden_kind(path: &str) -> Option<HiddenKind> {
    if is_mdgo_internal(path) {
        Some(HiddenKind::Mdgo)
    } else if is_sensitive_path(path) {
        Some(HiddenKind::Sensitive)
    } else {
        None
    }
}

/// 多个路径（如重命名的 old/new）任一命中隐藏规则即返回原因。
pub fn hidden_kind_any(paths: &[&str]) -> Option<HiddenKind> {
    paths.iter().find_map(|p| hidden_kind(p))
}

/// 路径是否可作为 Agent 的显式 `path` 参数：命中隐藏规则直接报错。
pub fn ensure_agent_readable(path: &str) -> Result<(), String> {
    if is_mdgo_internal(path) {
        return Err(format!(
            "`{path}` 位于 .mdgo 内部数据目录，严格模式下不允许读取。"
        ));
    }
    if is_sensitive_path(path) {
        return Err(format!(
            "`{path}` 属于敏感文件（密钥/凭据/环境变量等），严格模式下不允许读取。"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdgo_matches_any_component_case_insensitive() {
        assert!(is_mdgo_internal(".mdgo/config.yaml"));
        assert!(is_mdgo_internal("a/.MDGO/skills/x.md"));
        assert!(is_mdgo_internal("a\\b\\.mdgo"));
        assert!(!is_mdgo_internal("docs/mdgo.md"));
        assert!(!is_mdgo_internal(".mdgo-notes/x.md"));
    }

    #[test]
    fn sensitive_env_is_strict_including_example() {
        assert!(is_sensitive_path(".env"));
        assert!(is_sensitive_path("config/.env"));
        assert!(is_sensitive_path(".env.example"));
        assert!(is_sensitive_path("deploy/.env.production"));
        assert!(!is_sensitive_path("docs/environment.md"));
        assert!(!is_sensitive_path("src/env.rs"));
    }

    #[test]
    fn sensitive_keys_and_credentials() {
        assert!(is_sensitive_path("keys/server.pem"));
        assert!(is_sensitive_path("certs/private.key"));
        assert!(is_sensitive_path("id_rsa"));
        assert!(is_sensitive_path(".ssh/id_ed25519"));
        assert!(is_sensitive_path("credentials.json"));
        assert!(is_sensitive_path("infra/prod.tfvars"));
        assert!(is_sensitive_path("terraform.tfstate.backup"));
        assert!(!is_sensitive_path("src/keyboard.rs"));
        assert!(!is_sensitive_path("docs/license.key.md"));
    }

    #[test]
    fn generated_paths_are_content_skipped() {
        assert!(is_generated_path("node_modules/pkg/index.js"));
        assert!(is_generated_path("dist/app.min.js"));
        assert!(is_generated_path("Cargo.lock"));
        assert!(is_generated_path("pnpm-lock.yaml"));
        assert!(!is_generated_path("src/lock.rs"));
    }

    #[test]
    fn explicit_path_guard_rejects_hidden() {
        assert!(ensure_agent_readable(".env").is_err());
        assert!(ensure_agent_readable(".mdgo/config.yaml").is_err());
        assert!(ensure_agent_readable("src/main.rs").is_ok());
    }
}
