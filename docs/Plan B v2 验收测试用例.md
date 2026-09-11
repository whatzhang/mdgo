# Plan B v2 文档解析管线 —— 验收测试用例

> 适用版本：`pdf-inspector = "=1.19.0"` + `anydoc = "=0.2.4"` + `[patch.crates-io] lopdf`（见 `tauri/src-tauri/Cargo.toml`）
> 被测范围：FileKind 注册表 / `DocumentLoader` 唯一入口 / 版本契约 v2 与按类型失效 / PDF 页码 provenance / anydoc 19 格式 / 转换缓存 / 表格表头 metadata / 索引诊断
> 不在范围：OCR（决策 Q5）、bbox 高亮（决策 Q6）
>
> 文中 `$REPO` = 仓库根（如 `G:\gitProject\mdgo`），`$CRATE` = `$REPO\tauri\src-tauri`。

---

## 0. 如何使用本文档

### 0.1 三个执行层次

| 层次 | 章节 | 需要什么 | 耗时 | 建议 |
|---|---|---|---|---|
| **A 自动化** | §1 ~ §3 | 只需仓库 | 首次编译后 <1 min | **每次改动必跑**，是回归底线 |
| **B 真实样本** | §4 | 一份多格式样本目录 | 首次编译后 <1 min | 转换保真度的主要证据 |
| **C GUI 手工** | §5 ~ §7 | 运行中的 App + 样本 | 30~60 min | 面向用户可见行为，无法自动化 |

### 0.2 前置准备（一次性）

```powershell
# 依赖已锁定，无需网络（除首次拉取 lopdf 的 git rev，见 §8 已知限制）
cd $CRATE
$PSNativeCommandUseErrorActionPreference = $false   # 必需：否则 cargo 的 stderr 会让 PowerShell 误报 exit 1
cargo build --release                                # 可选，GUI 测试需要
cargo test --lib                                     # 冒烟：应 433 passed / 0 failed
```

### 0.3 样本目录准备（层次 B 与 C 共用）

建一个目录（例：`G:\kb-samples`），按下表放入样本。**样本不进仓库**，故用环境变量指定。

| 类别 | 最少需要 | 用途 | 备注 |
|---|---|---|---|
| 文本型多页 PDF | 1 个（**≥3 页**） | 页码 provenance、跨页 chunk | 必须是**可选中文字**的 PDF，不是扫描件 |
| 扫描件 PDF | 1 个 | `needs_ocr` 跳过路径 | 整篇是图片、无文字层 |
| 多语言/CJK PDF | 1 个 | 字体编码（bcmaps）不产生乱码 | 中文/日文内容最佳 |
| `.docx` | 2 个（1 个带多级标题） | 结构恢复（标题层级） | |
| `.xlsx` | 1 个（**首行是表头**） | 表格 → GFM + 表头列名 metadata | 表头用英文列名更好验证检索 |
| `.pptx` | 1 个 | slide → H2 标题 | |
| 旧版 OLE `.doc` / `.ppt` / `.xls` | 各 1 个（可选但强烈建议） | 旧二进制格式可解析 | 这三个是最容易被漏的路径 |
| `.rtf` / `.odt` / `.epub` | 各 1 个（可选） | 其余 anydoc 路径 | 无样本时该行标记"未验证" |
| 混合页 PDF（**可选**） | 1 个 | **部分索引**（部分页跳过）用例 TC-G10 | 需自备：可用任意 PDF 工具把 1 页文本 + 1 页扫描图合并 |
| 文本样本 | `a.md`、`note.txt`、`a.markdown`、`b.tsv`、`Makefile` | 原生直读、可打开集合用例 | 内容随意，`Makefile` 需**无扩展名** |

> ⚠️ **本用例集中的"部分索引"与"跨页页码"两条需要上述特殊样本**；仓库内没有夹具（§8 已说明原因）。没有样本时请把对应用例标"未验证"，不要标"通过"。

### 0.4 判定与记录

每条用例用三态记录：**通过 / 不通过 / 未验证（附原因）**。§9 有汇总表可直接填写。
"不通过"请附：用例号 + 实际输出（截图或日志）+ 所用样本文件名。

日志位置：Windows `%APPDATA%\com.mdgo\logs\mdgo.log`；界面右下角可调日志级别（`set_log_level`），排查跳过原因时调到 DEBUG。

---

## 1. A1 —— 单元测试与静态检查（P0）

| 用例 | 命令 | 预期 | 失败含义 |
|---|---|---|---|
| **TC-A1** | `cargo test --lib` | **433 passed / 0 failed / 0 ignored** | 有断言被破坏 |
| **TC-A2** | `cargo check --lib --tests` | **0 warning / 0 error** | 新增死代码或未用导入 |
| **TC-A3** | `cargo build --release` | 成功（首次约 27 min） | 依赖或条件编译问题 |

> 若数量与 433 不同：先确认是否有人新增/删除了测试；**数量变化本身不是失败，failure 数才是**。
>
> **已知偶发失败（非本次改动引入，请勿误判）**：`core::knowledge::bookmark::importer::tests::import_truncates_over_50000_entries`
> （5 万条书签导入的 SQLite 重负载测试）在全量并行跑时**偶发** panic。实测：单跑 3/3 通过、随后全量连跑 4 次全绿，
> 仅在 1 次全量中被观察到失败；该模块与本次改动**无关联**。若遇到，单独重跑一次确认即可，
> 并建议后续为它加串行标记或重试（属独立小修）。

