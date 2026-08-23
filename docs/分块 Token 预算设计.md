最后更新：2026-08-23（RAG P0 批次后）

# RAG Token 预算改造设计文档（P0-1 / P0-2 / P0-3 / P0-4 / P0-5）

> 配套评审：`docs/代码审查报告-RAG全链路P0批次.md`（问题定义、证据与本批次审查发现；审查基于 `edab77e` HEAD + working tree P0 批次改动）
> 状态：**已实施**（P0-1~P0-5 全链路落地；`cargo check --all-targets` 通过，`cargo test --lib` **321 通过 / 0 失败**）
> 原则：**预算控制最终送进 embedding 模型的 `embedding_text` 的 token 数，而不是原始 `text` 的字符数**；
> Chunker 只负责语义分组，由独立的 `TokenBudgetValidator` 做最终裁决，彻底取消"超限自动截断后继续 embedding"的静默行为。

---

## 1. 目标与非目标

### 目标

1. **P0-1**：消灭静默截断——任何 chunk 在进入 embedding 前必须通过 token 预算校验；超限走"重切 → 显式降级"，绝不静默 truncate。
2. **P0-2**：分块预算从"字符数"升级为"token 数"，消除中英文 token 密度失衡（448 字符对英文仅 ~21% 窗口利用率）。
3. **P0-3**：建立分块不变量测试矩阵（可注入 fake tokenizer，不依赖模型下载，可进 CI）。
4. **P0-4**：索引参数版本化——修改 chunk 参数后旧索引可识别过期并提示重建。
5. **P0-5**：chunk 稳定身份（内容哈希）+ embedding 缓存——增量索引只重嵌变化 chunk。

### 非目标（本阶段不做）

- 不改检索侧（hybrid recall / rerank / query planner / ±window 扩展）——检索侧改造归《混合检索逻辑契约》/《混合检索技术设计》文档体系。
- ~~不重写 `SemanticChunkSplitter` / `SentenceWindowChunkSplitter`（死代码处置另行决策）~~：**已超出原非目标**——两个旧 splitter 已在清扫项 D1 中作为死代码删除（连同 `split_sentences`、`MARKDOWN_TEXT_SEPARATORS`、`with_config`），`chunk_splitter.rs` 收敛为单一工厂路由。
- ~~不做 FrontMatter 元数据索引~~：**已超出原非目标**——已实现：`pipeline.rs` 对 Markdown 类文件先 `parse_frontmatter`，向全部 chunk 注入 `doc_title` / `tags`（BM25 tags 字段 + LanceDB tags 列，见《检索 V2 架构评估与实施记录》P0-1 / A3）。
- 不做 tree-sitter 符号提取、HTML 正文提取（评审报告 P1 项，另行立项）。

---

## 2. 核心概念与数据结构

### 2.1 ChunkBudget —— token 预算的单一事实来源

取消"各模块各算一套"（旧 `chunk_engine.rs` 的 1.25× 系数、`text_split.rs` 的字符窗口、`merge_small_chunks` 的 `0.4×` 下限、旧 splitter 的手动 overlap），统一为：

```rust
/// 分块 token 预算（唯一预算来源，所有 splitter / validator / 统计共用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkBudget {
    /// 硬上限：最终 embedding_text 的最大 token 数（含路径前缀 + 正文 + overlap 尾部）。
    /// 必须满足：hard_max_tokens ≤ embedding_max_tokens - special_tokens_reserve
    pub hard_max_tokens: usize,
    /// 目标值：分组时尽量接近的 token 数（贪心分组的触发阈值）
    pub target_tokens: usize,
    /// 最小下限：碎块合并目标（注意：是"合并目标"，不是整文档硬下限——
    /// 单句文档仍须产出 1 个 chunk）
    pub min_tokens: usize,
    /// 相邻 chunk 间的 overlap（拼接前块尾部 token 数）。必须满足 target + overlap ≤ hard_max
    pub overlap_tokens: usize,
    /// embedding 路径前缀（heading_path / 树形路径）最大 token 预算
    pub prefix_max_tokens: usize,
    /// special tokens 预留（[CLS]/[SEP] 等，BGE 系取 8）
    pub special_tokens_reserve: usize,
}

impl ChunkBudget {
    /// 从 embedding 模型窗口构建默认预算（唯一生产构造点）
    pub fn from_model_window(max_position_embeddings: usize) -> Self;
    /// 从现有配置（chunk_size / chunk_overlap，语义为 token）构建
    pub fn from_config(chunk_size: usize, chunk_overlap: usize, max_position_embeddings: usize) -> Self;
}
```

**默认值公式**（`from_model_window`，以 bge-small-zh `max_position_embeddings = 512` 为例）：

