use std::path::{Path, PathBuf};
use std::time::Duration;

// ─── 下载源配置 ───
//
// 模型从 HuggingFace 官方仓库逐文件直链下载，不再依赖自托管
// GitHub/Gitee Release 的 zip 包。按顺序依次尝试以下源：
// 1. ModelScope（国内可达的 HF 仓库镜像，阿里系 CDN，最稳定）
// 2. 中国境内官方镜像：hf-mirror.com（注意：其 LFS 大文件会回源到 AWS CDN）
// 3. HuggingFace 主站（全球）：huggingface.co
// 每个文件先尝试前面的源，全部失败后自动切换到下一个源。

/// HuggingFace 官方主站
pub const HF_ENDPOINT: &str = "https://huggingface.co";
/// HuggingFace 中国境内官方镜像
pub const HF_ENDPOINT_MIRROR: &str = "https://hf-mirror.com";
/// ModelScope 首选源（HF 仓库镜像，URL 模板为 /models/{repo}/resolve/master/{path}）
pub const HF_ENDPOINT_MODELSCOPE: &str = "https://modelscope.cn";

// ─── Embedding 模型（bge-small-zh-v1.5）───

/// 模型名称（缓存目录名，与仓库目录一致）
pub const MODEL_NAME: &str = "bge-small-zh-v1.5";
/// HuggingFace 仓库（Xenova 提供的 ONNX 导出版）
pub const MODEL_REPO: &str = "Xenova/bge-small-zh-v1.5";
/// 文件映射：(仓库内路径, 本地文件名)。权重放最后，先下载小文件。
pub const MODEL_FILES: &[(&str, &str)] = &[
    ("config.json", "config.json"),
    ("tokenizer.json", "tokenizer.json"),
    ("tokenizer_config.json", "tokenizer_config.json"),
    ("special_tokens_map.json", "special_tokens_map.json"),
    ("onnx/model.onnx", "model.onnx"),
];

/// 模型完整性必需文件（与 embedding::ensure_initialized 一致）
const REQUIRED_FILES: [&str; 5] = [
    "model.onnx",
    "tokenizer.json",
    "config.json",
    "tokenizer_config.json",
    "special_tokens_map.json",
];

// ─── Reranker 模型（bge-reranker-base）───

/// Reranker 模型名称（缓存目录名）
pub const RERANKER_MODEL_NAME: &str = "bge-reranker-base";
/// HuggingFace 仓库（Xenova 提供的 ONNX 导出版，XLM-RoBERTa 架构）
pub const RERANKER_MODEL_REPO: &str = "Xenova/bge-reranker-base";
/// 文件映射：(仓库内路径, 本地文件名)。权重放最后，先下载小文件。
pub const RERANKER_MODEL_FILES: &[(&str, &str)] = &[
    ("config.json", "config.json"),
    ("tokenizer.json", "tokenizer.json"),
    ("tokenizer_config.json", "tokenizer_config.json"),
    ("special_tokens_map.json", "special_tokens_map.json"),
    ("onnx/model.onnx", "model.onnx"),
];

/// Reranker 完整性必需文件（与 search::rerank::ensure_initialized 一致）
const RERANKER_REQUIRED_FILES: [&str; 5] = [
    "model.onnx",
    "tokenizer.json",
    "config.json",
    "tokenizer_config.json",
    "special_tokens_map.json",
];

/// Reranker 模型缓存目录：{root}/bge-reranker-base/
pub fn reranker_cache_dir() -> PathBuf {
    cache_dir_for(&model_root_dir(), RERANKER_MODEL_NAME)
}

/// Reranker 模型是否已完整下载并部署
pub fn is_reranker_cached() -> bool {
    is_model_cached_impl(&reranker_cache_dir(), &RERANKER_REQUIRED_FILES)
}

/// 确保 Reranker 模型已下载并部署到本地缓存目录（复用 Embedding 的下载/校验/部署流程）。
///
/// 下载源优先级与 Embedding 模型一致：ModelScope → hf-mirror → HuggingFace。
pub fn ensure_reranker_downloaded() -> Result<PathBuf, String> {
    ensure_model_downloaded_impl(
        &reranker_cache_dir(),
        RERANKER_MODEL_NAME,
        RERANKER_MODEL_REPO,
        RERANKER_MODEL_FILES,
        &RERANKER_REQUIRED_FILES,
    )
}

/// 单文件下载整体超时（小文件适用）
const TOTAL_TIMEOUT: Duration = Duration::from_secs(600);
/// 大权重（model.onnx）下载整体超时：1.1GB 在 <1MB/s 的慢网下也能完成，
/// 避免 600s 内读不完导致自动下载反复失败
const TOTAL_TIMEOUT_LARGE: Duration = Duration::from_secs(1800);
/// 建连超时
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

// ─── 缓存目录 ───

