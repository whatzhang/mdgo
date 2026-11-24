//! 进程级共享 HTTP 客户端（连接池复用）。
//!
//! 背景：v3 内核里适配器**每次请求**都 `reqwest::Client::builder().build()` 新建客户端
//! （`core/loop/openai.rs`、`services/anthropic.rs`），连接池随对象丢弃，于是每个 step
//! 都要重做 DNS/TCP/TLS 握手（公网端点 50–200ms/次，多步任务累计可达数秒）。
//!
//! 这里收敛为进程内单例：连接池、HTTP keep-alive、TLS session 全进程复用。
//! 单次请求的总超时（流式 30 分钟兜底、非流式 600s）仍由调用方用
//! `RequestBuilder::timeout()` **逐请求**设置——不把超时烘进客户端，避免"一个档位
//! 一个客户端"又把池子拆散。
//!
//! 注意：本模块只提供"传输基础设施"，不承载任何协议/重试语义（策略仍在适配器与
//! `services/llm.rs::retry_loop`）。

use std::sync::OnceLock;
use std::time::Duration;

/// 建连超时：连接阶段快速失败（读超时由各调用方逐请求设置）。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 空闲连接保持时间：跨 step / 跨轮复用同一连接（默认 90s 足够覆盖一次多步任务）。
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// TCP keep-alive 探测间隔（长时间流式读时保活，避免中间设备静默断链）。
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

fn build() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .build()
        .expect("共享 reqwest client 构建失败")
}

/// 取共享客户端（`Client` 内部是 `Arc`，clone 极廉价）。
///
/// 调用方约定：流式/非流式请求各自在 `RequestBuilder` 上设置 `.timeout(...)`。
pub fn shared() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(build).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_client_builds_request_with_per_request_timeout() {
        // 冒烟：共享客户端可复用，且"逐请求超时"能挂在 RequestBuilder 上（不烘进客户端）。
        let client = shared();
        let request = client
            .get("http://example.invalid/chat/completions")
            .timeout(Duration::from_millis(5))
            .build();
        assert!(request.is_ok(), "共享 client 应能构建请求: {:?}", request.err());
    }
}
