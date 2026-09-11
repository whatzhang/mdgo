# 文档解析管线升级方案 v2（Plan B v2：anydoc + pdf-inspector）

> 状态：**待评审 v2**（v1 已评审，本版按评审意见修订契约层；仍不含已落地代码）
> 目标版本：v1.1
> 影响面：`core/pipeline.rs`、`core/indexer.rs`、`core/document/*`、`core/db/*`、`core/config.rs`、`core/types.rs`、`commands/*`、`tauri/src-tauri/Cargo.toml`、`main.html`
> 证据：上游 API 结论均为**一手源码核验**（见 §12）；本地代码结论均标注 `文件:行号`
> 配套：`分块 Token 预算设计.md`（预算不变式）、`混合检索逻辑契约.md`（检索契约）、`Embedding 缓存设计.md`（缓存范式）

---

## 0. 结论先行

用 **anydoc**（纯 Rust、MIT）接管 Office/旧格式/ODF/RTF/EPUB 的「格式 → 结构化文本」，用 **pdf-inspector** 接管 PDF 的「提取 + 逐页内容 + 页码/坐标 + OCR 路由」，两者统一收敛到 **唯一入口 `DocumentLoader`**（索引与预览共用），并让分块按「内容形态」而非源扩展名路由。

### 0.1 三条架构原则（本版确立）

> **1. 转换器负责恢复结构，Chunker 负责语义切分。**
> **2. Page 是 Source Provenance，不是 Chunk 边界。**
> **3. Markdown 是转换层的交换格式；mdgo 的内部数据模型是既有的 `DocumentNode` 树 + provenance（本版只为它补 `source` 字段，不新建 AST）。**

### 0.2 v2 相对 v1 的改动（评审采纳表）

| 评审项 | 判定 | v2 落地 |
|---|---|---|
| P0-1「不要退化成 String + page」 | 方向采纳，表述修正 | 不新建 AST；给既有 `NodeMetadata` 加 `source: SourceLocation`；转换器产出 `SourceSegment` + **构造式行区间映射**（§4.3） |
| P0-2「PDF 不要逐页 chunk」 | **完全采纳** | 跨页语义 chunk + provenance 并集：chunk 由语义定界，`page_start/page_end` + `source_spans` 只做溯源（§4.4） |
| P0-3「不要靠 byte offset 反查」 | 采纳（并补实证） | 偏移**构造**而非**反查**；`pipeline.rs:151-179` 的 frontmatter/HTML 清洗正是"反查必错"的现成证据（§4.3） |
| converter 用结构体 | 采纳 | `ConverterInfo { id, version, options_hash }`（§5.2） |
| **版本契约按类型失效** | **采纳并在 V1 实现**（裁决 R2） | chunk 落库 `source_kind` + `converter`；`KbStatus.stale_kinds`；`params_version_mismatch` 改为按 kind；新增「只重建受影响类型」（§5.5） |
| 转换缓存用 `content_hash` | 采纳（并补硬论据） | 主键 = `source_hash(SHA-256) + converter_id + converter_version + options_hash`；不做 mtime/size 拼串（§5.6） |
| FileKind 不耦合 RetrievalIntent | 采纳 | 拆出中性 `FileCapabilities`（能力位），QueryPlan 从能力位推导扩展名集合（§4.5 / §5.1） |
| `max_bytes` 不写死在 FileKind | 采纳 | 独立 `ConversionPolicy`（§4.6） |
| `skipped_files` 语义混用 | 采纳 | 拆 `DocStatus`（doc 级）+ `PageDiagnostic`（page 级）（§5.3） |
| `KbIndexResult` 拆分 | 采纳 | `KbIndexResult` + `IndexDiagnostics`（§5.3） |
| preview 与 index 共用 loader | **采纳并升级为 P0 边界** | `DocumentLoader → ConversionCache → DocumentSource` 为索引与预览唯一通路（§4.1） |
| Phase 0 拆 0A/0B/0C | 采纳（**补发布约束**） | 拆阶段可以，**拆发布不行**：改变 chunk 产物的阶段必须与版本升级同批发布（§6） |
| 验收标准太弱 | 采纳（**改两层指标**） | 转换保真度 / 分块质量分层度量（§8.4） |
| A/B 样本量 | 采纳 | 分层抽样 30 篇 / 60–100 问，并标注统计效力边界（§8.3） |
| bbox 数据模型 Phase 1 预留 | 采纳 | `SourceLocation.bbox` 从 Phase 1 起就在模型里，只填 `page`（§5.2/§5.4） |
| 「真流式」 | **部分不采纳（做不到）** | anydoc/pdf-inspector 入口均为整份 `&[u8]` → 改为大小护栏 + 大 PDF 页批处理（§9） |
| 「按类型失效几乎无额外复杂度」 | 采纳裁决，但成本诚实入账 | 见 §5.5 成本项与 §10 |
| PPTX slide / EPUB chapter provenance | 采纳裁决：**只做 Heading 层级近似** | 明确列为非目标；`SourceLocation.slide/chapter` 字段预留但不填（§2.3） |
| 表格分块范围 | 采纳裁决：**只补真缺口** | 表头重复/表格原子性**已实现**（`chunk_engine.rs:396/577` + 测试 `:721`）→ 只补「表头列名进 metadata」（§4.7） |

---

## 0.3 实施状态（滚动更新）

| 阶段 | 状态 | 证据 |
|---|---|---|
| Phase 0A 注册表 + 能力位 + `ConversionPolicy` | ✅ 已落地 | `core/document/filekind.rs`（新增 60 项声明、能力位、策略、派生查询）；`db/utils.rs::KB_SUPPORTED_EXTS` 删除；`indexer::classify_ext`、`query_plan::intent_allowed_exts`、工厂注册、`scan_directory`、watcher 增量全部改为派生 |
| Phase 0B `DocumentLoader` + Converter trait + preview 统一 | ✅ 已落地 | `core/document/loader.rs`（新增 `SkipReason`/`DocStatus`/`PageDiagnostic`/`ConverterInfo`/`PageSpan`/`DocumentSource`/`load_document`）；`pipeline::read_document` 删除，四个调用点改走 `load_document`；`chunk_document(&DocumentSource, …)` 按注册表 `form` 路由；`commands/doc.rs::document_preview` 与索引共用同一入口；新增跳过统计（镜像 `budget_stats` 基线差分） |
| Phase 0C 版本契约 v2 + 按类型失效 | ✅ 已落地 | `CHUNK_IDENTITY_VERSION` → `mdgo-chunk-v2`；`chunk_params_version()` → `budget-v2:…:registry-3`；`DocumentChunk`/`SearchHit`/BM25/符号路/RRF 全链路新增 `source_kind`+`converter`；`IndexMeta.kind_converters` 快照；`KbStatus.stale_kinds`；`params_version_mismatch` 旁新增 `mismatched_kinds`（增量按 kind 放行）；`Indexer::reindex_kinds` + `kb_reindex_kinds` 命令 |
| Phase 1 PDF（pdf-inspector + 跨页语义 chunk + 页码） | ✅ 已落地 | `pdf-inspector 1.19` 接入（`Converter::PdfInspector`）；pdf 注册为 `form=Markdown` → 复用 comrak AST 语义分块（C3）；逐页提取 + `page_spans`/`line_page_map` **构造式**记账（§4.3）；`detect_pdf_mem` 预分类短路；`PdfError` 变体 → `SkipReason` 穷尽映射；部分索引（Q8）产出 `DocStatus::PartiallyIndexed{skipped_pages}` + `PageDiagnostic`。**chunk 级 provenance 已落库**：`Chunk`/`ChunkResult`/`DocumentChunk` 全链路携带 `line_start/line_end` → `page_start`/`page_end`/`source_spans`，LanceDB 建表与写入同步，`SearchHit` 透传；`ChunkSplitter::split_with_pages` 默认方法 + `MarkdownChunkSplitter` 覆盖；**变换守卫**（`cleaned.len() == content.len()`）保证行号未漂移才注入页映射 |
| Phase 2 anydoc 19 格式 + 前端守卫 + DocAgent 白名单 | ✅ 已落地 | `anydoc = "=0.2.4"` 接入（`Converter::AnyDoc`）；注册 19 个扩展名（doc/docx/docm、ppt/pps/pot/pptx/pptm/ppsx/ppsm、xls/xlsx/xlsm/xlsb、odt/ods/odp、rtf/epub）统一 `form=Markdown` + `binary=true` + `source_kind="office"`；`ConvertError` 七变体 → `SkipReason` 映射（含 `#[non_exhaustive]` catch-all，不 panic）；前端两条分发链新增 `renderConvertedDocFile`（**只传路径**，不搬字节）；`supportsEdit` 显式排除；DocAgent 白名单改为 `filekind::is_document_material`；`docagent::read_text_at` 改走 loader |
| Phase 3 转换缓存 + 性能收尾 | ✅ 核心已落地 | `core/db/conversion_cache.rs`：SHA-256(内容) + converter id + version + options hash 四元组主键；`Indexer::load_with_cache` 接入 5 个装载点；LRU 裁剪 `CACHE_MAX_ENTRIES=20_000`。**页批处理（页批次抽取）经评估后放弃**，理由见下方偏差第 5 条 |

**验证状态（本轮交付）**

- `cargo test --lib` = **433 passed / 0 failed**，`cargo check --lib --tests` **零 warning**。
- **真实样本验收**（新增 `src/acceptance.rs`，经 `MDGO_ACCEPT_DIR` 启用）：15 个真实样本 → 成功 13 / 跳过 2，累计 160 chunk。
  - 覆盖：`.doc`/`.ppt`/`.xls`（**旧版 OLE 二进制，真实样本**）、`.docx`×3、`.pptx`×2、`.xlsx`、`.rtf`、多语言文本 PDF、两个扫描件 PDF、`.md`/`.txt` 原生基线（含子目录递归）。
  - 关键结论：OLE `.doc` 抽出标题层级 + 超链接（11016 字符 / 16 标题）；OLE `.xls` 抽出**带表头的 GFM 表格**；`.pptx` 保留 slide 边界为 H2 标题（R1 的 Heading 近似成立）；文本 PDF 页码 provenance **15/15 chunk 全覆盖**（`page_start/page_end` 落在 1..3）；两个扫描件 PDF 正确报 `needs_ocr`（经查 `/Font=0` 且仅含 image 对象 → 判定正确）。
  - 乱码率（U+FFFD 占比）全部样本 **0.00%**。
  - 三个"疑似低产"样本经**溯源核对后判定为正确**，非遗漏：`sample-1.rtf`（824KB 中 808KB 是内嵌 EMF 十六进制块，可读正文仅约 637 字符）、两个 `.pptx`（slide XML 可见文本本就只有 56/72 字符，为图片型演示稿）。核对方法与数据见下方"验收方法"。
- **转换缓存机制验收**：同一 `.ppt` 首次 18.58 ms → 二次 0.79 µs（命中，条目数不变），且还原的 `text`/`source_kind`/`page_spans` 与首次逐字节一致。
- **跨层一致性守卫**（单测 `frontend_ext_tables_match_registry`）已升级为**全域不变式**，四处扩展名清单必须一致：
  1. 注册表 ⇄ `main.html::CONVERTED_DOC_EXT_SET`（Office 需转换预览集合 = 注册表 Office 全集 − 走原生渲染器的 `xls`/`xlsx`）；
  2. 该集合 ⊆ `DOC_EXT_SET`（否则 `checkFileExt` 直接拒开）；
  3. `support.js::CONVERTED_DOC_RE` 与之逐项相等；
  4. `_EXT_TYPE_MAP` 覆盖每个 Office 扩展名；
  5. **`注册表扩展名 ∪ DOC_FILE_NAMES ⊆ 前端可打开集合`**——「后端能索引却点不开」这类缺陷从此由测试拦住（该条不变式在本轮实际抓出并修掉了 7 个扩展名）。
  **已实证守卫会失败**：人为删 `epub` / 删 `markdown` → 测试均以明确信息报错（含"后端可索引、但前端 checkFileExt 会拒开"）。
