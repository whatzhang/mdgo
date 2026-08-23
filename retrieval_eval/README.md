# Retrieval Benchmark（P0-3）

知识库检索质量的度量闭环。**每次修改 chunk / BM25 / RRF / reranker / query planner / metadata 后，
都应跑一遍本基准**，用 Recall / MRR / NDCG 判断改动是变好还是变差。

## 当前基准集（v1，42 条）

- **文件**：`queries.jsonl`（42 条查询）+ `expected.jsonl`（对应标注）
- **覆盖 9 类**：中文语义(10) / 英文(4) / 代码符号(10) / 精确查找(4) / 文件导航(4) / 跨文档(4) / 实体符号(3) / 标签主题(3)
- **难度分布**：easy 14 / medium 22 / hard 6
- **语料基准**：本仓库自身（KB 根 = `G:\gitProject\mdgo`），`relevant_documents` 为索引后的
  相对路径（`docs/xxx.md`、`tauri/src-tauri/src/core/xxx.rs`），**必须与 `doc_name` 精确一致**。

## 数据格式

`queries.jsonl`：

```json
{"id": "q001", "query": "为什么 embedding 后英文 chunk 数量特别多", "intent": "semantic", "difficulty": "medium", "language": "zh"}
```

`expected.jsonl`：

```json
{"query_id": "q001", "relevant_documents": ["docs/分块 Token 预算设计.md"], "relevant_chunks": []}
```

`intent` 取值：`semantic` / `code` / `exact` / `navigation` / `cross` / `entity` / `tag`。

## 标注原则（保证指标可靠）

1. `relevant_documents` = "**应该被召回的文档**"（Recall 目标），宁精勿滥——误标会直接污染指标；
2. 路径必须真实存在且与索引输出一致（`scan_directory` 的 `strip_prefix(base)` + 反斜杠转正斜杠）；
3. 跨文档类（cross）允许多个相关文档；单点类（exact/entity）只标 1-2 个最相关；
4. **标签类（tag）注意**：当前仓库语料无 frontmatter，tag 类查询命中依赖正文/标题主题词，
   tags 字段收益需用带 frontmatter 的语料单独验证；
5. 新增查询遵循覆盖矩阵（见 README 下方表格），避免集中在同一文件。

## 运行

```text
cd tauri/src-tauri
cargo run --bin benchmark --features bench -- --kb G:\gitProject\mdgo --queries ..\..\retrieval_eval\queries.jsonl --expected ..\..\retrieval_eval\expected.jsonl --topk 20 --reindex --yes-wipe
```

- `--features bench`：benchmark bin 依赖 `bench` feature（默认构建不暴露 core 层 API）。
- `--reindex`：先全量重建索引（修改分块/BM25/schema 后必须重建）；会清空目标目录
  `.mdgo` 的索引与 embedding 缓存——请先退出正在运行的应用，并加 `--yes-wipe` 显式确认。
- 输出：每查询明细（耗时/命中数/recall@10）+ 汇总（Recall@5/10/20、MRR、NDCG@5/10、Latency avg/p95）。

## 指标说明

- **Recall@K**：相关文档召回覆盖率（Recall 稳不稳，第一道关）
- **MRR**：首个相关文档排名倒数（"对不对"）
- **NDCG@K**：排序质量（相关文档是否排前面）
- **Latency**：全链路总耗时（含向量 + BM25 + RRF + reranker）

## 版本基线记录

| 日期 | 版本/改动 | Recall@10 | MRR | NDCG@5 | Latency avg | 备注 |
|---|---|---|---|---|---|---|
| - | 基线 v1（首跑） | 0.683 | 0.539 | 0.509 | 5644ms | 项目根 3567 文件 / 26 万 chunk；rerank 不截断占 90%+ 延迟 |
| - | rerank 截断 30 | 0.548 | 0.517 | 0.469 | 1169ms | 延迟大降但漏 RRF 30 名外相关文档 |
| - | **基线 v2（截断 50 + 基准集修正）** | **0.605** | **0.519** | **0.479** | **1290ms** | 延迟/精度折中；avg -77% vs 首跑 |

> 说明：语料为项目自身（含 css_js/cdn 大 min.js 与 5 万行 HTML 噪声）；真实笔记知识库指标预期更高。
> 已知局限：中文查询 vs 英文代码标识符（q042 类）——符号路只对 CamelCase/snake_case 查询词生效。
> tag 类查询需带 frontmatter 的语料才能体现 tags 字段/过滤价值（当前语料无 frontmatter）。
> **2026-08-23 语料变更**：docs/ 目录已全部改为中文命名（如 `分块 Token 预算设计.md`），
> `expected.jsonl` 的相关文档路径已同步更新；重新跑基准时请先 `--reindex` 并记录新基线。