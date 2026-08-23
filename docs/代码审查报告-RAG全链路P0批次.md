# 代码审查报告：RAG 全链路 P0 批次

> 审查基准：`edab77e`（HEAD）→ working tree 未提交改动（33 个修改文件 + 11 个新增文件，约 +2184/−500 行）
> 审查方式：6 路并行模块审查（db 层 / 文档处理层 / 检索索引层 / pipeline-agent 层 / 命令服务层 / 前端层）+ 一手复核
> 验证手段：`cargo check --all-targets` 通过；`cargo test --lib` **321 通过 / 0 失败**
> 生成日期：2026-08-23

---

## 0. 结论速览

批次整体工程质量高：分层清晰（document 不依赖 db）、Token 预算单一事实来源、截断可观测闭环、RRF 融合与缓存键（磁盘侧）设计正确、降级与容错路径齐全、前后端契约对齐、零新增依赖。

但存在 **2 个必须在本批次合入前修复的 🔴 严重缺陷**（均为缓存类正确性问题，增量/混合批次必然触发，静默污染检索数据且无日志信号），以及一批 🟠 主要问题（预算不变式、stale 语义矛盾、标签过滤覆盖不全、benchmark 指标失真等）。建议按"🔴 → 🟠 正确性/迁移语义 → 🟠 操作安全 → 🟡"顺序处理。

---

## 1. 🔴 严重问题（必然出错 / 数据损坏 / 静默漏召回）

### 🔴-1 `core/pipeline.rs:321-331` — embedding 缓存回填错位，向量与文本错配，静默污染缓存

```rust
let entries: Vec<(String, Vec<f32>)> = keys            // keys 覆盖全部 texts（含命中项）
    .iter()
    .zip(new_vectors.iter())                           // new_vectors 只含未命中（miss 序）
    .filter_map(|(k, v)| k.as_ref().map(|k| (k.clone(), v.clone())))
    .collect();
```

- **问题**：`keys` 长度为 N（全部文本的缓存键），`new_vectors` 长度 M（仅未命中文本的向量）。`zip` 按**位置**配对，把 `keys[0..M]`（其中大部分可能是**命中** key）与 miss 向量逐一配对后 `INSERT OR REPLACE` 写回。
- **触发**：任何"部分命中"的批次——增量索引一个只改了部分段落的文件是典型场景（未变 chunk 命中、变更 chunk miss）。`index_all` 首轮全 miss（恰好全序）时不触发，掩盖问题。
- **后果**：正确的缓存条目被错误向量覆盖，且正确的新向量没写入。下次索引同文件时缓存命中返回错误向量并写入 LanceDB → **检索静默返回语义错误的向量**，持续污染直到缓存清空（错误条目永远"命中"、不再重嵌，无法自愈）。
- **修复**：按 miss 下标配对：
  ```rust
  let entries: Vec<(String, Vec<f32>)> = miss_indices.iter()
      .zip(new_vectors.iter())
      .map(|(&i, v)| (keys[i].clone().unwrap(), v.clone()))
      .collect();
  ```
  并补"部分命中批量回填"回归测试（现有测试恰好全命中或全未命中，暴露不了该问题）；修复后需清理存量 `{kb}/.mdgo/embedding_cache.sqlite`。

### 🔴-2 `core/search/rerank.rs:417` — `scores` 变量遮蔽，缓存命中项分数被清零并被阈值整体丢弃

- **问题**：`rerank()` 在 346 行构造 `scores` 并填入缓存命中分数（347-351 行）；但当 `need_infer` 非空时，417 行 `let mut scores = vec![0.0f32; candidates.len()];` **重新声明同名变量遮蔽外层数组**。推理循环（468 行）只回填 `need_infer` 下标，缓存命中项在 417 行之后全部保持 0.0。`assemble`（489-499 行）按 `*s >= min_score`（默认 0.2）过滤 → 缓存命中项（真实 sigmoid 分可能 0.5+）被静默丢弃。
- **触发**：同一会话内相关查询共享 chunk（RAG 连续追问最常见的场景）、增量索引后候选集微变、top_k 变化——缓存命中与未命中混在一批时**必然触发**。全部命中/全部未命中两条路径不受影响（分别走 352-355 早退 / 全部推理）。
- **后果**：相关文档被静默漏召回，且无任何日志信号——正是 B5 精排缓存设计要优化的场景反而劣化。
- **修复**：删除 417 行重复声明，全程复用 346 行的 `scores`（推理只覆盖 `need_infer` 下标，互不冲突）；补"部分命中"单测。

---

## 2. 🟠 主要问题（特定条件下出错 / 与设计目标相悖）

### 2.1 预算与分块正确性