---

## 2. A2 —— 关键单测逐项核对（P0）

这些测试各自钉住一条**契约**。下表给出"测试名 → 它保证什么 → 破坏后用户会看到什么"，便于判断失败的影响面。

### 2.1 注册表与跨层一致性

| 测试名（模块 `core::document::filekind`） | 保证 | 破坏后的用户可见后果 |
|---|---|---|
| `registry_builds_without_duplicate_matchers` | 无重复登记 | 某扩展名路由到错误转换器 |
| `phase2_formats_use_anydoc_markdown_form` | 19 个 Office/ODF/RTF/EPUB = anydoc + Markdown + binary | Office 文件走 UTF-8 直读 → 乱码 |
| `pdf_uses_pdf_inspector_in_phase1` | pdf = pdf-inspector + Markdown + paginated | PDF 无页码、无结构 |
| `wps_is_still_unsupported` | `.wps` 不入册（Q3） | 承诺外的格式被"支持" |
| `frontend_ext_tables_match_registry` | 5 条跨层不变式（见下） | 见下 |
| `category_matches_legacy_classify_ext_except_documented_d4_delta` | 类型分布仅按已记录的 delta 变化 | 统计图口径漂移 |

**`frontend_ext_tables_match_registry` 钉住的 5 条**（**重点用例**）：
1. `CONVERTED_DOC_EXT_SET ⊆ DOC_EXT_SET`
2. `CONVERTED_DOC_EXT_SET` = 注册表 Office 全集 − `{xls, xlsx}`（后两者走前端原生表格渲染）
3. `support.js::CONVERTED_DOC_RE` 与 `CONVERTED_DOC_EXT_SET` 逐项相等
4. `_EXT_TYPE_MAP` 覆盖每个 Office 扩展名
5. **注册表扩展名 ∪ `DOC_FILE_NAMES` ⊆ 前端可打开集合**（"后端能索引、前端点不开"由此拦住）

> **建议加做一次"守卫有效性"验证（TC-A4）**：临时把 `main.html` 里 `CONVERTED_DOC_EXT_SET` 的 `'epub'` 删掉 → `cargo test --lib frontend_ext_tables_match_registry` **应失败**并提示 `CONVERTED_DOC_EXT_SET（main.html）与注册表不一致` → **务必还原**。
> 另可删 `DOC_EXT_SET` 里的 `'markdown'`，应报 `后端可索引、但前端 checkFileExt 会拒开：["markdown"]`。
> 一个从不失败的守卫等于没有守卫，这一步是本用例集的"元测试"。

### 2.2 装载与转换（`core::document::loader`）

| 测试名 | 保证 |
|---|---|
| `pdf_assembly_builds_exact_page_spans` | 逐页**行区间**与由正文反推的真值一致（**曾因此抓出跨页错位严重缺陷**） |
| `pdf_assembly_partially_indexes_when_some_pages_need_ocr` | 部分页需 OCR → `PartiallyIndexed{skipped_pages}`，且不影响后续页行号 |
| `pdf_assembly_reports_whole_doc_ocr_when_no_page_extractable` | 一页都提不出 → 报整篇需 OCR（**而非** EmptyContent） |
| `pdf_chunks_carry_page_provenance` | 页码真的流到 chunk 上（非空过） |
| `pdf_error_mapping_is_exhaustive` / `skip_reason_codes_are_stable` | 错误码稳定可聚合 |
| `too_large_is_pre_guarded` / `too_small_content_is_reported` | 尺寸护栏 |
| `non_utf8_is_reported_as_not_utf8` | 非 UTF-8 报明确原因而非静默跳过 |
| `garbage_pdf_is_reported_not_panicking` / `malformed_office_is_reported_not_panicking` | 坏文件不 panic |
| `rtf_uses_anydoc_and_reaches_markdown_chunking` | RTF 走 anydoc 并复用 Markdown 分块 |
| `office_format_detection_falls_back_to_extension` | 无内容签名时退回扩展名 |
| `filename_only_kinds_load_via_registry` | `Dockerfile`/`Makefile`/`GNUmakefile` 可装载 |
| `mdx_keeps_legacy_no_frontmatter_behavior` | `.mdx` 不解析 frontmatter（保持旧行为） |

### 2.3 页码 provenance 与分块（`core::db::chunk_splitter`）

| 测试名 | 保证 |
|---|---|
| `single_page_chunk_maps_to_one_page` | 单页 chunk：`page_start == page_end` |
| `cross_page_chunk_reports_page_range` | 跨页 chunk：`page_end > page_start` |
| `markdown_splitter_applies_page_map` | 页映射送达分块器 |
| `missing_map_yields_no_provenance` | 无映射 → 全 None（非分页格式行为不变） |
| `out_of_range_lines_yield_no_page_but_keep_span_when_intersecting` | 越界行不产生幽灵页码 |
| `union_provenance_merges_ranges` / `union_provenance_handles_missing_side` | 合并块 provenance 并集 |
| `default_split_with_pages_forwards_to_split` | 默认 trait 方法不改变非分页行为 |
| `factory_routes_by_extension` / `new_registry_exts_are_routable` / `factory_unknown_ext_falls_back` | 工厂按注册表路由 |
| `code_lang_table_matches_registry` | 代码语言表与注册表一致（防 D4 复发） |