```text
special_tokens_reserve = 8                    （常量 DEFAULT_SPECIAL_RESERVE）
hard_max_tokens       = (512 - 8).max(64) = 504    （模型窗口 - 预留，最终裁决线）
target_tokens         = (504 - 56).clamp(128, 1024) = 448
overlap_tokens        = 56
min_tokens            = ⌊448 × 0.36⌋ = 161   （≈ target × 0.36，替代旧 0.4× 字符下限）
prefix_max_tokens     = min(40, target - 16)  （常量 DEFAULT_PREFIX_MAX = 40，不得独占目标预算）
```

- `from_config`：`hard_max` 同式；`target = chunk_size.clamp(64, hard_max)`；`overlap = chunk_overlap.min(hard_max - target)`（非法组合被收敛，不 panic）。
- `assemble` 内建两条 `debug_assert!` 不变量（I-budget-1/2/3），debug 构建下破坏即 panic。

**关键不变式（写入代码注释与测试）**：

```text
I-budget-1: hard_max_tokens ≤ embedding_max_tokens - special_tokens_reserve
I-budget-2: target_tokens + overlap_tokens ≤ hard_max_tokens
I-budget-3: min_tokens ≤ target_tokens ≤ hard_max_tokens
I-budget-4: prefix_max_tokens ≤ target_tokens（路径前缀不得独占目标预算）
```

### 2.2 TokenCounter —— token 计数抽象（测试可注入）

```rust
/// token 计数抽象：生产用真实 BGE tokenizer，测试注入确定性 fake。
pub trait TokenCounter: Send + Sync {
    /// 精确 token 数。口径必须与 embedding 路径一致（含 special tokens，
    /// 对齐 embedding 层的 encode(text, true)）。
    fn count(&self, text: &str) -> Option<usize>;
    /// token → 字符边界（用于 token 边界切分）；None 表示不可用（降级字符切分）
    fn token_char_boundaries(&self, text: &str) -> Option<Vec<usize>>;
}
```

- **生产实现** `BgeTokenizerCounter`：包装 `core/embedding.rs` 的 `tokenize_with_offsets`（token 数 + 字符边界，口径含 special tokens）。
- **全局单例** `global_token_counter()`：`OnceLock` 懒初始化（进程级一个 `Arc<dyn TokenCounter>`）。
- **测试实现** `FixedRateCounter`（`N 字符 = 1 token`，共享 fake 位于 `document/token_budget.rs::test_util`，`pub(crate)`）。
- **字符预算折算工具** `char_budget_pair(text, max_tokens, overlap_tokens, counter)`：按文本实际 token 密度把 token 预算折算为字符预算（10% 余量防密度波动），供代码/树形/纯文本等 char-based 分块器使用——**防止 token 语义的 `max_size` 被误当字符数**；counter 不可用时退化为 1 字符 ≈ 1 token（CJK 最坏情形）。
- 模型未初始化时 `count` 返回 `None` → Validator 降级为"字符预算 + 记录 degraded 统计"（此时 embedding 本就不会执行，语义自洽）。

### 2.3 TokenBudgetValidator —— 最终裁决层

```rust
/// 校验结果统计（进 KbIndexResult / 日志；健康态要求 truncated_count = 0）
pub struct ValidationReport {
    pub chunks_in: usize,
    pub chunks_out: usize,
    pub resplit_count: usize,       // 重切次数
    pub truncated_count: usize,     // 显式降级（原子块超限）数 —— 必须为 0 才是健康态
    pub degraded_token_count: bool, // tokenizer 不可用，降级字符预算
}
```

**重切策略注册表**（按 chunk 类型分发，默认三策略）：

| 策略 | 匹配类型 | 切分方式 |
|---|---|---|
| `TableReSplitStrategy` | `table` | 按行分组 + **每片重复表头**（保持表格语义单元完整）；表头本身超限或"表头+最窄数据行"超限 → 原子块，整体仅 1 次显式降级 |
| `CodeReSplitStrategy` | `code` 或 `symbol_name.is_some()` | 按行边界切分；**符号名/类型只保留在首片**（后续片是延续代码） |
| `ProseReSplitStrategy` | `paragraph/quote/list/section/html/root/heading` 或 None | 按 段落 → 句子 → 字符 逐级降级切分 |

**裁决流程**（对每个 chunk，`validate_one`）：

```text
count(embedding_text) ≤ hard_max_tokens
   ├─ 是 → Pass
   └─ 否 → 找 ReSplitStrategy
            ├─ 可重切 → 重切 → 每片递归校验（防振荡：最多 max_resplit_rounds=2 轮，
            │           仍超限走显式降级）
            └─ 原子块（单行超长 / 超宽表行 / 重切后仍超限）→ TruncateWithWarning
```

