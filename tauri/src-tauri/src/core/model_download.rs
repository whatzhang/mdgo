use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

// ─── 模型下载配置 ───

/// 模型名称（缓存目录名，与仓库目录一致）
pub const MODEL_NAME: &str = "bge-small-zh-v1.5";
/// 模型 zip 下载地址（GitHub Release，主地址）
pub const MODEL_ZIP_URL: &str =
    "https://github.com/whatzhang/mdgo/releases/download/v1.0.0/bge-small-zh-v1.5.zip";
/// 模型 zip 的 SHA-256 校验文件地址（内容为 64 位十六进制摘要）
pub const MODEL_SHA256_URL: &str =
    "https://github.com/whatzhang/mdgo/releases/download/v1.0.0/bge-small-zh-v1.5.zip.sha256";
/// 备用 zip 下载地址（Gitee 镜像）：主地址下载失败且重试也失败时启用
pub const MODEL_ZIP_URL_BACKUP: &str =
    "https://gitee.com/whatzhangy/mdgo/releases/download/v1.0.0/bge-small-zh-v1.5.zip";
/// 备用 SHA-256 校验文件地址（Gitee 镜像）
pub const MODEL_SHA256_URL_BACKUP: &str =
    "https://gitee.com/whatzhangy/mdgo/releases/download/v1.0.0/bge-small-zh-v1.5.zip.sha256";

/// 模型完整性必需文件（与 embedding::ensure_initialized 一致）
const REQUIRED_FILES: [&str; 5] = [
    "model.onnx",
    "tokenizer.json",
    "config.json",
    "tokenizer_config.json",
    "special_tokens_map.json",
];

/// 下载/解压/校验整体超时（大模型 zip 可能较大，放宽到 10 分钟）
const TOTAL_TIMEOUT: Duration = Duration::from_secs(600);
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

/// 模型缓存目录：{root}/bge-small-zh-v1.5/
pub fn model_cache_dir() -> PathBuf {
    model_root_dir().join(MODEL_NAME)
}

/// 下载成功并完成完整性校验的标记文件（存在即认为缓存有效，避免半解压状态被误用）
fn ready_marker() -> PathBuf {
    model_cache_dir().join(".download_ok")
}

/// 模型是否已完整下载并部署到缓存目录
pub fn is_model_cached() -> bool {
    let cache_dir = model_cache_dir();
    if !ready_marker().exists() || !cache_dir.join("model.onnx").exists() {
        return false;
    }
    REQUIRED_FILES
        .iter()
        .all(|f| cache_dir.join(f).exists())
}

// ─── 下载 + 校验 + 解压 ───

/// 确保模型已下载并部署到本地缓存目录。
///
/// 流程：下载 zip → 下载 sha256 摘要 → 校验 SHA-256 → 解压到临时目录 →
/// 定位模型目录（兼容 zip 内含顶层文件夹）→ 部署到缓存目录 → 写入就绪标记。
/// 任一环节失败会清理临时产物并返回错误，调用方下次可重试。
pub fn ensure_model_downloaded() -> Result<PathBuf, String> {
    let cache_dir = model_cache_dir();
    if is_model_cached() {
        return Ok(cache_dir);
    }

    let root = model_root_dir();
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("创建模型缓存目录失败: {}", e))?;

    let zip_path = root.join(format!("{}.zip", MODEL_NAME));
    let sha_path = root.join(format!("{}.zip.sha256", MODEL_NAME));
    let extract_tmp = root.join(format!("{}.extract", MODEL_NAME));

    // 无论成功失败，函数返回时统一清理临时产物（zip / sha / 解压临时目录），
    // 避免失败路径在磁盘残留大文件或半成品目录
    let _cleanup = TempCleanup::new(vec![
        zip_path.clone(),
        sha_path.clone(),
        extract_tmp.clone(),
    ]);

    // ── 1. 下载 sha256 校验文件（先下载，文件小，失败成本低）──
    //    同时确定下载源（主/备），后续 zip 强制同源，避免跨源校验失败
    log::info!("[model_download] 本地未找到模型，开始远程下载...");
    let sha_urls = [MODEL_SHA256_URL, MODEL_SHA256_URL_BACKUP];
    let (source_index, source_name) =
        download_with_fallback(&sha_urls, &sha_path, 0)?; // sha 文件小，跳过大小校验

    // ── 2. 下载 zip（强制使用与 sha256 相同的源，保证同源）──
    const MIN_ZIP_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
    let zip_urls = [MODEL_ZIP_URL, MODEL_ZIP_URL_BACKUP];
    download_file_from_source(source_index, &zip_urls, &zip_path, MIN_ZIP_SIZE)?;
    log::info!(
        "[model_download] zip 下载完成（源: {}）",
        source_name
    );

    // ── 3. 校验 SHA-256 ──
    let expected_hex = read_sha256_file(&sha_path)?;
    let actual_hex = sha256_hex(&zip_path)?;
    if !actual_hex.eq_ignore_ascii_case(&expected_hex) {
        return Err(format!(
            "模型 SHA-256 校验失败: 期望 {}, 实际 {}",
            expected_hex, actual_hex
        ));
    }
    log::info!("[model_download] SHA-256 校验通过: {}", actual_hex);

    // ── 4. 解压到临时目录 ──
    if extract_tmp.exists() {
        std::fs::remove_dir_all(&extract_tmp)
            .map_err(|e| format!("清理解压临时目录失败: {}", e))?;
    }
    std::fs::create_dir_all(&extract_tmp)
        .map_err(|e| format!("创建解压临时目录失败: {}", e))?;
    extract_zip(&zip_path, &extract_tmp)?;

    // ── 5. 定位模型目录（zip 可能包含顶层文件夹）──
    let model_src = locate_model_dir(&extract_tmp)?;

    // ── 6. 部署到缓存目录（先清空可能存在的半成品）──
    if cache_dir.exists() {
        std::fs::remove_dir_all(&cache_dir)
            .map_err(|e| format!("清理模型缓存目录失败: {}", e))?;
    }
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| format!("创建模型缓存目录失败: {}", e))?;
    copy_tree(&model_src, &cache_dir)?;

    // ── 7. 完整性检查 + 写入就绪标记 ──
    for name in REQUIRED_FILES {
        if !cache_dir.join(name).exists() {
            return Err(format!("模型解压后缺少必需文件: {}", name));
        }
    }
    std::fs::write(ready_marker(), b"ok")
        .map_err(|e| format!("写入模型就绪标记失败: {}", e))?;

    log::info!(
        "[model_download] 模型下载并部署完成: {}",
        cache_dir.display()
    );
    Ok(cache_dir)
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

