//! Retrieval Benchmark（P0-3）：对知识库检索质量与延迟的度量闭环。
//!
//! 用法（在 `tauri/src-tauri` 下）：
//! ```text
//! cargo run --bin benchmark -- --kb <知识库目录> --queries <queries.jsonl> --expected <expected.jsonl> [--topk 20] [--reindex]
//! ```
//!
//! 数据格式（`retrieval_eval/` 有示例）：
//! - queries.jsonl：`{"id": "q1", "query": "..."}`
//! - expected.jsonl：`{"query_id": "q1", "relevant_documents": ["docs/a.md"], "relevant_chunks": []}`
//!
//! 指标（doc 级）：Recall@5/10/20、MRR、NDCG@5/10 + 每查询总延迟。
//! 每次修改 chunk/BM25/RRF/reranker/query planner/metadata 后都应跑一遍，作为回归基线。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use mdgo_lib::bench_api::{call_embedding_query, ensure_model_ready, ConfigStore, Indexer, IndexerConfig};

#[derive(serde::Deserialize)]
struct QueryItem {
    id: String,
    query: String,
}

#[derive(serde::Deserialize)]
struct ExpectedItem {
    query_id: String,
    #[serde(default)]
    relevant_documents: Vec<String>,
    /// chunk 级标注（第一版 doc 级指标已够判断方向；chunk 级评测 V2 接入）
    #[serde(default)]
    #[allow(dead_code)]
    relevant_chunks: Vec<String>,
}

struct Args {
    kb_dir: String,
    queries_path: String,
    expected_path: String,
    topk: u32,
    reindex: bool,
    /// 🟠 M25：显式确认清空已有索引/缓存（`--reindex` 会 drop 目标目录 `.mdgo`
    /// 的 LanceDB 表、BM25 索引与 embedding 缓存并全量重建）
    yes_wipe: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut kb_dir = None;
    let mut queries_path = None;
    let mut expected_path = None;
    let mut topk: u32 = 20;
    let mut reindex = false;
    let mut yes_wipe = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--kb" => kb_dir = args.next(),
            "--queries" => queries_path = args.next(),
            "--expected" => expected_path = args.next(),
            "--topk" => {
                let raw = args
                    .next()
                    .ok_or("--topk 缺少参数值".to_string())?;
                topk = raw
                    .parse()
                    .map_err(|_| format!("--topk 非法值: {}（应为正整数）", raw))?;
            }
            "--reindex" => reindex = true,
            "--yes-wipe" => yes_wipe = true,
            "--help" | "-h" => {
                println!(
                    "用法: benchmark --kb <知识库目录> --queries <queries.jsonl> --expected <expected.jsonl> \
                     [--topk N] [--reindex] [--yes-wipe]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("未知参数: {}", other)),
        }
    }
    Ok(Args {
        kb_dir: kb_dir.ok_or("缺少 --kb <知识库目录>")?,
        queries_path: queries_path.ok_or("缺少 --queries <queries.jsonl>")?,
        expected_path: expected_path.ok_or("缺少 --expected <expected.jsonl>")?,
        topk,
        reindex,
        yes_wipe,
    })
}

fn load_jsonl<T: serde::de::DeserializeOwned>(path: &str) -> Result<Vec<T>, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败: {}", path, e))?;
    let mut out = Vec::new();
    for (line_no, l) in content.lines().enumerate() {
        let l = l.trim();
        if l.is_empty() {
            continue;
        }
        // 🟠 L32：错误消息带行号（旧实现"解析 {} 第行失败"缺行号）
        out.push(
            serde_json::from_str::<T>(l)
                .map_err(|e| format!("解析 {} 第 {} 行失败: {}", path, line_no + 1, e))?,
        );
    }
    Ok(out)
}

fn dcg(rel: &[f64]) -> f64 {
    rel.iter()
        .enumerate()
        .map(|(i, r)| r / (i as f64 + 2.0).log2())
        .sum()
}

fn ndcg_at_k(relevant: &HashSet<&str>, ranked: &[String], k: usize) -> f64 {
    let k = k.min(ranked.len());
    if k == 0 {
        return 0.0;
    }
    let rel: Vec<f64> = ranked[..k]
        .iter()
        .map(|d| if relevant.contains(d.as_str()) { 1.0 } else { 0.0 })
        .collect();
    let dcg_val = dcg(&rel);
    let ideal: Vec<f64> = (0..k)
        .map(|i| if i < relevant.len().min(k) { 1.0 } else { 0.0 })
        .collect();
    let idcg = dcg(&ideal);
    if idcg > 0.0 {
        dcg_val / idcg
    } else {
        0.0
    }
}

fn recall_at_k(relevant: &HashSet<&str>, ranked: &[String], k: usize) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let hit = ranked
        .iter()
        .take(k)
        .filter(|d| relevant.contains(d.as_str()))
        .count();
    hit as f64 / relevant.len() as f64
}

