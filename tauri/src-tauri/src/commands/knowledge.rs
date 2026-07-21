use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::AppHandle;

use crate::db::bm25::Bm25Index;
use crate::db::lance::{DocumentChunk, LanceStore, SearchHit};
use crate::db::utils;
use crate::db::utils::IgnoreMatcher;

const KB_SUPPORTED_EXTS: &[&str] = utils::KB_SUPPORTED_EXTS;

// ─── 数据结构 ───

#[derive(Debug, Serialize)]
pub struct KbIndexResult {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
}

#[derive(Debug, Serialize)]
pub struct KbStatus {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexMeta {
    file_count: u32,
    chunk_count: u32,
    vector_count: u32,
    indexed_at: u64,
}

// ─── 命令 ───

/// 索引当前目录（解决 C6：先清理旧数据再重新索引）
///
/// 采用分批处理策略控制内存峰值：
/// 1. 扫描文件列表
/// 2. 按文件分组读取 → 分块 → Embedding → 写入 LanceDB + BM25
/// 3. 每处理完一批立即释放内存，避免 OOM
const BATCH_CHUNK_LIMIT: usize = 200; // 每批最多处理 200 个文本块

#[tauri::command]
pub async fn kb_index(
    app: AppHandle,
    dir_path: String,
    embedding_endpoint: String,
    embedding_token: Option<String>,
    embedding_model: String,
    embedding_dimension: u32,
    dir_blacklist: Vec<String>,
    file_blacklist: Vec<String>,
) -> Result<KbIndexResult, String> {
    eprintln!("[kb_index] === 开始索引 ===");
    eprintln!("[kb_index] dir_path = {}", dir_path);
    eprintln!("[kb_index] embedding_endpoint = {}", embedding_endpoint);
    eprintln!("[kb_index] embedding_model = {}", embedding_model);
    eprintln!("[kb_index] embedding_dimension = {}", embedding_dimension);

    let base_dir = Path::new(&dir_path);
    if !base_dir.exists() {
        return Err(format!("目录不存在: {}", dir_path));
    }

    eprintln!("[kb_index] 目录存在，开始清理旧索引...");
    // 先清理旧索引数据
    kb_clear_inner(&app, &dir_path).await?;
    eprintln!("[kb_index] 旧索引清理完成");

    // 1. 扫描文件
    utils::emit_progress(&app, 0, "正在扫描目录...");
    let ignore = IgnoreMatcher::new(&dir_blacklist, &file_blacklist);
    eprintln!("[kb_index] 开始扫描目录...");
    let files = scan_directory(base_dir, &ignore)?;
    let total = files.len() as u32;
    eprintln!("[kb_index] 扫描完成，共 {} 个文件", total);
    for (i, f) in files.iter().enumerate() {
        eprintln!("[kb_index]   文件 {}: {:?}", i + 1, f);
    }

    if total == 0 {
        return Err("目录中没有可索引的文件".into());
    }

    utils::emit_progress(&app, 2, &format!("已发现 {} 个文件", total));

    // 预创建 LanceDB 表（仅一次）
    eprintln!("[kb_index] 预创建 LanceDB 表...");
    let data_dir = utils::get_data_dir(&dir_path);
    eprintln!("[kb_index] data_dir = {}", data_dir);
    let store = LanceStore::new(&data_dir, "vectors");
    store.create_table(embedding_dimension).await?;
    eprintln!("[kb_index] LanceDB 表创建完成");

    // 预创建/打开 BM25 索引（仅一次）
    eprintln!("[kb_index] 预创建 BM25 索引...");
    let bm25_dir = utils::get_bm25_dir(&dir_path);
    eprintln!("[kb_index] bm25_dir = {}", bm25_dir);
    let bm25 = if Path::new(&bm25_dir).exists() {
        Bm25Index::open(&bm25_dir)?
    } else {
        Bm25Index::create(&bm25_dir)?
    };
    eprintln!("[kb_index] BM25 索引准备完成");

    // 2. 分批处理：读取 → 分块 → Embedding → 写入
    let mut batch_chunks: Vec<DocumentChunk> = Vec::with_capacity(BATCH_CHUNK_LIMIT);
    let mut file_count = 0u32;
    let mut total_chunks = 0u32;
    let mut total_vectors = 0u32;
    let mut batch_index = 0u32;

    for (i, file_path) in files.iter().enumerate() {
        eprintln!("[kb_index] 处理文件 {}/{}: {:?}", i + 1, total, file_path);
        // 读取文件内容
        eprintln!("[kb_index]   开始读取文件...");
        let content = match read_file_content(file_path) {
            Some(c) => c,
            None => {
                eprintln!("[kb_index]   → 跳过（读取失败/非UTF-8）");
                continue;
            }
        };
        eprintln!("[kb_index]   文件读取完成，大小: {} 字节", content.len());
        if content.len() < 10 {
            eprintln!("[kb_index]   → 跳过（内容过短: {} 字符）", content.len());
            continue;
        }

        eprintln!("[kb_index]   开始分块...");
        let chunks = utils::split_text(&content, 1000, 200);
        eprintln!("[kb_index]   分块完成");
        if chunks.is_empty() {
            eprintln!("[kb_index]   → 跳过（分块为空）");
            continue;
        }
        eprintln!("[kb_index]   → 分块数量: {}", chunks.len());

        let rel_path = file_path
            .strip_prefix(base_dir)
            .unwrap_or(file_path)
            .to_string_lossy()
            .to_string();

        let doc_chunks = utils::build_document_chunks(&rel_path, &chunks);
        batch_chunks.extend(doc_chunks);
        file_count += 1;

        let pct = 5 + ((i + 1) * 30 / total.max(1) as usize) as u8;

        // 批次未满时继续累积
        if batch_chunks.len() < BATCH_CHUNK_LIMIT && i + 1 < total as usize {
            utils::emit_progress(
                &app,
                pct.min(35),
                &format!("读取文件 {}/{} (已缓存 {} 个文本块)", i + 1, total, batch_chunks.len()),
            );
            continue;
        }

        // ── 批次已满或最后一批：处理当前批次 ──
        batch_index += 1;
        eprintln!(
            "[kb_index] 开始处理第 {} 批 ({} 个文本块, {}/{} 文件)",
            batch_index,
            batch_chunks.len(),
            i + 1,
            total
        );
        utils::emit_progress(
            &app,
            pct.min(35),
            &format!(
                "处理第 {} 批 ({} 个文本块, {}/{} 文件)...",
                batch_index,
                batch_chunks.len(),
                i + 1,
                total
            ),
        );

        // 3. Embedding API
        let embed_batch_size = 20usize;
        let mut batch_vectors: Vec<Vec<f32>> = Vec::with_capacity(batch_chunks.len());
        let total_embed_batches = batch_chunks.len().div_ceil(embed_batch_size).max(1);
        eprintln!("[kb_index]   Embedding 开始，共 {} 小批", total_embed_batches);

        for (embed_idx, embed_batch) in batch_chunks.chunks(embed_batch_size).enumerate() {
            let texts: Vec<&str> = embed_batch.iter().map(|c| c.text.as_str()).collect();
            eprintln!(
                "[kb_index]   调用 Embedding {}/{} ({} 个文本)...",
                embed_idx + 1,
                total_embed_batches,
                texts.len()
            );
            match utils::call_embedding(
                &embedding_endpoint,
                &embedding_token,
                &embedding_model,
                &texts,
            )
            .await
            {
                Ok(vectors) => {
                    eprintln!("[kb_index]   → Embedding 成功，返回 {} 个向量", vectors.len());
                    batch_vectors.extend(vectors);
                }
                Err(e) => {
                    eprintln!("[kb_index]   → Embedding 失败: {}", e);
                    let file_names: Vec<&str> =
                        embed_batch.iter().map(|c| c.doc_name.as_str()).collect();
                    return Err(format!(
                        "Embedding 失败 (第 {} 批-{}, 文件: {:?}): {}",
                        batch_index,
                        embed_idx + 1,
                        &file_names[..file_names.len().min(3)],
                        e
                    ));
                }
            }

            let embed_pct = 35 + ((embed_idx + 1) * 50 / total_embed_batches) as u8;
            utils::emit_progress(
                &app,
                embed_pct.min(85),
                &format!(
                    "向量化第 {} 批 {}/{}",
                    batch_index,
                    batch_vectors.len(),
                    batch_chunks.len()
                ),
            );
        }

        // 4. 写入 LanceDB
        eprintln!("[kb_index]   写入 LanceDB...");
        store.add_chunks(&batch_chunks, &batch_vectors).await?;
        eprintln!("[kb_index]   → LanceDB 写入成功");

        // 5. 写入 BM25
        eprintln!("[kb_index]   写入 BM25...");
        bm25.add_documents(&batch_chunks)?;
        eprintln!("[kb_index]   → BM25 写入成功");

        total_chunks += batch_chunks.len() as u32;
        total_vectors += batch_vectors.len() as u32;

        // 释放当前批次内存
        batch_chunks.clear();
        batch_vectors.clear();

        utils::emit_progress(
            &app,
            85,
            &format!(
                "已处理 {}/{} 文件 (累计 {} 文本块, {} 向量)",
                i + 1,
                total,
                total_chunks,
                total_vectors
            ),
        );
    }

    if total_chunks == 0 {
        eprintln!("[kb_index] 错误：所有文件都未能提取有效内容");
        return Err("未能从文件中提取有效内容".into());
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // 持久化索引元数据
    let meta = IndexMeta {
        file_count,
        chunk_count: total_chunks,
        vector_count: total_vectors,
        indexed_at: now,
    };
    save_metadata(&data_dir, &meta);

    eprintln!(
        "[kb_index] === 索引完成 === 文件: {}, 文本块: {}, 向量: {}",
        file_count, total_chunks, total_vectors
    );
    utils::emit_progress(&app, 100, "索引完成");

    Ok(KbIndexResult {
        file_count,
        chunk_count: total_chunks,
        vector_count: total_vectors,
        indexed_at: now,
    })
}

/// 混合检索（向量 + BM25 + RRF）（解决 M6：k 值定义为常量）
const RRF_K: u32 = 30;

#[tauri::command]
pub async fn kb_search_hybrid(
    _app: AppHandle,
    dir_path: String,
    query_vector: Vec<f32>,
    query: String,
    top_k: u32,
) -> Result<Vec<SearchHit>, String> {
    let data_dir = utils::get_data_dir(&dir_path);
    let bm25_dir = utils::get_bm25_dir(&dir_path);

    let vec_k = (top_k * 2).max(10);
    let store = LanceStore::new(&data_dir, "vectors");
    let vec_hits = store.search_vectors(&query_vector, vec_k).await.unwrap_or_default();

    let bm25_k = (top_k * 2).max(10);
    let bm25_hits = match Bm25Index::open(&bm25_dir) {
        Ok(idx) => idx.search(&query, bm25_k).unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    let fused = rrf_merge(&vec_hits, &bm25_hits, RRF_K);
    let result: Vec<SearchHit> = fused.into_iter().take(top_k as usize).collect();
    Ok(result)
}

/// 获取索引状态
///
/// 使用元数据缓存文件（index_meta.json）而非全表扫描，避免 CPU/内存暴涨。
/// count_rows(None) 在 LanceDB 0.31 中会触发全表扫描，大数据量时极慢。
#[tauri::command]
pub async fn kb_status(_app: AppHandle, dir_path: String) -> Result<KbStatus, String> {
    let data_dir = utils::get_data_dir(&dir_path);
    let store = LanceStore::new(&data_dir, "vectors");

    // 仅检查表是否存在（轻量操作），不调用 count_rows
    let table_exists = store.open_table().await.is_ok();

    let meta = load_metadata(&data_dir);
    let (status, vector_count) = if table_exists {
        if let Some(ref m) = meta {
            ("indexed", m.vector_count)
        } else {
            // 表存在但无元数据 → 旧数据或损坏，保守返回 0
            ("unknown", 0)
        }
    } else {
        ("unknown", 0)
    };

    Ok(KbStatus {
        file_count: meta.as_ref().map(|m| m.file_count).unwrap_or(0),
        chunk_count: meta.as_ref().map(|m| m.chunk_count).unwrap_or(0),
        vector_count,
        indexed_at: meta.as_ref().map(|m| m.indexed_at).unwrap_or(0),
        status: status.into(),
    })
}

/// 清除索引
#[tauri::command]
pub async fn kb_clear(app: AppHandle, dir_path: String) -> Result<(), String> {
    kb_clear_inner(&app, &dir_path).await
}

async fn kb_clear_inner(_app: &AppHandle, dir_path: &str) -> Result<(), String> {
    let data_dir = utils::get_data_dir(dir_path);
    let bm25_dir = utils::get_bm25_dir(dir_path);

    let store = LanceStore::new(&data_dir, "vectors");
    if store.open_table().await.is_ok() {
        store.clear().await?;
    }

    if Path::new(&bm25_dir).exists() {
        if let Ok(bm25) = Bm25Index::open(&bm25_dir) {
            let _ = bm25.clear();
        }
    }

    let meta_path = Path::new(&data_dir).join("index_meta.json");
    let _ = std::fs::remove_file(&meta_path);

    Ok(())
}

// ─── 辅助函数 ───

fn scan_directory(base_dir: &Path, ignore: &IgnoreMatcher) -> Result<Vec<std::path::PathBuf>, String> {
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(base_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                // 硬编码排除 .mdgo 数据目录，防止索引自身数据
                if name == ".mdgo" {
                    return false;
                }
                let rel_path = e.path().strip_prefix(base_dir).unwrap_or(e.path());
                let rel = rel_path.to_string_lossy().replace('\\', "/");
                return ignore.is_kb_dir_allowed(&name, &rel);
            }
            true
        });

    for entry in walker {
        let entry = entry.map_err(|e| format!("扫描目录失败: {}", e))?;
        if entry.file_type().is_file() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let rel_path = entry.path().strip_prefix(base_dir).unwrap_or(entry.path());
            let rel = rel_path.to_string_lossy().replace('\\', "/");
            if !ignore.is_kb_file_allowed(&file_name, &rel) {
                continue;
            }
            if let Some(ext) = entry.path().extension() {
                let ext = ext.to_string_lossy().to_lowercase();
                if KB_SUPPORTED_EXTS.contains(&ext.as_str()) {
                    files.push(entry.path().to_path_buf());
                }
            }
        }
    }
    Ok(files)
}