- 字符预算按**当前实际密度**折算（`proportional_char_budget`，留 8 token 安全余量，保证分片能通过再校验）。
- 重切后的 `text` 与 `embedding_text` 同置为片段（AST 路径退化为紧凑路径文本——重切是低频兜底路径，可接受）。
- **`count_only`**：只统计不重切（对话消息等原子单元场景），超限仅计数 + warn。
- 降级分支：`truncated_count += 1` + `log::warn!`——**允许 embedding 层截断但绝不再静默**。fail 的粒度是 chunk，不是文件（一个病态行不阻断整文件索引）。

### 2.4 规范化（Normalizer）与稳定身份（P0-5 前置）

管线顺序（已实现）：`Parser → Splitter（语义分组）→ ChunkNormalizer → TokenBudgetValidator → 注入 FrontMatter 元数据 → build_document_chunks（内容哈希 id）→ Embedding Cache → Embedding`。

- **`normalize_chunks`**（`core/db/token_budget.rs`）：
  - BOM 剥离；
  - 换行归一（`\r\n` → `\n`，与 markdown 解析器唯一 normalize 点对齐）；
  - 每行行尾空白剔除；
  - **不做更多**（不做大小写折叠、不做标点归一——避免过度归一化导致语义坍缩）。`text` 与 `embedding_text` 同步处理。
- **content_hash**（P0-5 已启用）：
  ```text
  chunk_id = rel_path#hash(身份版本 + 路径 + chunk 位置 + 规范化文本 + 元数据[含 tags])
  ```
  哈希在 Validator **之后**计算（重切会改变内容）。**实现取舍**：用零依赖 **FNV-1a 128**（`fnv1a_128` + `stable_hash_hex`）替代设计初稿的 SHA256——避免新增 `sha2` 依赖；非加密用途，KB 规模（10^6 级 chunk）碰撞概率可忽略，且碰撞后果仅是缓存未命中/同 id 覆盖（幂等）。

---

## 3. 与现有代码的映射（已替换 / 已落地）

| 旧实现 | 问题 | 当前实现（替换） |
|---|---|---|
| 旧 `chunk_engine.rs` `max_single = max_size * 1.25` | 可致中文 chunk 560 字符 > 512 token 静默截断 | **已取消**：`oversize_factor` 移除；贪心分组按 token 累积（块级缓存计数），硬裁决由 Validator 兜底 |
| 旧 `available = max_size - context_len`（按渲染前缀扣预算） | 扣的是 `text` 侧前缀，非 `embedding_text` | `embed_target = target_tokens - counter.count(embed_prefix)`；embed 前缀截断 ≤ `prefix_max_tokens`（保留最近 ≤3 级，context 保留完整 Markdown 渲染） |
| 组间无 overlap | 索引期 overlap 失效 | 引擎按 "target + overlap ≤ hard_max" 预留 overlap 头寸后分组，拼接前块正文尾部（按密度折算字符）；硬上限仍由 Validator 兜底 |
| 旧 `text_split.rs` 字符窗口 + 死条件 `1.5×` | 非 token 精确、死代码 | `split_text_token_aware`：一次 tokenize + 按 token 预算定位切分点；overlap 按 token；字符模式保留为 `TokenCounter` 不可用时的降级 |
| `merge_small_chunks` `0.4×` 下限 + 手动 overlap | 与 AST 路径 overlap 语义不一致 | 下限语义统一为 `min_tokens`（合并目标）；overlap 拼接收敛到引擎/Validator 两处共享口径 |
| 旧 `embedding.rs` `truncated_len = ids.len().min(max_seq_len)` | 静默截断 | **保留为最后兜底但可观测**：截断计数（进程内原子计数）+ 每批 warn + 进 `KbIndexResult.truncated_chunks` |
| 旧 `build_document_chunks` 随机 UUID id | 不可幂等 | **内容哈希 id**（`rel_path#hash(版本+路径+位置+文本+元数据)`），幂等、内容敏感、同文档唯一（LanceDB 主键约束） |

---

## 4. 配置与前端（P0-4 前置的兼容方案）

### 4.1 配置字段

- **字段名 `chunk_size` / `chunk_overlap` 保持不变**（无持久化迁移），**语义从"字符"升级为"token"**：
  - 对中文：448 字符 ≈ 448 token，**行为几乎不变**（兼容）；
  - 对英文：448 token ≈ 1800 字符，**利用率恢复 ~85%**（修复失衡）；
  - UI 文案与 tooltip 标注单位变化（tippy 更新为 token 语义）。