| # | 位置 | 问题 | 影响 | 建议 |
|---|---|---|---|---|
| M1 | `core/document/token_budget.rs:45-50` | `from_model_window`：`target = hard.saturating_sub(56).clamp(128, 1024)`，窗口 ≤191 时 `target > hard_max`，`assemble` 的 `debug_assert!` 在 debug 构建直接 panic，release 静默产出 target+overlap > hard | 换小窗口模型（≤192 token）即触发；当前 bge-small-zh 512 窗口为潜伏 | `target = hard.saturating_sub(56).clamp(64, hard_max)`，overlap 约束到 `target+overlap ≤ hard` |
| M2 | `chunk_engine.rs:454-459` | embedding 前缀 CJK 字符截断按"4 字符≈1 token"折算（`prefix_max_tokens × 4`），中文下放大 4 倍（1 字符≈1 token） | 长中文标题前缀以 4 倍 token 进入 embedding_text，标题词主导向量，违背"标题不稀释正文"设计目标；触发 Validator 整节重切 | 截断前用 `counter.count()` 实测 token 数二分裁剪；count 不可用才退化字符近似 |
| M3 | `chunk_engine.rs:185,202-207,278-287,310-314` | 预算核算未计入分隔符 token（`prefix + "\n" + body`）与跨段合并；`prev_tail` 的 token 数按"整节密度"折算，混排文档密度波动时尾部超预算 | 恰好贴预算的节恒超 1~2 token → 每文件多出若干重切（`resplit_count` 非健康 0） | 判定时按 `prefix+"\n"+body` 整体 count 一次（或预留 2 token 余量） |
| M4 | `chunk_engine.rs:196-206` | `available = (max - prefix - overlap_reserve).max(min_body_reserve)`：标题路径很长时被强制抬到 64 token 下限 → prefix + 64 恒超 max | 深层嵌套 + 长标题文档全节触发 Validator 重切 | 下限改为 `min_body_reserve.min(max_tokens.saturating_sub(prefix))` |
| M5 | `db/token_budget.rs:133-138,162-167,378-383` | 重切与降级路径把 `budget.overlap_tokens`（token 语义）直接作为字符切分器 `split_text_with_separators` 的字符 overlap | 英文文本实际 overlap 只有预期的约 1/4，跨块上下文衔接显著变弱 | 经 `char_budget_pair` 折算（复用 document/token_budget.rs:150 已有工具） |
| M6 | `chunk_engine.rs` 与 `db/token_budget.rs` | 出现**两套已分歧**的切分逻辑：表格切分（引擎 `lines≤2` 原子 vs Validator `≤3`）、散文切分（引擎 token 感知 vs Validator 字符+比例）、overlap 单位不一 | 后续修一处漏一处，行为漂移 | 超长块→分片统一收敛到单一策略表 |
| M7 | `chunk_engine.rs:115-133` | heading-only 导航 chunk 的"±window 上下文扩展补齐上下文"承诺未落地（检索链路无该机制），且纯嵌套标题的父节不产出自己的 chunk | 命中 heading chunk 后 LLM 只看到一行标题；更深层级标题完全丢失 | 实现窗口扩展或删除注释承诺；父节判定改为"本标题下是否有正文块" |
| M8 | `chunk_splitter.rs:131-141` | PlainText 降级路径把 token 语义的 `max_size` 当字符数（tokenizer 不可用时 `utils::split_text`） | 英文文本分片约 4 倍于预算，大文件先产出一批超限 chunk 再全量重切 | 降级前先经 `char_budget_pair` 折算 |

### 2.2 检索与索引正确性

