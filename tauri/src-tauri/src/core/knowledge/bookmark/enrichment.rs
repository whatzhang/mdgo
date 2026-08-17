//! Enrichment Pipeline：消费 `bookmarks.status='pending'`，异步推进 → ready/failed。
//!
//! 流水线（无独立任务队列表，直接认领 pending 书签）：
//! ```text
//! Import → bookmarks(status=pending)
//!               │
//!   阶段A 并发抓取（64 并发；SSRF 校验 + 10s 超时 + 2MB 上限）
//!               ├─ 成功 → 写 raw_content（仅用于 LLM 总结分类标签）
//!               └─ 失败 → status=failed, dead=1（后端死链识别），终态
//!               ▼
//!   阶段B LLM 总结（summary/category/tags 一次产出）
//!               ├─ 成功 → 写 summary/category/tags
//!               └─ 失败 → status=failed（不再后续，不入向量库），终态
//!               ▼
//!   阶段C 批量 embedding → 增量 upsert LanceDB → status=ready
//! ```
//!
//! 设计原则：
//! - **单 Worker 串行 tick**（无并发 claim），崩溃遗留的 pending 会在下次启动后重新处理；
//! - **网络与索引分离**：fetch 阶段并发拉网；embedding 阶段因 `call_embedding_parallel`
//!   内部全局 Session 天然序列化，故按一批批量推理，不做无意义并发；
//! - 并发任务内**不触碰 BookmarkStore 锁跨 await**（Connection 非 Sync）；
//!   URL 读取与写库集中在主协程，网络请求任务只做纯 I/O/CPU；
//! - embedding 文本：Tags/Category 置前 + Summary 截断——BGE 模型 512 token 上限，
//!   旧实现 Title+Summary+Tags 顺序在长摘要时会把末尾的 tags 截掉，导致向量缺 tags。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use futures_util::StreamExt;
use tokio::time::sleep;

use super::BookmarkStore;
use crate::services::llm::BookmarkSummaryOut;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024; // 2MB 响应上限
/// raw_content 保存上限（正文仅用于 LLM 总结分类，无需全量存档）
const MAX_RAW_CONTENT_CHARS: usize = 20_000;
/// embedding 文本中 summary 截断长度（中文 ≈1 字符/token；512 token 预算内留足
/// Tags/Category/Title 空间，保证存储文本与模型实际嵌入内容一致——超长摘要宁可截断
/// 尾部 summary，也不能让 tags 被模型侧静默截掉）
const MAX_SUMMARY_CHARS_IN_EMBED: usize = 300;
/// 单轮抓取批上限
const FETCH_BATCH: usize = 256;
/// 单轮并发抓取数（HTTP 并发池大小）
const FETCH_CONCURRENCY: usize = 128;
/// LLM 总结并发数（网络任务并发；落库集中在主协程锁内短操作）
const SUMMARIZE_CONCURRENCY: usize = 16;
/// fetcher 连接池：每 host 最大空闲连接（提高同源书签抓取复用率）
const POOL_MAX_IDLE_PER_HOST: usize = 32;

/// 书签摘要的 LLM 提供者（由 lib.rs 用 AppState/AppHandle 实现并注入，解耦 Worker 与 tauri/配置）。
///
/// - `Ok(Some(out))`：成功产出摘要产物（summary/category/tags）
/// - `Ok(None)`：LLM 调用成功但空响应/输出不可解析（视为不可用）
/// - `Err(e)`：调用失败（未配置/网络/超时/取消等），e 可写入 `last_error` 供 UI 定位失败步骤
pub trait BookmarkSummarizer: Send + Sync {
    fn summarize(
        &self,
        title: String,
        url: String,
        content: String,
    ) -> BoxFuture<'static, Result<Option<BookmarkSummaryOut>, String>>;
}

/// Enrichment Pipeline Worker：与 AppState 共享同一 store 缓存（Arc<Mutex<HashMap>>）。
pub struct EnrichmentWorker {
    stores: Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<BookmarkStore>>>>>,
    client: reqwest::Client,
    /// 可选：LLM 摘要提供者（None = 未接入 LLM，摘要任务将按失败处理并标注步骤）
    summarizer: Option<Arc<dyn BookmarkSummarizer + 'static>>,
}