### 2.4 表格表头 metadata（`core::document::chunk_engine` + `core::db::utils`）

| 测试名 | 保证 |
|---|---|
| `table_headers_metadata_extracted_and_not_in_embedding` | 列名正确进 metadata，且**未被额外注入** `embedding_text` |
| `table_header_detection_requires_separator_row` | 判定依赖"表头行 + 分隔行"，不把数据行误判为表头 |
| `chunk_carries_table_headers_without_affecting_identity` | 表头透传到 `DocumentChunk`，且**不参与身份哈希**（否则 id 不稳定） |
| `oversize_table_repeats_header` | 超长表每片重复表头（既有能力，回归保护） |

### 2.5 版本契约与按类型失效（`core::indexer`）

| 测试名 | 保证 |
|---|---|
| `stale_kinds_isolates_single_converter_upgrade` | 只升级 pdf → 只有 pdf 过期，其他类型不牵连 |
| `stale_kinds_reports_each_changed_kind` / `stale_kinds_ignores_kinds_absent_from_snapshot` | 快照比对语义 |
| `expected_kind_converters_cover_registry` | 期望快照覆盖全部 kind |
| `diagnostics_merge_preserves_untouched_and_refreshes_processed` | **诊断按路径合并**：增量索引不清空其他文件的历史诊断 |
| `skipped_and_partial_are_kept_separate` | "整文件跳过"与"部分索引"分开 |
| `index_meta_without_diagnostics_fields_still_loads` | **旧 `index_meta.json` 缺新字段仍能反序列化**（否则被迫全库重建） |

### 2.6 转换缓存（`core::db::conversion_cache`）

| 测试名 | 保证 |
|---|---|
| `source_hash_is_sha256_hex` | 内容指纹形态 |
| `key_includes_converter_identity` / `miss_when_any_key_part_differs` | 四元组主键任一变化即未命中 |
| `pdf_conversion_is_cached_and_equivalent` | 缓存还原与冷转换**等价** |
| `payload_round_trip_preserves_provenance` | `page_spans`/`DocStatus`/诊断完整往返 |
| `plain_text_sources_are_not_cached` | 直读类不缓存（避免无谓开销） |

### 2.7 DocAgent（`core::docagent`）

| 测试名 | 保证 |
|---|---|
| `registered_binary_never_falls_back_to_lossy_garbage` | 已登记格式转换失败**报错**；未登记格式保持 lossy 宽容 |
| `path_escape_rejected` | 路径越权被拒 |

### 2.8 Agent 文档读取工具（`core::agent::tools::document_read_tests` + `loop_tools`）

| 测试名 | 保证 |
|---|---|
| `binary_documents_route_to_read_document_and_text_does_not` | **两个工具的分工契约**：13 个文档扩展名必须走 `read_document`，10 个文本/代码扩展名必须走 `read`（漏一个就会出现"read 读到乱码"） |
| `rtf_document_converts_to_markdown_via_anydoc` | **端到端**：`.rtf` 经 `DocumentLoader` → anydoc → Markdown，正文含原文、无替换字符 |
| `read_text_refuses_binary_document_with_actionable_hint` | `read` 读二进制文档**明确拒绝并指路** `read_document`（防乱码回归） |
| `read_text_still_reads_plain_text_files` | 守卫**不误伤**文本文件；分页语义仍在 |
| `unregistered_extension_is_not_treated_as_document` | `.log` 等未登记格式仍走宽容路径 |
| `document_output_has_metadata_header_and_body` | 工具输出的**格式契约**：元信息行含转换器/来源类型/形态；非分页格式不出现页数；越界 offset 给出提示 |
| `document_output_survives_injection_wrapper` | 注入防护对正常内容无损 |
| `skip_remedy_covers_every_skip_code` | 11 个跳过原因码**都有专门建议**（不落兜底），且 `needs_ocr` 的建议点明 OCR |
| `read_document_spec_and_base_tools_registration` | 工具规格（名称/必填 `path`/并行标记）+ **必须在 `BASE_TOOLS`**（否则模型看不到该工具） |

> **为什么需要"分工契约"这条测试**：它是一张**双向**清单。只测"文档能被解析"是不够的——若某个文本扩展名被误判为文档，`read` 就会对它报错，属于把修 A 变成坏 B。

### 2.8 落库往返（`core::db::lance`）

| 测试名 | 保证 |
|---|---|
| `chunk_metadata_round_trips_through_lancedb` | 真实建表→写入→读回，逐列核对 `chunk_type`/`source_kind`/`converter`/`source_spans`/`table_headers`/`page_start`/`page_end` |

> **注意**：该测试在本地 embedding 模型不可用时**会打印跳过信息而返回成功**。判定时请看输出里有没有 `[skip] 本地 embedding 模型不可用`——有 skip 则本用例为**未验证**，不是通过。

---

## 3. A3 —— 检索回归（P1）

改动涉及 chunk/BM25/RRF/metadata 时**必须**跑。