- **页码归属真值校验**：`assemble_pdf_pages` 的单测与验收 harness 都改为**由正文反推真值**（用 `page_spans` 的字节区间数换行符得出每页真实行区间），而不是把实现的游标算术抄一遍。这一改动直接抓出了上面 R1 的严重缺陷——原写法把 off-by-one 固化成了"契约"。真实 PDF（3 页多语言）现按真值逐页比对通过。
- **未运行**：§8.3 的分层 A/B（30 篇 PDF / 60–100 问的 Recall@k 基线对比）——需要带标注的问句-文档对语料，仓库内 `retrieval_eval/` 只有示例格式，本轮未构造该语料，故**不声明检索质量提升**。首次升级到本次交付版本会强制一次全量重建（旧索引无 `source_kind` 列）。

**验收与 code review 结果（第二轮，含 1 个严重缺陷）**

独立对抗式审计（另一 agent，只读代码 + 临时测试验证，事后已还原工作树）发现并已修复：

| # | 严重度 | 问题 | 修复 | 证据 |
|---|---|---|---|---|
| **R1** | **严重** | **PDF 页→行映射每过一个页边界错 1 行且随页号累积**：拼接页间分隔 `"\n\n"` 时写成 `cur_line += 2`。上一页正文经 `trim_end` 后不带尾换行，第一个 `\n` 只是结束上一页最后一行，**只有第二个 `\n` 才多出一行**，故应为 `+= 1`。第 N 页偏移 +(N−1)，导致页码引用系统性错位（每页首行判给上一页），还会产出 `page_start=None` 却有 `page_end=Some(2)` 的自相矛盾 provenance | `loader.rs::assemble_pdf_pages`：`cur_line += 1` | 修复前测试三条全红：合成 3 页「实现 `[5,6)` vs 真值 `[4,5)`」；真实 `sample-multilingual-text.pdf`「实现 `[26,53)` vs 真值 `[25,52)`」；部分索引场景「跳过第 2 页后第 3 页错位」。修复后 414 passed |
| R2 | 中 | `load_document` 的"前置护栏"实际在 `fs::read` **之后**（注释却称"避免把 2GB 读进内存"），`max_file_bytes` 形同虚设 | 抽出 `precheck_size` 并在读盘前调用；`load_document_bytes` 保留同一判断覆盖"字节由调用方提供"路径 | 代码顺序（`precheck_size` 为 `load_document` 第一条语句）；**未加自动化测试**——需造 200MB 临时文件，CI 代价不值 |
| R3 | 中 | DocAgent 对**已登记但转换失败**的文件仍退化为 `from_utf8_lossy` → 扫描件 PDF/加密 docx 的原始字节被当正文送进模型上下文（正是 N7 要修的"垃圾元数据"） | `read_text_at`：已登记格式失败即报错（可解释）；lossy 兜底仅保留给**未登记**格式 | 新增测试 `registered_binary_never_falls_back_to_lossy_garbage`（双向覆盖：`.docx` 报错 / `.log` 保持 lossy） |
| R4 | 中 | `ConverterInfo::PDF_INSPECTOR.version` 是手写常量，而 Cargo.toml 用 caret `1.19` → `cargo update` 静默升到 1.19.x 就换了实现，缓存主键与 stale 快照却不变，旧转换结果继续命中且不提示 | Cargo.toml 改 `pdf-inspector = "=1.19.0"`（与 anydoc `=0.2.4` 同口径），常量与断言同步为 `1.19.0` | 升级必须显式改 Cargo.toml，常量被迫同步 |
| R5 | 中 | **按类型失效完全没有前端入口**（§7.2 F5 未实现）：`stale_kinds`/`kb_reindex_kinds` 在前端零引用 → R2 的"只重建受影响类型"对用户不可用，且单个转换器换代时横幅仍误报"分块参数已变更" | `main.html`：区分两种 stale（`stale_kinds` 非空 → 列出类型 + 「只重建 X」/「全量重建」双按钮；为空 → 仅全量）；新增 `renderStaleActions`/`reindexStaleKinds` 调 `kb_reindex_kinds` | 前端符号核对：`stale_kinds` 已消费、「kb_reindex_kinds」可调用 |
| R6 | 中 | **前端打不开后端能索引的文件**：注册表在册 46 个扩展名，而前端 `checkFileExt` 只放行 `DOC∪CODE∪视频/图片/RAW` → `markdown`/`mdown`/`mdx`/`tsv`/`cfg`/`makefile`/`gnumakefile` 被拒开（检索结果里能搜到、点开提示"文件类型不支持"） | 补进 `DOC_EXT_SET`/`CODE_EXT_SET`/`_EXT_TYPE_MAP`；Markdown 家族改为按 Markdown 渲染（`isMarkdownDoc`，两条链同步） | 跨层守卫升级为**全域不变式**并已实证会失败（删掉 `markdown` → 报「后端可索引、但前端拒开」） |
| R7 | 轻 | `ext_of` 与 `Path::extension()` 在尾点文件名（`"trailing."`）上不等价，注释宣称"语义一致"不准确 | 保留实现（空扩展名与 `None` 都不在册，**无路由影响**），作为已知语义差异记录 | 审计实测 21 组对照，仅此一处不一致 |
| R8 | 轻 | `document_preview` 未接入转换缓存，违反 §4.1/§7.3 的 `DocumentLoader → ConversionCache` 唯一通路；`DocumentPreview.bytes` 是截断前**字节**数而 `text` 按**字符**截断 | 抽出 `load_preview_source`：给出 `dir_path` 时走 `ConversionCache::open_shared(get_cache_dir(dir))`，否则退化为直接装载。**`bytes` 的命名/语义不一致未改**（前端未消费，留待顺手修） | 预览与索引现在共用同一缓存目录 |
| **R9** | **严重** | **`index_files_batch` 会把"因类型过期而被跳过"的 kind 也刷进快照，静默清空 `stale_kinds`**：`touched = source_kinds_of(files)` 用的是"本批收到的文件"，而其中已被"该类型过期 → `continue`"的守卫剔除。后果：升级转换器后 watcher 只要收到**一个**该类型文件的事件，快照即被刷成当前转换器 → `stale_kinds` 变空 → 用户不再被提示重建，而库里该类型 chunk 仍是旧转换器产物（**正是按类型失效要防的新旧混库**）。转换失败的文件同理 | 改为只统计**实际成功写入**的 kind（`written_kinds`，两条写入路径分别记录），删除已无用的 `source_kinds_of` | 代码审查发现；**未加自动化测试**（`index_files_batch` 需真实 LanceDB+BM25+embedding 模型，属重量级集成路径），按代码路径核对确认 |
| R10 | 中 | `reindex_kinds`（本版新增入口）缺 `params_version_mismatch` 全局守卫，而另三个索引入口都有 → 分块参数已变更时仍可写入新粒度 chunk（新旧分块口径混库） | `reindex_kinds` 开头加守卫，返回可解释错误："分块参数已变更：请先执行一次「全量重建」，再使用按类型重建" | 与 `index_file`/`index_files_batch`/`index_unindexed` 同口径 |
| R11 | 中 | **分块器内部的 frontmatter 剥离会让行号前移**：`markdown.rs::parse` 在 `chunk_document` 的守卫**之后**还会做 `\r\n→\n` 归一与 `strip_frontmatter`。归一不改变行数（页映射是行号制，安全），但剥离会**删掉开头若干行** → 页码系统性偏移且无任何报错。触发条件窄（PDF 转换产物首行恰为 `---` 且前 50 行含 `键: 值`），但属"静默错页"同类风险 | `chunk_document` 增加第二道预检：用**同一个** `markdown::parse_frontmatter` 判定，只有剥离结果与输入逐字节一致时才注入页映射 | 保守但精确（用同一判定函数，不重复实现规则） |

**第三轮（补齐方案剩余交付）**

| # | 交付 | 实现 | 证据 |
|---|---|---|---|
| C1 | **决策 R3 / §4.7：表头列名进 metadata（`table_headers`）** | `Chunk` 增 `table_headers`；新函数 `extract_table_headers` 在 `make_chunk` 内按**「表头行 + GFM 分隔行」结构**提取（不是"行首是 `|`"——那会把数据行误判为表头）；全链路透传 `ChunkResult` → `DocumentChunk` → **LanceDB 新列**（建表 schema + RecordBatch 数组 + 行映射 + 符号路 select）→ `SearchHit` → RRF `Entry` | §8.1 `table_headers_metadata_extracted_and_not_in_embedding` + `table_header_detection_requires_separator_row` + `chunk_carries_table_headers_without_affecting_identity`；**并补了原本完全不存在的 LanceDB 往返测试**（见 C2）。**未建 TableChunker**（R3 明确不建） |
| C2 | **LanceDB 落库往返测试**（此前 `lance.rs` **零测试**） | `chunk_metadata_round_trips_through_lancedb`：真实建表→写入→读回，逐列核对 `chunk_type`/`source_kind`/`converter`/`source_spans`/`table_headers`/`page_start`/`page_end`。这条路径的风险是「schema 字段顺序 ↔ RecordBatch 数组顺序」错位——**相邻两个 Utf8 列互换不会编译报错，只会在运行时静默把 A 列的值写进 B 列** | 模型不可用时跳过（避免把环境依赖带进 CI）；本机实测**真跑通**（0.56s，无 skip） |
| C3 | **§5.3 / §7.2 F9：诊断拆分 + 诊断面板** | 后端：`pipeline::PartialFile` 采集器（镜像既有 `SkippedFile` 基线差分设计）+ `record_partial` 挂在 `load_with_cache`（一处覆盖所有索引入口）；`IndexMeta` 持久化两个清单（`#[serde(default)]` 兼容旧文件）；`KbStatus`/`KbIndexResult` 双路暴露；**按路径合并** `merge_diagnostics`。前端：`renderIndexDiagnostics` 挂进既有健康面板，**两类分开显示**并带页号 | 5 个新单测：`diagnostics_merge_preserves_untouched_and_refreshes_processed`、`skipped_and_partial_are_kept_separate`、`diagnostic_dtos_json_keys_are_frontend_contract`（钉死前端读的 snake_case 键名）、`record_partial_collects_skipped_pages`、`index_meta_without_diagnostics_fields_still_loads`（**旧 `index_meta.json` 缺字段必须仍能反序列化**，否则整个索引被当作未索引、被迫全库重建） |

> **Q8 的部分索引结果现在用户可见了**：此前 `DocStatus::PartiallyIndexed{skipped_pages}` 与 `PageDiagnostic` 虽然算得很完整，但 indexer 从不聚合、`KbIndexResult` 也无字段，等于白算。现在「文件进了库但有页被跳过」与「整个文件没进库」在仪表盘上**分开列出**（§8.4 可解释性：路径 + 原因 + 页码）。

**未完成项（诚实清单）**