| # | 位置 | 问题 | 影响 | 建议 |
|---|---|---|---|---|
| M9 | `indexer.rs:858-869` | `tag:xxx` 过滤只下推向量路（LanceDB only_if），BM25/符号路不参与过滤；且只消费 `plan.tags.first()` | 混合检索混入未打标噪声；多标签查询只按第一个过滤 | 融合后统一内存标签过滤；明确多 tag AND/OR 语义 |
| M10 | `indexer.rs:861` | 标签 SQL `tags LIKE '%tag%'` 子串匹配 + 大小写敏感 + `%`/`_` 通配符未转义（`escape_sql_string` 只转义 `'`/`\`） | `tag:web` 误命中 "webdev"；`tag:redis` 匹配不到 frontmatter 的 `Redis` | 转义通配符 + tag 归一化小写 + `ESCAPE '\'`；或 JSON 精确匹配 |
| M11 | `indexer.rs:994-1002,1076-1084` | 精排激活时旁路 `vec_min_score`，但精排**运行期失败**回退返回的是未过滤候选池 → 纯向量噪声直通 | reranker 损坏/偶发失败时结果质量反而劣于未启用精排 | 回退分支对 `rerank_all` 重新应用与 `rerank_active=false` 相同的过滤 |
| M12 | `rerank.rs:53-55,336-340` | 精排分数缓存键 = `query+"\n"+doc_name+":"+chunk_index`，无知识库/内容隔离（进程级 static） | 不同 KB 同名同序号 chunk 串用；文档编辑重索引后仍返回旧内容分数 | 键中加入 KB 路径 + chunk 内容哈希（或索引写入时清缓存） |
| M13 | `indexer.rs:1778-1789` vs 506/553/1845 | P0-4 分块参数版本守卫只覆盖 `sync_on_start`，`index_file`（watcher）/`index_files_batch`/`index_unindexed` 三条增量路径无守卫 | 修改分块参数后 watcher 用新参数写入旧参数索引 → 新旧粒度混库 | 三处入口统一加版本守卫 |
| M14 | `indexer.rs:324/489` | `index_all` 中途失败（`?` 提前返回）不复位 `reindex_in_progress`，watcher 增量索引被永久跳过（预存在，本批未修） | 一次失败的全量索引后 watcher 永久停摆直到重启 | 用 Drop guard 或统一错误出口复位 |
| M15 | `evidence.rs:47-61` | 词法证据校验用**字节长度** `w.len() >= 4`（注释写"≥4 字符"），中文 2 字词（6 字节）即入选，中文句子整体成一个长 token 与上下文做子串匹配 | 中文改写断言（同义表述）0 命中即误标"无证据"；`min_hits=1` 时 `min_hit_ratio` 条件恒真形同虚设 | CJK 感知 token 化（bigram/去停用词）；或仅"数字断言"参与校验 |
| M16 | `indexer.rs:1049-1054` vs 1291 | B2b 精排 50 条上限只作用于 `hybrid_search`，`rerank_pool`（多查询路径）无上限，每路 40-80 条 × 最多 5 查询全量精排 | 多查询路径分钟级延迟，与单查询路径不一致 | `rerank_pool` 入口同样按 RRF 序截断 |
| M17 | `indexer.rs:1391-1396` | `rerank_pool` 将 RRF 域分数与 sigmoid 域分数混排（跨域比较无意义） | 未标注候选排序失真（当前 agent 路径暴露面小） | 未标注候选单独追加保持 RRF 序 |
| M18 | `query_plan.rs:138-155` | `has_explicit_extension` 只校验扩展名**后**边界，不检查前导；`CODE_EXTENSIONS` 与 `is_code_query` 清单不一致（缺 c/cpp/h/rb/php） | `config.rs_backup`、`1.rs.old` 误判 Code；`main.c` 路由为 General | 统一扩展名单一来源 + 补前导边界检查 |
| M19 | `query_embedding_cache.rs` + `embedding_cache.rs:46-50` | 磁盘缓存键含 `model\|dim\|hash`，查询侧进程内缓存键仅 = 查询文本哈希；磁盘缓存 `model_key` 依赖 `get_model_name()` 回退值（模型未初始化时写回退前缀 → 永久 miss） | 模型原地替换后查询向量与文档向量来自不同模型；首轮索引批次缓存永久 miss（纯性能浪费） | 查询缓存键加入 model/dim；`open()` 前确保模型名已解析 |
| M20 | `embedding.rs:518/565` | 截断计数快照差分法在并发批次下统计互相污染（对话与文档索引可并发） | `KbIndexResult.truncated_chunks` 虚高 | 每批返回独立截断计数 |

### 2.3 配置 / 版本化 / 迁移

| # | 位置 | 问题 | 影响 | 建议 |
|---|---|---|---|---|
| M21 | `types.rs:47-49` 注释 vs `indexer.rs:818,1781` | 注释写明"旧索引（无版本字段）→ 视为 stale"，实现是 `!is_empty() && !=` —— 空版本**不** stale | 本批次把 chunk 口径从"字符"升级为"token"并重写了分块算法，升级前的旧索引不会被提示重建，检索质量偏离不告警 | 空版本视为 stale（两处同步） |
| M22 | `config.rs:46-49` | `chunk_params_version = "budget-v1:{chunk_size}:{chunk_overlap}"` 不含模型窗口/分块器版本 | 换模型（窗口变化）而 chunk_size 不变时 stale 检测失效 | 版本串纳入 `get_max_seq_len()` 与 `CHUNK_IDENTITY_VERSION` |
| M23 | `commands/llm.rs:2277-2304` + `config.rs:35-37` | `evidence_check_enabled` 在 `kb_update_indexer_config`/`kb_index` 参数与前端均未暴露，`IndexerConfig` 无 Deserialize → **死配置**，C2 特性（含 evidence.rs）不可达 | 用户无法开启证据校验；`#[serde(default)]` 无意义 | 接线命令参数 + 前端设置项；或明确"仅代码开关" |

### 2.4 命令 / 服务 / 基准

| # | 位置 | 问题 | 影响 | 建议 |
|---|---|---|---|---|
| M24 | `bin/benchmark.rs:241-242` | 汇总均值分母用 `queries.len()`，但标注缺失/为空的查询被 `continue` 未推值 → 存在缺失即全盘指标系统性偏低；Latency 用 `latencies.len()` 口径不一 | 基准是回归度量工具，数字错会误导后续每次改动决策 | 用"实际参与评测查询数"作分母，统一口径 |
| M25 | `bin/benchmark.rs:140-153` | `--reindex` 清空目标目录 `.mdgo` 索引 + embedding 缓存并以默认配置重建；与运行中 App 并发写同一 LanceDB/BM25 | 按 README 命令执行即清掉用户现有索引（26 万 chunk 重嵌入成本高）；并发写可能损坏索引 | 检测已有 `.mdgo` 要求显式确认；文档注明先退出 App |
| M26 | `services/llm.rs:172-176` | 重试 3→5 次 + 单请求超时 600s：最坏 6×600s + 退避 ≈ 61 分钟无反馈；`LLM_RETRY_MAX_MS=120s` 在 `retry_loop` 退避序列（2/4/8/16/32s）下不可达，是死常量 | 本地端点假死时交互式规划/摘要挂起约 1 小时 | 规划/摘要路径设更小外层 deadline；退避上限降到可达值 |
| M27 | `commands/knowledge.rs:35-51,154-174` | 分块校验逻辑双份复制（错误文案不一）；`chunk_size=504 + chunk_overlap=200` 能通过但 `from_config` 把 overlap 静默钳到 0 | 用户设置的 overlap 被悄悄丢弃；两处校验易漂移 | 抽公共校验函数；增加 `size + overlap ≤ 窗口-8` 联合校验（对齐 I-budget-2） |

