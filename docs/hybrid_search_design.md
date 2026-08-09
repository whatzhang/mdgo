# 知识库混合检索 —— 技术设计文档（存档）

> 本文档为 **[RETRIEVAL_LOGIC_CHECKLIST.md](./RETRIEVAL_LOGIC_CHECKLIST.md)**（逻辑契约）的配套解释文档：
> 逻辑清单回答"**标准是什么**"（编号契约 + 禁止项），本文档回答"**为什么这么设计**"（动机、原理、权衡）。
> 两者共同构成检索链路的存档标准。**后续任何更改：先对照逻辑清单，再参考本文档理解设计意图。**

- 状态：**已生效（2026-08-09）**
- 适用范围：`tauri/src-tauri/src` 检索链路全栈（pipeline + 下游消费方）
- 对应逻辑契约：RETRIEVAL_LOGIC_CHECKLIST.md 全部编号（ARCH / FLOW / RECALL / FUSE / THRESHOLD / SCORE / RERANK / DIVERSITY / CTX / CONFIG / FAILOVER / MODEL）

---

## 1. 背景与问题定义

### 1.1 重构前的精度问题（阶段③调研结论）

| # | 缺陷 | 现象 | 根因 |
|---|---|---|---|
| P0 | 融合分数不可比 | 向量/BM25 分数尺度不同，某一路异常分数压制另一路 | `alpha*vec + (1-alpha)*bm25` 线性加权要求两路分数物理可比，实际不可比 |
| P0 | 检索顺序错误 | 先全量检索再过滤，候选池大、噪声多、精排成本高 | Retrieve→Fusion→Filter 顺序，过滤信息（意图/扩展名白名单）未前置 |
| P1 | OR 语义过宽 | 长查询只命中一个词的低相关文档也被召回 | QueryParser 默认词间 OR |
| P1 | 无绝对阈值 | 语义噪声直接进入上下文 | 相对自适应阈值（max×0.3/0.5）在分数整体偏低时放水 |
| P2 | 无精排 | 余弦高但语义不相关的误匹配无法校正 | 无 cross-encoder 最后裁决层 |
| P2 | 无聚簇 | 同一文档大量 chunk 挤占上下文，多样性差 | 无 OPML 层级去重、无每文档 chunk 上限 |

### 1.2 业界对标

- **Azure AI Search / Elasticsearch / Weaviate** 混合检索均默认 **RRF（Reciprocal Rank Fusion）k=60**：只依赖排名而非分数，对分数尺度完全鲁棒。
- **Cross-Encoder 精排**是向量检索精度的标准最后一公里：Bi-Encoder（嵌入）负责召回，Cross-Encoder 负责精排裁决。
- 本项目对齐以上两者，并保留 `fusion_alpha` 配置作为**每路权重偏置**（兼容既有配置语义，不破坏前端契约）。

### 1.3 重构目标

业务检索精度要高（召回准、过滤狠、精排精），且全链路逻辑闭环（pipeline 内部自洽 + 下游消费方按统一契约裁决）。

---

## 2. 总体架构（ARCH-1 ~ ARCH-4）

### 2.1 五层管线

```
Query Understanding ─► Multi-Recall ─► RRF Fusion ─► Threshold + Rerank ─► Diversity ─► Context
   (RuleQueryPlanner)   (Vec‖BM25‖Symbol)   (rank-based)   (双阈值/精排)     (去重/聚簇)   (窗口合并)
```

**关键顺序约束（违反即架构回退）**：
1. **Filter 前置**（P0）：检索前由 `QueryPlan.allowed_exts` 生成过滤条件，向量路 LanceDB `only_if` SQL 预过滤、BM25 路内存过滤、符号路内存过滤。**禁止回到 Retrieve→Fusion→Filter**。
2. **阈值在融合后**：融合后才产生统一排序，阈值才有统一语义。
3. **精排在阈值后**：先粗过滤噪声（收敛候选集），再精排（成本可控）。
4. **Diversity→Context→take(top_k)**：先去重聚簇，再注入上下文窗口，最后截断。

### 2.2 模块分解与依赖倒置（SOLID）