- **§7.2.2 两条分发链收敛为一张 `ext → renderer` 注册表**：**未做**（仅按 §7.2 F3/F4 在两条链各自插入了分支并保持同步）。理由：`main.html` 是 52k 行手维护文件且**无前端测试框架**，改动主分发结构会同时影响全部既有文件类型的打开行为，回归风险与收益不成比例。**这是本版最大的一处主动裁剪**，建议单独立项（先给前端加最小冒烟测试再动）。
- **§7.2.1「预览不经前端搬字节」只做到一半**：`renderConvertedDocFile` 只传路径（已绕开 `getFile()` 读进 JS），但 `renderFile` 在分派前仍无条件 `getFile()`（`main.html`，50MB 门禁）→ 转换类文件仍是双倍搬运。彻底修需要把 `getFile()` 下沉到真正需要它的分支里，属同一条主分发链的结构调整，与上一条一并另行处理。
- **§4.5 Document 意图未按能力位推导**（现为 `doc_like_exts()` = `md/markdown/mdown/rst/txt`）：**保持现状，属有意为之**。经 git 核对，改造前就是 `Some(&["md","markdown","mdown","rst","txt"])`，与本版实现**逐项相同**——即本版忠实保留了既有检索语义，**不是回归**。§4.5 的 `searchable && !is_code` 是方案里的**目标态**；改成它会让含"文档/笔记"等词的查询额外召回 `.pdf/.docx/.html/.csv`，属**检索行为变更**，必须由 §8.3 的 A/B 来裁决，不能顺手改。
- **§5.2 `NodeMetadata.source`/`SourceLocation`、Q6/R1 的 `bbox`/`slide`/`chapter`"字段预留"未落地**：provenance 改由 `Chunk.line_start/line_end` 直接传播（用户可见结果等价），但方案里写的**数据结构本身不存在**。属"契约写法与实现不一致"，非功能缺失；已在"本轮新增偏差 6"记录。
- **§8.3 分层 A/B 检索回归**：**已运行（部分）**——`src/bin/benchmark` 在改动后的代码上全链路跑通，42 条查询出齐 Recall@5/10/20、MRR、NDCG、延迟；结果记入 `retrieval_eval/README.md` 的**基线 v5**。**但这不是方案 §8.3 要求的严格 A/B**：§8.3 要求「30 篇 PDF + 60–100 问、A=`pdf-extract`+PlainText vs B=`pdf-inspector`+跨页 chunk」，而本仓库的评测语料是**仓库自身**（以 markdown/代码为主，PDF 极少），且语料规模在 v4→v5 之间增长了约 60%（1285→2060 文件）。因此结论仅限于：**改动后检索链路可正常跑通、指标与 v4 同量级、无数量级退化**；**不宣称"提升了召回"**。严格 A/B 需要专门构造 30 篇 PDF 语料并同一语料跑两版代码（两次全量重建）。运行方式：基准跑在**仓库的临时副本**上，**未清空**仓库自身 `.mdgo` 索引。
- **§8.2 夹具**：未落 `tests/fixtures/convert/`（改为**真实样本 + `MDGO_ACCEPT_DIR`** 的 `src/acceptance.rs`）。理由：真实样本比合成夹具更能暴露解析器在真实文件上的行为（本轮正是靠真实样本确认了 3 个"疑似低产"样本其实正确、2 个扫描件判定正确）；代价是这些样本不进仓库，验收需人工指定目录。
- **Q9 体积增量**：未测量（未做 release 体积对比）。
- **`DocumentPreview.bytes` 命名/语义不一致**（截断前字节数 vs `text` 按字符截断）：未改（前端未消费）。

**验收方法（可复现）**

```powershell
# 真实样本端到端验收（样本不进仓库，故经环境变量启用）
$env:MDGO_ACCEPT_DIR = '<含 doc/docx/ppt/pptx/xls/xlsx/rtf/pdf 的目录>'
$env:MDGO_ACCEPT_DUMP = '260'   # 可选：打印每个样本转换后正文前 260 字符，供人眼核对保真度
cd tauri\src-tauri
cargo test --lib acceptance -- --nocapture
```

**本轮新增偏差（需评审确认）**

1. **`docx` 在 0A 暂时移出白名单**（旧登记是 D1 幽灵条目，无提取器）；Phase 2 随 anydoc 一并登记。
2. **类型统计有意外可见变更**：`jsx/tsx/cs/swift/kt/bash/zsh/r/scala` 旧口径统计为"其他"，但一直由**代码语言感知分块器**处理（`CODE_LANG_SEPARATORS` 含它们）——这正是 D4 漂移。本版让统计跟随 `form = Code` → 归入"代码"。**类型分布图会变化**（有测试记录该 delta）。
3. **`log` 未登记、`.env`/`.gitignore` 从白名单删除**：三者被 `IgnoreMatcher::is_kb_file_allowed`（`db/utils.rs:273`）按"隐藏文件不索引"策略排除，登记只会重建死配置（D2 的另一半无法修）。
4. **`pdf`/Office 的 `category` 保持"其他"**：注册表 `category_of_ext` 不改统计词表，前端另有 `_EXT_TYPE_MAP` 负责分组；`form` 已为 `Markdown`。因此 `form_routing_is_consistent_with_capabilities` 断言的是**单向**不变式（`category=="Markdown"` ⇒ `form==Markdown`）。
5. **放弃"页批次抽取"**：方案 Phase 3 原列"页批处理"。评估后放弃——抽取分批既不减少最终拼接后的正文体积，也不降低整篇 comrak AST 的峰值内存；真正的杠杆是 `ConversionPolicy.max_file_bytes` 前置护栏。**这是与方案的一处主动裁剪**，非遗漏。
6. **`NodeMetadata.source` 未新增**：§5.2 原计划把 `SourceLocation` 挂到 AST 节点。实际改为让 `Chunk` 携带 `line_start/line_end`，在分块边界一次性折算为 `page_start/page_end/source_spans`——用户可见结果相同（命中即可定位页码），但少了一层每节点元数据的搬运与失效面。
7. **转换缓存落库形态**：主键四元组按方案，但 payload 存**单列 JSON**（而非方案里拆开的 text/line_page_map/diagnostics 多列），便于随 `DocumentSource` 字段演进而不改表结构。
8. **`core::pipeline` 可见性**：由 `mod` 提为 `pub(crate) mod`，使 in-crate 验收 harness 能复用真实 `chunk_document` 调用链（与既有 `pub(crate) mod model_download` 同惯例）；`core` 对 crate 外仍私有（L31 边界未破）。
9. **`pdf` 的过渡转换器已退役**：Phase 0A/0B 期间 pdf 走 `Converter::LegacyPdf`（沿用 `pdf-extract`）以保证行为等价；Phase 1 起已切换为 `Converter::PdfInspector`，`LegacyPdf` 保留在枚举中仅为兼容旧索引的 `converter` 值判定。
10. **前端扩展名清单仍是副本（受 Q1 约束）**：注册表是后端唯一真源，但 `main.html`/`support.js`/`file-system.js` 各自仍需一份（`index.html`/`index_cdn.html` 为冻结副本，Q1 决定只改 `main.html`）。为此新增跨层一致性单测把三处钉死，防止重演 D7。

**⚠ 依赖树重大发现：`lopdf` ↔ `time` 编译冲突（已打补丁，需评审）**

- `tantivy 0.26`（BM25 引擎）要求 `time = "^0.3.47"`；`time 0.3.47` 把 `FormatItem::StringLiteral` 改名为 `Literal`。
- `pdf-inspector ≥1.18.0` 依赖 `lopdf ^0.44.0`，其 `src/datetime.rs` 仍引用旧名 → **`lopdf` 直接编译失败**（上游 issue #518，master 注释原文：*"it referenced a nonexistent `FormatItem::StringLiteral`"*）。
- 三条常规出路都被堵死：① 不能降 `time`（tantivy 的 `^0.3.47` 下限）；② 不能升 `lopdf` 到 0.45（`^0.44.0` 不接受，`[patch]` 同样要求满足原版本需求）；③ 不能从下游关闭 lopdf 的默认 feature（`time` 在 `default` 里，而 Cargo feature 是**并集**语义——printpdf 能绕开是因为它是 lopdf 的**直接**依赖方，我们是经 pdf-inspector 的**传递**依赖方）。
- **采用的方案**：`[patch.crates-io] lopdf = { git = ".../lopdf", rev = "a15329d0…" }` —— 指向上游**修复后、版本号仍为 `0.44.0`** 的提交（0.45 的 release 提交才改版本号，故该 rev 满足 `^0.44.0`）。已通过 `cargo check` 验证。
- 影响：首次构建/CI 需访问 GitHub 拉取该 rev；`Cargo.lock` 已锁定。
- **移除条件**：`pdf-inspector` 依赖 `lopdf >= 0.45` 后删除补丁。**`lopdf 0.45.0` 已发布**，故该出口是现实的（Cargo.toml 内已注明）。

**依赖决策复核（第四轮：是否可用「降级 pdf-inspector」删掉该补丁）**

起因：外部评审提出「不该长期维护 lopdf 补丁，应升级 pdf-inspector 让其自行解决」。核查后发现该建议的**前提有误**（1.16.0 不是当前版；当前最新 = 我们已在用的 1.19.0），但「删掉补丁」这个目标本身值得评估，故做了一次完整审计：

| 核查项 | 结论 |
|---|---|
| pdf-inspector 版本 → lopdf 需求 | ≤0.1.7=`^0.41.0`；0.1.8~**1.17.0**=`^0.42.0`；**1.18.0 起 = `^0.44.0`**。→ 唯一的"删补丁"路径是**降级到 ≤1.17.0** |
| 降级是否可编译/可运行 | **可行**：`=1.17.0` + 删补丁 → cargo 解析出 registry 的 `lopdf 0.42.0`，补丁被报 `not used in the crate graph`；**当时 423 条测试全绿**（该数字为那一轮实验时的用例数，此后新增了 Agent 文档读取等用例，当前基线见 §0.3）、15 个真实样本验收逐项一致。API 为**纯超集**，我们消费的 `PdfType`/`PageMarkdown`/`PagesExtractionResult` **逐字节相同**，`PdfError` 变体相同 |
| 降级的能力代价 | **不等价**：1.17.0→1.19.0 = **+11053/−1392 行 / 45 文件**。1.17.0 **缺失**：`xref_repair`（重建 19 字节 classic xref 条目——ISO 32000-1 §7.5.4 要求 20 字节，一批公文归档导出器产出 19 字节，mupdf/pdfium/pdf.js/qpdf 均接受而 lopdf ≤0.44 **整篇加载失败**报 "invalid file trailer"；上游 lopdf#564 截至 2026-09 未发布）→ **这一类 PDF 会直接打不开**；`identity_overrides`（过期 CMap 修复 → 直击中日文/bcmaps 乱码，即 §3.2 陷阱 6 与 §8.4 的中日文验收项）；`bounded_load_options`（不可信输入资源上限）；新增 5 个 extractor 模块（`clip_boundaries`/`page_box`/`text_paint`/`geometry`/`scripts`）及 `content_stream.rs +1970`/`xobjects.rs +962`/`fonts.rs +283`/tables 系列改写。且降级会连带降级 `aes/cbc/cipher/inout/weezl` |
| 证据局限 | 上述能力差异来自**两个发布版 crate 原件的直接 diff** 与上游源码注释；**未在本机构造 19 字节 xref 的 PDF 做实测**。而"验收一致"的结论受样本量限制（15 个样本中只有 **1 篇**文本 PDF），**不足以探测上述差异**——不可据此认为两版等价 |
| **决策** | **保留补丁、留在 1.19.0。** 用"删 git source"换"PDF 解析能力倒退（含一类 PDF 完全打不开）"不划算。若将来 GitHub 可达性成为构建硬约束，另有**不损失能力**的出路：把修复版 lopdf 源码 vendor 进仓库改用 `path =` 补丁 |

**Phase 0 与方案的已知偏差（需评审确认）**

> 下表为 Phase 0A/0B 期间记录的历史偏差；其中第 1~3 条已并入上方"本轮新增偏差"（同一事实，不重复计），第 4 条已在 Phase 1 消解。

1. **D2 只修了一半，另一半"删掉"**：`.env` / `.gitignore` 被 `IgnoreMatcher::is_kb_file_allowed`（`db/utils.rs:273`）按"隐藏文件不索引"策略排除，登记它们只会重建死配置 → 二者**从白名单删除**（`Dockerfile` / `Makefile` / `GNUmakefile` 按 `Matcher::FileName` 修复）。同理 `*.log` 被同一函数排除，`log` 未登记。→ 见上方偏差 3。
2. **`docx` 在 0A 暂时移出白名单**（旧登记是 D1 幽灵条目，无提取器）；Phase 2 随 anydoc 一并登记。→ 见上方偏差 1。
3. **类型统计有意外可见变更**：`jsx/tsx/cs/swift/kt/bash/zsh/r/scala` 旧口径统计为"其他"，但一直由**代码语言感知分块器**处理（`CODE_LANG_SEPARATORS` 含它们）——这正是 D4 漂移。本版让统计跟随 `form = Code` → 归入"代码"。**类型分布图会变化**（有测试记录该 delta）。→ 见上方偏差 2。
4. **~~`pdf` 在 0A/0B 走过渡转换器 `Converter::LegacyPdf`~~**（沿用 `pdf-extract`），保证行为等价；**Phase 1 已换成 `PdfInspector`**。→ 见上方偏差 9。