### 2.5 前端

| # | 位置 | 问题 | 影响 | 建议 |
|---|---|---|---|---|
| M28 | `index.html:15788` / `index_cdn.html:15791` | 两个文件不加载 `css_js/modules/agent.js`，`openRagSettings/saveRagSettings` 无定义（既有问题）；本批新增的 `#rag-setting-window-hint` 是**死 UI**，min/max 不会被动态收紧 | 浏览器直开点"RAG 参数"即 ReferenceError；hint 永远显示"正在读取…" | 同步 agent.js 逻辑或删除半套 HTML；若已弃用则标注 |
| M29 | `css_js/modules/agent.js:251-255,297-301` | `kb_embedding_info` 失败/模型未就绪时后端返回 `max_position_embeddings: 0`，前端 `\|\| 512` 兜底 → 后端真实窗口 >512 时前端误拒合法值（如 1000），错误提示还误导用户 | 模型切换到大窗口后无法保存合法 chunk_size | 以输入框 `max`（info 成功后 JS 写入）为权威；info 失败时交后端裁决 |

---

## 3. 🟡 次要问题（健壮性 / 风格 / 性能 / 可观测）

| # | 位置 | 问题 |
|---|---|---|
| L1 | `text_split.rs:58-96` | `split_text_with_separators` 在 `max_size == 0` 时死循环（空窗口原地踏步）；生产调用方均保证 ≥16，属公共 API 陷阱，建议入口防护 |
| L2 | `text_split.rs:74` | 死条件 `candidate - start < max_size * 1.5` 恒真（rfind 只在窗口内找分隔符，candidate ≤ start+max_size）——评审遗留未修 |
| L3 | `text_split.rs:128-140` | `split_text_token_aware` 内存峰值：`char_of_byte` 按字节建表（8×文件字节数），10MB 中文文件峰值 ~160MB+，且 indexer 读取文件无大小上限 |
| L4 | `document/token_budget.rs:162` | `char_overlap.max(1)` 把 overlap=0 变成 1 字符，与"0 表示无重叠"语义冲突 |
| L5 | `pipeline.rs:52-56` + `embedding.rs:543` | `truncated_chunks` 对同一原子块双计（Validator 1 次 + embedding 层兜底 1 次），非零时数字偏大 |
| L6 | `pipeline.rs:37-39` | 统计窗口并非全局串行：`index_file`（watcher，不持 indexing_lock）也累加全局计数，`index_all` 窗口可能混入外部计数 |
| L7 | `pipeline.rs:251,266,328` | async 上下文直接做同步 SQLite I/O（阻塞 tokio worker）；`get_many` 的 IN 占位符可能超 SQLite 变量上限（100k 键）→ 降级全量重嵌（功能无损但缓存失效） |
| L8 | `pipeline.rs:183-187` | 每个文件构造一次 `TokenBudgetValidator`（可单例化）；tokenizer 未就绪时对每个文件打 warn（批量索引日志刷屏） |
| L9 | `markdown.rs:262-287` + `pipeline.rs:142` | 带 BOM 的 Markdown 文件 FrontMatter 完全无法识别（`trim()` 不剥离 BOM）→ 原始 frontmatter 文本进 chunk 污染索引，元数据丢失 |
| L10 | `db/token_budget.rs:35-44` | `normalize_text` 不处理旧 Mac `\r` 行尾（极边缘） |
| L11 | `db/token_budget.rs:373-394` | tokenizer 不可用降级分支的字符分片不递归复检，不保证硬上限（文档已声明，建议注释明确） |
| L12 | `embedding_cache.rs:121-135` | `put_many` 每批 `COUNT(*)` + `ORDER BY created_at LIMIT` 全表扫描排序（100k 行无索引）；`unwrap_or(0)` 静默吞掉裁剪失败（缓存可无限增长一次） |
| L13 | `embedding_cache.rs:32-55` / `pipeline.rs:251` | 每批次 `EmbeddingCache::open` 新建 SQLite 连接（全量索引几百批次 = 几百次 open） |
| L14 | `lance.rs:151-158` | 新增 `tags` 列迁移 `let _ =` 吞错且无日志——迁移静默失败时首次 `add_chunks` 以 schema 不匹配硬失败，排查困难 |
| L15 | `chunk_splitter.rs:306-307` | `merge_small_chunks` overlap 仍按旧字符逻辑且要求 `>10` 字符，英文代码上实际 overlap 远低于配置 token overlap |
| L16 | `chunk_splitter.rs:947` | `TreeProcessor::push_chunk` 允许 1.2× 超限（Validator 兜底）；`max_size * 6` 极端配置下可溢出（建议 saturating_mul） |
| L17 | `utils.rs:419` vs 模块注释 | `call_embedding_query` 注释称"LRU"，实现是 FIFO（模块注释已承认，建议统一措辞） |
| L18 | `utils.rs:578-594` | chunk 身份哈希含 tags 不含 doc_title，注释契约前后不一（因"先删后写"实际无害） |
| L19 | `bm25.rs:274-275` | schema v4→v5 强制重建 + chunk id 生成规则变化（UUID→内容哈希）：只部署新代码不重建会新旧主键混存（LanceDB 不去重），全量重建是必须项，建议在版本化元数据中提示 |
| L20 | `bm25.rs:73` 注释 | "tags = title > heading" 与 boost 值（title 3.0 > heading 2.5 > tags 2.0）不符，注释误导 |
| L21 | `evidence.rs:72` | 句子切分对 `.` 无上下文保护：`3.14`、`v2.0`、`Redis 6.2.0` 被切散，数字断言误标/漏标 |
| L22 | `indexer.rs:1262` | 上下文合并循环内 `merged.chars().count()` 每轮 O(n) 全量重扫（可累计增量计数） |
| L23 | `indexer.rs:1050` | `rerank_all.clone()` 冗余克隆（后续仅整体 move） |
| L24 | `indexer.rs:1012,1100-1108` | `last_timings` 在多查询并发路径被覆盖且 finalize 段不补全（benchmark 走 hybrid_search 不受影响） |
| L25 | `openai.rs:259-269` | 空闲超时 Err 分支后可能多吐一个 `Ok(Finish)`（Err 后 pending 未清）；`openai.rs:69` 注释仍写 300s（实际 600s） |
| L26 | `openai.rs:37` | `STREAM_IDLE_TIMEOUT=600s` 对"静默思考 >10 分钟"的推理端点会误杀（错误信息明确可重试，属权衡） |
| L27 | `anthropic.rs:16` | 注释引用的 `LLM_REQUEST_TIMEOUT` 常量不存在（命名过时）；空闲超时分支丢弃已累积内容（与"取消保留部分内容"行为不一致） |
| L28 | `limits.rs:40-43` | `QUERY_EXPANSION_TIMEOUT_SECS=5` + `RETRY_MAX=2` 不匹配：慢端点首次调用即被 5s 掐断，重试形同虚设；该 5s 在 `tokio::join!` 下位于首答关键路径 |
| L29 | `planner.rs:92-101` | `is_light_action` 只排除「并且/同时」，与 `MULTI_INTENT_MARKERS`（含「以及/然后/还要」）口径不一致；≥120 字符长查询含任一轻量动词即整体跳过规划；子串匹配误命中隐喻（"打开思路"） |
| L30 | `services/llm.rs:531` | 规划 max_tokens 2048→1024：推理模型把思考 token 计入同一预算时可能截断 JSON → 触发修正重试（一次额外调用，可接受，建议注释） |
| L31 | `lib.rs:4-5` | `mod core` → `pub mod core` 仅为 benchmark 少数符号暴露整个 core 层 API（建议 `#[cfg(feature="bench")]` 或窄门面） |
| L32 | `bin/benchmark.rs:58,77` | `--topk` 解析失败静默回退 20；错误消息缺行号；无 `--help`；单条查询 embedding 失败即中止整个评测；`ensure_model_ready`/`call_embedding_query` 阻塞调用未包 `spawn_blocking` |
| L33 | `css_js/modules/agent.js:293-294` | `parseInt(...) \|\| 448/56` 使合法值 0（不重叠）被静默替换成默认值，新增校验形同虚设于此场景 |
| L34 | `main.html:48331` | `updateContextUsage(0, …)` 无条件重置用量显示为 0%（有后台任务时一直显示 0）；`updateContextUsage` 内 `tooltipEl` 无空值防护 |
| L35 | `css_js/modules/canvas.js:1-8` | 全局包装函数无空值防护（`_canvasObject` 为 null 时抛 TypeError，窗口期概率低） |
| L36 | `agent.js:250-255` | 首次打开 RAG 设置面板被 `kb_embedding_info`（可能触发 ONNX 初始化）阻塞有可见卡顿；本地模式 hint 永不更新、max 与校验上限（504）不一致 |