| 模块 | 职责（SRP） | 契约 |
|---|---|---|
| `core/search/query_plan.rs` | 查询理解：意图路由 + 符号提取 → `QueryPlan` | `QueryPlanner` trait：`plan(&str) -> QueryPlan` |
| `core/search/rrf.rs` | 多路融合：rank-based 加权 RRF | `rrf_fuse(vec, bm25, symbol, &RrfConfig) -> Vec<SearchHit>` |
| `core/search/rerank.rs` | 精排：本地 cross-encoder + Broker 加载 | `Reranker` trait：`rerank(&str, &[SearchHit], f32) -> Result<Vec<SearchHit>, String>` |
| `core/indexer.rs` | 管线编排（`hybrid_search`）+ 上下文窗口 + OPML 去重 | 无外部依赖抽象，直接消费上述三个模块 |
| `core/agent/mod.rs` | 下游聚合（`aggregate_hits`）+ 上下文文本（`build_context_text`） | 按 **SCORE 分数域契约** 三域裁决 |
| `core/commands/llm.rs` | 命令层编排 + 技能参数覆盖（`effective_*`） | 精排阈值只取全局（CONFIG-3） |

**依赖方向**：`indexer → {query_plan, rrf, rerank}`（经 trait 倒置）；`agent/mod.rs ← indexer 输出`（消费方）。`hybrid_search` 不接收 `intent` 参数（ARCH-4）：意图唯一来源是管线内部 `RuleQueryPlanner.plan()`，杜绝双信息源漂移。

### 2.3 演进路线（query_plan.rs 头部注释）

规则路由不废弃，逐步增强：`RuleRouter → Rule + QueryPlan → LLM Planner`（预留）。当前为第一阶段（零 LLM 开销的规则路由），检索链路独立于 LLM 网关工作，避免每次检索叠加 LLM 延迟与失败点。

---

## 3. 查询理解层（RECALL / FLOW）

### 3.1 意图路由（`route_intent`）

`RetrievalIntent` 四态：`Code / Document / Outline / General`。判定优先级：**Code → Outline → Document → General**。

- **Code 判定**（`is_code_query`）：代码语法特征（`::`/`->`/`()`）、CamelCase/snake_case 标识符、代码文件扩展名 + 代码关键词、中文"知识库中的代码"短语。
- **Outline 判定**：仅使用具象大纲格式词（opml/freemind/思维导图/大纲笔记等），避免泛化词"大纲"把普通文档查询误路由。
- **Document 判定**：仅使用名词性文档词（readme/markdown/文档/笔记/文章），避免动词性"说明/解释"把中文代码提问误路由为文档。

> 设计动机：意图决定候选文件白名单（元数据过滤），是 Filter 前置的决策输入。规则启发式足够区分常见查询类型，且零模型开销。

### 3.2 符号提取（`extract_symbol_tokens`）

仅 Code 意图执行：提取 CamelCase / snake_case 标识符 token（过滤纯数字、中文字符），供符号路召回。上限 3 个符号、每符号取前 5 条（RECALL-6）。

---

## 4. 多路召回层（RECALL）

三路并行召回，互不依赖：

### 4.1 向量路（语义）

- `search_vectors_with_filter(query_vector, vec_k, only_if_sql)`：LanceDB 查询层 SQL 预过滤（`LOWER(doc_name) LIKE '%.ext'`），IVF-SQ 向量索引 + Cosine 距离。
- 无意图白名单时走 `search_vectors`（全量）。
- 候选池 `vec_k = candidate_k.max(top_k)`（默认 100）。

### 4.2 BM25 路（关键词，msm 严格语义）

- 生产检索**唯一**入口 `search_with_plan(query, bm25_k, msm_ratio)`。旧宽松 OR 的 `Bm25Index::search` 已删除（RECALL-2）。
- **切词**（`segment_query_terms`）：jieba 中文（hmm=true 未知词识别）+ ASCII token 化 + 保守停用词过滤 + 去重 + 小写化，与索引侧分词器对齐（RECALL-4）。
- **msm 查询结构**（`build_msm_query`）：`OR(词1, 词2, ..., 词n)`，其中每词 = `OR(text/title/heading/symbol_name/file_path 的 TermQuery)`，任一字段命中即算该词命中；词间最低命中数 `min_should = ceil(n × msm_ratio).max(1)`（RECALL-3）。
- **字段 boost**（固定）：title 3.0 / heading 2.5 / symbol_name 2.0 / file_path 1.0 / text 1.0。
- **单/零词退化**：`terms.len() <= 1` 时走 QueryParser 宽松 OR，避免"唯一词被 msm 过滤"（RECALL-5）。
- **BM25 分数归一化**：`score_bm25 = raw / 本批最高分`，clip [0,1]。
- **内存过滤**：tantivy 无 SQL 级过滤，msm 检索后按 `allowed_exts` 内存过滤（FLOW-1）。