impl EnrichmentWorker {
    pub fn new(
        stores: Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::Mutex<BookmarkStore>>>>>,
        summarizer: Option<Arc<dyn BookmarkSummarizer + 'static>>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
            // 重定向由 fetch_body_with_guard 手动跟随（逐跳校验，防 SSRF 绕回内网）
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self { stores, client, summarizer }
    }

    /// 启动后台循环（setup 时调用一次）。
    /// 必须用 `tauri::async_runtime::spawn`（持有全局 runtime 句柄，setup 同步回调上下文可用）；
    /// `tokio::spawn` 在 setup 无 reactor 上下文会 panic（"there is no reactor running"）。
    ///
    /// 忙态不空转：当仍有 pending 书签时连续处理（无 500ms 轮询间隔），
    /// 仅在全部 store 均空闲时才 sleep `POLL_INTERVAL`——消除大批量导入下每批之间的固定延迟。
    pub fn spawn(self: Arc<Self>) {
        tauri::async_runtime::spawn(async move {
            loop {
                let did_work = match self.tick().await {
                    Ok(did_work) => did_work,
                    Err(e) => {
                        log::error!("[bookmark] Enrichment Pipeline 轮询失败: {}", e);
                        false
                    }
                };
                if !did_work {
                    sleep(POLL_INTERVAL).await;
                }
            }
        });
    }

    /// 单轮：遍历所有 store，逐库处理一批 pending 书签。
    /// 返回是否至少处理了 ≥1 条（供调用方决定是否空转等待）。
    async fn tick(&self) -> Result<bool, String> {
        let dirs: Vec<String> = {
            let guard = self.stores.lock().unwrap_or_else(|e| e.into_inner());
            guard.keys().cloned().collect()
        };
        let mut any = false;
        for dir in dirs {
            let store = {
                let guard = self.stores.lock().unwrap_or_else(|e| e.into_inner());
                guard.get(&dir).cloned()
            };
            let Some(store) = store else { continue };
            if self.process_store(store).await? {
                any = true;
            }
        }
        Ok(any)
    }