/// 模型缓存根目录（跨平台应用数据目录 + mdgo/models，与 lib.rs 日志目录风格一致）。
///
/// 可用环境变量 `MDGO_MODEL_CACHE_DIR` 覆盖（测试/离线部署用）。
/// - Windows: `%APPDATA%/mdgo/models/`
/// - macOS:   `~/Library/Application Support/mdgo/models/`
/// - Linux:   `$XDG_DATA_HOME/mdgo/models/` 或 `~/.local/share/mdgo/models/`
pub fn model_root_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MDGO_MODEL_CACHE_DIR") {
        return PathBuf::from(dir);
    }

    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .map(|p| PathBuf::from(p).join("mdgo").join("models"))
            .unwrap_or_else(|_| PathBuf::from("models"))
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
            .join("Library")
            .join("Application Support")
            .join("mdgo")
            .join("models")
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
                    .join(".local")
                    .join("share")
            })
            .join("mdgo")
            .join("models")
    }
}

/// 指定模型名的缓存目录：{root}/{name}/
fn cache_dir_for(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}

/// 模型缓存目录：{root}/bge-small-zh-v1.5/
pub fn model_cache_dir() -> PathBuf {
    cache_dir_for(&model_root_dir(), MODEL_NAME)
}

/// 下载成功并完成完整性校验的标记文件（存在即认为缓存有效，避免半下载状态被误用）
fn ready_marker_for(cache_dir: &Path) -> PathBuf {
    cache_dir.join(".download_ok")
}

/// 指定缓存目录的模型是否已完整下载并部署
fn is_model_cached_impl(cache_dir: &Path, required_files: &[&str]) -> bool {
    if !ready_marker_for(cache_dir).exists() || !cache_dir.join("model.onnx").exists() {
        return false;
    }
    required_files
        .iter()
        .all(|f| cache_dir.join(f).exists())
}

/// 模型是否已完整下载并部署到缓存目录
pub fn is_model_cached() -> bool {
    is_model_cached_impl(&model_cache_dir(), &REQUIRED_FILES)
}

// ─── 下载 + 校验 + 部署 ───

/// 确保模型已下载并部署到本地缓存目录。
///
/// 流程：逐文件从 HuggingFace 仓库下载（ModelScope → hf-mirror → 主站）→ 校验 config.json →
/// 部署到缓存目录 → 写入就绪标记。
/// 任一环节失败会清理临时产物并返回错误，调用方下次可重试。
pub fn ensure_model_downloaded() -> Result<PathBuf, String> {
    ensure_model_downloaded_impl(
        &model_cache_dir(),
        MODEL_NAME,
        MODEL_REPO,
        MODEL_FILES,
        &REQUIRED_FILES,
    )
}

/// 泛化的模型下载/校验/部署流程。
fn ensure_model_downloaded_impl(
    cache_dir: &Path,
    name: &str,
    repo: &str,
    files: &[(&str, &str)],
    required_files: &[&str],
) -> Result<PathBuf, String> {
    if is_model_cached_impl(cache_dir, required_files) {
        return Ok(cache_dir.to_path_buf());
    }

    let root = model_root_dir();
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("创建模型缓存目录失败: {}", e))?;

    let tmp_dir = root.join(format!("{}.download", name));
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)
            .map_err(|e| format!("清理下载临时目录失败: {}", e))?;
    }
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| format!("创建下载临时目录失败: {}", e))?;

    // 无论成功失败，函数返回时统一清理临时目录，避免失败路径在磁盘残留大文件
    let _cleanup = TempCleanup::new(vec![tmp_dir.clone()]);

    log::info!(
        "[model_download] 本地未找到模型 {}，开始下载（ModelScope → hf-mirror → HuggingFace）...",
        name
    );

    // ── 1. 逐文件下载（权重放最后，小文件失败可尽早暴露）──
    const MIN_MODEL_SIZE: u64 = 10 * 1024 * 1024; // 大权重最小 10 MB，防 CDN 错误页
    for (hf_path, local_name) in files {
        let dest = tmp_dir.join(local_name);
        let min_size = if *local_name == "model.onnx" {
            MIN_MODEL_SIZE
        } else {
            1
        };
        // 大权重放宽整体超时，避免慢网下 600s 读不完而失败
        let timeout = if *local_name == "model.onnx" {
            TOTAL_TIMEOUT_LARGE
        } else {
            TOTAL_TIMEOUT
        };
        download_with_fallback(repo, hf_path, &dest, min_size, timeout)
            .map_err(|e| format!("下载 {} 失败: {}", local_name, e))?;
    }

    // ── 2. 校验 config.json 为合法 JSON（防错误页被当作配置写入）──
    let config_path = tmp_dir.join("config.json");
    let config_raw = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("读取 config.json 失败: {}", e))?;
    if serde_json::from_str::<serde_json::Value>(&config_raw).is_err() {
        return Err("config.json 不是合法 JSON，模型文件可能损坏".to_string());
    }

    // ── 3. 部署到缓存目录（先清空可能存在的半成品）──
    if cache_dir.exists() {
        std::fs::remove_dir_all(cache_dir)
            .map_err(|e| format!("清理模型缓存目录失败: {}", e))?;
    }
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("创建模型缓存目录失败: {}", e))?;
    copy_tree(&tmp_dir, cache_dir)?;

    // ── 4. 完整性检查 + 写入就绪标记 ──
    for file_name in required_files {
        if !cache_dir.join(file_name).exists() {
            return Err(format!("模型缺少必需文件: {}", file_name));
        }
    }
    std::fs::write(ready_marker_for(cache_dir), b"ok")
        .map_err(|e| format!("写入模型就绪标记失败: {}", e))?;

    log::info!(
        "[model_download] 模型下载并部署完成: {}",
        cache_dir.display()
    );
    Ok(cache_dir.to_path_buf())
}