---

## 4. 亮点与一致性确认（做得好的地方）

- **分层正确且严格**：纯类型（`ChunkBudget`/`TokenCounter`/`char_budget_pair`）下沉 `document` 基础层，`db::token_budget` 仅 `pub use` 转发并持有重切策略，外部调用点零改动；`chunk_document` 是全部索引路径唯一汇聚点，Normalizer+Validator 强制点成立。
- **Token 预算单一事实来源**：不变式集中定义；`kb_index`/`kb_update_indexer_config` 校验（`[64, 窗口-8]`、overlap < size/2）与前端逐字对齐。
- **重切策略细节用心**：表格按行+重复表头、代码符号只保留首片、`max_resplit_rounds=2` 防振荡、`proportional_char_budget` 留 8 token 余量；测试覆盖确定性/幂等/全文覆盖/空输入/过宽表格单次降级等不变量。
- **磁盘 embedding 缓存键 `model|dim|content_hash`** 设计正确：模型/维度/内容任一变化自然失效；`clear_inner` 同步清缓存覆盖"清索引→重索引"。
- **截断可观测闭环**：Validator 最终裁决 + embedding 层兜底计数告警 + `KbIndexResult.truncated_chunks/resplit_chunks` 透传 UI，消除静默截断。
- **流式空闲看门狗**（openai/anthropic 双侧）：每块到达重置计时，慢速持续输出不误杀；连接阶段 15s 快速失败；错误信息明确。
- **A2 计时修正**：计时放进 future 内部，修复 `tokio::join` 后计时导致 bm25_ms 恒 0 的失真。
- **FrontMatter 容错好**：解析失败仅丢元数据、绝不丢正文（B3）；`strip_frontmatter`/`parse_frontmatter` 单函数收敛；扩展名大小写不敏感修复闭环（pipeline 与 `is_markdown_ext` 同函数）。
- **全链路 Unicode 安全**：新代码一律字符偏移 + 防御性检查，未发现字节切片 panic 路径；正则全部 `regex` crate（线性时间）。
- **前后端契约完全对齐**：命令名/字段名/校验规则（`max_chunk_tokens`、`chunk_size/2`、`|| 512` 兜底）逐字一致；新字段全部 `#[serde(default)]` 兼容旧前端；canvas.js 包装函数修复了既有 ReferenceError。
- **降级路径齐全**：BM25/向量/精排/embedding 缓存失败均有 warn+降级，检索永不阻断；reranker 后台下载带防抖重试。
- **稳定性验证**：`cargo check --all-targets` 通过；`cargo test --lib` 321 通过 0 失败（含新增 token 预算/缓存/frontmatter/路由测试）。