- 默认值单一来源：`IndexerConfig::default()` 改由 `ChunkBudget::from_model_window(get_max_seq_len())` 生成（`chunk_size = target_tokens`、`chunk_overlap = overlap_tokens`）；旧 `recommended_chunk_size()` 保留为 `ChunkBudget` 便捷委托（`#[allow(dead_code)]`）。
- 参数校验（`commands/knowledge.rs`，`kb_update_indexer_config` 与 `kb_index` 同规则）：
  - `chunk_size ∈ [64, 模型窗口 - special_tokens_reserve]`（`max_chunk_tokens` 提供上限，前端同步约束）；
  - `chunk_overlap < chunk_size / 2`；
  - 越界 → 返回错误（前端 toast），不再静默接受。

### 4.2 配置版本化（P0-4）

- `IndexMeta.chunk_params_version`（`indexer.rs` 序列化的元数据）由 `IndexerConfig::chunk_params_version()` 生成：`"budget-v1:{chunk_size}:{chunk_overlap}"`。
- `sync_on_start` / `status()` 加载时比对：版本不一致 → 标记 `stale=true` → 前端"重建索引"提示条（`kbStatus.stale` → 状态文案追加"分块参数已变更，请重建索引" + 状态点变 error）；**不自动重建**（避免静默大成本操作），且 `sync_on_start` 在参数变更时跳过增量。
- 已知限制（审查 M13/M21/M22）：版本守卫只覆盖 `sync_on_start` 路径，`index_file`（watcher）/`index_files_batch`/`index_unindexed` 三条增量路径暂无守卫；空版本号不视为 stale；版本串不含模型窗口/分块器版本——均为审查遗留，修复排期见审查报告。

### 4.3 前端

- 默认值改由后端 `kb_get_indexer_config` 下发（删除硬编码 448/56 的漂移源）；
- `saveRagSettings` 增加 min/max 校验与非法值提示（与后端同规则）；
- 设置面板展示"当前 embedding 模型窗口（token）"（`kb_embedding_info.max_position_embeddings`），约束 chunk_size 上限；`ragDefaults.maxPositionEmbeddings` 承载。

---

## 5. 管线集成点（P0-1 的最小闭环，已实现）

```text
pipeline.rs::chunk_document                          ← 唯一强制点（index_all/index_file/index_files_batch/index_unindexed 全部经此）
  ├─ splitter.split(...)                             ← 语义分组（工厂路由：md→AST / html→AST / 代码→符号感知 / opml·mm→树形 / 未知→PlainText）
  ├─ [已实现] normalize_chunks(...)                  ← 规范化（BOM/换行/行尾空白）
  ├─ [已实现] TokenBudgetValidator.validate(...)     ← 裁决 + 按类型重切 + 统计（截断/重切计数累加到全局）
  ├─ [已实现] 注入 FrontMatter 元数据（doc_title/tags）
  └─ utils::build_document_chunks(...)               ← 内容哈希 id（FNV-1a 128）
```

补充强制点：

- `indexer.rs::index_chat_session`（单消息 = 单 chunk）：长消息走 Validator `count_only`（只计数不重切，超限计数 + 告警）。
- `embedding.rs::call_embedding_parallel`：截断计数（进程内原子）+ 每批 warn；统计经 `KbIndexResult.truncated_chunks` / `resplit_chunks` 透传 UI。
- `pipeline.rs::embed_chunks`：缓存键 `model|dim|content_hash` 命中跳过推理，只推理未命中 chunk，结果按原序组装；缓存失败降级全量推理（不阻断索引）。

---

## 6. 实际落地文件清单

