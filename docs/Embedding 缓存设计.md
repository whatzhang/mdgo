# Embedding 缓存设计

最后更新：2026-08-23

> 两级 embedding 缓存（P0-5 / B1）：**文档侧持久缓存**（SQLite，磁盘）加速增量索引、
> **查询侧进程内缓存**（内存）加速重复查询。目标：增量索引只对**内容变化**的 chunk 重新推理。
> 另附三级"缓存键对比表"（含精排缓存），并如实记录已知缺陷。

---

## 1. 总览

```text
┌─ 文档侧持久缓存 ─────────────────────────────┐
│  {kb}/.mdgo/embedding_cache.sqlite（SQLite 单表）│
│  键 = model|dimension|content_hash             │
│  上限 100k 条 ≈ 150MB（按 created_at 最旧裁剪）│
└────────────────┬───────────────────────────────┘
                 │ embed_chunks（core/pipeline.rs）
                 ▼
文件修改 → 重新分块 → 内容哈希 → 缓存命中（未变化）→ 跳过 embedding
                              → 未命中 → 只推理变化 chunk → 写回缓存

┌─ 查询侧进程内缓存 ────────────────────────────┐
│  core/db/query_embedding_cache.rs              │
│  键 = 原始查询文本的 FNV-1a 128 哈希           │
│  容量 512，FIFO 淘汰（全局单例，懒初始化）      │
└────────────────┬───────────────────────────────┘
                 │ call_embedding_query / call_embedding_queries
                 ▼
重复/近似查询 → 零推理；未命中 → 加 BGE instruction 前缀后批量推理并回填
```

---

## 2. 文档侧持久缓存（`core/db/embedding_cache.rs`）

### 2.1 存储与键

- 存储：`{kb}/.mdgo/embedding_cache.sqlite`，单表 `embedding_cache(cache_key TEXT PRIMARY KEY, vector BLOB, created_at INTEGER)`；
- 键 = `model|dimension|content_hash`：
  - `model|dimension` 前缀来自 `embedding::get_model_name()` / `get_embedding_dimension()`；
  - `content_hash` = **最终送进 embedding 的文本**（优先 `embedding_text`，退化 `text`）的
    稳定 FNV-1a 128 十六进制哈希（`core/db/utils.rs::stable_hash_hex`）；
- **自然失效**：模型/维度变化 → 前缀变化 → 键变化；分块参数变化 → 文本本身变化 →
  哈希变化 → 键变化。**缓存正确性不依赖人工失效**。

### 2.2 上限与裁剪

- `CACHE_MAX_ENTRIES = 100_000`（注释估算：~100k × 384-dim × 4B ≈ 150MB）；
- 写入用 `INSERT OR REPLACE`（同键覆盖，`created_at` 刷新为当前时间 → 近似 LRU）；
- 超上限时按 `created_at ASC` 删除最旧的超出部分。

### 2.3 清索引同步清缓存

- `indexer.rs::clear_inner`（`index_all` / `index_file` / `index_unindexed` 清索引共用）在
  清 LanceDB + BM25 + meta 之后，同步 `EmbeddingCache::open(...).clear()` 清空缓存
  （`indexer.rs:776-783`；失败仅 warn，不阻断）。

### 2.4 接线点：`core/pipeline.rs::embed_chunks`

```text
chunks → texts（embedding_text 优先）→ 计算全部缓存键
       → get_many 批量查询 → 命中项跳过推理
       → 仅对 miss 文本 spawn_blocking 批量推理（utils::call_embedding）
       → 按原输入顺序组装（命中取缓存、miss 取新向量）
       → 写回缓存（失败仅 warn，不影响本次索引）
```

- 全部命中时直接跳过推理（日志：`全部 N 条命中缓存，跳过推理`）；
- `cache_dir` 为空（未传 `utils::get_cache_dir(dir_path)`）时降级全量推理。

---

## 3. 查询侧进程内缓存（`core/db/query_embedding_cache.rs`）

- 键 = **原始查询文本**的 FNV-1a 128 哈希（`fnv1a_128(query.as_bytes())`）——
  **不缓存 BGE instruction 前缀后的文本**，保证同一用户查询可命中；
- 容量 `CACHE_CAPACITY = 512`，满时 **FIFO** 淘汰（查询侧调用频率低，Mutex 足够）；
- 全局单例：`global_query_embedding_cache()`（`OnceLock` 懒初始化）；
- 接入：
  - `utils.rs::call_embedding_query`：先查缓存，命中直接返回；未命中加
    `BGE_QUERY_INSTRUCTION` 前缀推理后按**原始文本**写回；
  - `utils.rs::call_embedding_queries`（预检索多查询路径）：逐条查缓存（保持输入顺序），
    仅对未命中项合一批推理（`call_embedding` 内部 BATCH_SIZE=128），逐条回填。