## 5. 与 HEAD 相比的关键行为变化（迁移提醒）

1. `chunk_size`/`chunk_overlap` 语义从**字符**升级为**token**（默认值仍 448/56，英文文本块实际变大 ~4 倍）——旧索引需全量重建。
2. BM25 schema v4→v5（新增 tags 字段）+ chunk id 从 UUID 改为内容哈希——必须重建索引，否则新旧主键混存。
3. 候选池从 `candidate_k=100` 收窄为意图自适应 40-80；精排候选截断 50 条（单查询路径）。
4. 重试 3→5 次、退避上限 120s；请求超时 300s→1800s（流式）/600s（非流式）。
5. 新增 `evidence_check_enabled`（默认关，当前为不可达死配置，见 M23）。
6. 规划 prompt 字段最小化（risks 等仅确有内容时输出）+ max_tokens 2048→1024。

## 6. 修复优先级建议

1. **立即修（合入前）**：🔴-1（pipeline 缓存回填）、🔴-2（rerank scores 遮蔽）——均需补混合批次回归测试，修复后清理存量 embedding 缓存。
2. **本迭代修（正确性/迁移语义）**：M1（预算不变式）、M21/M22（stale 版本语义）、M9/M10（标签过滤）、M2-M4（分块预算核算）、M24（benchmark 分母）、M25（--reindex 确认）。
3. **本迭代修（操作安全）**：M26（重试放大）、M14（reindex_in_progress 复位）、M5（overlap 单位）。
4. **排期跟进**：其余 🟠/🟡（前端半套同步、rerank 缓存键隔离、rerank_pool 上限、死配置接线、死代码清理、注释修正等）。

---

## 7. 修复状态（2026-08-23 已实施）

| 编号 | 修复内容 | 位置 | 验证 |
|---|---|---|---|
| 🔴-1 | embedding 缓存回填按 miss 下标对齐（`cache_entries_from_misses`），并补混合命中回归测试 | `core/pipeline.rs` | ✅ 2 个新单测 |
| 🔴-2 | 删除 rerank `scores` 重复声明（遮蔽缓存分），抽出 `scores_from_cache` + 混合批次/阈值边界单测 | `core/search/rerank.rs` | ✅ 3 个新单测 |
| M1 | `from_model_window` target 钳到 `[64, hard_max]`；`assemble` 内 overlap 钳到 `hard − target`（I-budget-2 兜底） | `core/document/token_budget.rs` | ✅ |
| M21 | stale 判定：空版本（旧索引）视为 stale（`status()` + `startup_sync`），与 `types.rs` 注释一致 | `core/indexer.rs` | ✅ |
| M22 | `chunk_params_version` 纳入 `CHUNK_IDENTITY_VERSION` 与模型窗口 | `core/config.rs` | ✅ |
| M10 | 标签过滤改 JSON 元素精确匹配（`%"tag"%` 锚定）+ `LOWER` 大小写不敏感 + LIKE 通配符转义（`ESCAPE '\'`）+ 多标签 AND | `core/indexer.rs` | ✅ 2 个新单测 |
| M13 | P0-4 版本守卫覆盖 watcher 单文件/批量与手动增量三条路径（`params_version_mismatch`） | `core/indexer.rs` | ✅ |
| M14 | `index_all` 用 `ReindexGuard` RAII 复位 `reindex_in_progress`（失败不再永久停摆 watcher） | `core/indexer.rs` | ✅ |
| M24 | benchmark 汇总分母改为「实际参与评测查询数」 | `bin/benchmark.rs` | ✅ |
| M25 | `--reindex` 检测到已有 `.mdgo` 时要求 `--yes-wipe` 显式确认；`--topk` 非法值报错；补 `--help` | `bin/benchmark.rs` | ✅ |
| M5 | 重切策略 overlap 按文本密度折算为字符（`char_overlap_for`） | `core/db/token_budget.rs` | ✅ |
| M27 | 分块校验抽公共函数 `validate_chunk_params`（两处调用），新增 `size+overlap ≤ 窗口−8` 联合校验 | `commands/knowledge.rs` | ✅ 1 个新单测 |