### 4.3 符号路（Code 意图专用）

- 触发条件：`intent == Code && !symbols.is_empty()`。
- 多符号 `join_all` 并行召回，每符号 `search_symbols(symbol, 5)`，最多 3 个符号（RECALL-6）。
- **质量分级**（RECALL-7）：精确 0.95 / 前缀 0.85 / 包含 0.7，同名逐条递减 0.02，下限 0.1。该分数仅用于符号路召回排序，**不进 RRF 分数域**（`score_vec`/`score_bm25` 保持 0）。
- **扩展名过滤**：与 BM25 路一致，按 `allowed_exts` 内存过滤。

---

## 5. RRF 融合层（FUSE）

### 5.1 为什么 RRF 而非线性加权

向量余弦分数（0~1 窄带）与 BM25 分数（最高分恒 1.0、长尾极陡）**物理意义不可比**。线性加权会让某一路异常分数压制另一路，且对语料分布敏感。RRF 只依赖**排名**，对分数尺度完全鲁棒。

### 5.2 加权 RRF（保留 fusion_alpha 语义）

```
score(doc, chunk) = Σ weight_route × 1/(k + rank_route + 1)
  weight_vec = alpha（默认 0.6）  weight_bm25 = 1-alpha（默认 0.4）  weight_symbol = 1.0
  k = rrf_k（默认 60）
```

- 按 `(doc_name, chunk_index)` 合并各路贡献（FUSE-4）。
- **RRF 分数归一化**：除以本批最高分 → [0,1]，供无精排阶段排序与阈值使用。
- `score_vec` / `score_bm25` 保留各路原始分（供阈值判据，非排序主键）。
- 符号路命中**不写**向量/BM25 分（FUSE-5），其存活由 `symbol_name` 信号承载。

### 5.3 alpha 动态计算（`compute_alpha`）

```
alpha = base_alpha + intent_delta，clamp [0.3, 0.95]
  intent_delta：Code −0.2 / Document +0.1 / Outline +0.05 / General 0
再按查询长度微调：
  短查询（ASCII 词 ≤2 且中文 ≤6 字）→ −0.2，clamp [0.2, 1.0]
  长查询（ASCII 词 ≥5 或中文 ≥14 字）→ +0.1，上限 0.95
```

- **设计动机**：代码查询偏 BM25（符号/标识符精确匹配更可靠）；短查询依赖关键词精确匹配；长查询语义丰富偏向量。CJK 按字符数计，避免整句中文被计为 1 token 全部落入"短查询"分支。

### 5.4 融合元数据合并规则（FUSE-6）

Vec 路覆盖 text 并补全 path_json/sentence_window/chunk_type；Bm25 路仅填充空缺；Symbol 路不改写元数据（向量路文本为基准，避免覆盖更完整文本）。

---

## 6. 阈值体系与分数域契约（THRESHOLD / SCORE）

### 6.1 三阈值协调语义

| 阈值 | 默认 | 域 | 语义 | 裁决时机 |
|---|---|---|---|---|
| `vec_min_score` | 0.35 | 原始余弦 | 过滤**无监督语义噪声**（无 BM25/符号佐证、仅向量召回的命中） | pipeline 步骤 4 |
| `rerank_min_score` | 0.2 | sigmoid | **cross-encoder 相关性概率**判定门槛 | pipeline 步骤 5 + 下游 sigmoid 域 |
| `min_score` | 0.3 | RRF 归一化 | 融合兜底阈值（精排未激活/失败回退时的质量安全网） | 下游 aggregate_hits RRF 域 |

### 6.2 阈值协调（THRESHOLD-3，关键设计）

精排激活（`reranker_enabled && is_reranker_cached()`）时，**向量候选不按 `vec_min_score` 提前砍**：

```
rerank_active = reranker_enabled && is_reranker_cached()
pipeline 过滤条件（顺序）：
  score_bm25 > 0 || symbol_name.is_some() || rerank_active || score_vec >= vec_min_score
```

- 动机：余弦低但 cross-encoder 判定高度相关的候选不应被无监督阈值误杀。相关性裁决整体移交精排 sigmoid 阈值。
- 语义边界：该条件仅过滤"三路皆无信号"的纯语义噪声，不承担相关性裁决职责。

### 6.3 分数域契约（SCORE-1 ~ SCORE-4，核心创新）

`SearchHit.score` 在整个链路承载三种域，**域由字段自描述**：