```powershell
cd $CRATE
# ⚠️ --reindex --yes-wipe 会清空目标目录的 .mdgo（索引 + embedding 缓存）
#    若目标目录是你自己的知识库，请勿直接对其执行；建议先复制一份语料再用副本跑。
cargo run --bin benchmark --features bench -- `
  --kb <语料目录> `
  --queries "$REPO\retrieval_eval\queries.jsonl" `
  --expected "$REPO\retrieval_eval\expected.jsonl" `
  --topk 20 --reindex --yes-wipe
```

| 用例 | 预期（对照 `retrieval_eval/README.md` 基线 v5） | 判定 |
|---|---|---|
| **TC-C1** | Recall@10 ≈ **0.712**、MRR ≈ **0.502**、NDCG@5 ≈ **0.481**、Latency avg ≈ **1203ms** | 与基线同量级即通过 |
| **TC-C2** | 42 条查询全部出结果、无 panic、无 schema 报错 | 通过 |

**判定要点（务必遵守，否则会得出错误结论）**：
- 该基准对**语料规模/构成敏感**。基线 v5 是在 2060 文件 / 84.7MB 的语料上测的（v4 记录为 1285 文件）。**语料不同则数字不可直接比较**，只能判断"有没有数量级退化"。
- 若 `Recall@10` 低于基线 **5pp 以上** → 视为不通过，需查 chunk 边界/RRF/metadata 改动。
- 仓库语料以 markdown/代码为主、PDF 极少，**因此本基准证明不了 PDF 相关改动的效果**（这也是方案 §8.3 要求另建 30 篇 PDF 语料的原因）。

---

## 4. B —— 真实样本端到端验收（P0，转换保真度主要证据）

```powershell
cd $CRATE
$env:MDGO_ACCEPT_DIR = 'G:\kb-samples'     # 指向 §0.3 准备的样本目录
$env:MDGO_ACCEPT_DUMP = '260'               # 可选：打印每个样本转换后正文前 260 字符
cargo test --lib acceptance -- --nocapture --test-threads=1
```

### TC-B1 汇总判定

期望输出形如（**你的样本集不同则数字会不同，但结构应一致**）：

```
样本 N 个：成功 X / 跳过 Y；累计 chunk Z；anydoc 路 A；pdf-inspector 路 B
```

| 断言（测试内硬断言，不通过即 FAILED） | 含义 |
|---|---|
| 每个 Office 文件 `converter = anydoc@0.2.4` 且 `source_kind = office` | 转换器身份正确 |
| 每个 PDF 文件 `converter = pdf-inspector@1.19.0` 且 `source_kind = pdf` | 同上 |
| PDF `page_spans` 非空 | 页码 provenance 存在 |
| PDF **至少 1 个 chunk 带 `page_start`** | 页码真的流到 chunk（防"空过"） |
| **每页行区间 = 由正文反推的真值**（逐页比对 + 行区间文本 == 该页正文） | **跨页错位回归**（本次修过的严重缺陷） |
| 乱码率（U+FFFD 占比）< 1% | 二进制未被当作文本读入 |
| `page_start ≤ page_end ≤ 总页数`、页码 ≥1 | 页码自洽 |

### TC-B2 人眼核对保真度（配合 `MDGO_ACCEPT_DUMP`）

对每个成功样本，检查 `----- DUMP <文件> -----` 段落：

| 检查点 | 期望 |
|---|---|
| `.docx` 带多级标题 | 出现 `#` / `##` 层级，且与原文标题一致 |
| `.xlsx` | 出现 GFM 表格，**表头行是第一行**，列名与源一致 |
| `.pptx` | 每张 slide 的可见文本都在，边界近似为 `##` 标题 |
| 旧版 `.doc` / `.ppt` / `.xls` | 有可读正文（**不是**乱码、不是空） |
| `.rtf` | 文本与源一致（若源含大量内嵌图形，字符数少属**正常**——图形是二进制块） |
| 多语言 PDF | 中文/日文正常显示，无 `?`/方块/乱码 |

> **判定提示**：`chars` 很小**不等于**失败。本次实测遇到过 3 个"疑似低产"样本经溯源核对后判定**正确**：824KB 的 `.rtf` 里 808KB 是内嵌 EMF 十六进制块（可读正文仅约 637 字符）；两个 `.pptx` 的 slide XML 可见文本本就只有 56/72 字符（图片型演示稿）。**字符数必须与源文件的实际文字量对比**，不能只看大小。

### TC-B3 缓存机制验收

测试 `acceptance_conversion_cache_roundtrip` 的输出应形如：

```
[缓存验收] <文件>
  首次（未命中→转换+写回）: XX ms，缓存条目 1
  二次（应命中缓存）      : <1 ms，缓存条目 1
```

| 断言 | 期望 |
|---|---|
| 两次 `text` 逐字节相等 | 缓存可信 |
| 两次 `source_kind` / `page_spans` 相等 | provenance 可缓存 |
| 两次后条目数均为 **1** | 未发生重复转换 |

---

## 5. C1 —— GUI：格式打开与预览（P0）

**通用步骤**：把样本目录作为知识库打开 → 等索引完成（或手动"全量重新索引"）→ 在文件树中**单击**目标文件。
**通用反例断言（每次都要看）**：
- ❌ 不得出现"⚠️ 文件类型不支持"
- ❌ 不得出现 Monaco 代码编辑器里满屏乱码
- ❌ 转换类文件**不应**出现可点的"编辑"按钮（避免把二进制覆盖成文本）