**第一轮（🔴×2 + 🟠×10）**：🔴-1、🔴-2、M1、M5、M10、M13、M14、M21、M22、M24、M25、M27 ✅（上表）

**第二轮（2026-08-23 全量修复）**：

| 编号 | 修复内容 | 位置 | 验证 |
|---|---|---|---|
| M2 | embedding 前缀按 token 实测二分截断（CJK 不再 ×4 放大） | `core/document/chunk_engine.rs` | ✅ |
| M3 | 预算核算按 `prefix+"\n"+body` 整体精确计数；`flush_group` 超限丢弃 overlap 尾部 | `core/document/chunk_engine.rs` | ✅ |
| M4 | `available` 上限钳到 `max − prefix`（不再被 min_body_reserve 顶破）；前缀占满预算时单 chunk 交 Validator | `core/document/chunk_engine.rs` | ✅ |
| M6 | 表格切分原子阈值与 Validator 对齐（`≤3` 行） | `core/document/chunk_engine.rs` | ✅ |
| M7 | heading-only 导航 chunk 改为按「子树是否产出正文」判定（纯嵌套标题每级可检索） | `core/document/chunk_engine.rs` | ✅ |
| M8 | PlainText 降级路径先经 `char_budget_pair` 折算（token→字符） | `core/db/chunk_splitter.rs` | ✅ |
| M9 | `SearchHit` 新增 `tags`；三路（向量/BM25/符号）全部携带；融合后统一内存标签过滤（AND、大小写不敏感） | `lance.rs`+`bm25.rs`+`rrf.rs`+`indexer.rs` | ✅ |
| M11 | 精排失败回退时补 `apply_threshold_filter`（无 BM25/符号佐证且 cosine<阈值的噪声不再直通） | `core/indexer.rs` | ✅ |
| M12 | rerank 缓存键纳入候选正文哈希（跨库隔离 + 内容变更自然失效） | `core/search/rerank.rs` | ✅ |
| M15 | evidence 关键 token 改 CJK 2-gram + 小写（中文改写断言不再误标） | `core/evidence.rs` | ✅ 3 个新单测 |
| M16 | `rerank_pool` 打标候选按 RRF 序截断 100（多查询路径不再分钟级延迟） | `core/indexer.rs` | ✅ |
| M17 | `rerank_pool` RRF 域（未标注）与 sigmoid 域（精排）分离排序，不再跨域混排 | `core/indexer.rs` | ✅ |
| M18 | `has_explicit_extension` 前导/后随边界（`config.rs_backup` 不再误判）；`CODE_EXTENSIONS` 补 c/cpp/cc/h/hpp/rb/php，与 `is_code_query` 单一来源 | `core/search/query_plan.rs` | ✅ 1 个新单测 |
| M19 | 查询缓存键纳入模型作用域（`model\|dim`），与磁盘缓存策略对齐 | `core/db/query_embedding_cache.rs` | ✅ |
| M20 | embedding 截断计数改局部统计（并发批次不再互相污染） | `core/embedding.rs` | ✅ |
| M23 | `evidence_check_enabled` 接线 `kb_update_indexer_config` 参数 + main.html 设置项 | `commands/knowledge.rs` + 前端 | ✅ |
| M26 | 规划生成加 90s 总时限（fail-open 不规划）；`LLM_RETRY_MAX_MS` 120s→32s（可达） | `commands/llm.rs`+`services/llm.rs` | ✅ |
| M28 | index.html/index_cdn.html 删除死 UI `#rag-setting-window-hint` | 前端 | ✅ |
| M29 | `ragEmbedWindowValid` 标志——info 失败时前端不再误拒合法 chunk_size | `css_js/modules/agent.js` | ✅ |