---

## 4. 设计要点

1. **缓存正确性不依赖人工失效**：模型/维度/内容三者任一变化都会改变键，天然失效；
2. **增量索引只对内容变化的 chunk 重新推理**：未变 chunk 命中缓存直接复用向量；
3. **chunk id 同源改为稳定内容哈希**（`core/db/utils.rs::build_document_chunks`）：
   - id = `rel_path#hash`，哈希输入 =
     `CHUNK_IDENTITY_VERSION | rel_path | chunk_index | text | embedding_text | path_json | symbol_name | tags`；
   - 同内容同 id（幂等：重复索引产出相同 id）；tags 参与哈希——文档标签变化 → 检索行为变化 →
     chunk 身份随之更新；
   - 这是 embedding 缓存（按内容哈希）成立的前提：索引身份与缓存键同源，增量路径才能对齐。

---

## 5. 已知缺陷（如实记录）

| # | 位置 | 问题 | 影响 |
|---|---|---|---|
| 🔴-1 | `core/pipeline.rs:321-331` | **缓存回填 zip 错位**：`keys`（全部 texts 的键，长度 N）与 `new_vectors`（仅 miss 向量，长度 M）按位置 zip 配对——`keys[0..M]` 中大部分可能是命中 key，被错误配对写入 | 混合命中批次（增量索引典型场景）会**污染缓存**：正确条目被错误向量覆盖、新向量没写入；下次索引命中错误向量并写入 LanceDB → 检索静默返回语义错误向量，且无法自愈（错误条目永远"命中"）。详见《代码审查报告-RAG全链路P0批次.md》🔴-1（修复：按 `miss_indices` 配对；修复后需清理存量 `embedding_cache.sqlite`） |
| M19 | `query_embedding_cache.rs` + `embedding_cache.rs:46-50` | 磁盘缓存键含 `model\|dim\|hash`，查询侧进程内缓存键**仅** = 查询文本哈希；磁盘缓存 `model_key` 依赖 `get_model_name()` 回退值（模型未初始化时写回退前缀 → 永久 miss） | 模型原地替换后查询向量与文档向量可能来自不同模型；首轮索引批次缓存永久 miss（纯性能浪费）。详见审查报告 M19 |
| — | `embedding_cache.rs:get_many` | `IN (?,?,...)` 占位符数量 = keys 长度，**可能超过 SQLite 变量上限**（大批次时查询失败） | 大索引批次 get_many 报错 → 走 `unwrap_or_default()` 空结果 → 全量推理（功能正确、性能退化）；建议分批查询 |

> **使用建议**：🔴-1 修复前，慎用文档侧缓存处理"部分命中"的增量批次（`index_all` 首轮全 miss
> 恰好不触发；`index_file`/`index_unindexed` 增量路径是高风险场景）。

---

## 6. 附：缓存键对比表

| 缓存 | 位置 | 键 | 淘汰/上限 | 持久性 |
|---|---|---|---|---|
| 磁盘 embedding 缓存 | `core/db/embedding_cache.rs` | `model\|dimension\|content_hash`（文本稳定 FNV-1a 128） | 100k 条 ≈150MB，按 `created_at` 最旧裁剪（INSERT OR REPLACE 近似 LRU） | SQLite 持久（`{kb}/.mdgo/`） |
| 查询 embedding 缓存 | `core/db/query_embedding_cache.rs` | 原始查询文本哈希（**不含** BGE instruction 前缀） | 512 条，FIFO | 进程内（全局单例） |
| 精排分数缓存 | `core/search/rerank.rs:53-55` | `fnv1a_128(query + "\n" + doc_name + ":" + chunk_index)` | 2048 条，FIFO | 进程内（全局单例） |

**三键策略不一致（审查发现 M12/M19）**：

- 磁盘缓存键含模型/维度前缀（内容隔离），查询缓存键只含文本（无模型隔离），精排缓存键
  含 query + doc 定位但**无知识库/内容隔离**——不同 KB 同名同序号 chunk 会串用精排分数，
  文档编辑重索引后仍可能返回旧内容分数（M12，建议键中加入 KB 路径 + chunk 内容哈希）；
- 三套缓存的失效模型各不相同（磁盘靠键自然失效、查询侧靠 FIFO、精排侧无失效），
  设计时需按各自的数据生命周期分别审视，不能假设"某处清了缓存就全清了"。