    /// 单库处理：认领 pending 批 → 并发抓取（死链识别）→ LLM 总结 → 批量 embedding → ready。
    /// 任一步失败即置 failed（终态），不再入向量库。
    ///
    /// 返回是否实际处理了 ≥1 条（false = 本库无 pending，调用方可据此空转等待）。
    async fn process_store(&self, store: Arc<std::sync::Mutex<BookmarkStore>>) -> Result<bool, String> {
        // 1. 认领一批 pending（单 Worker 串行，无需 RUNNING 标记）
        let jobs = {
            let s = store.lock().map_err(|e| e.to_string())?;
            s.claim_pending(FETCH_BATCH)?
        };
        if jobs.is_empty() {
            return Ok(false);
        }
        log::info!("[bookmark] 处理批 {} 条", jobs.len());

        // ─── 阶段A：并发抓取 + 死链识别 ───
        let mut fetched: Vec<(super::Bookmark, String)> = Vec::new();
        {
            let tasks: Vec<(String, String, reqwest::Client)> = jobs
                .iter()
                .map(|b| (b.id.clone(), b.url.clone(), self.client.clone()))
                .collect();
            let mut stream = futures_util::stream::iter(tasks)
                .map(|(id, url, client)| async move {
                    Self::fetch_one(&client, &url, &id).await
                })
                .buffer_unordered(FETCH_CONCURRENCY);

            // 主协程串行处理结果：写库 + 死链标记（锁内短操作）
            let mut ok = 0usize;
            let mut dead = 0usize;
            while let Some((id, _url, result)) = stream.next().await {
                match result {
                    Ok(markdown) => {
                        let truncated: String = markdown.chars().take(MAX_RAW_CONTENT_CHARS).collect();
                        let s = store.lock().map_err(|e| e.to_string())?;
                        s.update_raw_content(&id, &truncated)?;
                        // 取回书签（含最新 raw_content）供后续总结；LLM 输入用完整正文
                        if let Some(b) = s.get(&id)? {
                            fetched.push((b, markdown));
                        }
                        ok += 1;
                    }
                    Err(e) => {
                        // 抓取失败 → 死链，终态
                        let s = store.lock().map_err(|e| e.to_string())?;
                        s.mark_failed(&id, &e, true)?;
                        dead += 1;
                    }
                }
            }
            log::info!("[bookmark] 抓取完成：成功 {} / 死链 {}（本批共 {}）", ok, dead, jobs.len());
        }

        // ─── 阶段B：LLM 总结（失败直接终态，不后续） ───
        let summarized = self.summarize_batch(&store, &fetched).await?;

        // ─── 阶段C：批量 embedding → 增量写向量 → ready ───
        if summarized.is_empty() {
            return Ok(true);
        }
        let dir_path = {
            let s = store.lock().map_err(|e| e.to_string())?;
            s.dir_path().to_string()
        };
        // 1. 组装 embedding 文本（Tags/Category 置前 + Summary 截断，防 512 token 截断丢 tags）并落库
        let texts: Vec<(String, String)> = {
            let s = store.lock().map_err(|e| e.to_string())?;
            let mut out = Vec::with_capacity(summarized.len());
            for (b, bs) in &summarized {
                // 从 DB 重读最新行（阶段B 已落 summary/category/tags；阶段A 快照无 tags）
                let fresh = s.get(&b.id)?.unwrap_or_else(|| b.clone());
                let text = build_embedding_text(
                    fresh.title.as_deref().unwrap_or_default(),
                    &bs.summary,
                    fresh.tags.as_deref(),
                    bs.category.as_deref(),
                );
                s.update_embedding_text(&b.id, &text)?;
                out.push((b.id.clone(), text));
            }
            out
        };

        // 2. 批量 ONNX 推理（spawn_blocking；内部按 embedding::BATCH_SIZE 分批）
        let embeddings: Vec<Vec<f32>> = {
            let batch: Vec<String> = texts.iter().map(|(_, t)| t.clone()).collect();
            tokio::task::spawn_blocking(move || {
                let refs: Vec<&str> = batch.iter().map(|t| t.as_str()).collect();
                crate::core::db::utils::call_embedding(&refs, None)
            })
            .await
            .map_err(|e| format!("embedding 任务失败: {}", e))?
            .map_err(|e| format!("书签向量化失败: {}", e))?
        };
        log::info!("[bookmark] 向量化批推理完成：{} 条", texts.len());

        // 3. 增量 upsert → LanceDB（按 bookmark_id 覆盖；text 列存完整 embedding_text 便于核对）
        let uri = crate::core::db::utils::get_data_dir(&dir_path);
        let vec_rows: Vec<(String, String, String, Vec<f32>)> = texts
            .iter()
            .zip(embeddings.into_iter())
            .filter(|(_, emb)| !emb.is_empty())
            .map(|((id, text), emb)| (id.clone(), id.clone(), text.clone(), emb))
            .collect();
        if !vec_rows.is_empty() {
            super::vector::upsert_batch(&uri, vec_rows).await?;
        }

        // 4. 置 ready
        {
            let s = store.lock().map_err(|e| e.to_string())?;
            for (id, _) in &texts {
                s.mark_ready(id)?;
            }
        }
        log::info!("[bookmark] 批处理完成，{} 条置为 ready", texts.len());
        Ok(true)
    }