| 特征 | 域 | 说明 |
|---|---|---|
| `score_rerank: Some(s)` | sigmoid 域 | 精排激活，`score == s` |
| `score_rerank: None` | RRF 归一化域 | 精排未激活 / 失败回退 |
| `score_vec` / `score_bm25` | 原始分（恒在） | 各路的原始 cosine / 归一化 BM25 分，供阈值判据，非排序主键 |

**为什么需要契约**：pipeline 输出后，下游消费方（`aggregate_hits`）必须知道每个命中来自哪个分数域才能选对阈值。若下游用 RRF 域阈值（0.3）去裁决 sigmoid 域分数（通常 0.1~0.6），会系统性误杀/放水。契约用字段存在性自描述域，杜绝跨域阈值误判。

**契约禁止项**（SCORE-3/4）：
- 禁止在 pipeline 内 `score_rerank` 之外另造"精排标记"；
- 禁止下游用 `score_rerank` 以外字段推断精排状态；
- 精排失败回退时不得残留部分精排分数（单次检索内域必须统一）。

### 6.4 下游三域裁决（SCORE-2，唯一权威实现）

`aggregate_hits(all_hits, min_score, rerank_min_score, max_docs, max_chunks_per_doc)`：

| 命中特征 | 裁决阈值 |
|---|---|
| `score_rerank.is_some()` | `rerank_min_score`（精排模型判定门槛，幂等兜底） |
| `symbol_name.is_some()` 且未精排 | **完全放行**（符号强信号；符号路召回已按质量分级截断，RRF 归一化分无判别力） |
| 其余（RRF 域） | `min_score`（融合兜底） |

**裁决后的管道**：按 doc+chunk 去重保留最高分 → 按 doc_name 分组 → 文档代表分 = 最佳 chunk 分 → 文档内按分数降序截断 `max_chunks_per_doc` → 文档按代表分降序（同分按 doc_name 字典序保证确定性）→ 取 top `max_docs`。

> 设计动机：绝对阈值替换旧"相对自适应阈值"（max×0.3/0.5 在分数整体偏低时放水、偏高时误杀）。融合分数已归一化 [0,1]，绝对阈值有确定语义。

---

## 7. 精排层（RERANK）

### 7.1 模型与输入构造

- 模型：`Xenova/bge-reranker-base`（cross-encoder，XLM-RoBERTa 架构，ONNX 导出版）。
- 输入：`query` + `passage` 拼接为 pair，`passage = doc_name + "\n" + text`。
  - **设计动机（RERANK-2）**：文件名是文档主题强信号，前缀拼接把它 feature 化进入模型输入，替代旧"文件名事后加分"逻辑（后者是手工启发式，无法学习）。

### 7.2 推理与过滤

- `BATCH_SIZE = 16`；分组按序列长度降序（长度相近的 pair 一组，降低 padding 浪费）；截断到 `max_position_embeddings`（config.json 读取，默认 512）。
- 输出 sigmoid → 相关性概率（0,1）；`score < rerank_min_score` 的候选被丢弃；输出按 sigmoid 降序。
- 精排输出覆盖 `score = sigmoid` 且写入 `score_rerank = Some(s)`（SCORE-4）。

### 7.3 Session 单例（RERANK-5）

`LocalBgeReranker` 为无状态结构体；Session 为全局单例（`GLOBAL_SESSION`，Mutex + 双检锁，参考 embedding.rs）。tokenizer 按线程缓存（thread_local），避免每次检索重建。

---

## 8. 多样性层（DIVERSITY）

1. **OPML 层级去重**（`dedup_opml_hierarchy`）：同一大纲文档中，路径前缀关系（`is_path_prefix`，JSON 反序列化保证正确性）的 chunk 保留最深节点。**仅对 `.opml`/`.mm` 生效**——Markdown 语义分块父子节内容互斥，去重会误删合法父节 chunk。
2. **文件聚簇**：每文档最多保留 `max_chunks_per_doc`（默认 3）个 chunk（按融合/精排分数降序）。
3. **截断**：`take(top_k)`。

---

## 9. 上下文构建层（CTX）