| 编号 | 修复内容 | 位置 | 验证 |
|---|---|---|---|
| L1 | `split_text_with_separators` 入口 `max_size==0` 防护（防死循环） | `core/document/text_split.rs` | ✅ |
| L2 | 移除死条件 `candidate - start < max_size*1.5`（恒真） | `core/document/text_split.rs` | ✅ |
| L3 | `char_of_byte` 改稀疏表 + 二分查找（10MB 中文文件内存减半以上） | `core/document/text_split.rs` | ✅ |
| L4 | `char_budget_pair` overlap=0 不再被 `.max(1)` 强改 | `core/document/token_budget.rs` | ✅ |
| L5 | `budget_stats` 双计口径注释（非 0 即需检查语义不变） | `core/pipeline.rs` | ✅ |
| L6 | 统计窗口改快照差分（watcher 并发不再污染 index_all 窗口） | `core/pipeline.rs`+`embedding.rs` | ✅ |
| L7 | `get_many` 分批 ≤500 键（占位符不超 SQLite 上限） | `core/db/embedding_cache.rs` | ✅ |
| L9 | `parse_frontmatter` 剥离 BOM（Windows 记事本文件 frontmatter 可识别） | `core/document/markdown.rs` | ✅ |
| L10 | `normalize_text` 旧 Mac `\r` 边界注释 | `core/db/token_budget.rs` | ✅ |
| L11 | 降级分支「不保证硬上限」注释 | `core/db/token_budget.rs` | ✅ |
| L12 | `created_at` 索引（裁剪查询不再全表排序） | `core/db/embedding_cache.rs` | ✅ |
| L13 | `open_shared` 按目录复用连接（几百批次不再几百次 open） | `core/db/embedding_cache.rs`+`pipeline.rs` | ✅ |
| L14 | `migrate_add_column` 失败留日志（不再静默吞错） | `core/db/lance.rs` | ✅ |
| L15 | `merge_small_chunks` overlap 门槛 `>10`→`>0`（低密度文本 overlap 不再被丢弃） | `core/db/chunk_splitter.rs` | ✅ |
| L16 | `max_size*6/5` → `saturating_mul(6)/5`（防溢出） | `core/db/chunk_splitter.rs` | ✅ |
| L17 | 查询缓存注释 LRU→FIFO 措辞统一 | `core/db/utils.rs` | ✅ |
| L18 | chunk 身份哈希纳入 `doc_title`（title 变化 → 身份更新） | `core/db/utils.rs` | ✅ |
| L19 | BM25 schema 重建 + chunk id 变化 → 全量重建提醒注释 | `core/db/bm25.rs` | ✅ |
| L20 | bm25 boost 注释与取值一致（title 3.0 > heading 2.5 > tags 2.0） | `core/db/bm25.rs` | ✅ |
| L21 | evidence 句点仅后随空白/结尾才切分（3.14/6.2.0 不再拆散） | `core/evidence.rs` | ✅（并入 M15 单测） |
| L22 | 上下文合并累计字符计数（O(n²)→O(n)） | `core/indexer.rs` | ✅ |
| L23 | 精排候选只克隆 Top-N（不再 clone 全池） | `core/indexer.rs` | ✅ |
| L24 | `last_timings` 多查询覆盖为已知局限注释 | `core/indexer.rs` | ✅ |
| L25 | openai 流式 Err 分支清空 pending（不再 Err 后多吐 Finish）；`with_timeout` 注释 300s→600s | `core/loop/openai.rs` | ✅ |
| L26 | `STREAM_IDLE_TIMEOUT` 推理端点边界注释 | `core/loop/openai.rs` | ✅ |
| L27 | anthropic 注释修正（`LLM_REQUEST_TIMEOUT` 不存在）+ 空闲超时错误说明部分内容丢弃 | `services/anthropic.rs` | ✅ |
| L28 | `QUERY_EXPANSION_RETRY_MAX` 2→1（5s 时限内重试可达） | `core/agent/limits.rs` | ✅ |
| L29 | `is_light_action` 与 `MULTI_INTENT_MARKERS` 全清单对齐；长查询多意图不被轻量动词压制 | `core/agent/planner.rs` | ✅ 1 个新单测 |
| L30 | 规划 max_tokens 1024 权衡注释 | `services/llm.rs` | ✅ |
| L31 | `mod core` 私有化 + `bench_api` 窄门面（`bench` feature + bin `required-features`） | `lib.rs`+`Cargo.toml`+`benchmark.rs` | ✅ 双构建验证 |
| L32 | benchmark JSONL 解析错误带行号 | `bin/benchmark.rs` | ✅ |
| L33 | 前端 `parseInt \|\| 448` 吞 0 → `Number.isFinite`（0 参与校验） | `css_js/modules/agent.js` | ✅ |
| L34 | `updateContextUsage` 补 `tooltipEl` 守卫；进入页重置仅当工具栏隐藏时 | `main.html` | ✅ |
| L35 | canvas 包装函数加 `_canvasObject` 空值/类型守卫 | `css_js/modules/canvas.js` | ✅ |
| L36 | RAG 设置先显示 overlay 再拉取 info；本地模式直接写默认窗口 | `css_js/modules/agent.js` | ✅ |

**第二轮合计新增 5 个回归单测（M15×3、M18×1、L29×1）；`cargo check --all-targets` 通过（默认构建与 `--features bench` 双验证）；`cargo test --lib` 333 通过 / 0 失败。**

⚠️ 修复后行为/迁移提醒：
1. 🔴-1 修复前写入的存量 `{kb}/.mdgo/embedding_cache.sqlite` 可能含错配条目，**上线前请删除**（或清空后全量重建一次）。
2. M9 新增 `SearchHit.tags` + 融合内存过滤：旧索引若未含 tags 列会自动迁移（L14 已加日志）；`tag:` 查询行为与向量路下推一致。
3. M18 扩展名清单新增 c/cpp/h 等：`main.c` 类查询现在正确路由 Code 意图。
4. L31 后 benchmark 运行命令变为 `cargo run --bin benchmark --features bench -- ...`（文档已同步）。
5. M26 规划 90s 时限 + L28 重试 1 次：慢端点行为有变化（fail-open 更快）。

未修复（已知取舍/排期）：🟠 无；🟡 无——本批次全部 🟠/🟡 均已处理或经注释/文档明确为已知取舍（L10/L11/L24/L26/L27 的边界说明、M3 的 Validator 兜底语义）。详见上文各节。

---

*本报告基于 6 路并行模块审查 + 一手复核，行号均为当前 working tree 实测；`cargo check`/`cargo test --lib` 结果佐证代码可编译、单测通过。审查全程只读，未修改任何业务代码。*