### TC-G1 新格式走后端转换预览（不乱码）

对下表逐个文件执行通用步骤：

| 文件 | 预期可见 | 预期不可见 |
|---|---|---|
| `.docx` | 渲染后的 Markdown（标题/列表/表格样式） | 乱码；编辑器 |
| `.docm` | 同上 | 同上 |
| `.pptx` / `.pptm` / `.ppsx` / `.ppsm` | 每页 slide 文本，近似 `##` 标题 | 乱码 |
| `.ppt` / `.pps` / `.pot` | 同上 | 乱码 |
| `.xlsm` / `.xlsb` | Markdown 表格 | 乱码 |
| `.odt` / `.ods` / `.odp` | 文本 / 表格 | 乱码 |
| `.rtf` | 正文 | 乱码 |
| `.epub` | 章节文本 | 乱码 |
| `.doc` | 正文（旧版 OLE） | 乱码 |

> `.xlsx` / `.xls` 走**前端原生表格渲染**（SheetJS），不是后端转换预览——这是**设计如此**（见 `CONVERTED_DOC_EXT_SET` 排除这两者）。

### TC-G2 预览与入库内容一致（同一 loader）

1. 索引含独特词的文件（例：`.docx` 里含 `Quantum`）。
2. 检索该词 → **应命中**该文件。
3. 单击该文件预览 → 预览正文里**应有**该词，且措辞与检索命中片段一致。

**通过标准**：预览与检索看到的是同一份转换结果（后端 `document_preview` 与索引共用 `DocumentLoader`）。

### TC-G3 曾"能搜到却点不开"的格式现在可打开（本次修复）

在知识库里放入：`a.markdown`、`b.mdown`、`c.mdx`、`d.tsv`、`e.cfg`、`Makefile`（无扩展名）、`x.makefile`。
逐个单击：**全部能打开**，其中 `.markdown`/`.mdown`/`.mdx` **按 Markdown 渲染**（标题/表格正常显示），其余按文本打开。

> 修复前症状：这些都提示"文件类型不支持"（后端能索引、前端拒开）。

### TC-G4 PDF 文本件：预览 + 页码

1. 单击**文本型多页 PDF**。
2. 预期：PDF 查看器正常显示；能翻页；正文可选中（说明文字层在）。

> 页码 provenance 的用户可见验证在 TC-G6。

### TC-G5 PDF 扫描件：明确提示需 OCR

1. 单击**扫描件 PDF**。
2. 预期：显示 `该文件无法预览：扫描件/图片页需 OCR（第 1,2,3 页，共 3 页）` 之类的**明确原因**（页码随实际变化）。
3. **不得**显示空白、不得报"未知原因"、不得进入乱码编辑器。

### TC-G6 页码定位（跨页用例，**需多页文本 PDF**）

1. 检索一个只出现在**第 2 页**的词。
2. 看命中块的来源标注（前端来源/引用区）。
3. 预期：标注的页码为 **2**，且与 PDF 中实际页一致。
4. 再检索一个**跨页**的内容（如第 2 页结尾 + 第 3 页开头的连续论述）→ 命中块应给出**页区间 2–3**。

> 这是本次修复过的严重缺陷的验收点：修复前每页首行会被判给**上一页**，偏移随页号累积。

---

## 6. C2 —— GUI：索引生命周期与诊断（P1）

### TC-G7 全量索引 / 增量索引

| 步骤 | 预期 |
|---|---|
| "索引选项" → 全量重新索引 | 进度条推进；完成后统计数字刷新 |
| 往知识库加 1 个新文件 → "索引选项" → 增量索引 | 只处理新增文件；完成后 `未索引` 计数回到 0 |
| 修改 1 个已索引文件 → 增量索引 | 该文件被重新处理 |

### TC-G8 按类型重建（双按钮）——**需要模拟"类型过期"**

正常状态下横幅不会出现，需要人为制造过期。**可复现的模拟方法**：

1. 先做一次全量索引（保证索引存在）。
2. 编辑 `$CRATE\src\core\document\loader.rs`，把
   `pub const PDF_INSPECTOR: Self = Self { id: "pdf-inspector", version: "1.19.0" };`
   临时改成 `version: "1.19.1"`。
3. `cargo build`（debug 即可）→ 启动 App → 打开知识库面板。
4. 预期：
   - 状态文本出现 `⚠️ 以下类型的转换器已更新，需重建索引：pdf`
   - 旁边出现**两个按钮**：`只重建 pdf` 与 `全量重建`
5. 点 **`只重建 pdf`** → 预期：进度条走完；完成后横幅**消失**；**其它类型（docx/代码/markdown）未被重跑**。
6. 反向验证（可选）：重复步骤 2-3，这次点 `全量重建` → 横幅同样消失。
7. **务必把 `version` 改回 `"1.19.0"` 并重新 build**。

| 判定 | 说明 |
|---|---|
| 通过 | 文案列出了具体类型 + 两个按钮都在 + 只重建生效 |
| 失败信号 | 文案只说"分块参数已变更，请重建索引"（**说明类型信息丢了**）；或只有全量按钮；或点只重建后横幅仍在 |