- **窗口计算**（`compute_context_window`）：词数 ≤3 → 3、≤10 → 2、否则 1。短查询需要较大上下文定位，长查询已够具体 → 小窗口。
- **区间查询**（`fetch_chunks_between`）：按 doc_name 分组，同文档的多个命中 chunk 合并为**一次**区间查询（区间并集），`only_if(doc_name=...)` 预过滤 + 零向量 Cosine + limit 5000；多文档 `join_all` 并行。把"N 次串行全表扫描"降为"唯一文档数 次并行单文档查询"。
- **子窗口提取**：命中 chunk 取 ±window 子窗口；OPML 父节点（`is_path_prefix`）检测并入。
- **写入条件**：仅当合并文本比原 text 长 10 字符以上才写 `sentence_window`（避免无效扩展）。
- **下游文本组装**（`build_context_text`）：文档顺序保持聚合输出（分数降序）；文档内按 `chunk_index` 阅读序重排（保证连贯可读）；优先 `sentence_window`；总长 ≤ `MAX_CONTEXT_CHARS = 12_000`。

---

## 10. 配置体系（CONFIG）

### 10.1 配置默认值及依据

见 RETRIEVAL_LOGIC_CHECKLIST.md CONFIG-1 表格（本项目实现为 `core/config.rs` 的 `IndexerConfig::default()`）。要点：

| 字段 | 默认 | 设计依据 |
|---|---|---|
| `fusion_alpha` | 0.6 | 语义为主、关键词为辅；配合 alpha 动态微调 |
| `candidate_k` | 100 | 召回候选池规模（Filter 前置后的检索上限），覆盖 top_k 多数场景 |
| `rrf_k` | 60 | Azure / Elasticsearch / Weaviate 通用取值 |
| `vec_min_score` | 0.35 | 无监督语义噪声底线（bge-small-zh 余弦经验分布） |
| `rerank_min_score` | 0.2 | bge-reranker-base sigmoid 经验阈值（高于此才算"相关"） |
| `bm25_msm_ratio` | 0.6 | 词间命中比例：至少 60% 查询词命中才进入候选 |
| `min_score` | 0.3 | RRF 归一化域兜底 |
| `reranker_enabled` | true | 默认启用精排；模型未就绪自动降级（FAILOVER） |

### 10.2 参数校验（CONFIG-2）

分数/比例类 clamp [0,1]（`fusion_alpha`/`vec_min_score`/`rerank_min_score`/`bm25_msm_ratio`）；`candidate_k ≥ 10`、`rrf_k ≥ 1`、`max_context_docs ≥ 1`、`max_chunks_per_doc ≥ 1`。

### 10.3 技能覆盖（CONFIG-3）

- `effective_min_score = skill_ctx.min_score.unwrap_or(kb_cfg.min_score)`（技能可放宽 RRF 域兜底）；
- `effective_rerank_min_score = kb_cfg.rerank_min_score`（**精排阈值不支持技能覆盖，只取全局**）——精排阈值是模型判定门槛，业务放水不应改模型裁决标准。

---

## 11. 降级策略（FAILOVER）

| 触发 | 降级路径 | 质量安全网 |
|---|---|---|
| 精排推理失败 / 任务异常 | 回退 RRF 排序候选（`candidates`） | 下游 RRF 域 `min_score` |
| 模型未缓存（`reranker_enabled` 且未就绪） | 后台触发一次下载（进程内单飞 + 失败 120s 防抖），本次回退 RRF | 同上 |
| 向量检索失败 | 退化为纯 BM25 | 下游兜底 |
| BM25 检索失败 | 退化为纯向量 | 下游兜底 |
| 符号检索失败 | 忽略（仅丢符号佐证） | 符号路为增强路，不阻断 |

**核心原则**：检索**永不阻断**（FAILOVER-1）；降级只影响召回质量，不绕过下游过滤（FAILOVER-4）——`aggregate_hits` 按分数域兜底（RRF 域 `min_score`）是降级路径的最终质量防线。

---

## 12. 模型与 Broker（MODEL）

### 12.1 模型来源与完整性

- `model_download::ensure_reranker_downloaded` 统一流程：**ModelScope → hf-mirror → HuggingFace** 多源直链下载（非 zip 打包），带浏览器 UA（绕 Tengine UA ACL 黑名单）。
- 缓存目录 `{root}/bge-reranker-base/`，必需文件 `model.onnx` / `tokenizer.json` / `config.json`。
- `is_reranker_cached()` = 三文件齐全（磁盘级检查，MODEL-3）。

### 12.2 Broker 平台分流（MODEL-2）

| 平台 | 后端 | 策略 |
|---|---|---|
| Windows | ONNX Runtime + DirectML | GPU 优先，`commit_from_file` 失败回退 CPU 重建 Session |
| macOS Apple Silicon | ONNX Runtime + CoreML | 同上 |
| Intel Mac / Linux | tract-onnx | 纯 CPU 推理 |