---

## 1. 现状与问题（代码事实）

### 1.1 现有链路

```text
文件 → read_document()      → chunk_document()          → embed_chunks() → write_chunks()
       pipeline.rs:82            pipeline.rs:139            pipeline.rs:241    pipeline.rs:399
       （仅 pdf 走 pdf-extract；    （按 rel_path 扩展名
         其余一律 UTF-8 直读）          选 splitter）
```

四个调用点：`index_all`（`indexer.rs:433`）、`index_file`（`:585`）、`index_files_batch`（`:662`）、`index_unindexed`（`:2077`）。

### 1.2 现有资产盘点（**v2 新增：避免重复造**）

评审提出的"要建 AST / TableChunker"，实测**已经存在**：

| 资产 | 位置 | 说明 |
|---|---|---|
| **文档 AST** | `core/document/node.rs:7-54` | `NodeType { Root, Heading, Paragraph, CodeBlock, Table, List, Quote, ThematicBreak, HtmlBlock }`；`NodeMetadata { level, start_line, end_line }`；`DocumentNode` 树（Heading 的 children 含其下所有子块） |
| **行区间元数据** | `node.rs:50-53` | `start_line`/`end_line`（1-based，源码切片用）→ **比 byte offset 更稳的现成映射锚点** |
| **元素类型传播** | `chunk_engine.rs:30/169` | `Chunk.chunk_type`（由 `dominant_type(&blocks)` 从 AST 推导）；`ChunkResult.chunk_type`（`chunk_splitter.rs:31`）→ `DocumentChunk.chunk_type`（`lance.rs:49`） |
| **表格感知切分** | `chunk_engine.rs:396` + `:577-600` | `"table" => split_oversize_table`：≤3 行小表整体保留（阈值与 `token_budget::TableReSplitStrategy` 对齐）、行贪心分组、**每组重复 GFM 表头+分隔行**（`:597`）、超长单行原子 |
| **表格测试** | `chunk_engine.rs:721` | `oversize_table_repeats_header()`：断言 `chunk_type == "table"` 且正文含表头 |
| **表格解析开关** | `markdown.rs:29-32` | `options.extension.table = true`，注释明确"否则 Table 节点落入 Paragraph，chunk_type=table 永不产生" |

**结论**：现状不是"String + page"，而是 **"AST + 行区间 + 元素类型"**。真正缺的只有 **provenance（page/bbox/slide/chapter）** 与**多来源入口**。这决定了 v2 的改动面比 v1 更小、比"新建统一 AST"小得多。

### 1.3 已确认缺陷

| # | 缺陷 | 证据 | 后果 |
|---|---|---|---|
| D1 | `docx` 在白名单但无提取器 | `utils.rs:12` 含 `docx`；`pipeline.rs:82` 只对 `pdf` 特判 | docx 走 UTF-8 读 → `InvalidData` → 静默跳过 |
| D2 | 4 个白名单项永不命中 | `env`/`gitignore`/`dockerfile`/`makefile` 依赖 `Path::extension()`（`indexer.rs:2221`），而这四类文件名 `extension()` 为 `None` | 死配置 |
| D3 | watcher 绕过白名单 | `watcher.rs:667` → `index_files_batch` 无扩展名过滤；全量走 `scan_directory`（`indexer.rs:2185`） | 增量入库的格式在全量重建后静默消失；二进制被反复 UTF-8 读 |
| D4 | 意图白名单与索引白名单漂移 | `CODE_EXTENSIONS`（`query_plan.rs:31`）不含 `swift/kt/cs/scala/r/jsx/tsx/css/ps1/bash/zsh`，但都可被索引；`allowed_exts` 是硬过滤（`indexer.rs:940`） | 已入库文件在 Code 意图下漏检 |
| D5 | PDF 无结构、无页码 | `pdf-extract` 出纯文本 → `PlainTextChunkSplitter` | 无标题层级、无法定位到页、扫描件静默为空 |
| D6 | 失败不可解释 | 只有 `truncated_chunks`/`resplit_chunks`（`types.rs:12`），且**前端零消费**（grep 无命中，仅 `stale` 被消费） | 用户无法自查 |
| D7 | 五处格式清单并行 | `utils.rs:12`、`chunk_splitter.rs:1322`、`html_clean.rs:17`、`indexer.rs:219`、`query_plan.rs:31` | 漂移（D1/D2/D4 皆由此产生） |
| D8 | DocAgent / LLM 白名单只认 `md/txt/markdown` | `commands/doc.rs:77/126/190` 三处硬编码；`core/docagent/mod.rs:180-206` 用 `from_utf8_lossy` 直读无白名单 | **现状下 PDF/HTML/CSV 就无法作为 DocAgent 资料** |
| D9（v2 新增） | 无 provenance，且 stale 粒度是"整库" | `DocumentChunk`（`lance.rs:30`）无 page/source_kind/converter；`chunk_params_version()`（`config.rs:46`）单串比对 | 引用无法定位；任一转换器升级导致全库 stale |

---

## 2. 范围

### 2.1 核心（Plan B 本体）

| 编号 | 内容 |
|---|---|
| C1 | PDF：`pdf-extract` → **pdf-inspector**，逐页内容 + 页码 provenance |
| C2 | Office/旧格式/ODF/RTF/EPUB：接入 **anydoc**（`doc docm ppt pps pot pptm ppsx ppsm xls xlsm xlsb odt ods odp rtf epub` + 既有 `docx/pptx/xlsx`） |
| C3 | 转换产物按**内容形态**路由到既有 `MarkdownChunkSplitter`/`SemanticChunkEngine` |
| C4（v2） | **跨页语义分块 + provenance**：chunk 由语义定界，page 只做溯源（§4.4） |
| C5（v2） | **`DocumentLoader` 唯一入口**：索引与预览共用同一 loader 与转换缓存（§4.1） |

### 2.2 必要配套

| 编号 | 内容 |
|---|---|
| N1 | `FileKindRegistry`（唯一格式清单，含中性能力位）+ `ConversionPolicy` → 一次修 D1/D2/D3/D4/D7 |
| N2 | 版本契约 v2：chunk 落 `source_kind`+`converter`；**按类型失效**（§5.5） |
| N3 | 诊断拆分：`DocStatus` + `PageDiagnostic`；`KbIndexResult`/`IndexDiagnostics` 分离（§5.3） |
| N4 | 前端清单 + 二进制预览守卫（两条分发链） |
| N5 | 转换缓存（`content_hash` 主键） |
| N6 | 转换在 `spawn_blocking` 中执行（与既有 embedding 同范式，`pipeline.rs:309`） |
| N7 | DocAgent / LLM 侧白名单放开（修 D8） |
| N8（v2） | `NodeMetadata.source` + 行区间映射（§4.3） |

### 2.3 非目标（本版明确不做）

| 项 | 理由 |
|---|---|
| 扫描件本地 OCR | 需 `firecrawl-pdfium` + ONNX + 模型分发；且与 mdgo 现有 `ort` 版本冲突（§9） |
| 远程 OCR（`ocr:'hosted'`） | 与本地优先冲突 |
| **PPTX slide / EPUB chapter provenance** | **公开 API 拿不到**：anydoc `model::Block` 无 slide/chapter 维度；pdf-inspector 与 PPTX/EPUB 无关。改为 Heading 层级近似（裁决 R1）；字段预留不填 |
| bbox 高亮（实现） | 数据模型预留（`SourceLocation.bbox`），实现放 Phase 5 |
| 图片/附件资产入库 | Markdown 侧图片已是 alt text；`DocumentChunk` 无 asset 位 |
| **真流式转换** | anydoc/pdf-inspector 入口均为整份 `&[u8]` → 做不到；改为大小护栏 + 大 PDF 页批处理 |
| 独立 TableChunker / 表格 AST | 已存在表格感知路径（§1.2）；本版只补表头列名进 metadata |
| 新建 Unified Document AST | 已存在 `DocumentNode`（§1.2）；只补 provenance 字段 |

### 2.4 顺带修复

- D2：4 个永不命中项改为**文件名匹配**（`.env`/`Dockerfile`/`Makefile`/`.gitignore`）
- D4：意图白名单由**能力位**派生（§4.5），不再手写
- 补入白名单的纯文本格式：`markdown` `mdown` `rst` `log` `tsv` `jsonl` `bat` `cmd` `cc` `vue` `svelte` `astro` `dart` `gradle` `graphql` `gql` `proto`

---

## 3. 外部依赖评估（一手核验）

### 3.1 anydoc

| 项 | 事实 |
|---|---|
| 版本/许可 | crates.io `0.2.4`（2026-08-27）；MIT；`rust-version = 1.88`；edition 2024 |
| 依赖 | `cfb` `csv` `flate2` `encoding_rs` `log` `pdf-inspector 1.14.2` `quick-xml` `zip`；**零 feature flag**（`Cargo.toml` 无 `[features]` 段）→ PDF 解析栈必然随包编译 |
| 生产采用 | FastGPT（`@fastgpt-sdk/anydoc`，`packages/service/worker/readFile/extension/anydoc.ts`）——其 17 个补充格式**恰好就是本方案的 C2 清单**，PDF 不在其列 |

```rust
pub use error::ConvertError;
pub mod model;
pub enum Format { Doc, Docx, Odt, Pdf, Ppt, Pptx, Rtf, Epub, Excel, Ods, Odp, Csv }
impl Format {
    pub fn from_bytes(bytes: &[u8]) -> Option<Format>;   // 内容嗅探；CSV 无签名 → None
    pub fn from_extension(ext: &str) -> Option<Format>;  // docm→Docx、xls/xlsm/xlsb→Excel、pps/pot→Ppt
    pub fn from_path(path: &Path) -> Option<Format>;
}
pub fn to_markdown(path: impl AsRef<Path>) -> Result<String, ConvertError>;
pub fn to_markdown_bytes(bytes: &[u8], format: impl Into<Option<Format>>) -> Result<String, ConvertError>;
pub fn to_document(bytes: &[u8], format: impl Into<Option<Format>>) -> Result<model::Document, ConvertError>; // PDF 必定 Unsupported
```

**错误契约（`src/error.rs`）**

```rust
#[derive(Debug)]
#[non_exhaustive]                     // ← 必须保留 catch-all，且不得 panic
pub enum ConvertError {
    Unsupported(String),
    NeedsOcr { pages: Vec<u32>, page_count: u32 },      // pages 为 1-based
    Malformed { part: Option<String>, detail: String },
    Encrypted,
    ResourceLimit { limit: &'static str, detail: String },
    MissingPart { part: String },
    Io(std::io::Error),
}
impl ConvertError { pub fn code(&self) -> &'static str; }
```

**四条必须写进实现的发现**

1. **Rust 侧无 options 结构体**；图片在 Markdown 中以 **alt text** 呈现，字节只在 `to_document().assets`。FastGPT 的 `embeddedImageMode`/`asset:N` **不属于任何已发布的 `@firecrawl/anydoc`**（逐版本 d.ts 核对）→ **mdgo 不需要 asset 契约**。
2. **`.wps` 不在 `from_extension`**：需自行嗅探（`PK\x03\x04` + `word/document.xml` → `docx`，否则 `doc`）。本版**不支持**（Q3）。
3. **PDF 在 anydoc 内是薄委托**（`src/formats/pdf.rs` → `pdf_inspector::process_pdf_mem`），逐页结构拿不到 → PDF 必须直连 pdf-inspector（C1）。
4. **内容探测顺序**：`{\rtf` → OLE 魔数+流名 → ZIP 包身份（mimetype/OPC/根命名空间/路径前缀/约定位置）→ 前 1024 字节 `%PDF-` → `None`（CSV、空字节、普通 zip、**加密 OOXML**）。
   → **实现必须用 `from_bytes().or_else(from_path())`**：加密 OOXML 探测返回 `None`，只靠内容嗅探会退化成 `Unsupported`，丢掉"加密"这一更有价值的诊断。