> **为什么要这么麻烦**：`stale_kinds` 只在"注册表里的转换器身份 ≠ 索引快照记录的转换器身份"时非空。改 `version` 常量是最小、可逆的模拟方式（该常量同时参与转换缓存主键与 `kind_converters` 快照——这正是它的设计用途）。

### TC-G9 诊断面板：未入库文件

1. 样本目录里**放入扫描件 PDF**，其余正常。
2. 全量索引 → 打开知识库面板（需索引非空，"健康度"区域才显示）。
3. 预期在健康度区域下方出现：
   ```
   ⚠️ 未入库文件 1 个（整文件未进入索引，搜索不到）
   · <文件名>.pdf — 扫描件/图片页需 OCR（第 1,2,3 页）
   ```
4. 点击/检索该文件 → 确认**确实搜不到**（与提示一致）。

### TC-G10 诊断面板：部分索引文件（**需自备混合页 PDF**）

1. 放入**混合页 PDF**（部分页有文字层、部分页是扫描图）。
2. 全量索引 → 打开面板。
3. 预期：出现**另一个**区块
   ```
   ⚠️ 部分索引文件 1 个（已入库，但下列页未进库）
   · <文件名>.pdf — 跳过 第 4 页 / 共 6 页：<页级原因>
   ```
4. 检索该文件里**有文字的那些页**的内容 → **应能命中**（证明文件确实进了库）。
5. 关键判定：**两类诊断必须分开显示**。若扫描件被显示成"部分索引"或反之 → 不通过。

### TC-G11 诊断的持久性与增量不误清（本次修复）

1. 按 TC-G9 得到一个"未入库文件"记录。
2. **不要**修复该文件，改为触发一次**增量索引**（或改动一个无关文件让 watcher 触发）。
3. 预期：那条"未入库"记录**仍然在**。
4. 然后真的修复它（把扫描件换成文本件，或删掉它）→ 增量索引 → 预期：该记录**消失**。

> 修复前症状：一次增量就会把诊断清空，用户以为"问题自愈了"，而文件从未被重新处理。

### TC-G12 转换缓存生效（二次索引更快）

1. 全量索引一次，记下耗时。
2. 再全量索引一次（未改任何文件）。
3. 预期：**第二次的"转换"阶段明显更快**（Office/PDF 从缓存还原），总耗时低于首次。
4. 可选：`%APPDATA%\com.mdgo` 之外的 `{知识库}\.mdgo\conversion_cache.sqlite` 应存在且行数 > 0。

---

## 7. C3 —— GUI：其它回归与可解释性（P1/P2）

### TC-G18 Agent 读取二进制文档（**本次新增能力，对应实际报障场景**）

> 报障原状：在 Agent 界面问某个 PPT 文件的具体问题，**没有任何工具能读到文件内容**（内容是二进制）。

**前置**：知识库里有一个 `.pptx`（或 `.docx`/`.pdf`），知识库已完成索引，Agent 会话已打开。

| 步骤 | 预期 |
|---|---|
| 1. 直接问 Agent：**"<PPT 文件名> 里讲了什么？"** | Agent **应当自行调用 `read_document`**（工具面板中可见该调用） |
| 2. 查看 `read_document` 的返回 | 是 **Markdown 正文**，开头有 `[文档] <路径> ｜ 转换器=anydoc@0.2.4 ｜ 来源类型=office ｜ 形态=markdown` |
| 3. 继续追问文件里的**具体细节**（如某页的标题、某个数据） | 回答**有据**且与文件内容一致；引用区显示该文件 |
| 4. 打开该文件的前端预览（TC-G1）对比 | Agent 读到的内容与预览内容**一致**（同一 `DocumentLoader`） |

**不通过信号（重点观察）**：
- ❌ Agent 回答"我无法读取二进制文件"/"没有工具能读这个文件" → 工具未注册或未进 `BASE_TOOLS`
- ❌ `read_document` 返回**乱码**（大量 `�`、控制字符）→ 走了 lossy 路径
- ❌ `read_document` 返回**空**，或只说"读取失败"而没有原因 → 错误信息不可解释
- ❌ Agent 反复用 `read` 重试却拿不到内容 → `read` 的引导文案失效

**换格式各测一遍**（至少覆盖 3 种）：`.docx`、`.pdf`（文本型）、`.xls`/`.xlsx`、`.pptx`、`.rtf`、`.odt`/`.epub`（有样本时）。

### TC-G19 Agent 面对扫描件 PDF 的可解释性

1. 知识库里放**扫描件 PDF**，问 Agent 关于它的内容。
2. 预期：Agent 调用 `read_document` 后拿到**明确原因**——`无法解析 <文件>（needs_ocr）：扫描件/图片页需 OCR（第 1,2,3 页，共 3 页）` + `建议：…需先用 OCR 生成带文字层的 PDF…`。
3. 预期：Agent 把"需要 OCR"如实转述给你，**不得**编造内容、**不得**把乱码当正文。

### TC-G20 `read` 对二进制文档的拒绝与引导（防乱码回归）

（这条通常由 Agent 间接触发，也可用"让 Agent 用 read 读这个 pptx"的方式直接观察）