GPU 执行提供者初始化失败（虚拟机、无驱动、远程桌面）时检索仍可用，仅精排速度下降。与 embedding.rs 的 Broker 模式保持一致。

---

## 13. 全链路闭环验证（消费方视角）

```
indexer.hybrid_search ──► Vec<SearchHit>（score 携带域信息）
        │
        ├─► [kb_search 工具] ──► aggregate_hits(min_score, rerank_min_score, ...)
        │                              │  三域裁决（SCORE-2）
        │                              ▼
        │                        build_context_text → 模型上下文
        │                              │
        │                              └─► search_sink ──► rag:done 引用来源
        │
        └─► [预检索 Stage 2/3]（llm.rs）──► aggregate_hits(effective_min_score,
                                              effective_rerank_min_score, ...)
```

**闭环保证**：
1. pipeline 输出的每个命中，其分数域由字段自描述（SCORE-1）；
2. 所有下游消费方（kb_search 工具、预检索、code_lookup）共用唯一权威实现 `aggregate_hits`，按域裁决（SCORE-2）；
3. 精排阈值在 pipeline 与下游同源（都取 `config.rerank_min_score`），幂等兜底；
4. 任何降级路径最终都回落到 `aggregate_hits` 的质量安全网（FAILOVER-4）。

---

## 14. 关键权衡与已知限制

### 14.1 权衡

| 决策 | 取舍 |
|---|---|
| 规则路由而非 LLM Planner | 零模型开销、检索独立于 LLM 网关；代价是意图边界模糊的查询分类不准（预留演进） |
| RRF 而非线性加权 | 牺牲分数细粒度（只用排名），换取跨尺度鲁棒 |
| msm 严格语义 | 牺牲单关键词弱命中的召回，换取精确率大幅提升；单/零词退化兜底 |
| 精排让位 vec_min_score | 候选集略大（精排成本略增），换取不误杀"余弦低但语义相关"的候选 |
| 纯本地推理 | 隐私 + 离线可用；代价是首次需下载模型（后台下载 + 降级兜底） |

### 14.2 已知限制

- `is_reranker_cached()` 为磁盘级检查，不校验模型与库版本兼容性（推理失败自动回退，FAILOVER-1 兜底）。
- BM25 路内存过滤发生在 msm 检索之后，候选池已被 `bm25_k` 截断，极端情况下白名单外文件可能挤占候选名额（召回损失有界）。
- `extract_symbol_tokens` 仅识别 CamelCase/snake_case，下划线标识符在 BM25 路被拆词、依赖符号路兜底（`lru_cache` → `lru`/`cache`）。
- 测试模块（`#[test]`）当前已清理，无自动化回归保障；后续恢复测试须对齐 RETRIEVAL_LOGIC_CHECKLIST.md 契约。

### 14.3 演进方向（预留，不在此标准内实现）

- 查询理解第三阶段：LLM Planner 生成结构化 `QueryPlan`（同一结构，平滑替换 RuleQueryPlanner）。
- 精排模型的规模化（更大 reranker）与批量调度优化。
- 符号路质量分数的语义化（与 RRF 域的可比化改造需先更新 SCORE/FUSE 契约）。

---

## 15. 与逻辑清单的对应关系速查

| 技术设计章节 | 对应逻辑契约编号 |
|---|---|
| §2 总体架构 | ARCH-1 ~ ARCH-4 |
| §3 查询理解 | RECALL-4/5/6，FLOW-1 |
| §4 多路召回 | RECALL-1 ~ RECALL-7 |
| §5 RRF 融合 | FUSE-1 ~ FUSE-6 |
| §6 阈值与分数域 | THRESHOLD-1 ~ THRESHOLD-5，SCORE-1 ~ SCORE-4 |
| §7 精排 | RERANK-1 ~ RERANK-5 |
| §8 多样性 | DIVERSITY-1 ~ DIVERSITY-3 |
| §9 上下文 | CTX-1 ~ CTX-5 |
| §10 配置 | CONFIG-1 ~ CONFIG-3 |
| §11 降级 | FAILOVER-1 ~ FAILOVER-4 |
| §12 模型与 Broker | MODEL-1 ~ MODEL-3 |

---

*本文档与 RETRIEVAL_LOGIC_CHECKLIST.md 互为存档标准。改代码前先对照逻辑清单逐条打勾，改设计时同步更新本文档。*