| 文件 | 内容 |
|---|---|
| **新增** `core/document/token_budget.rs` | **纯类型层**：`ChunkBudget` / `TokenCounter` trait / `char_budget_pair` / `budget_from_config` / `max_chunk_tokens` / `global_token_counter` / 共享 fake counter（`test_util`）。放 document 基础层（分块引擎可用），维持"document 不依赖 db"分层 |
| **新增** `core/db/token_budget.rs` | **裁决层**：`TokenBudgetValidator` / `ValidationReport` / 三策略注册表（表格/代码/正文）/ `normalize_chunks` / `count_only`；`pub use` 转发纯类型，外部调用点不变 |
| `core/document/chunk_engine.rs` | 贪心分组 token 化（块级缓存计数）；超长块 token 边界切分；embed 前缀截断（≤3 级 + ≤ prefix_max）；组间 overlap；移除 `oversize_factor` |
| `core/document/text_split.rs` | `split_text_token_aware`（token 预算定位切分点 + token overlap）；字符模式保留为降级 |
| `core/db/chunk_splitter.rs` | 各 splitter 注入 token 预算与 counter；PlainText 走 token 感知；代码/树形按 `char_budget_pair` 折算字符预算；`min_body_reserve_chars` → `min_body_reserve_tokens`（64） |
| `core/embedding.rs` | `tokenize_with_offsets`（含 special tokens 口径）；截断计数 + 每批 warn；`get_max_seq_len` 提升 pub(crate) |
| `core/pipeline.rs` | `chunk_document` 强制点（Normalizer → Validator → 元数据注入）；`embed_chunks` 缓存命中跳过；截断/重切统计累加器（reset/read） |
| `core/indexer.rs` | `KbIndexResult.truncated_chunks/resplit_chunks` 透传；`KbStatus.stale`；`IndexMeta.chunk_params_version`；`sync_on_start` 参数变更跳过增量；`index_chat_session` 走 `count_only` |
| `core/config.rs` | `IndexerConfig::default()` 改由 `ChunkBudget::from_model_window` 生成；`chunk_params_version()` |
| `core/db/utils.rs` | `fnv1a_128` + `stable_hash_hex`；`build_document_chunks` 内容哈希 id；`get_cache_dir` |
| **新增** `core/db/embedding_cache.rs` | SQLite 单表，键 `model|dim|content_hash`，上限 10 万条（≈150MB）按最旧裁剪 |
| `commands/knowledge.rs` | `kb_update_indexer_config` / `kb_index` 分块参数校验；`kb_embedding_info` 返回 `max_position_embeddings` |
| 前端 `css_js/modules/agent.js` + 3 个 HTML | 设置面板拉取模型窗口、输入范围约束、保存校验（与后端同规则）、token 语义文案 |

---

## 7. 测试矩阵（P0-3，已实现）

### 7.1 不变量（全部用 fake TokenCounter，`cargo test` 可离线跑）

```text
I-1  全文覆盖：所有输入字符（除 overlap 尾部与规范化剔除的空白）出现在且只出现在 ≥1 个 chunk；
     拼接顺序 = 文档顺序
I-2  预算：每 chunk 的 count(embedding_text) ≤ hard_max_tokens（精确断言，fake counter 可控）
I-3  空 chunk 不产生（空文档 → 0 chunk；纯 frontmatter → 0 chunk 且不报错）
I-4  确定性：同输入 + 同配置 + 同 counter → 输出逐字节一致
I-5  排序稳定：chunk_index 递增且 = 文档顺序
I-6  chunk ID 稳定：同输入同配置 → 同 content_hash（P0-5 后）
I-7  类型策略：md→AST 语义分块；html→HtmlChunkSplitter；代码→符号感知；opml/mm→树形；
     未知扩展名→PlainText
I-8  embedding_text token ≤ text token（路径前缀不污染）
I-9  overlap 不产生"纯重复"chunk（防 overlap 拼接实现 bug）
I-10 幂等：Validator 输出再进 Validator → 0 重切、0 截断（防振荡）
```

### 7.2 用例矩阵

| 类别 | 用例 |
|---|---|
| Markdown | 常规 / 超长节（>2×target）/ 超长表格（>1 屏 + 超宽单元格）/ 围栏代码 / 嵌套列表含标题 / frontmatter / 纯标题文档 / 无标题文档 / 6 级深标题 + 100 字符长标题 / 标题后紧跟更高一级标题（跳级） |
| HTML | 常规 / 含 footer+aside（跳过清单，不入索引）/ 空标题 `<h1><img></h1>` / 内联容器 `<div>hello <a>link</a></div>` |
| Code | 多函数文件 / 单行超长（minified JS）/ 注释块+函数合并 / `export function` / `const foo = () =>` / 混合语言 |
| Plain | 超长段落（无分隔符）/ 中英混合 / URL 长行 / 代码日志 |
| Unicode | emoji / 代理对（😀）/ 组合字符（é）/ 零宽字符 / BOM / `\r\n` 混合换行 / 全角标点 |
| 边界 | 空文档 / 极小文档（< min_tokens 单句）/ 仅 frontmatter / 仅分割线 / 全空白 / 非法扩展名 |
| 性能守卫 | ~1MB 文档分块 + 校验总耗时 < 20s（防 O(n²) 回归） |

### 7.3 已实现测试（+28 条，全库 321 通过）