### 3.2 pdf-inspector

| 项 | 事实 |
|---|---|
| 版本/许可 | crates.io `1.19.0`（2026-09-09）；MIT；`rust-version = 1.88` |
| 依赖 | 默认构建纯 Rust（`lopdf` + `rayon` 等），**无模型、无网络、无外部二进制** |
| 基准 | opendataloader-bench（200 PDF）Overall 0.875 / Reading order 0.915 / Tables 0.814 / Headings 0.788 / 0.470s。**两点保留**：① 官方声明只与"无模型解析的本地引擎"对比、OCR 关闭；② 被基准版本是 **0.2.6**，当前 crate 是 **1.19.0** → 分数不代表将引入的版本 |

```rust
process_pdf / process_pdf_mem(bytes) -> PdfProcessResult
//   { pdf_type, markdown: Option<String>, page_count, processing_time_ms,
//     pages_needing_ocr(1-indexed), ocr_reasons_by_page, title, confidence,
//     layout: LayoutComplexity, has_encoding_issues }
detect_pdf / detect_pdf_mem -> 仅分类（DetectOnly）
process_pdf_with_options(path, PdfOptions)     // .mode() .pages([1,3,5]/*1-idx*/) .detection() .markdown() .password()

extract_pages_markdown_mem(bytes, Option<&[u32]>) -> PagesExtractionResult
//   pages: Vec<PageMarkdown { page(0-indexed), markdown, needs_ocr, ocr_reason }>
//   pages_with_tables(1-idx) / pages_with_columns(1-idx) / pages_needing_ocr(1-idx) / is_complex

extract_text_with_positions(path) -> Vec<TextItem>   // page(1-idx), x,y,width,height,rotation,font,is_bold/italic/underline,item_type,mcid,baseline_shift
extract_structure_elements{,_mem}(path|bytes, Option<&[u32]>/*1-idx*/) -> Vec<StructureElement>  // { page(1-idx), mcid, role("H1".."H6","P",…) }
to_markdown_from_items(items, MarkdownOptions) / to_markdown_from_items_with_rects(…)
enum PdfType { TextBased, Scanned, ImageBased, Mixed }
enum PdfError { Io, Parse(String), Encrypted, InvalidStructure, NotAPdf(String) }
```

**⚠ 页码口径表（源码逐 API 核对；混用不报错、只静默错页）**

| 入口 / 字段 | 口径 |
|---|---|
| `PdfOptions::pages` / `page_filter` | **1-indexed** |
| `PdfProcessResult.pages_needing_ocr` / `PagesExtractionResult.pages_needing_ocr` | **1-indexed** |
| `TextItem.page` / `StructureElement.page` | **1-indexed** |
| `extract_structure_elements{,_mem}(…, pages)` 入参 | **1-indexed** |
| `extract_pages_markdown{,_mem}(bytes, pages)` **入参** | **0-indexed** |
| `PageMarkdown.page` / `PageRegionResult.page` | **0-indexed** |
| **`classify_pdf_mem().pages_needing_ocr`** | **0-indexed**（最易踩） |

**其他必须知道的实现事实**

1. **`needs_ocr` 页的 markdown 被清空为 `String::new()`**（`src/lib.rs:703-716`）→ "部分索引"（Q8）实现上就是"跳过空页 + 记录页号"。
2. **`process_pdf*` 对整篇扫描件提前返回**（`Scanned|ImageBased` → `markdown: None`，不抽取）；另有启发式：`TextBased` 且无 OCR 页但 `chars_per_page < 50 && markdown_len < 500` → `pages_needing_ocr` 置为全部页。
3. **`MarkdownOptions.include_page_numbers` 默认 `false`**；标记语法 `<!-- Page N -->\n\n`（`src/markdown/convert.rs:945-947`，1-indexed）。**`extract_pages_markdown*` 强制关闭该标记**（`src/lib.rs:647-653`）→ 逐页路线无标记，页归属只靠 `PageMarkdown.page`。别与 `remove_page_numbers`（默认 `true`，删正文自印页码）混淆。
4. **`DetectionConfig::default()` 是 `ScanStrategy::Sample(8)`**，而上游 README/文档表格写 `EarlyExit (default)` —— **以源码为准**。
5. **无 serde**（`Cargo.toml` 中 0 处）→ 所有结果类型无 `Serialize`，必须自建映射结构体（本方案的 `DocumentSource` 映射本来就要写）。
6. **部署级陷阱**：`src/tounicode.rs:1217-1237` 在运行时读 `env!("CARGO_MANIFEST_DIR")/external/bcmaps`（编译期烘焙路径，可用 `PDF_INSPECTOR_BCMAPS_DIR` 覆盖）→ Tauri 分发后该目录不存在则 CID/Adobe 系字体的 CMap 回退失效（**降级非崩溃**）。**方案要求随包分发 `external/bcmaps` 并设环境变量。**
7. OCR feature 图：`ocr = ["render-pdfium", "ocr-oar", "model-download"]`；`ocr-oar` 依赖 `ort = "=2.0.0-rc.13"`（`load-dynamic`，`ORT_DYLIB_PATH`），而 mdgo 现用 `ort = "2.0.0-rc.11"`（`Cargo.toml:121`）→ **精确版本冲突**（§9，仅 Phase 4 受影响）。

### 3.3 版本对齐

```toml
anydoc = "=0.2.4"        # 精确锁定（0.2.x 迭代快）
pdf-inspector = "1.19"   # anydoc 依赖 ^1.14.2，语义兼容 → cargo 统一为单份
```

**API 可用性已交叉验证**：anydoc 自身 `src/formats/pdf.rs` 就调用 `extract_pages_markdown_mem(bytes, Some(&flagged))` 并读 `.pages_needing_ocr` → 该 API 在 anydoc 允许的 `^1.14.2` 区间内必然存在。

**Phase 1 前置核验（3 条）**：① `cargo tree -i pdf-inspector` 只解析出**单份**；② 解析到的版本暴露 `extract_pages_markdown_mem` 与 `PageMarkdown`；③ `cargo build` 通过。不兼容则退化为 `=1.14.2`。

---

## 4. 目标架构（v2）

### 4.1 `DocumentLoader`：唯一入口（P0 边界）

```text
                    ┌──────────────── DocumentLoader ────────────────┐
  index 路径 ───────►│  FileKindRegistry → Converter → ConversionCache │───► DocumentSource
  preview 路径 ─────►│  （两者同一实现，preview 必然命中缓存）           │
                    └────────────────────────────────────────────────┘
```

**契约**：索引与预览**必须**走同一 loader。`document_preview` 命令不得自行调用 anydoc/pdf-inspector。收益：预览与索引输出天然一致；预览不产生第二次解析；未来加格式只改一处。

### 4.2 结构恢复与语义切分分离

```text
File → FileKindRegistry → DocumentLoader（转换器）→ DocumentSource（结构化分段 + provenance）
     → 既有 DocumentNode AST（comrak，整篇一次解析）→ SemanticChunkEngine（语义定界）
     → Chunk（+ provenance 并集）→ LanceDB/BM25
```

### 4.3 转换阶段：`SourceSegment` + **构造式**行区间映射（P0-3）

**核心原则：偏移是构造出来的，不是反查出来的。**

```rust
pub struct SourceSegment {
    pub text: String,
    pub page: Option<u32>,      // 1-indexed（PDF）
    pub slide: Option<u32>,     // 预留，不填（非目标）
    pub chapter: Option<u32>,   // 预留，不填（非目标）
}

pub struct DocumentSource {
    pub text: Arc<str>,                            // segments 拼接结果（整篇一次 comrak 解析）
    pub line_page_map: Vec<LineSpan>,              // [line_start, line_end) → page，拼接时逐段记账
    pub segments: Vec<SourceSegment>,
    pub converter: ConverterInfo,
    pub doc_status: DocStatus,
    pub page_diagnostics: Vec<PageDiagnostic>,
}

pub struct LineSpan { pub line_start: usize, pub line_end: usize, pub page: u32 }
```

为什么这是唯一可靠的映射：拼接时**我们知道每段从第几行开始**（text 由我们逐段 push），因此行区间表是**构造产物**，无需任何模糊匹配。对照 `pipeline.rs:151-179`——分块前已经做了 `parse_frontmatter`（剥离 frontmatter）+ `html_clean::strip_custom_html_tags`（清洗 HTML 标记），任何"文件字节 → chunk 字节"的反查都会因这两步变换系统性错位。**反查本来就不可行，构造才可行。**

**变换登记**：转换/清洗步骤必须登记为有序列表（`frontmatter 剥离 → HTML 清洗 → 表格/列表归一`），行区间表随之偏移。这样"文本变换"与"provenance 映射"是同一机制，而非两套。

### 4.4 分块阶段：跨页语义 chunk + provenance 并集（P0-2）

修正 v1 的"逐页分块"：**page 只做溯源，不做边界**。

1. loader 拼接所有 segment → 记录 `line_page_map`；
2. **整篇一次 comrak 解析** → `DocumentNode` 树（每个节点自带 `start_line/end_line`，`node.rs:50-53`）；
3. `SemanticChunkEngine` 按既有规则（标题层级分组、列表/表格/代码整体参与、token 预算裁决）语义定界 → **可跨页**；
4. 每个 chunk 收集成员节点的行区间 → 经 `line_page_map` 映射 → `page_start = min(page)`、`page_end = max(page)`、`source_spans` = 去重后的 `[{page, line_start, line_end}]`；
5. 引用展示：单页 → "第 N 页"；跨页 → "第 N–M 页"。

这样解决评审举的两个反例：
- 第 10 页 `## 3.2 数据库` + 第 11 页正文 → 整篇解析使标题层级**跨页保持**，chunk 带 `path_json`；
- 第 10 页"如下表所示" + 第 11 页表格 → 表格是 AST 节点、参与同一分组，不再被页边界斩断。

**代价（诚实入账，须进 A/B）**：跨页 chunk 平均变大 → 可能伤召回精度；"按页过滤"语义从精确变为区间；`page_start != page_end` 时前端需展示页码范围。

### 4.5 能力位与 `FileKind`（解耦 RetrievalIntent）

注册表**不再持有 `RetrievalIntent`**，只持有中性能力位；策略归 QueryPlan：

```rust
pub struct FileCapabilities {
    pub searchable: bool,          // 是否进检索索引
    pub is_code: bool,
    pub is_structured: bool,       // 表格/大纲/幻灯片等结构化来源
    pub paginated: bool,           // 能提供 page provenance
    pub binary: bool,              // 禁止 UTF-8 直读
    pub source_kind: &'static str, // "pdf"|"office"|"markdown"|"code"|"text"|"data"（版本失效的粒度键）
}
```

`QueryPlan` 侧：`intent_allowed_exts(intent)` 由能力位**推导**（Code → `is_code`；Document → `searchable && !is_code`；Outline → 注册表标记），从而既单一来源又不耦合。

### 4.6 `ConversionPolicy`

```rust
pub struct ConversionPolicy {
    pub max_file_bytes: u64,     // 转换前置护栏（非真正边界）
    pub max_expansion: u64,      // 预留：解压膨胀上限
    pub max_asset_bytes: u64,    // 预留：内嵌资源上限
    pub timeout: Duration,       // spawn_blocking + 超时
}
```

**事实校正**：上游各自已有硬上限（anydoc `MAX_TOTAL_BYTES` 512MiB / `MAX_ENTRY_BYTES` 128MiB / `MAX_ASSET_TOTAL_BYTES` 128MiB，私有常量不可配）→ mdgo 的 policy 是**前置护栏**，用于快速跳过超大文件与防挂死，而非真正的内容边界。