fn mrr(relevant: &HashSet<&str>, ranked: &[String]) -> f64 {
    for (i, d) in ranked.iter().enumerate() {
        if relevant.contains(d.as_str()) {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let args = parse_args()?;

    // 初始化本地 embedding 模型（分块预算/查询向量依赖真实窗口）
    ensure_model_ready()
        .map_err(|e| format!("embedding 模型就绪失败: {}", e))?;

    let indexer = Indexer::new(Arc::new(ConfigStore::new(IndexerConfig::default())));

    if args.reindex {
        // 🟠 M25：`--reindex` 会清空目标目录 `.mdgo` 的索引与 embedding 缓存并以
        // 默认配置重建——若目录已有索引数据，必须显式 `--yes-wipe` 确认，
        // 且应退出正在运行 App（避免双进程并发写同一 LanceDB/BM25）。
        let mdgo_dir = std::path::Path::new(&args.kb_dir).join(".mdgo");
        if mdgo_dir.exists() && !args.yes_wipe {
            return Err(
                "检测到目标目录已有索引数据（.mdgo/）。--reindex 会清空索引与 embedding 缓存并全量重建，\
                 请先退出正在运行的应用，然后加 --yes-wipe 显式确认"
                    .into(),
            );
        }
        println!("[benchmark] 全量重建索引: {}", args.kb_dir);
        let result = indexer
            .index_all(&args.kb_dir, |pct, msg| {
                if pct % 25 == 0 {
                    println!("[benchmark] 索引进度 {}%: {}", pct, msg);
                }
            })
            .await?;
        println!(
            "[benchmark] 索引完成: {} 文件 / {} chunk（截断 {} 重切 {}）",
            result.file_count, result.chunk_count, result.truncated_chunks, result.resplit_chunks
        );
    }

    let queries = load_jsonl::<QueryItem>(&args.queries_path)?;
    let expected: HashMap<String, HashSet<String>> = load_jsonl::<ExpectedItem>(&args.expected_path)?
        .into_iter()
        .map(|e| {
            let docs: HashSet<String> = e.relevant_documents.into_iter().collect();
            (e.query_id, docs)
        })
        .collect();

    if queries.is_empty() {
        return Err("queries 为空".into());
    }

    println!(
        "[benchmark] 开始评测: {} 条查询, topk={}",
        queries.len(),
        args.topk
    );

    let mut recalls = [vec![], vec![], vec![]]; // @5 @10 @20
    let mut mrrs = Vec::new();
    let mut ndcgs5 = Vec::new();
    let mut ndcgs10 = Vec::new();
    let mut latencies = Vec::new();
    let mut missed: Vec<(&str, &str)> = Vec::new(); // (query_id, 缺失标注)

    for q in &queries {
        let relevant = match expected.get(&q.id) {
            Some(r) if !r.is_empty() => r,
            Some(_) => {
                missed.push((&q.id, "relevant_documents 为空"));
                continue;
            }
            None => {
                missed.push((&q.id, "expected 缺失"));
                continue;
            }
        };

        let start = Instant::now();
        let query_vec = call_embedding_query(&q.query)
            .map_err(|e| format!("查询向量生成失败 ({}): {}", q.id, e))?
            .into_iter()
            .next()
            .ok_or_else(|| format!("查询向量为空 ({}): {}", q.id, q.query))?;
        let hits = indexer
            .hybrid_search(&args.kb_dir, &query_vec, &q.query, args.topk)
            .await?;
        let latency = start.elapsed();
        latencies.push(latency.as_millis() as f64);

        // A2：分阶段耗时（最近一次检索）
        let timings = indexer.last_retrieval_timings().await;

        // doc 级判定（去重，保持顺序）
        let mut seen = HashSet::new();
        let ranked: Vec<String> = hits
            .iter()
            .map(|h| h.doc_name.clone())
            .filter(|d| seen.insert(d.clone()))
            .collect();
        let rel: HashSet<&str> = relevant.iter().map(|s| s.as_str()).collect();

        recalls[0].push(recall_at_k(&rel, &ranked, 5));
        recalls[1].push(recall_at_k(&rel, &ranked, 10));
        recalls[2].push(recall_at_k(&rel, &ranked, args.topk as usize));
        mrrs.push(mrr(&rel, &ranked));
        ndcgs5.push(ndcg_at_k(&rel, &ranked, 5));
        ndcgs10.push(ndcg_at_k(&rel, &ranked, 10));

        println!(
            "[benchmark] {} | {:.1}ms | hits={} | recall@10={:.2} | {}",
            q.id,
            latency.as_millis(),
            ranked.len(),
            recalls[1].last().copied().unwrap_or(0.0),
            q.query
        );
        if let Some(t) = timings {
            println!(
                "[benchmark]   → planner={}ms dense={}ms bm25={}ms symbol={}ms rrf={}ms rerank={}ms finalize={}ms",
                t.planner_ms, t.dense_ms, t.bm25_ms, t.symbol_ms, t.rrf_ms, t.rerank_ms, t.finalize_ms
            );
        }
    }

    // 🟠 M24：汇总分母必须用「实际参与评测的查询数」——标注缺失/为空的查询在
    // 上方被 continue、未推值；旧实现用 queries.len() 作分母会把所有均值系统性稀释。
    // latencies 与 recalls 在同一循环迭代内推值，计数一致，统一用 evaluated。
    let evaluated = recalls[0].len();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / evaluated.max(1) as f64;
    println!("\n===== 汇总（{} 条查询，其中 {} 条参与评测）=====", queries.len(), evaluated);
    println!("Recall@5   : {:.3}", mean(&recalls[0]));
    println!("Recall@10  : {:.3}", mean(&recalls[1]));
    println!("Recall@{}  : {:.3}", args.topk, mean(&recalls[2]));
    println!("MRR        : {:.3}", mean(&mrrs));
    println!("NDCG@5     : {:.3}", mean(&ndcgs5));
    println!("NDCG@10    : {:.3}", mean(&ndcgs10));
    let avg_lat = latencies.iter().sum::<f64>() / latencies.len().max(1) as f64;
    let p95 = {
        let mut l = latencies.clone();
        l.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        l.get((l.len() as f64 * 0.95) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    println!("Latency avg: {:.0}ms / p95: {:.0}ms", avg_lat, p95);
    if !missed.is_empty() {
        println!("\n[benchmark] 注意: {} 条查询缺少标注（不计入指标）: {:?}", missed.len(), missed);
    }
    Ok(())
}