- **Validator 不变量**（`db/token_budget.rs`）：I-1 全文覆盖 / I-2 预算精确断言 / I-3 空输入 / I-4 确定性 + I-10 幂等 / 表格重复表头 / 过宽表格单次原子降级 / 代码符号只保留首片 / `count_only` / normalize 语义保持。
- **引擎**（`chunk_engine.rs`）：chunk ≤ token 预算 / 短节单 chunk / embed 前缀 ≤3 级且 ≤ 预算 / 组间 overlap 尾部 / 超长表格重复表头 / 性能守卫。
- **token 切分**（`text_split.rs`）：预算与拼接恢复 / Unicode（emoji/代理对/组合字符不 panic 不丢内容）/ overlap 尾部 / 小输入与空输入 / counter 无 boundaries 返回 None。
- **哈希与身份**（`db/utils.rs`）：FNV-1a 确定性 / 同内容同 id（幂等）/ 内容变化 id 变化 / 同文档重复内容 id 唯一。
- **缓存**（`db/embedding_cache.rs`）：put/get/覆盖写/清空 roundtrip / 键含 model|dim|hash / 空批次 noop。
- **工厂路由 I-7**（`db/chunk_splitter.rs`）：md→AST（path_json）/ txt·未知→纯文本 / rs→符号感知 / html→AST / opml→树形。

---

## 8. 分阶段实施计划与验收（全部完成）

| 阶段 | 内容 | 验收标准 | 状态 |
|---|---|---|---|
| **P0-1** | ChunkBudget + TokenCounter + Validator + Normalizer + 可观测截断 + 首批测试 | `cargo test` 全绿；索引含超长节的中文/英文 MD，`ValidationReport.truncated_count = 0`；`KbIndexResult.truncated_chunks` 出现 | ✅ 已完成 |
| **P0-2** | AST 分组 token 化 + token 边界切分 | 英文文档 chunk 数下降（token 利用率 21%→85%）；I-2 全绿 | ✅ 已完成 |
| **P0-3** | 测试矩阵补全 + 性能守卫 | §7 全矩阵通过；性能守卫通过 | ✅ 已完成 |
| **P0-4** | 配置版本化 + 过期提示 | 修改 chunk_size 后 `status.stale=true`；前端出现重建提示；确认后全量重建 | ✅ 已完成（后端全量；前端横幅见 §11 偏差 3） |
| **P0-5** | content_hash + embedding cache | 改 1 行后增量索引：仅重嵌变化 chunk（`[pipeline] 缓存命中 N 条`）；缓存命中率上报 | ✅ 已完成 |

---

## 9. 风险与兼容性（现状复核）

1. **chunk_size 语义变更（字符 → token）**：已索引数据需**全量重建**（schema v5 + 内容哈希 id + token 口径三重迁移，见《代码审查报告》§5）；用户已有配置 448 对中文行为几乎不变、对英文显著变好；UI 已标注单位。
2. **chunk 边界变化**：AST 分组上限从 1.25× 字符改为 target token → chunk 数量与边界会变（英文更明显）；检索质量经 `retrieval_eval` 基准度量（首轮基线 v2：Recall@10≈0.605 / avg 1290ms）。
3. **Validator 性能**：每 chunk 一次 tokenize（远快于 embedding，<1% 开销）；P0-2 已规划块级缓存计数（已实现）。
4. **tokenizer 不可用**（模型未初始化）：Validator 降级字符预算 + `degraded_token_count` 上报；此时 embedding 本就不执行，语义自洽。
5. **缓存正确性（P0-5）**：cache key 含 `model|dim|content_hash`，模型/维度/内容任一变化自然失效——**不依赖人工失效**，比设计初稿更稳。
6. **回滚**：P0-1/P0-2 为纯函数式改造（Validator 不改变 chunk 语义，只强制预算）；但 P0-4/P0-5 变更了索引元数据与主键语义，回滚需全量重建索引。

---

## 10. 评审结论（2025 落地基线，已兑现）

1. **默认数值**：`hard=504 / target=448 / overlap=56 / min≈161 / prefix=40`（512 窗口示例，公式见 §2.1）。
2. **字段语义**：`chunk_size` 保名升级语义为 token（不新增字段，不做持久化迁移；UI 标注单位变化）。
3. **降级策略**：允许显式降级——原子块超限允许截断但必须计数 + 告警 + UI 可见；健康态要求 `truncated_count = 0`。
4. **配置版本化**：P0-4 并入 P0-1 一并落地（`IndexMeta.chunk_params_version` + 过期检测 + 前端重建提示）。

---

## 11. 实施状态（P0-1 / P0-2 / P0-3 / P0-4 / P0-5 全部落地）

### P0-1（预算体系 + 可观测截断）— 已完成