    /// 阶段B：LLM 总结（summary/category/tags 一次产出）。
    /// 成功 → 落库并返回供向量阶段使用；空响应/失败 → 直接置 failed（终态，不入向量库）。
    ///
    /// # 并发
    /// 原实现逐条串行 await——1 万条 × 单次 LLM 1-3s 远超 5 分钟目标。现改为
    /// `buffer_unordered` 并发调用（`SUMMARIZE_CONCURRENCY` 并发），与抓取阶段同理：
    /// 网络任务并发，落库集中在主协程（锁内短操作）。落库顺序与请求顺序无关。
    async fn summarize_batch(
        &self,
        store: &std::sync::Mutex<BookmarkStore>,
        fetched: &[(super::Bookmark, String)],
    ) -> Result<Vec<(super::Bookmark, BookmarkSummaryOut)>, String> {
        let mut summarized: Vec<(super::Bookmark, BookmarkSummaryOut)> = Vec::new();
        let mut ok = 0usize;
        let mut failed = 0usize;

        // 构造并发任务的 owned 数据（逐步 push，避免 collect 对 dyn trait 对象的类型归约差异）
        let mut owned: Vec<(
            super::Bookmark,
            String,
            Option<Arc<dyn BookmarkSummarizer + 'static>>,
        )> = Vec::with_capacity(fetched.len());
        {
            let summarizer = self.summarizer.clone();
            for (b, content) in fetched {
                owned.push((b.clone(), content.clone(), summarizer.clone()));
            }
        }
        // 预构建 BoxFuture 列表（类型擦除，规避 `iter().map()` 闭包参数的生命周期精化 HRTB 失败）
        let mut fut_vec: Vec<
            BoxFuture<'static, (super::Bookmark, Result<Option<BookmarkSummaryOut>, String>)>,
        > = Vec::with_capacity(owned.len());
        for (b, content, summarizer) in owned {
            fut_vec.push(
                (async move {
                    let out = match summarizer {
                        Some(provider) => provider.summarize(
                            b.title.clone().unwrap_or_default(),
                            b.url.clone(),
                            content,
                        )
                        .await,
                        None => Err("LLM 摘要未配置（未注入 summarizer）".to_string()),
                    };
                    (b, out)
                })
                .boxed(),
            );
        }
        let mut stream = futures_util::stream::iter(fut_vec).buffer_unordered(SUMMARIZE_CONCURRENCY);

        // 主协程按完成顺序逐个处理结果（落库为锁内短操作）
        while let Some((b, out)) = stream.next().await {
            match out {
                Ok(Some(bs)) => {
                    let tags_json = if bs.tags.is_empty() {
                        None
                    } else {
                        serde_json::to_string(&bs.tags).ok()
                    };
                    // 结构化输出下 category 为必填（网关强制），空串归一为 None 避免落脏数据
                    let category = bs
                        .category
                        .as_deref()
                        .filter(|c| !c.trim().is_empty())
                        .map(|c| c.to_string());
                    let s = store.lock().map_err(|e| e.to_string())?;
                    s.update_summary(
                        &b.id,
                        if bs.summary.trim().is_empty() { None::<String> } else { Some(bs.summary.clone()) },
                        category,
                        tags_json,
                    )?;
                    summarized.push((b, bs));
                    ok += 1;
                }
                Ok(None) => {
                    let s = store.lock().map_err(|e| e.to_string())?;
                    s.mark_failed(&b.id, "LLM 摘要空响应或输出不可解析", false)?;
                    failed += 1;
                }
                Err(e) => {
                    let s = store.lock().map_err(|e| e.to_string())?;
                    s.mark_failed(&b.id, &format!("LLM 摘要失败：{}", e), false)?;
                    failed += 1;
                }
            }
        }
        log::info!("[bookmark] 摘要完成：{} 成功 / {} 失败（失败见 last_error）", ok, failed);
        Ok(summarized)
    }

    /// 单条抓取：下载(限 2MB)→ spawn_blocking 解析 → 返回 markdown。
    /// 不触碰 store（Send 安全）。
    async fn fetch_one(
        client: &reqwest::Client,
        url: &str,
        bookmark_id: &str,
    ) -> (String, String, Result<String, String>) {
        let parsed = match reqwest::Url::parse(url) {
            Ok(p) => p,
            Err(e) => return (bookmark_id.to_string(), url.to_string(), Err(format!("URL 解析失败: {}", e))),
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            return (bookmark_id.to_string(), url.to_string(), Err("仅支持 http/https 抓取".to_string()));
        }
        // 抓取（含逐跳 SSRF 校验 + 手动重定向跟随，最多 3 跳；避免重定向绕回内网）
        let bytes = match Self::fetch_body_with_guard(client, &parsed).await {
            Ok(b) => b,
            Err(e) => return (bookmark_id.to_string(), url.to_string(), Err(e)),
        };
        let html = String::from_utf8_lossy(&bytes).to_string();
        // CPU 密集解析 → 阻塞线程池（不占 async runtime）
        let url_owned = parsed.clone();
        let markdown = tokio::task::spawn_blocking(move || extract_readable_markdown(&html, &url_owned))
            .await
            .unwrap_or_else(|_| String::new());
        (bookmark_id.to_string(), url.to_string(), Ok(markdown))
    }

