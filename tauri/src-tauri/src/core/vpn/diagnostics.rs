//! 连接诊断：内核 / 端口 / 系统代理 / 当前节点 / 外网连通。

use std::time::{Duration, Instant};

use super::api::ApiClient;
use super::paths;
use super::system_proxy;
use super::types::{DiagCheck, Diagnostics, Ports, Settings};

pub struct DiagContext<'a> {
    pub api: &'a ApiClient,
    pub ports: &'a Ports,
    pub settings: &'a Settings,
    pub core_running: bool,
    pub proxy_services: &'a [String],
}

pub async fn run(ctx: DiagContext<'_>) -> Diagnostics {
    let mut checks = Vec::new();

    // ① 内核
    let core = if ctx.core_running {
        match ctx.api.version().await {
            Ok(version) => DiagCheck {
                id: "core".into(),
                name: "Mihomo 内核".into(),
                ok: true,
                message: format!("运行中 · {version}"),
                latency_ms: None,
            },
            Err(e) => DiagCheck {
                id: "core".into(),
                name: "Mihomo 内核".into(),
                ok: false,
                message: format!("进程在但 API 不可用: {e}"),
                latency_ms: None,
            },
        }
    } else {
        DiagCheck {
            id: "core".into(),
            name: "Mihomo 内核".into(),
            ok: false,
            message: "未运行".into(),
            latency_ms: None,
        }
    };
    checks.push(core);

    // ② 本地端口
    let start = Instant::now();
    let port_ok = matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(("127.0.0.1", ctx.ports.mixed))
        )
        .await,
        Ok(Ok(_))
    );
    checks.push(DiagCheck {
        id: "port".into(),
        name: "本地代理端口".into(),
        ok: port_ok,
        message: if port_ok {
            format!("127.0.0.1:{} 可连接", ctx.ports.mixed)
        } else {
            format!("127.0.0.1:{} 未监听", ctx.ports.mixed)
        },
        latency_ms: Some(start.elapsed().as_millis() as u64),
    });

    // ③ 系统代理
    let sys_ok = system_proxy::verify(ctx.ports.mixed, ctx.proxy_services)
        .await
        .unwrap_or(false);
    checks.push(DiagCheck {
        id: "sysproxy".into(),
        name: "系统代理".into(),
        ok: sys_ok,
        message: if sys_ok {
            format!("已指向 127.0.0.1:{}", ctx.ports.mixed)
        } else {
            "未开启或未指向本地端口".into()
        },
        latency_ms: None,
    });

    // ④ 当前节点
    let mut node_check = DiagCheck {
        id: "node".into(),
        name: "当前节点".into(),
        ok: false,
        message: "未选中节点".into(),
        latency_ms: None,
    };
    if ctx.core_running {
        if let Ok(groups) = ctx.api.proxies().await {
            if let Some(group) = groups.iter().find(|g| !g.now.is_empty()) {
                let name = group.now.clone();
                let result = ctx
                    .api
                    .delay(&name, &ctx.settings.test_url, ctx.settings.test_timeout_ms)
                    .await;
                node_check.ok = result.ok;
                node_check.message = if result.ok {
                    format!("{name} · {}ms", result.delay_ms.unwrap_or(0))
                } else {
                    format!("{name} 不可用（{}）", result.message)
                };
                node_check.latency_ms = result.delay_ms;
            }
        }
    }
    checks.push(node_check);

    // ⑤ 外网连通（经本地混合端口）
    let egress = egress_check(ctx.ports.mixed, &ctx.settings.test_url, ctx.settings.diag_timeout_ms).await;
    checks.push(egress);

    // 结论
    let core_ok = checks[0].ok;
    let egress_ok = checks[4].ok;
    let mut suggestions = Vec::new();
    let conclusion = if core_ok && egress_ok && checks[2].ok {
        "代理链路正常：内核、系统代理与外网连通均已通过".to_string()
    } else if core_ok && !checks[2].ok {
        suggestions.push("在「设置 → 系统代理」重新开启系统代理".into());
        "内核运行正常，但系统代理未生效：浏览器不会经过代理".to_string()
    } else if core_ok && !checks[3].ok {
        suggestions.push("在「代理」页切换其他节点后重试".into());
        "内核与系统代理正常，但当前节点不可用".to_string()
    } else if core_ok && !egress_ok {
        suggestions.push("切换节点后重试".into());
        suggestions.push("在「日志」页查看该节点的错误信息".into());
        "代理核心运行正常，但目标网络无法访问".to_string()
    } else if !core_ok {
        suggestions.push("点击顶部「开启代理」启动内核".into());
        "内核未运行，请先开启代理".to_string()
    } else {
        "部分检查未通过，请查看下方明细".to_string()
    };

    Diagnostics {
        ok: checks.iter().all(|c| c.ok),
        checks,
        conclusion,
        suggestions,
        at: paths::now_secs(),
    }
}

async fn egress_check(port: u16, url: &str, timeout_ms: u64) -> DiagCheck {
    let proxy = match reqwest::Proxy::all(format!("http://127.0.0.1:{port}")) {
        Ok(p) => p,
        Err(e) => {
            return DiagCheck {
                id: "egress".into(),
                name: "外网连通性".into(),
                ok: false,
                message: e.to_string(),
                latency_ms: None,
            }
        }
    };
    let client = match reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_millis(timeout_ms))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return DiagCheck {
                id: "egress".into(),
                name: "外网连通性".into(),
                ok: false,
                message: e.to_string(),
                latency_ms: None,
            }
        }
    };
    let start = Instant::now();
    match client.get(url).send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            DiagCheck {
                id: "egress".into(),
                name: "外网连通性".into(),
                ok: code < 400,
                message: format!("HTTP {code} · {url}"),
                latency_ms: Some(start.elapsed().as_millis() as u64),
            }
        }
        Err(e) => DiagCheck {
            id: "egress".into(),
            name: "外网连通性".into(),
            ok: false,
            message: format!("请求失败: {e}"),
            latency_ms: Some(start.elapsed().as_millis() as u64),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn egress_reports_failure_without_proxy() {
        // 端口 1 上没有代理，应快速失败且不 panic
        let check = egress_check(1, "https://www.gstatic.com/generate_204", 800).await;
        assert!(!check.ok);
    }
}