| 项 | 文件 | 说明 |
|---|---|---|
| ChunkBudget / TokenCounter / ReSplitStrategy / TokenBudgetValidator / ValidationReport / Normalizer | `core/document/token_budget.rs` + `core/db/token_budget.rs` | 预算单一来源；按类型重切（表格重复表头/代码保留首片符号/正文逐级降级）；原子块显式降级；`count_only` 供对话消息 |
| 不变量测试 | `token_budget.rs` 内 `#[cfg(test)]` + 引擎/切分/哈希/缓存测试 | fake counter 注入，`cargo test` 离线可跑 |
| 可观测截断 | `core/embedding.rs` | `tokenize_with_offsets`（含 special tokens 口径）；截断计数 + 每批 warn；`get_max_seq_len` 提升 pub(crate) |
| 管线集成 | `core/pipeline.rs` | `chunk_document` 出口统一走 Normalizer → Validator；截断/重切统计累加器 + `reset/read` |
| 配置默认值单一来源 | `core/config.rs` | `IndexerConfig::default()` 改由 `ChunkBudget::from_model_window` 生成；`chunk_params_version()` |
| 索引统计与版本化 | `core/indexer.rs` / `core/types.rs` | `KbIndexResult.truncated_chunks/resplit_chunks`；`KbStatus.stale`；`IndexMeta.chunk_params_version`；`sync_on_start` 参数变更跳过增量；`index_chat_session` 走 `count_only` |
| 配置校验 | `commands/knowledge.rs` | `kb_update_indexer_config` / `kb_index` 拒绝非法 chunk_size/overlap；`kb_embedding_info` 返回 `max_position_embeddings` |
| 前端 | `css_js/modules/agent.js` + 3 个 HTML | 打开设置拉取模型窗口展示提示 + 约束输入范围；保存时校验（与后端同规则）；tippy 文案更新为 token 语义；`ragDefaults.maxPositionEmbeddings` |

### P0-2（Token 感知分块）— 已完成

| 项 | 文件 | 说明 |
|---|---|---|
| 纯类型上移 document 层 | `core/document/token_budget.rs`（新增） | `ChunkBudget`/`TokenCounter`/`char_budget_pair` 放基础层（引擎可用），`db::token_budget` re-export 转发，外部调用点不变 |
| token 感知切分 | `core/document/text_split.rs` | `split_text_token_aware`：一次 tokenize + 按 token 预算定位切分点；overlap 按 token；字符模式保留为降级 |
| AST 引擎 token 化 | `core/document/chunk_engine.rs` | 贪心分组按 token 累积（块级缓存计数）；超长块 token 边界切分；**embed 前缀截断**（最近 ≤3 级 + ≤ `prefix_max_tokens`，context 保留完整 Markdown 渲染）；**组间 overlap**（前块正文尾部拼接）；移除 `oversize_factor`（1.25× 字符宽松上限取消，硬裁决归 Validator） |
| 各 splitter 适配 | `core/db/chunk_splitter.rs` | Markdown/HTML 引擎注入 token 预算与 counter；PlainText 走 token 感知；代码/树形按文本密度折算字符预算（`char_budget_pair`，英文 ~4 字符/token 自动放大窗口） |
| 默认配置更新 | `core/db/chunk_splitter.rs` | `MarkdownSplitConfig` 移除 `oversize_factor`，`min_body_reserve_chars` → `min_body_reserve_tokens`（64） |

### P0-3（测试矩阵）— 已完成（+28 条，全库 321 通过）

- **Validator**（`db/token_budget.rs`）：I-1~I-4、I-10 + 表格/代码/原子降级专项；
- **引擎**（`chunk_engine.rs`）：chunk ≤ token 预算 / 短节单 chunk / embed 前缀 ≤3 级且 ≤ 预算 / 组间 overlap 尾部 / 超长表格重复表头 / **性能守卫**（~1MB 文档分块 < 20s，防 O(n²)）；
- **token 切分**（`text_split.rs`）：预算与拼接恢复 / **Unicode**（emoji/代理对/组合字符不 panic 不丢内容）/ overlap 尾部 / 小输入与空输入 / counter 无 boundaries 时返回 None；
- **哈希与身份**（`db/utils.rs`）：FNV-1a 确定性 / 同内容同 id（幂等）/ 内容变化 id 变化 / **同文档重复内容 id 唯一**（LanceDB 主键约束）；
- **缓存**（`db/embedding_cache.rs`）：put/get/覆盖写/清空 roundtrip / 键含 model|dim|hash / 空批次 noop；
- **工厂路由 I-7**（`db/chunk_splitter.rs`）：md→AST（path_json）/ txt·未知→纯文本 / rs→符号感知 / html→AST / opml→树形。

### P0-5（内容哈希 + Embedding 缓存）— 已完成