### 4.7 表格路径（只补真缺口）

已具备：`chunk_type="table"`、表格原子性（≤3 行整体保留）、超长表**每组重复 GFM 表头+分隔行**（`chunk_engine.rs:577-600`）、对应测试（`:721`）、comrak `extension.table = true`（`markdown.rs:30`）。

**本版只补**：解析 GFM 表格首行得到**列名**，写入新字段 `table_headers: Option<Vec<String>>`（`ChunkResult` → `DocumentChunk` → BM25 加权字段；**不进 `embedding_text`**，避免污染向量）。收益：按列名检索（如"订单金额"）能命中表格块。

---

## 5. 数据结构与契约

### 5.1 注册表（`core/document/filekind.rs`，新增）

```rust
pub enum Matcher { Ext(&'static str), FileName(&'static str) }
pub enum Converter { Plain, AnyDoc, PdfInspector }
pub enum DocumentForm { Markdown, Html, Tree, Code, Plain }

pub struct FileKind {
    pub matcher: Matcher,
    pub converter: Converter,
    pub form: DocumentForm,
    pub category: &'static str,        // 索引统计分类
    pub caps: FileCapabilities,        // §4.5（不再有 intents）
    pub policy: ConversionPolicy,      // §4.6（不再有裸 max_bytes）
}
pub static FILE_KINDS: &[FileKind] = &[ /* 见下表 */ ];
```

| 扩展名/文件名 | 匹配 | 转换器 | 形态 | source_kind | 备注 |
|---|---|---|---|---|---|
| `md` `mdx` | Ext | Plain | Markdown | markdown | 现状 |
| ▲ `markdown` `mdown` `rst` | Ext | Plain | Markdown | markdown | `is_markdown_ext` 已认，白名单缺失 |
| `html` `htm` | Ext | Plain | Html | markdown | 保留 `htmlCodeShowBlacklist` 逻辑 |
| `opml` / `mm` | Ext | Plain | Tree | data | 现状 |
| `txt` `cfg` `conf` `ini` `properties` | Ext | Plain | Plain | text | 现状 |
| ▲`.env` / ▲`Dockerfile` / ▲`Makefile` / ▲`.gitignore` | **FileName** | Plain | Plain | text | 修 D2 |
| ▲ `log` `tsv` `jsonl` | Ext | Plain | Plain | data | 补白名单 |
| `csv` | Ext | Plain | Plain | data | 表头列名进 metadata（§4.7） |
| `json` `xml` `yaml` `yml` `toml` | Ext | Plain | Plain | data | 现状 |
| 代码 24 种 + ▲ 11 种 | Ext | Plain | Code | code | 修 D4 |
| ★ `pdf` | Ext | **PdfInspector** | Markdown | **pdf** | paginated=true；页码 + NeedsOcr |
| ★ `docx` `docm` `doc` | Ext | **AnyDoc** | Markdown | **office** | 修 D1 |
| ★ `pptx` `pptm` `ppsx` `ppsm` `ppt` `pps` `pot` | Ext | **AnyDoc** | Markdown | office | |
| ★ `xlsx` `xlsm` `xlsb` `xls` | Ext | **AnyDoc** | Markdown | office | is_structured=true |
| ★ `odt` `ods` `odp` `rtf` `epub` | Ext | **AnyDoc** | Markdown | office | |
| ✗ `wps` | — | — | — | — | Q3：不支持 |

### 5.2 `SourceLocation` 挂到既有 AST（P0-1 的落地方式）

```rust
// core/document/node.rs —— 只加一个字段，不新建 AST
pub struct NodeMetadata {
    pub level: Option<u8>,
    pub start_line: usize,
    pub end_line: usize,
    pub source: SourceLocation,     // ← v2 新增
}

pub struct SourceLocation {
    pub page: Option<u32>,          // 1-indexed；Phase 1 起填
    pub bbox: Option<BBox>,         // 预留；Phase 5 填
    pub slide: Option<u32>,         // 预留；非目标（R1）
    pub chapter: Option<u32>,       // 预留；非目标（R1）
}

pub struct ConverterInfo { pub id: &'static str, pub version: &'static str, pub options_hash: u64 }
// Display: "anydoc@0.2.4" / "pdf-inspector@1.19.0" / "native@1"
```

**为什么这样最小**：`NodeMetadata` 已带行区间（`node.rs:50-53`），只需解析后按 `line_page_map` 为每个节点填 `source.page`；分块时做并集即得 chunk 的 provenance。**不改 comrak 管道、不新建节点类型、不动 `SemanticChunkEngine` 的分组算法**（只加 provenance 传播）。

### 5.3 诊断与结果（拆分）

```rust
pub enum DocStatus { Indexed, PartiallyIndexed { skipped_pages: Vec<u32> }, Skipped(SkipReason) }
pub struct PageDiagnostic { pub page: u32, pub code: &'static str, pub detail: String }
// code 对齐上游取值：needs_ocr / suspected_garbled_text / vector_text / no_text / …

pub enum SkipReason {
    Unsupported { ext: String }, NeedsOcr { pages: Vec<u32>, page_count: u32 }, Encrypted,
    Malformed { detail: String }, ResourceLimit { limit: String }, MissingPart { part: String },
    TooLarge { size: u64, limit: u64 }, TooSmall { size: u64 }, EmptyContent, Io { detail: String },
}

pub struct KbIndexResult {
    pub indexed_count: u32, pub failed_count: u32, pub converted_count: u32,
    pub chunk_count: u32, pub vector_count: u32, pub indexed_at: u64,
    pub diagnostics: IndexDiagnostics,      // ← 与"统计值"分离
}
pub struct IndexDiagnostics {
    pub stale_kinds: Vec<String>,           // §5.5
    pub skipped_files: Vec<SkippedFile>,    // 上限 100 条，超出只计数
    pub partial_files: Vec<PartialFile>,    // 部分索引（扫描页）——**不再混进 skipped_files**
    pub warnings: Vec<String>,
    pub truncated_chunks: u32, pub resplit_chunks: u32,
}
```

> **语义修正（评审 §十四）**：`skipped_files` 只表示**整个文件未入库**；"300 页 PDF 中第 47 页是扫描页"属于 `PartialFile { indexed_pages, skipped_pages }`。前端两个面板分开显示。
> **爆炸半径已确认极小**：前端目前**没有**消费 `truncated_chunks`/`resplit_chunks`（grep 零命中，只有 `stale` 被消费）。

### 5.4 Chunk 元数据与落库（`core/db/lance.rs`）

```rust
pub struct DocumentChunk {
    /* 既有字段不变 */
    pub source_kind: Option<String>,   // §5.5 版本失效粒度键 —— 必须落库
    pub converter: Option<String>,     // "pdf-inspector@1.19.0"
    pub page_start: Option<u32>,       // 1-indexed
    pub page_end: Option<u32>,
    pub source_spans: Option<String>,  // JSON 字符串（沿用 path_json 先例）：[{page,line_start,line_end}]
    pub bbox: Option<String>,          // 预留（Phase 5）
    pub table_headers: Option<String>, // JSON 数组（仅表格块；§4.7）
}
impl SearchHit { /* + page_start, page_end, source_kind, table_headers */ }
```

- `source_spans`/`table_headers`/`bbox` 用 JSON 字符串列（与既有 `path_json` 一致，避免 Arrow 嵌套类型负担）；
- `source_kind`/`converter` **必须落库**（版本失效查询与按类型重建都依赖它）——注意与 `doc_title`/`tags`（明确"不落 LanceDB 列"，仅 BM25）的取舍不同；
- **schema 变更与首次升级**：旧库无这些列 → v2 首次启动即判定"全部 stale（kind 未知）"→ 用户执行一次全量重建后，按类型失效才生效（§5.5）。

### 5.5 版本契约 v2：按类型失效（裁决 R2，V1 实现）

**身份构造**

```text
chunk identity = content_hash
               + source_kind
               + converter_id@converter_version
               + chunker_version(CHUNK_IDENTITY_VERSION)
               + chunk_params(chunk_size/overlap/max_seq)
```

**失效判定（按 kind）**

```rust
// KbStatus（types.rs）新增；stale = !stale_kinds.is_empty()，保持旧字段兼容
pub stale_kinds: Vec<String>,   // ["pdf"] / ["office","pdf"]

fn expected_converter(kind: &str) -> Option<ConverterInfo>   // 由注册表派生
// status()：取索引中出现的 (source_kind → converter) 集合，与期望值比对
//          不一致的 kind → stale_kinds；source_kind 为 None（v1 旧索引）→ 全部 kind 视为过期
```

**快照 vs 直查**：推荐 **`IndexMeta.kind_converters: BTreeMap<String, String>` 快照 + 写入时更新**（增量路径 `update_metadata_delta` 已在更新 meta），避免每次 status 都全表扫 LanceDB。

**必须同步改的一处（否则按类型失效会被"全局拦住"）**：
`params_version_mismatch`（`indexer.rs:563`）当前对**全局**版本不一致返回 true → 会拦住**所有**增量写入。v2 必须改成按 kind：

```rust
fn mismatched_kinds(&self, dir_path: &str) -> Vec<String>;   // 只返回过期 kind
// index_file / index_files_batch：仅当"本文件的 source_kind ∈ mismatched_kinds"才跳过
// 效果：升级 pdf-inspector 后，docx/md 的增量索引照常工作
```

**「只重建受影响类型」**

```rust
#[tauri::command]
pub async fn kb_reindex_kinds(app, dir_path, kinds: Vec<String>) -> Result<KbIndexResult, String>;
// 1) LanceDB delete where source_kind in kinds；BM25 同 kind 重建
// 2) scan_directory 过滤：只处理注册表中 source_kind ∈ kinds 的文件
// 3) 走既有 chunk/embedding/write 路径（转换缓存 + embedding 缓存自然复用）
```

**UI**：stale 提示区分类型与版本（例："PDF 索引已过期（pdf-inspector@1.19.0 → 1.20.0）；Word/Excel 索引正常"）+ 两个按钮：**只重建受影响类型** / **全量重建**。

**成本诚实入账（不同意"几乎无额外复杂度"）**：
① `DocumentChunk`/`IndexMeta` schema 变更；② `params_version_mismatch` → 按 kind（含两个增量入口分支）；③ LanceDB 按 kind 删除 + BM25 重建；④ `scan_directory` 过滤；⑤ `KbStatus`/前端文案与双按钮；⑥ 首次升级仍需一次全量重建。
**收益**：升级单一转换器只重建受影响类型；配合两级缓存，重建成本主要是"读文件 + 分块 + 写库"。

### 5.6 转换缓存（`core/db/conversion_cache.rs`，新增）

镜像 `EmbeddingCache`（`embedding_cache.rs`）：`{dir}/.mdgo/conversion_cache.sqlite`，`Mutex<Connection>` + `open_shared` 进程级复用。

```sql
CREATE TABLE IF NOT EXISTS conversion_cache (
    source_hash       TEXT NOT NULL,   -- SHA-256(文件字节)
    converter_id      TEXT NOT NULL,
    converter_version TEXT NOT NULL,
    options_hash      TEXT NOT NULL,
    text              BLOB NOT NULL,   -- 转换产物
    line_page_map     TEXT,            -- JSON（可空）
    diagnostics       TEXT,            -- JSON（可空）
    created_at        INTEGER NOT NULL,
    PRIMARY KEY (source_hash, converter_id, converter_version, options_hash)
);
CREATE INDEX IF NOT EXISTS idx_conversion_cache_created ON conversion_cache(created_at);
```

**为什么用 `content_hash` 而非 `mtime+size`（评审 §七，补硬论据）**：anydoc（`to_markdown_bytes`）与 pdf-inspector（`extract_pages_markdown_mem`）入口都是**整份 `&[u8]`**——**我们本来就持有全部字节**，哈希成本相对转换几乎免费，因此没有任何理由接受 mtime/size 的极端误命中（内容改回同尺寸 + mtime 被恢复）。纠正一处：SHA-256 对 500MB 文件约 0.3–0.5s（不是"几十毫秒"），但因字节已在内存且转换本身是数百 ms 级，仍划算。