| 步骤 | 预期 |
|---|---|
| 让 Agent 调用 `read` 读 `.pptx`/`.docx`/`.pdf` | 返回**明确错误**：`<路径> 是二进制文档（转换器：anydoc），不能按文本直读。请改用 read_document 工具读取其文本内容（它会转换成 Markdown）。` |
| 让 Agent 调用 `read_document` 读 `.md` | 返回**明确错误**引导回 `read`：`<路径> 是文本类文件（可直接读取），请用 read 工具…` |
| 让 Agent 用 `grep` 搜一个只存在于 `.pptx` 里的词 | 未命中，但结果附注 `（注：本次有 N 个文档文件是二进制格式（PDF/Office 等），grep 无法搜索其内容；若需查看其中内容，请用 read_document 读取该文件）` |

> 第三步很重要：它把"grep 没找到"与"文件里没有"区分开，避免 Agent 给出**错误的否定结论**。

### TC-G21 Agent 读取未登记格式仍保持宽容

1. 放入 `app.log`（内容随意），让 Agent 读取。
2. 预期：能正常读到内容（`.log` 未登记 → 走 lossy 宽容路径，**不报**"二进制文档"）。

### TC-G13 文档问答（DocAgent）对二进制不再喂乱码

| 步骤 | 预期 |
|---|---|
| 对 `.docx` 发起"小助手"文档问答 | 能引用到内容，回答有据 |
| 对**扫描件 PDF** 发起文档问答 | 给出**明确失败提示**（`该文件无法作为文本读取：…需 OCR…`），**不得**把乱码当正文喂给模型 |

### TC-G14 大文件不 OOM（尺寸护栏）

1. 复制一个较大的文件并把扩展名改成已登记格式（如把 300MB 的二进制改名为 `big.docx`），放入知识库。
2. 全量索引。
3. 预期：该文件被**跳过**并给出"文件过大"类原因；**进程不崩溃**、内存不飙到文件大小。
4. 预览该文件 → 也应给出明确原因而非卡死。

> 上限：`ConversionPolicy::max_file_bytes = 200MB`（`filekind.rs`）。

### TC-G15 表格按列名检索（本次新增 R3）

1. 索引一个 `.xlsx`，其表头含独特英文列名（例：`Sales_Rep_Name`）。
2. 检索 **`Sales_Rep_Name`**。
3. 预期：命中该表格所在 chunk。
4. 预期：命中块的**表头列名 metadata** 与源表头一致（可在来源/调试输出中确认；该字段不参与向量文本）。

### TC-G16 大纲类与既有格式未回归

| 文件 | 预期 |
|---|---|
| `.opml` / `.mm` | 思维导图/大纲视图正常 |
| `.csv` | 表格渲染正常 |
| `.html` | 渲染正常 |
| `.md` | 双向：预览 + 可编辑 |
| `.png` / `.mp4` | 图片/视频正常 |
| `.json` / `.yaml` | 文本/高亮正常 |

### TC-G17 未登记格式仍按宽容行为处理

1. 放入 `app.log`（非 UTF-8 字节也可）。
2. 预期：文档问答/读取**仍能**按 lossy 方式读出（保持旧宽容行为），**不报错、不崩溃**。

> 与 TC-G13 的区别：`.log` **未登记**（不在注册表），故保留 lossy 兜底；`.pdf`/`.docx` **已登记**，故失败必须报错。

---

## 8. 已知限制与未覆盖项（**请勿当作通过**）

| 项 | 说明 |
|---|---|
| **严格 A/B 检索对比** | 方案 §8.3 要求"30 篇 PDF + 60–100 问、改动前后同语料对比"。**未做**。仓库基准语料以 markdown/代码为主、PDF 极少，且语料规模在版本间变化过，故只能判断"无数量级退化"，**不能宣称检索质量提升** |
| **19 字节 xref PDF** | 依赖补丁保留了 `pdf-inspector 1.19.0`（其含 `xref_repair`，可恢复这类 PDF）。**未在本机构造该样本实测**，结论来自上游源码注释 |
| **CJK/CMap 陈旧字体** | 同上，1.19.0 含 `identity_overrides` 修复，**未针对性构造样本实测** |
| **部分索引（混合页 PDF）** | 需自备样本（TC-G10）；无样本时该分支仅有合成单测覆盖 |
| **加密文档** | 样本集中无加密 PDF/Office；`Encrypted` 映射只有单测覆盖 |
| **`.wps`** | 按决策 Q3 **不支持**，属预期行为 |
| **前端两条分发链收敛** | 未做（`main.html` 无前端测试框架）；两条链当前**已同步**，TC-G1 会覆盖主链，`previewFile` 副链需靠 TC-G1 的"编辑→预览"路径间接覆盖 |
| **Q9 体积增量** | 未测量 |
| **`renderFile` 仍会 `getFile()` 整文件进内存** | 大文件预览的"双倍搬运"只在新转换分支被绕开，主链未改；50MB 门禁仍在 |
| **依赖补丁** | `[patch.crates-io] lopdf` 为 git 源，**首次构建/CI 需访问 GitHub**。移除条件：`pdf-inspector` 依赖 `lopdf ≥0.45`（`0.45.0` 已发布） |

---

## 9. 验收汇总表（可直接填写）

### 9.1 自动化（§1–§3）