| 项 | 文件 | 说明 |
|---|---|---|
| 稳定哈希 | `core/db/utils.rs` | `fnv1a_128` + `stable_hash_hex`（零依赖 FNV-1a 128；非加密用途，KB 规模碰撞可忽略；**偏离设计初稿的 SHA256**——避免新增依赖，文档记录） |
| 内容哈希 id | `core/db/utils.rs` | `build_document_chunks` id = `rel_path#hash(版本+路径+位置+文本+元数据[含 tags])`——幂等、内容敏感、同文档唯一（替代随机 UUID） |
| 持久缓存 | `core/db/embedding_cache.rs`（新增） | SQLite 单表，键 = `model|dim|content_hash`（模型/维度/内容变化 → 自然失效，**不依赖人工失效**）；按最旧裁剪（上限 10 万条 ≈150MB） |
| 管线集成 | `core/pipeline.rs` | `embed_chunks(chunks, progress, cache_dir)`：命中跳过推理、只推理变化 chunk、结果按原序组装；缓存失败降级全量推理（不阻断索引） |
| 调用点 | `core/indexer.rs` | 4 处 `embed_chunks` 传入 `utils::get_cache_dir(dir_path)`；`clear_inner` 同步清空缓存 |

### 与设计的偏差（有意为之）

1. **模块拆分为两层**：纯类型（`ChunkBudget`/`TokenCounter`/`char_budget_pair`）落在 `core/document/token_budget.rs`（供分块引擎使用），裁决层（Validator/重切策略/Normalizer）留在 `core/db/token_budget.rs`（依赖 `ChunkResult`）并 `pub use` 转发纯类型——外部调用点（`core::db::token_budget::*`）不变，且"document 不依赖 db"的分层保持。
2. **测试文件**：`#[cfg(test)]` 内联各模块（非独立文件），需要访问内部策略/辅助；共享 fake counter 位于 `document/token_budget.rs::test_util`（`pub(crate)`）。
3. **前端 stale 横幅延后**：`kb-status-text/kb-status-dot/kb-progress` 为无 JS 接线的残留 UI 元素，本次先落地后端功能（`status.stale` + 启动同步跳过 + 日志告警）；横幅待既有 KB 状态流接线后补。
4. **哈希算法**：`FNV-1a 128` 替代设计初稿的 SHA256——避免新增 `sha2` 依赖；非加密用途，KB 规模（10^6 级 chunk）碰撞概率可忽略，且碰撞后果仅是缓存未命中/同 id 覆盖（幂等）。
5. **`recommended_chunk_size/overlap`**：保留为 `ChunkBudget` 的便捷委托（`#[allow(dead_code)]`）。
6. **组间 overlap 的头部预留**：引擎按"target + overlap ≤ hard_max"预留 overlap 头寸后分组，拼接前块正文尾部（按密度折算字符）；硬上限仍由 Validator 兜底。

### 验收（P0-1 ~ P0-5 验收标准，当前状态）

- `cargo test --lib`：**321 通过 / 0 失败**（含 token 预算/缓存/frontmatter/路由等新增测试）；`cargo check --all-targets`：0 警告；
- 运行时：索引含超长节的中文/英文 MD，`KbIndexResult.truncated_chunks = 0`（常规超限被自动重切）；仅当存在原子块（超宽表格/单行超长）时才 >0 且日志告警；
- 增量索引：修改文件一行后，未变化 chunk 命中 embedding 缓存（`[pipeline] 缓存命中 N 条` 日志），只重嵌变化 chunk；
- 配置漂移：修改 chunk_size 后 `status.stale=true`、启动同步跳过增量、日志告警（前端横幅见偏差 3）。

---

## 12. 实施状态速览（实现位置）

| 层 | 文件 | 职责 |
|---|---|---|
| 纯类型层 | `core/document/token_budget.rs` | `ChunkBudget` / `TokenCounter` / `char_budget_pair` / `budget_from_config` / `max_chunk_tokens` / `global_token_counter`——document 基础层，供分块引擎使用，无 db 依赖 |
| 裁决层 | `core/db/token_budget.rs` | `TokenBudgetValidator` / `ValidationReport` / 表格·代码·正文三策略 / `normalize_chunks` / `count_only`——最终裁决 + 重切 + 显式降级 |
| 强制点 | `core/pipeline.rs` | `chunk_document`：Normalizer → Validator → FrontMatter 元数据注入 → 内容哈希 id；`embed_chunks`：缓存命中跳过推理 |
| 截断计数 | `core/embedding.rs` | `tokenize_with_offsets`（含 special tokens）；截断原子计数 + 每批 warn——最后兜底但可观测 |
| 统计透传 | `core/indexer.rs` / `core/types.rs` | `KbIndexResult.truncated_chunks/resplit_chunks`；`KbStatus.stale`；`IndexMeta.chunk_params_version`；`sync_on_start` 版本守卫 |