    /// 抓取响应体，手动跟随重定向（最多 3 跳），**每一跳**都校验目标 host 是否为内网/私有，
    /// 防止「公共 URL 经重定向跳转到 192.168.x / localhost」的 SSRF 绕过。
    /// 返回非重定向响应的 body bytes（限 MAX_BODY_BYTES）。
    async fn fetch_body_with_guard(
        client: &reqwest::Client,
        start: &reqwest::Url,
    ) -> Result<Vec<u8>, String> {
        const MAX_REDIRECTS: usize = 3;
        let mut url = start.clone();
        for _ in 0..=MAX_REDIRECTS {
            // 每跳检查：发起请求前（含初始 URL）拒绝内网/私有主机
            let host = url.host_str().ok_or_else(|| "URL 缺少主机名".to_string())?;
            if is_private_host(host) {
                return Err(format!("拒绝抓取内网地址: {}", host));
            }
            if !matches!(url.scheme(), "http" | "https") {
                return Err("仅支持 http/https 抓取".to_string());
            }
            let resp = client
                .get(url.clone())
                .header("User-Agent", "mdgo-bookmark-enrich/1.0")
                .send()
                .await
                .map_err(|e| format!("抓取失败: {}", e))?;
            if resp.status().is_redirection() {
                // 手动跟随：读 Location 并解析为绝对/相对 URL
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| format!("重定向缺少 Location: HTTP {}", resp.status()))?
                    .to_string();
                url = url
                    .join(&loc)
                    .map_err(|e| format!("重定向目标解析失败: {}", e))?;
                continue;
            }
            if !resp.status().is_success() {
                return Err(format!("抓取失败 HTTP {}", resp.status()));
            }
            if let Some(len) = resp.content_length() {
                if len as usize > MAX_BODY_BYTES {
                    return Err(format!("响应过大（Content-Length {}）", len));
                }
            }
            // 读取 body（限流，防超限）
            let mut bytes = Vec::with_capacity(64 * 1024);
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| format!("读取响应失败: {}", e))?;
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_BODY_BYTES {
                    return Err(format!("响应过大（>{} bytes）", MAX_BODY_BYTES));
                }
            }
            return Ok(bytes);
        }
        Err("重定向次数过多".to_string())
    }
}

/// 组装 embedding 文本。顺序：Tags → Category → Title → Summary(截断)。
/// BGE 模型 max_position_embeddings=512 token（中文 ≈1 字符/token），
/// 把 tags/category 置前 + 截断 summary，保证标签永远进入向量（旧实现顺序相反导致 tags 被截掉）。
fn build_embedding_text(title: &str, summary: &str, tags: Option<&str>, category: Option<&str>) -> String {
    let summary: String = summary.chars().take(MAX_SUMMARY_CHARS_IN_EMBED).collect();
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = tags {
        if !t.is_empty() && t != "[]" {
            parts.push(format!("Tags: {}", t));
        }
    }
    if let Some(c) = category {
        if !c.is_empty() {
            parts.push(format!("Category: {}", c));
        }
    }
    if !title.is_empty() {
        parts.push(format!("Title: {}", title));
    }
    if !summary.trim().is_empty() {
        parts.push(format!("Summary: {}", summary));
    }
    parts.join("\n")
}

/// HTML → Markdown 正文提取（readability 去噪 → ammonia 消毒 → htmd 转换；
/// 任一环节失败降级 scraper 文本提取，保证健壮）。复制 webfetch 工具模式。
fn extract_readable_markdown(html: &str, url: &reqwest::Url) -> String {
    let mut cursor = std::io::Cursor::new(html.as_bytes());
    match readability::extractor::extract(&mut cursor, url) {
        Ok(product) => {
            let clean = ammonia::clean(&product.content);
            match htmd::convert(&clean) {
                Ok(md) if !md.trim().is_empty() => md,
                _ => extract_text_simple(html),
            }
        }
        Err(_) => extract_text_simple(html),
    }
}