// ─── 辅助函数 ───

/// 临时产物清理守卫：Drop 时（函数返回无论成败）自动删除指定路径。
///
/// 同时尝试按目录/按文件删除，忽略删除失败（文件已不存在等）。
struct TempCleanup {
    paths: Vec<PathBuf>,
}

impl TempCleanup {
    fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_dir_all(p);
            let _ = std::fs::remove_file(p);
        }
    }
}

/// 从多个源按顺序下载单个文件：每个源最多尝试 `ATTEMPTS` 次（首次 + 1 次重试），
/// 当前源全部失败后自动切换到下一个源。
///
/// 源顺序：ModelScope → hf-mirror 中国镜像 → HuggingFace 主站。
/// 每次下载失败会记录该地址与原因，最终错误汇总所有失败信息（便于排查网络原因）。
/// min_size: 最小字节数阈值，小于此值视为异常（如 CDN 错误页面）；传 1 表示仅要求非空
/// timeout: 单次请求整体超时（含 body 传输），大权重由调用方传入更宽松的值
fn download_with_fallback(
    repo: &str,
    hf_path: &str,
    dest: &Path,
    min_size: u64,
    timeout: Duration,
) -> Result<String, String> {
    const ATTEMPTS: usize = 2;
    let endpoints = [HF_ENDPOINT_MODELSCOPE, HF_ENDPOINT_MIRROR, HF_ENDPOINT];
    let source_names = ["ModelScope", "hf-mirror", "HuggingFace"];
    let mut errors: Vec<String> = Vec::new();
    for (i, endpoint) in endpoints.iter().enumerate() {
        let url = build_download_url(endpoint, repo, hf_path);
        for attempt in 0..ATTEMPTS {
            match download_file(&url, dest, min_size, timeout) {
                Ok(()) => return Ok(source_names[i].to_string()),
                Err(e) => {
                    errors.push(format!("{} (第{}次): {}", url, attempt + 1, e));
                    log::warn!(
                        "[model_download] 下载失败 (第{}次): {}",
                        attempt + 1,
                        e
                    );
                }
            }
        }
        if i + 1 < endpoints.len() {
            log::warn!(
                "[model_download] 当前地址下载失败，切换到备用源: {}",
                endpoints[i + 1]
            );
        }
    }
    Err(format!("所有下载地址均失败: {}", errors.join("; ")))
}

/// 构造文件直链：HuggingFace 系用 /{repo}/resolve/main/{path}，
/// ModelScope 用 /models/{repo}/resolve/master/{path}（分支名不同）
fn build_download_url(endpoint: &str, repo: &str, hf_path: &str) -> String {
    if endpoint == HF_ENDPOINT_MODELSCOPE {
        format!("{}/models/{}/resolve/master/{}", endpoint, repo, hf_path)
    } else {
        format!("{}/{}/resolve/main/{}", endpoint, repo, hf_path)
    }
}

/// 单次下载文件到本地路径（流式写入，避免大文件占用内存）
/// min_size: 最小字节数阈值，小于此值视为异常响应（如 CDN 错误页面）；传 1 表示仅要求非空
/// timeout: 本次请求整体超时（含 body 传输）
fn download_file(url: &str, dest: &Path, min_size: u64, timeout: Duration) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36")
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {}", e))?;

    let mut resp = client
        .get(url)
        .send()
        .map_err(|e| format!("下载失败 ({}): {}", url, e))?;

    if !resp.status().is_success() {
        return Err(format!("下载失败: HTTP {} ({})", resp.status(), url));
    }

    let mut file = std::fs::File::create(dest)
        .map_err(|e| format!("创建文件失败 {}: {}", dest.display(), e))?;
    std::io::copy(&mut resp, &mut file)
        .map_err(|e| {
            // 写入失败时删除已创建的部分文件
            let _ = std::fs::remove_file(dest);
            format!("写入文件失败 {}: {}", dest.display(), e)
        })?;

    let size = std::fs::metadata(dest)
        .map(|m| m.len())
        .unwrap_or(0);

    // 文件大小校验：小于阈值视为异常响应（如 CDN 错误页面）
    if size < min_size {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "下载文件大小异常 ({} bytes < {}): 可能是错误响应，URL: {}",
            size, min_size, url
        ));
    }

    log::info!("[model_download] 下载完成: {} ({} bytes)", url, size);
    Ok(())
}

/// 递归复制目录内容（仅文件，保留相对路径）
fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in walkdir::WalkDir::new(src).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_dir() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| format!("路径错误: {}", e))?;
        let target = dst.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
        }
        std::fs::copy(entry.path(), &target)
            .map_err(|e| format!("复制文件失败 {}: {}", entry.path().display(), e))?;
    }
    Ok(())
}