| 用例 | 内容 | 结果 | 备注 |
|---|---|---|---|
| TC-A1 | `cargo test --lib` = 433/0 | ☐ 通过 ☐ 不通过 | |
| TC-A2 | `cargo check --lib --tests` = 0 warning | ☐ 通过 ☐ 不通过 | |
| TC-A3 | `cargo build --release` | ☐ 通过 ☐ 不通过 | |
| TC-A4 | 守卫有效性验证（临时破坏 → 应失败 → 还原） | ☐ 通过 ☐ 不通过 | |
| TC-B1 | 真实样本端到端（硬断言全过） | ☐ 通过 ☐ 不通过 | 样本数：___ |
| TC-B2 | 转换保真度人眼核对 | ☐ 通过 ☐ 不通过 | |
| TC-B3 | 缓存等价（条目数恒为 1） | ☐ 通过 ☐ 不通过 | |
| TC-C1 | 检索指标与基线 v5 同量级 | ☐ 通过 ☐ 不通过 | 语料：___ |
| TC-C2 | 42 条查询无异常 | ☐ 通过 ☐ 不通过 | |

### 9.2 GUI（§5–§7）

| 用例 | 内容 | 结果 |
|---|---|---|
| TC-G1 | 19 个 Office/ODF/RTF/EPUB 变体不乱码、不进编辑器 | ☐ 通过 ☐ 不通过 ☐ 未验证 |
| TC-G2 | 预览与入库内容一致 | ☐ 通过 ☐ 不通过 |
| TC-G3 | 曾拒开的 7 个扩展名现在可打开 | ☐ 通过 ☐ 不通过 |
| TC-G4 | PDF 文本件预览正常 | ☐ 通过 ☐ 不通过 |
| TC-G5 | 扫描件提示需 OCR（含页码） | ☐ 通过 ☐ 不通过 |
| TC-G6 | 跨页页码定位正确 | ☐ 通过 ☐ 不通过 ☐ 未验证（需多页 PDF） |
| TC-G7 | 全量/增量索引 | ☐ 通过 ☐ 不通过 |
| TC-G8 | 按类型重建双按钮（含模拟 stale） | ☐ 通过 ☐ 不通过 |
| TC-G9 | 诊断：未入库文件 | ☐ 通过 ☐ 不通过 |
| TC-G10 | 诊断：部分索引（分开显示） | ☐ 通过 ☐ 不通过 ☐ 未验证（需混合页 PDF） |
| TC-G11 | 诊断持久性 + 增量不误清 | ☐ 通过 ☐ 不通过 |
| TC-G12 | 转换缓存使二次索引更快 | ☐ 通过 ☐ 不通过 |
| TC-G13 | DocAgent 不喂乱码 | ☐ 通过 ☐ 不通过 |
| TC-G14 | 大文件不 OOM | ☐ 通过 ☐ 不通过 |
| TC-G15 | 表格按列名检索 | ☐ 通过 ☐ 不通过 |
| TC-G16 | 既有格式无回归 | ☐ 通过 ☐ 不通过 |
| TC-G17 | 未登记格式保持宽容 | ☐ 通过 ☐ 不通过 |
| **TC-G18** | **Agent 读取二进制文档（PPT/PDF/Office）** | ☐ 通过 ☐ 不通过 |
| TC-G19 | Agent 对扫描件 PDF 给出"需 OCR" | ☐ 通过 ☐ 不通过 |
| TC-G20 | `read` 拒绝二进制 + 引导；grep 附注不可搜 | ☐ 通过 ☐ 不通过 |
| TC-G21 | Agent 读未登记格式仍宽容 | ☐ 通过 ☐ 不通过 |

### 9.3 结论

- 通过项：___ / 21（GUI）+ ___ / 9（自动化）
- 不通过项及影响：___
- 未验证项及原因：___
- **总体判定**：☐ 可发布 ☐ 需修复后重测（列出阻断项）

---

## 10. 附录：命令速查

```powershell
$CRATE = "G:\gitProject\mdgo\tauri\src-tauri"
$PSNativeCommandUseErrorActionPreference = $false   # 每次新开终端都要设

# 冒烟
cargo test --lib
cargo check --lib --tests

# 只跑某模块
cargo test --lib core::document::loader
cargo test --lib core::document::filekind
cargo test --lib core::indexer
cargo test --lib core::db::lance::tests::chunk_metadata_round_trips_through_lancedb -- --nocapture

# 真实样本验收
$env:MDGO_ACCEPT_DIR = 'G:\kb-samples'
$env:MDGO_ACCEPT_DUMP = '260'
cargo test --lib acceptance -- --nocapture --test-threads=1

# 检索回归（注意会清空目标目录的 .mdgo，建议用副本）
cargo run --bin benchmark --features bench -- --kb <副本目录> `
  --queries ..\..\retrieval_eval\queries.jsonl `
  --expected ..\..\retrieval_eval\expected.jsonl --topk 20 --reindex --yes-wipe

# 列出全部测试名
cargo test --lib -- --list
```

**环境变量**

| 变量 | 作用 |
|---|---|
| `MDGO_ACCEPT_DIR` | 真实样本目录；**不设则 acceptance 测试跳过** |
| `MDGO_ACCEPT_DUMP` | 打印每个样本转换后正文前 N 字符（人眼核对保真度） |

**日志**：`%APPDATA%\com.mdgo\logs\mdgo.log`（`set_log_level` 可热切 DEBUG）