/// 读取文件内容，非 UTF-8 则跳过（解决 M1）
fn read_file_content(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(c) => Some(c),
        Err(e) => {
            // 只记录错误不中断流程
            eprintln!(
                "跳过文件 {}: {}",
                path.display(),
                if e.kind() == std::io::ErrorKind::InvalidData {
                    "非 UTF-8 编码".to_string()
                } else {
                    e.to_string()
                }
            );
            None
        }
    }
}

fn save_metadata(data_dir: &str, meta: &IndexMeta) {
    let path = Path::new(data_dir).join("index_meta.json");
    if let Ok(json) = serde_json::to_string(meta) {
        let _ = std::fs::write(&path, &json);
    }
}

fn load_metadata(data_dir: &str) -> Option<IndexMeta> {
    let path = Path::new(data_dir).join("index_meta.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// RRF（倒数排名融合）合并两路检索结果
fn rrf_merge(vec_hits: &[SearchHit], bm25_hits: &[SearchHit], k: u32) -> Vec<SearchHit> {
    use std::collections::HashMap;

    let mut score_map: HashMap<(String, u32), (f32, String)> = HashMap::new();

    for (rank, hit) in vec_hits.iter().enumerate() {
        let key = (hit.doc_name.clone(), hit.chunk_index);
        let rrf_score = 1.0 / (k as f32 + rank as f32);
        score_map
            .entry(key)
            .or_insert_with(|| (0.0, hit.text.clone()))
            .0 += rrf_score;
    }

    for (rank, hit) in bm25_hits.iter().enumerate() {
        let key = (hit.doc_name.clone(), hit.chunk_index);
        let rrf_score = 1.0 / (k as f32 + rank as f32);
        let entry = score_map
            .entry(key)
            .or_insert_with(|| (0.0, hit.text.clone()));
        entry.0 += rrf_score;
        if entry.1.len() < hit.text.len() {
            entry.1 = hit.text.clone();
        }
    }

    let mut results: Vec<SearchHit> = score_map
        .into_iter()
        .map(|((doc_name, chunk_index), (score, text))| SearchHit {
            text,
            doc_name,
            chunk_index,
            score: score.min(1.0),
        })
        .collect();

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results
}