**哈希语义分离**：转换缓存 + 来源身份 → **SHA-256**；**现有 embedding 缓存保持 FNV-1a 128 不动**（`embedding_cache.rs:9-11` 的设计是"缓存正确性不依赖人工失效"，换哈希会全量失效，收益不抵成本）。

**容量**：`CACHE_MAX_ENTRIES = 20_000`，按 `created_at` 最旧裁剪；缓存独立于 LanceDB/BM25，**全量重建不清缓存**；读写失败仅告警并回退实转换。

---

## 6. 分阶段实施（拆阶段，不拆发布）

### Phase 0A —— `FileKindRegistry` + 能力位 + `ConversionPolicy`

| 项 | 内容 |
|---|---|
| 改动 | 新增 `filekind.rs`；`KB_SUPPORTED_EXTS`/`classify_ext`/工厂注册/`intent_allowed_exts` 改为派生；修 D2/D4/D3（watcher 也过注册表） |
| 输出 | **行为等价**（转换器仍只有 `Plain` + 既有 `pdf-extract`） |
| 验收 | 4 个死项可索引；白名单↔注册表一致性单测；`benchmark` 基线不下降 |
| 规模 | S–M |

### Phase 0B —— `DocumentLoader` + Converter trait（含 preview 统一）

| 项 | 内容 |
|---|---|
| 改动 | `read_document` → `DocumentLoader::load`；四个调用点（`indexer.rs:433/585/662/2077`）+ `document_preview` 全部走同一入口；`DocStatus`/`PageDiagnostic` 骨架 |
| 输出 | **行为等价** |
| 验收 | 跳过原因可解释（`.docx` 不再报"非 UTF-8"）；preview 与 index 输出一致性测试 |
| 规模 | M |

### Phase 0C —— 版本契约 v2 + 按类型失效

| 项 | 内容 |
|---|---|
| 改动 | §5.5 全部（`source_kind`/`converter` 落库、`KbStatus.stale_kinds`、`mismatched_kinds`、`kb_reindex_kinds`、UI 双按钮） |
| 输出 | 不改 chunk 产物，但 **schema 变更 → 首次需一次全量重建** |
| 验收 | 旧索引（无 `source_kind`）→ 全 stale；重建后按 kind 判定正确；模拟"仅 pdf 转换器升级"→ `stale_kinds==["pdf"]` 且 docx 增量仍可写 |
| 规模 | M |

### Phase 1 —— PDF：pdf-inspector + 跨页语义 chunk + 页码 provenance

| 项 | 内容 |
|---|---|
| 改动 | 加 `pdf-inspector = "1.19"`；`Converter::PdfInspector`：`detect_pdf_mem` 预分类 → `extract_pages_markdown_mem(bytes, None)` 逐页 → 拼接 + `line_page_map` → 整篇 comrak → 跨页语义 chunk → `page_start/page_end/source_spans`；`NodeMetadata.source` 填充；**页码口径按 §3.2 表归一（对外 1-indexed）** |
| 行为 | 含扫描页 PDF：**部分索引**（可读页入库 + `PartialFile{skipped_pages}`），非全篇拒绝（Q8） |
| 验收 | ① 页码正确率（人工核对 3 篇，错误 0）；② 跨页 heading 上下文保持（构造 p10 标题 + p11 正文样本）；③ 扫描件报 `PartiallyIndexed` 并列出页码；④ 加密 PDF → `Encrypted`；⑤ 中日文 PDF 抽取比对（bcmaps 陷阱）；⑥ A/B：Recall@k 不低于基线，并专门观察"chunk 变大是否伤召回" |
| 规模 | M–L |

### Phase 2 —— anydoc：17 格式 + 前端守卫 + DocAgent 白名单

| 项 | 内容 |
|---|---|
| 改动 | 加 `anydoc = "=0.2.4"`；`Converter::AnyDoc`：`from_bytes().or_else(from_path())`；`ConvertError` 全映射（**含 `#[non_exhaustive]` catch-all**）；注册表补 17 项；§7 前端；修 D8 |
| 验收 | ① 17 格式各 1 真实样本：结构正确（标题层级 → `path_json`）；② `.doc/.ppt/.xls` OLE 可解析；③ 加密 docx → `Encrypted`；④ 两条前端分发链均不乱码；⑤ 超限 → `TooLarge`；⑥ DocAgent 可圈选且元数据不乱码；⑦ **验证 anydoc 对 xlsx 多 sheet / 超宽表 / pptx slide 边界的 Markdown 形态**（决定列名提取位置与 R1 的实际影响） |
| 规模 | M–L |

### Phase 3 —— 转换缓存 + 性能收尾

| 项 | 内容 |
|---|---|
| 改动 | §5.6；`index_all` 进度区分"转换/向量化"；大 PDF 页批处理（`extract_pages_markdown_mem(bytes, Some(&range))` 分批，降低 markdown/AST 峰值） |
| 验收 | 二次 `kb_index` 转换阶段 ≥90% 命中；改一个文件只重转一个；缓存删除后自动回退 |
| 规模 | S–M |

### Phase 4（可选，独立评估）—— 扫描件本地 OCR

前置：解决 `ort` 版本冲突（升级 mdgo `ort` 到 `rc.13` 并回归 embed/rerank，或自实现 `OcrEngine` 复用现有 ort）；接受 `firecrawl-pdfium` 原生依赖与打包体积；模型分发复用 `model_download.rs` 的 ModelScope→hf-mirror→HF 范式 + `ModelDownloadPolicy::Offline`。

### Phase 5（可选）—— bbox 高亮

`extract_text_with_positions` 取 `TextItem`（page + bbox），在 chunk 上填 `bbox`（仅单页 chunk 可精确）；前端 PDF 预览按页跳转高亮。

> **发布约束（评审 §十七 的补充）**：**阶段可拆，发布不可拆。** 0A/0B 输出等价可独立合并；一旦某阶段改变 chunk 产物（0C 的 schema、Phase 1 的 PDF、Phase 2 的 Office），**该阶段必须与其版本字段升级同批发布**，否则新旧 chunk 混库。

---

## 7. 前端改动清单（N4）

### 7.1 三份 HTML 的事实

| 文件 | 大小 | 是否进 App | 结论 |
|---|---|---|---|
| `main.html` | 2,456,948 B | **是**（`vite.config.js:47` 唯一 build input；`tauri.conf.json` 的 `frontendDist`+`url`） | **必改** |
| `index.html` | 2,380,785 B | 否 | **已冻结**（末次提交 2026-08-28） |
| `index_cdn.html` | 2,380,129 B | 否 | **已冻结**（末次提交 2026-08-24） |
| `tauri/dist/main.html` | 2,449,534 B | 构建产物 | `npm run build` 生成，不手改 |

证据：提交 `a5c3dec` **首次创建 `main.html`**（+53953 行）并同期从另两份各删 6.5–6.9k 行（复制后分叉）；`main.html` 加载 `tauri/src/adapters/index.js` 与 14 个 `css_js/modules/*.js`，另两份都不加载；`package.json`/`build.sh`/`build.bat`/`release.yml` 全无 HTML 生成或同步步骤；`README.md:15-17`、`docs/Agent 内核重构蓝图.md:264` 已承认浏览器版定位。
**好消息**：10 个扩展名代码块三份**逐字节相同**（哈希一致），尚未漂移 → 只改 `main.html` 安全，但会**从此**引入表漂移（Q1 已定：(a)）。

### 7.2 必改点

| # | 位置 | 改动 |
|---|---|---|
| F1 | `main.html:17299` `DOC_EXT_SET` | 补 `docm pps pot pptm ppsx ppsm xlsm xlsb odt ods odp rtf epub`（否则 `checkFileExt` @17859/L19308 直接拦下） |
| F2 | `main.html:17296` `_EXT_TYPE_MAP` | 实测缺失：`docm` `odp` `mdown` `pps` `pot` `pptm` `ppsx` `ppsm` `xlsm` `xlsb` `pptx` `wps` |
| F3 | 主分发链 `17885-17925` | 在 `else`（`17919-17924`）**之前**插入新格式分支 → 走后端转换预览；**绝不允许触达 `getFileText`** |
| F4 | 副分发链 `previewFile()` `17948-17972` | 同一批分支同步加入（该链已比主链少 `.pdf`/`.xlsx`/`.mmd` 分支） |
| F5 | `50340` `kbStatus.stale` | 扩展为 `stale_kinds` 分类型文案 + 双按钮（只重建受影响类型 / 全量重建） |
| F6 | `26702-26709` `extractLinks` `EXT_SET` | 补新扩展名（附件识别） |
| F7 | `css_js/modules/support.js:88-95` `supportsEdit` | 把新格式加入排除清单（显式化） |
| F8 | `tauri/src/adapters/file-system.js:84-114` `_getMimeType` | 补 Office/ODF/RTF/EPUB MIME |
| F9 | 诊断面板 | `skipped_files`（整文件）与 `partial_files`（部分索引）**分开显示**，并展示页码 |

> **§7.2 交付状态**：F1 ✅ / F2 ✅（另**多修了** `markdown`/`mdown`/`mdx`/`tsv`/`cfg`/`makefile`/`gnumakefile` 这 7 个"后端能索引、前端拒开"的扩展名）/ F3 ✅ / F4 ✅ / F5 ✅ / F6 ✅ / F7 ✅ / F8 ✅ / F9 ✅。
> 唯 **§7.2.1 只做到一半**（新增的转换预览分支只传路径，已绕开 `getFile()`；但 `renderFile` 在分派前仍无条件 `getFile()`），**§7.2.2 未做**——两者同属"主分发链结构调整"，一并见 §0.3「未完成项」。

**当前 `.docx` 的真实行为（风险等级说明）**：`.docx` 在 `DOC_EXT_SET` 内 → 过 `checkFileExt` → 不匹配任何分支 → 命中 `else` → `enterEditMode(true, getFileText())` → `blob.text()` 固定 UTF-8 → 乱码；且 `isOtherFile=true` → `previewBtn=false`（`main.html:18022`）→ **用户卡在乱码编辑器且无法退回预览**。

### 7.2.1 性能：预览不要经前端搬字节

`renderFile` 无条件 `getFile()`（`17875`），而 Tauri 适配层 `TauriFileHandle.getFile()`（`file-system.js:69-81`）**总是整文件 `read_file_binary` 进内存再包 Blob**（另有 50MB 上限 `MAX_FILE_SIZE` @ `17220`）。对 `.pptx/.xlsb/.epub` 是**双倍搬运** → 新预览分支**只传路径**给 `document_preview(path)`，绕开 `getFile()`。

### 7.2.2 两条链收敛

`renderFile` 与 `previewFile` 是两张独立表（副链已漏分支）→ 借本次收敛为一张 `ext → renderer` 注册表，一次改两处。

### 7.3 新增 Tauri 命令

```rust
#[tauri::command]
pub async fn document_preview(path: String) -> Result<DocumentPreview, String>;
// DocumentPreview { markdown, page_count, page_map: Vec<[u32;3]>, doc_status, page_diagnostics, truncated: bool }
```

**必须经 `DocumentLoader → ConversionCache`**（§4.1），不得自行调用转换器。

---

## 8. 测试与验收

### 8.1 单元测试