/// 降级文本提取（scraper 选常见正文标签）
fn extract_text_simple(html: &str) -> String {
    let doc = scraper::Html::parse_document(html);
    if let Ok(sel) = scraper::Selector::parse("p,li,h1,h2,h3,h4,h5,pre,code,blockquote") {
        doc.select(&sel)
            .map(|n| {
                n.text()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    }
}

/// SSRF 缓解：判定主机是否属于回环 / 链路本地 / 常见私网段。
/// 基于 hostname 字符串（覆盖 localhost、IPv4 私网、IPv6 回环/链路本地、
/// 以及常见私有 TLD 后缀如 .local/.internal/.lan 等）。
fn is_private_host(host: &str) -> bool {
    let h = host.trim().to_lowercase();
    if h.is_empty() || h == "localhost" {
        return true;
    }
    // 私有 TLD / 保留后缀：.local .localhost .internal .lan .home .corp 及裸同义
    if h == "local" || h == "internal" || h == "lan" || h == "home" || h == "corp" {
        return true;
    }
    for suffix in [".local", ".localhost", ".internal", ".lan", ".home", ".corp", ".localdomain"] {
        if h.ends_with(suffix) {
            return true;
        }
    }
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_multicast()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // 链路本地 fe80::/10
            }
        };
    }
    // IPv4 私网段（host 可能是裸 IP 字符串）
    let is_ipv4_private = h.split('.').collect::<Vec<_>>().len() == 4 && {
        let parts: Vec<u32> = h
            .split('.')
            .filter_map(|p| p.parse::<u32>().ok())
            .collect();
        parts.len() == 4
            && (parts[0] == 10
                || parts[0] == 127
                || (parts[0] == 192 && parts[1] == 168)
                || (parts[0] == 172 && (16..=31).contains(&parts[1]))
                || (parts[0] == 169 && parts[1] == 254))
    };
    is_ipv4_private
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::knowledge::bookmark::{Bookmark, STATUS_PENDING};

    fn open_temp() -> (tempfile::TempDir, BookmarkStore) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let store = BookmarkStore::open_for_dir(
            dir.path().to_str().unwrap(),
            dir.path().join("test.db"),
        )
        .expect("打开测试库失败");
        (dir, store)
    }

    fn seed(store: &BookmarkStore, id: &str, status: &str) {
        let now = BookmarkStore::now_ms();
        let b = Bookmark {
            id: id.into(),
            url: format!("https://{}.com", id),
            canonical_url: Some(format!("https://{}.com", id)),
            title: Some("示例".into()),
            browser_folder: Some("AI".into()),
            added_at: Some(now),
            source_file: None,
            category: None,
            summary: None,
            tags: None,
            raw_content: Some("正文内容……".into()),
            embedding_text: None,
            status: status.into(),
            dead: false,
            last_error: None,
            revision: 1,
            created_at: now,
            updated_at: now,
        };
        store.insert(&b).unwrap();
    }

    /// 总是返回 Err 的摘要提供者：模拟 LLM 调用失败。
    struct FailingSummarizer;
    impl BookmarkSummarizer for FailingSummarizer {
        fn summarize(
            &self,
            _title: String,
            _url: String,
            _content: String,
        ) -> BoxFuture<'static, Result<Option<BookmarkSummaryOut>, String>> {
            Box::pin(async { Err("LLM 调用失败: timeout".to_string()) })
        }
    }

    /// 总是返回固定产物的摘要提供者：模拟 LLM 成功。
    struct OkSummarizer;
    impl BookmarkSummarizer for OkSummarizer {
        fn summarize(
            &self,
            _title: String,
            _url: String,
            _content: String,
        ) -> BoxFuture<'static, Result<Option<BookmarkSummaryOut>, String>> {
            Box::pin(async {
                Ok(Some(BookmarkSummaryOut {
                    summary: "这是一段摘要".to_string(),
                    category: Some("AI".to_string()),
                    tags: vec!["Agent".to_string(), "RAG".to_string()],
                }))
            })
        }
    }

    fn make_worker<T: BookmarkSummarizer + 'static>(summarizer: T) -> EnrichmentWorker {
        let stores = Arc::new(std::sync::Mutex::new(HashMap::new()));
        EnrichmentWorker::new(stores, Some(Arc::new(summarizer)))
    }

    #[test]
    fn embedding_text_puts_tags_first_and_truncates_summary() {
        // tags 置前：长摘要不会把 tags 挤出 512 token 预算
        let long_summary = "长".repeat(5000);
        let t = build_embedding_text("标题", &long_summary, Some("[\"Agent\",\"RAG\"]"), Some("AI/LLM"));
        assert!(t.starts_with("Tags:"), "tags 应位于最前: {}", t);
        assert!(t.contains("Category: AI/LLM"));
        assert!(t.contains("Title: 标题"));
        assert!(t.contains("Summary: "));
        assert!(t.chars().count() < 400, "summary 应被截断，总长受控（字符数）: {}", t.chars().count());
        // 无 tags/category 时跳过对应段
        let t2 = build_embedding_text("标题", "摘要", None, None);
        assert!(!t2.contains("Tags:") && !t2.contains("Category:"));
    }

    #[tokio::test]
    async fn summary_llm_failure_marks_failed_terminal() {
        let (_dir, store) = open_temp();
        seed(&store, "bm_f", STATUS_PENDING);
        let store_arc = Arc::new(std::sync::Mutex::new(store));
        let worker = make_worker(FailingSummarizer);
        // 只跑摘要阶段（模拟已抓取，跳过真实网络）
        let fetched = {
            let s = store_arc.lock().unwrap();
            let jobs = s.claim_pending(10).unwrap();
            jobs.into_iter()
                .map(|b| (b.clone(), b.raw_content.clone().unwrap_or_default()))
                .collect::<Vec<_>>()
        };
        let summarized = worker.summarize_batch(&store_arc, &fetched).await.unwrap();
        assert!(summarized.is_empty(), "全部失败，无产物进入向量阶段");
        let s = store_arc.lock().unwrap();
        let b = s.get("bm_f").unwrap().unwrap();
        assert_eq!(b.status, "failed", "LLM 失败应直接终态 failed");
        assert!(!b.dead, "总结失败非死链");
        assert!(b.summary.is_none());
        assert!(b.embedding_text.is_none(), "失败不应写 embedding 文本");
    }

    #[tokio::test]
    async fn summary_llm_success_writes_fields_and_keeps_pending() {
        let (_dir, store) = open_temp();
        seed(&store, "bm_o", STATUS_PENDING);
        let store_arc = Arc::new(std::sync::Mutex::new(store));
        let worker = make_worker(OkSummarizer);
        let fetched = {
            let s = store_arc.lock().unwrap();
            let jobs = s.claim_pending(10).unwrap();
            jobs.into_iter()
                .map(|b| (b.clone(), b.raw_content.clone().unwrap_or_default()))
                .collect::<Vec<_>>()
        };
        let summarized = worker.summarize_batch(&store_arc, &fetched).await.unwrap();
        assert_eq!(summarized.len(), 1, "成功产物进入向量阶段");
        let s = store_arc.lock().unwrap();
        let b = s.get("bm_o").unwrap().unwrap();
        assert_eq!(b.summary.as_deref(), Some("这是一段摘要"));
        assert_eq!(b.category.as_deref(), Some("AI"));
        assert_eq!(b.status, STATUS_PENDING, "总结成功不置终态，等向量阶段置 ready");
        let tags: String = b.tags.unwrap_or_default();
        assert!(tags.contains("Agent") && tags.contains("RAG"), "tags 应为 JSON 数组字符串");
    }

    #[tokio::test]
    async fn claim_pending_only_claims_pending() {
        let (_dir, store) = open_temp();
        seed(&store, "bm_p1", STATUS_PENDING);
        seed(&store, "bm_r1", "ready");
        seed(&store, "bm_f1", "failed");
        let jobs = store.claim_pending(10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "bm_p1");
    }
}