/// 按 URL 列表顺序下载文件到目标路径：每个地址最多尝试 `ATTEMPTS` 次（首次 + 1 次重试），
/// 当前地址全部尝试失败后切换到下一个备用地址，直到所有地址耗尽。
///
/// 返回成功源的索引（0=主地址，1=备用地址）和源名称（用于日志），供后续下载强制同源。
/// 每次下载失败会记录该地址与原因，最终错误汇总所有失败信息（便于排查网络原因）。
/// min_size: 最小字节数阈值，小于此值视为异常；传 0 跳过校验
fn download_with_fallback(urls: &[&str], dest: &Path, min_size: u64) -> Result<(usize, String), String> {
    const ATTEMPTS: usize = 2;
    let mut errors: Vec<String> = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        for attempt in 0..ATTEMPTS {
            match download_file(url, dest, min_size) {
                Ok(()) => {
                    let source_name = if i == 0 { "GitHub".to_string() } else { "Gitee".to_string() };
                    return Ok((i, source_name));
                }
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
        if i + 1 < urls.len() {
            log::warn!(
                "[model_download] 当前地址下载失败，切换备用地址: {}",
                urls[i + 1]
            );
        }
    }
    Err(format!("所有下载地址均失败: {}", errors.join("; ")))
}

/// 从指定源索引下载文件（强制同源下载用）
fn download_file_from_source(source_index: usize, urls: &[&str], dest: &Path, min_size: u64) -> Result<(), String> {
    let url = urls.get(source_index).ok_or_else(|| {
        format!(
            "源索引 {} 超出 URL 列表范围",
            source_index
        )
    })?;
    download_file(url, dest, min_size)
}

/// 单次下载文件到本地路径（流式写入，避免整包占用内存）
/// min_size: 最小字节数阈值，小于此值视为异常响应（如 CDN 错误页面）；传 0 跳过校验
fn download_file(url: &str, dest: &Path, min_size: u64) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(TOTAL_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
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
    if min_size > 0 && size < min_size {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "下载文件大小异常 ({} bytes < {}): 可能是错误响应，URL: {}",
            size, min_size, url
        ));
    }

    log::info!("[model_download] 下载完成: {} ({} bytes)", url, size);
    Ok(())
}

/// 读取 sha256 文件并提取 64 位十六进制摘要（兼容 `摘要 文件名` 两段式格式）
fn read_sha256_file(path: &Path) -> Result<String, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("读取校验文件失败 {}: {}", path.display(), e))?;
    let hex = content
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("校验文件格式无效: {}", path.display()))?;
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("校验文件内容无效: {}", hex));
    }
    Ok(hex.to_string())
}

/// 计算文件 SHA-256（分块读取，内存友好）
fn sha256_hex(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("打开文件失败 {}: {}", path.display(), e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("读取文件失败 {}: {}", path.display(), e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 解压 zip 到目录（使用 enclosed_name 防 zip-slip 路径穿越）
fn extract_zip(zip_path: &Path, out_dir: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("打开压缩包失败 {}: {}", zip_path.display(), e))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("读取压缩包失败 {}: {}", zip_path.display(), e))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("读取压缩条目失败: {}", e))?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| format!("压缩包包含不安全路径: {:?}", entry.name()))?;
        let out_path = out_dir.join(name);

        if entry.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("创建目录失败 {}: {}", out_path.display(), e))?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败 {}: {}", parent.display(), e))?;
        }
        let mut out = std::fs::File::create(&out_path)
            .map_err(|e| format!("创建文件失败 {}: {}", out_path.display(), e))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| format!("写入文件失败 {}: {}", out_path.display(), e))?;
    }
    Ok(())
}

/// 在解压目录中定位包含 model.onnx 的目录（兼容 zip 内含顶层文件夹的布局）
fn locate_model_dir(tmp: &Path) -> Result<PathBuf, String> {
    if tmp.join("model.onnx").exists() {
        return Ok(tmp.to_path_buf());
    }
    for entry in walkdir::WalkDir::new(tmp)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_name() == "model.onnx" {
            return Ok(entry
                .path()
                .parent()
                .unwrap_or(tmp)
                .to_path_buf());
        }
    }
    Err("解压后的模型包中未找到 model.onnx".to_string())
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