| 测试 | 断言 |
|---|---|
| `filekind_registry` | 每个 matcher 命中唯一 `FileKind`；4 个死项命中；白名单/能力位派生一致 |
| `line_page_map_constructed` | 拼接多 segment 的行区间表精确；frontmatter/HTML 清洗后行偏移仍正确（**变换登记**回归） |
| `cross_page_chunk_provenance` | 跨页 chunk 的 `page_start/page_end` 与 `source_spans` 并集正确；单页 chunk 二者相等 |
| `pdf_page_index_normalization` | 逐入口断言 §3.2 表：`extract_pages_markdown*` 入参 0-idx、`PageMarkdown.page` +1、`pages_needing_ocr`(1-idx) 不再 +1、**`classify_pdf_mem().pages_needing_ocr`(0-idx) 必须 +1** |
| `convert_error_mapping` | 7 个 `ConvertError` + 4 个 `PdfError` 变体映射正确；`#[non_exhaustive]` 保留 catch-all 且不 panic |
| `conversion_cache_identity` | 主键四元组任一变化 → 未命中；同内容不同 mtime → 命中 |
| `stale_kinds` | 仅 pdf 转换器升级 → `stale_kinds == ["pdf"]`；`mismatched_kinds` 不拦 office 增量；旧索引（无 `source_kind`）→ 全 stale |
| `table_headers_metadata` | GFM 表格块的 `table_headers` 正确；**不进 `embedding_text`** |
| `whitelist_parity` | 索引白名单 ⊆ 注册表；代码集合 ⊆ 注册表（防 D4 复发） |
| `docagent_whitelist_parity` | `commands/doc.rs:77/126/190` 与注册表一致 |
| `binary_never_utf8` | 注册表中 `Converter != Plain` 的格式绝不走 `read_to_string` |

### 8.2 夹具

`tests/fixtures/convert/`：17 种 anydoc 格式各 1 个小样本（≤100KB）+ `text.pdf` + `scanned.pdf` + `encrypted.pdf` + **跨页语义样本**（p10 标题 + p11 正文；p10"如下表所示" + p11 表格）+ 中日文 PDF（bcmaps 对照）。快照沿用仓库既有 `insta` 风格。

### 8.3 A/B 回归（反驳厂商自测基准）

- `cargo test` 全绿 + `cargo build --release`；
- `src/bin/benchmark.rs` 改动前后同语料对比（Recall@k / 延迟）；
- **分层 A/B**：**30 篇 PDF**（约 20% 纯文本 / 20% 多栏 / 20% 表格 / 15% 长文档 / 10% 扫描混合 / 10% 论文 / 5% 复杂版面）、**60–100 问**（事实型 / 表格型 / 跨段落 / 章节定位 / 数字日期 / 公式代码）；A=`pdf-extract+PlainText`、B=`pdf-inspector+跨页语义 chunk`，写入 `retrieval_eval/queries.jsonl` + `expected.jsonl`。
- **统计效力声明**：30 篇规模下 Recall 差异置信区间较宽 → **只用于判断方向与定位失败案例，不得宣称"提升 X%"**。

### 8.4 验收标准（两层指标）

**第一层：转换保真度**（转换器是否正确，与分块无关）

| 指标 | 目标 |
|---|---|
| 文本关键字保留率 | ≥98%（对照人工抽取的关键词表） |
| 标题保留率 | 文档级标题 100% 出现在 Markdown 标题层级中 |
| 表格行列保留率 | 行列数与源文档一致（xlsx/csv/docx） |
| 页码归属正确率 | 抽 3 篇 PDF 逐 chunk 核对，错误 **0** |

**第二层：分块与检索质量**（chunker 是否正确——"chunk_count>0"不作数）

| 指标 | 目标 |
|---|---|
| 表格完整性 | 不出现"表头与数据分离"；超长表每块含表头（既有测试扩展） |
| `heading_path` 非空率 | 结构化来源（pdf/docx/epub/md）≥95% 的块带 `path_json` |
| 跨页上下文保持 | 构造样本：p10 标题 + p11 正文的 chunk 带正确 `path_json` |
| Recall@5 / Recall@10 / MRR | 不低于现状基线（±1pp 噪声内） |
| 定位可用性 | 命中块给出正确页码；跨页块给出正确区间 |
| 可解释性 | 任意跳过/部分索引文件在 UI 可见「路径 + 原因 + 页码」 |
| 不混库 | 升级后 `stale_kinds` 正确；重建后无旧版本残留 |
| 二次重建加速 | 转换缓存命中率 ≥90% |

---

## 9. 风险与缓解 / 回滚

| 风险 | 等级 | 缓解 | 回滚 |
|---|---|---|---|
| anydoc 0.2.x API 漂移 | 中 | `=0.2.4` 精确锁版；适配层集中在 `convert.rs`；升级须过 §8 全部验收 | 回退依赖版本 |
| 上游质量被高估（自测基准 0.2.6 版） | 中 | §8.3 自有语料 A/B；不采信厂商数字 | 保留 `pdf-extract` 一个版本（Q7） |
| 跨页 chunk 变大伤召回 | 中 | 专门观察项（§8.3）；必要时给"页内优先"开关 | 开关回退逐页语义 |
| 页码口径混用静默错页 | 中 | §3.2 表 + 单测锁死；对外统一 1-indexed | — |
| **`external/bcmaps` 编译期路径**（分发后失效） | 中 | 随包分发 + 设 `PDF_INSPECTOR_BCMAPS_DIR`；中日文 PDF 抽取比对 | 不设也可运行（仅解码降级） |
| 按类型失效的实现面比预期大 | 中 | §5.5 六项成本已列；0C 单独成阶段便于定位回归 | 退回全局 stale（保字段、关分支） |
| 首次升级需一次全量重建 | 低 | 既有 `stale` 提示 + 明确文案 | — |
| DocAgent「能看不能问」 | 中 | N7 + 验收 ⑥ | — |
| 二进制解析安全面（OLE/zip/PDF） | 中 | `ConversionPolicy.max_file_bytes` + 超时；上游已有 fuzz 与硬上限 | 超限即跳过 |
| Phase 4 `ort` 版本冲突 | 中（仅 Phase 4） | 升级 mdgo `ort` 并回归，或自实现 `OcrEngine` | 不启用即无影响 |
| 体积上升（anydoc 零 feature → pdf 栈必然编译） | 低 | Phase 2 后实测记录（Q9） | 退化为只接 PDF |
| 三份 HTML 漂移 | 低 | 只改 `main.html`（Q1） | — |

---

## 10. 工作量（粗估）

| 阶段 | 规模 | 主要不确定性 |
|---|---|---|
| 0A 注册表/能力位/policy | S–M（2 人日） | 五处清单派生改造面 |
| 0B Loader + preview 统一 | M（2–3 人日） | 四个调用点 + 预览一致性 |
| 0C 版本契约 + 按类型失效 | M（3–4 人日） | schema/增量守卫/删除重建/UI（裁决纳入 V1，成本已如实计入） |
| 1 PDF + 跨页 provenance | M–L（4–6 人日） | 页码核对、跨页效果、A/B |
| 2 anydoc 17 格式 + 前端 + DocAgent | M–L（3–5 人日） | 样本制备、前端两条链、xlsx 表格形态验证 |
| 3 缓存 + 页批处理 | S–M（1–2 人日） | — |
| 4 OCR（可选） | L（5+ 人日） | ort 冲突、pdfium 打包、模型分发 |
| 5 bbox（可选） | M–L | 前端联动 |

---

## 11. 决策记录

**Q1–Q9（v1 评审已定）**：Q1=只改 `main.html`；Q2=接受升级后全量重建一次；Q3=不支持 `.wps`；Q4=做转换后 Markdown 预览；Q5=OCR 不纳入本方案；Q6=bbox 不纳入本方案（数据模型预留）；Q7=保留 `pdf-extract` 一个版本作回退；Q8=扫描页**部分索引**；Q9=接受固定体积增量。

**R1–R3（v2 评审已定）**

| # | 决策 | 记录 |
|---|---|---|
| R1 | PPTX slide / EPUB chapter provenance | **不做**，仅 Heading 层级近似；`SourceLocation.slide/chapter` 字段预留不填 |
| R2 | 版本失效粒度 | **V1 即实现按类型失效**（§5.5），成本六项已入账 |
| R3 | 表格分块范围 | 只补「表头列名进 metadata」（§4.7）；不新建 TableChunker |

**评审意见未采纳项（附理由）**

| 意见 | 处置 |
|---|---|
| 「需新建 Unified Document AST」 | 不采纳：`DocumentNode` 已存在（§1.2），改为给 `NodeMetadata` 加 `source` 字段 |
| 「需新建 TableChunker / 表格 AST」 | 不采纳：表格感知切分与表头重复已存在（§1.2/§4.7） |
| 「page_spans 用 byte mapping」 | 不采纳：改为**构造式行区间**（§4.3），byte offset 不参与 |
| 「真流式转换」 | 不采纳（做不到）：上游入口均为整份 `&[u8]`；改为大小护栏 + 页批处理 |
| 「按类型失效几乎无额外复杂度」 | 采纳裁决但修正成本判断：字段成本低，**失效判定与重建编排**才是成本（§5.5） |

---

## 12. 证据来源

**上游（源码/官方文档原文）**

- anydoc：`Cargo.toml`、`src/lib.rs`、`src/error.rs`、`src/model/{mod,asset,block,list,table,style}.rs`、`src/formats/{detect,pdf}.rs`、`src/render/markdown/inline.rs`、`src/package/limits.rs`、`node/README.md`、`node/anydoc.d.ts`、`examples/convert.rs`、crates.io API
- pdf-inspector：`Cargo.toml`（feature 图/MSRV）、`src/lib.rs`、`src/detector.rs`、`src/markdown/{convert,mod}.rs`、`src/types.rs`、`src/vision/{models,contracts,routing,fusion,render}.rs`、`src/tounicode.rs`、`src/bin/{pdf2md,detect_pdf}.rs`、`docs/rust-api.md`、README、crates.io API
- FastGPT：`packages/global/common/file/constants.ts`、`packages/service/package.json`、`packages/service/worker/readFile/extension/anydoc.ts`

**本地代码（文件:行号）**

- `core/document/node.rs:7-60`（既有 AST + 行区间）、`chunk_engine.rs:30/169/396/577-600/721`（chunk_type + 表格切分 + 表头重复测试）、`markdown.rs:26-32`（table 扩展）
- `core/pipeline.rs:82`（read_document）、`:139`（chunk_document）、`:151-179`（frontmatter 剥离 + HTML 清洗 —— P0-3 实证）、`:241/309/399`
- `core/indexer.rs:219/433/563/585/662/840/940/2077/2185/2221`
- `core/db/utils.rs:12/564/643`、`chunk_splitter.rs:1291/1322/1344`、`lance.rs:30/66`、`embedding_cache.rs:1-70`
- `core/config.rs:46`、`core/types.rs:12/39`、`core/search/query_plan.rs:31/37`、`core/document/html_clean.rs:17`
- `commands/doc.rs:77/126/190`、`core/docagent/mod.rs:180-206`
- `main.html:17220/17296/17299/17308-17318/17851-17928/17930-17973/18022/18335/19308/25039/25310/25350/26702-26709/34484-34497/50340`
- `tauri/vite.config.js:47`、`tauri/src-tauri/tauri.conf.json`、`tauri/src/adapters/file-system.js:69-81/84-114`、`css_js/modules/support.js:88-95`、`tauri/src-tauri/Cargo.toml:14/121`

**已核验消除的不确定项**：`ConvertError` 全变体与字段（含 `#[non_exhaustive]`）；anydoc 零 feature/无 options/无网络；FastGPT 选项非官方 API；`MarkdownOptions` 默认值；`PdfOptions` 全字段；pdf-inspector 全部入口签名与结果字段、多套页码口径、无 serde、bcmaps 运行时路径。

**仍未验证（不在本文断言）**：依赖树合并后实际解析版本是否同时满足两侧 API（§3.3 前置核验）；本方案未编译未运行任何上游代码（"第 1 页也输出 `<!-- Page 1 -->`"属源码推断）；OCR 运行时平台可用性（Phase 4）；`--compact` 行为；区域/表格类 API 字段细节；**anydoc 对 xlsx 多 sheet / 超宽表 / pptx slide 边界的 Markdown 形态**（Phase 2 首个验证项）。
