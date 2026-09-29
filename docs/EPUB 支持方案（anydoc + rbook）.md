# EPUB 支持方案（anydoc + rbook）

> **状态**：已落地（Phase 4 / 缺口 G1–G5）　**最后更新**：2026-09
> **关联文档**：`docs/文档解析管线升级方案（anydoc + pdf-inspector）.md`（Phase 0–3，
> 本文件是其 Phase 4 续篇）、`docs/文档解析与索引管线（Plan B v2）功能说明.md`（面向使用者）
> **上游现状**：`anydoc = "=0.2.4"`、`rbook = "0.7.10"`（Phase 4 新增）

---

## 1. 需求与结论

**需求**：支持 `.epub`；用户在文件树点击该格式文件后，正文区打开其内容（流式 Markdown 阅读），
**内嵌图片必须可见**。

**结论**：不必引入前端 EPUB 阅读器。改造前正文链路**已经能出文本**（`anydoc` 的 EPUB 通路
已接入），真正缺的是 5 个具体缺口；补齐这 5 个缺口即可满足需求，且能守住
「索引与预览共用 `DocumentLoader`」的既有硬契约。

选型对比（详见 §4）：**后端 `anydoc` 出正文 + `rbook` 出真目录**。`rbook` 与 `anydoc`
共用同一批依赖（`zip 8.6` / `quick-xml 0.41` / `indexmap 2.x` / `thiserror 2.x`），
`cargo` 实测**只新增 1 个包**（`Adding rbook v0.7.10`，无任何重复版本）。

---

## 2. 改造前的五个缺口（均为实测/源码核实）

| # | 缺口 | 根因 | 证据 |
|---|---|---|---|
| **G1** | 内嵌图片全部丢失，只剩 alt 文本 | `anydoc` 的 Markdown 渲染器对 `ImageSource::Asset` 只输出 alt；图片字节虽在 `Document::assets`，但 Markdown 里**没有任何占位** | `anydoc-0.2.4/src/render/markdown/inline.rs`（`Asset(_) \| Unavailable =>` 分支只 push alt）；README §134 自述 |
| **G2** | 长书被截断 | `PREVIEW_TEXT_LIMIT = 200_000` 字符，是「按 PDF/报告」定的量级，一本 20 万字的中文长篇正好卡边界 | `commands/doc.rs` |
| **G3** | 跨章引用 / 内链点不动 | `anydoc` 假设渲染器会生成 GFM 标题 slug（`anchors.rs::gfm_slug`），而本仓 `marked v15` 配了 `headerIds: false` 不生成 id，随后 `addHeadingIDs()` 又把标题 id 统一改写成 `heading-{n}` | `main.html`：`marked.setOptions({headerIds:false})`、`addHeadingIDs`；实测 anydoc 产出 `[见 2.1](#21-子节)` |
| **G4** | 书的真目录完全没被读取，右侧大纲靠标题反推 | `anydoc` 的 EPUB 实现只按 spine 顺序拼接正文，**整个 crate 无一处 nav/ncx 代码** | `anydoc-0.2.4/src/formats/epub/mod.rs`（268 行）；真实书上「目录标签 vs 章节标题」文字匹配命中率很低：`Metamorphosis-jackson.epub` 仅 **3/8**（Cover / Title Page / Copyright 等条目根本没有对应标题） |
| **G5** | 转换类文件被整本读进 JS | `renderFile` 在分派前无条件 `getFile()`，而 Tauri 适配层的 `getFile` 会把整个文件读进内存再包 Blob，对 `.pptx/.xlsb/.epub` 是双倍搬运 | `main.html::renderFile`；方案 §7.2.1 早已记录「只做到一半」 |

---

## 3. 落地清单

### 3.1 后端（Rust）

| 文件 | 改动 |
|---|---|
| `Cargo.toml` | 新增 `rbook = { version = "0.7.10", default-features = false, features = ["threadsafe"] }`（去掉只写不读的 write 半边）与 `zip = { version = "8.6", default-features = false, features = ["deflate"] }`（读/重建容器）。两者与 anydoc 共用同一批依赖，不新增 crate 版本 |
| `core/document/epub.rs`（**新增**） | ① `enrich()`：把 zip 内 XHTML 的 `<img src="相对路径">` 重写为 `mdgoasset://local/<base64url(绝对路径)>`，并把图片导出到内容哈希目录；② `rebuild_zip()`：重建容器（`mimetype` 强制首位 + Stored）；③ `extract_toc()`：rbook 读 EPUB 3 `nav.xhtml` / EPUB 2 `toc.ncx`；④ `map_headings()`：把目录条目精确对应到正文标题序号；⑤ `asset_dir_for()`：`<cache>/mdgo/epub-assets/<内容哈希前16位>` |
| `core/document/filekind.rs` | 新增 `Converter::Epub`；**把 `epub` 从 Office 组拆出单独登记**，`source_kind = "epub"`（失效粒度键独立于 `office`）；更新跨层守卫用例与 `phase2` 用例 |
| `core/document/loader.rs` | 新增 `ConverterInfo::EPUB = { id: "epub-enhanced", version: "1" }`；`DocumentSource` 增加 `epub_toc` / `asset_root`；新增 `Converter::Epub` 分支与 `convert_epub()`；`convert_anydoc()` 增加 `explicit: Option<Format>` 形参（重建后的容器不再依赖内容嗅探） |
| `core/db/conversion_cache.rs` | `Converter::Epub => ConverterInfo::EPUB`：**缓存主键与 Office 分离**——富化实现改动只让 EPUB 旧结果失效 |
| `core/indexer.rs` | `expected_kind_converters()` 增加 `Converter::Epub` 分支，`epub` 成为独立失效 kind |
| `core/agent/tools/mod.rs` | `document_converter_label()` 增加 `Converter::Epub => Some("epub")` |
| `commands/doc.rs` | `PREVIEW_TEXT_LIMIT` 20 万 → **400 万**（G2）；`DocumentPreview` 增加 `toc` 与 `asset_root` 两个 snake_case 字段 |
| `acceptance.rs` | 新增 `Converter::Epub` 断言分支；缓存验收样本选择纳入 Epub |

### 3.2 为什么图片必须「先改字节、再交给 anydoc」

`anydoc` 的 `render` 模块是**私有**的（`mod render;`），公开 API 只有
`to_markdown_bytes()` / `to_document()`，因此**无法**把 `Document` 重新渲染成 Markdown，
也无法把 `ImageSource::Asset(id)` 改成 `External(url)`。而图片在它产出的 Markdown 里
**连占位都没有**（只有 alt 文本），后处理拿不到插入位置。

唯一的入口是 `image_source()` 的形参：带 scheme 的**绝对 URI** 会走
`ImageSource::External` 分支并渲染成 `![](url)`。所以方案是在**解析之前**改写容器内 XHTML 的
`<img src>`，让 anydoc 自己吐出真实图片 Markdown。

**URL 采用 `mdgoasset://local/<文件名>`（`<sha256 前 32 位>.<ext>`），只放裸文件名**：
字符集仅 `[a-z0-9]` 与 `.`，不含 `%`、空白、括号、`|`，因此对 Markdown 层、`marked` 的
`encodeURI`、DOMPurify 等任何一层 URL 处理都免疫（anydoc 的 `format_url` 只在含空白或括号时
才用尖括号包裹，这里永不触发）。**不放绝对路径**是刻意的：
① 隐私——正文可能被复制/导出，绝对路径里带用户名与目录结构；
② 体积——文件名约 40 字符，绝对路径 base64 后常 150+ 字符；
③ 安全——前端只接受「后端给的 `asset_root` + 裸文件名」，构造过的 EPUB 连表达 `..`
或绝对路径的机会都没有（早期版本用「base64(绝对路径) + 前缀校验」，安全性更弱也更啰嗦）。
文件名是图片字节哈希，内容相同即天然去重。

### 3.3 为什么正文富化放在**装载**里而不是只在预览命令里

因为富化**改变了正文**（插入了图片 Markdown）。若只在预览路径做，同一本书在「检索侧」与
「阅读侧」内容就不一致，破坏方案 §4.1 的唯一入口契约。放在 `DocumentLoader` 里则索引与预览
拿到同一份正文，且转换缓存（Phase 3）继续生效。

### 3.3.1 索引侧必须清理图片目标地址（**复核新增**）

富化把图片写进正文，而**分块文本取的是 Markdown 源码行切片**（`document/markdown.rs` 的
sourcepos 切片，不是 inline 纯文本）→ 图片 URL 会原样进入 **BM25 文本与 embedding 输入**。
实测确认（回归用例 `enriched_image_urls_do_not_leak_into_index_text` 一度**失败**）：
一本书几十张图就是几十段 URL 噪声，既污染关键词检索又白占 token 预算；
早期「base64(绝对路径)」形态还会把用户目录结构一并灌进索引与给模型的证据文本。

因此 `pipeline::chunk_document` 在分块前对 **`source_kind == "epub"`** 的文本调用
`epub::strip_image_destinations()`，把 `![alt](dest)` 清成 `![alt]()`：
**alt 文本（真正的检索价值）保留、AST 形态与 `chunk_type` 不变、预览渲染不受影响**
（预览用的是原始 `src.text`）。行偏移会变化，但 EPUB 的 `page_spans`/`line_page_map` 恒为空，
不存在页归属错位风险——这与 `html_clean` 对 Markdown 的既有清洗同一道理。

**为什么只作用于 EPUB**：普通 Markdown 的图片路径属于**用户自己的内容**，改动它会变更既有
索引文本并触发全量重建，收益与风险不成比例，故保持原行为。

### 3.4 前端（`main.html` + `css_js/modules/epub-anchor.js`）

| 缺口 | 改动 |
|---|---|
| G1 | 新增模块 `css_js/modules/epub-anchor.js`：`mdgoResolveAssetSrc()` 解析 `mdgoasset://local/<文件名>`——**只接受裸文件名**（正则 `^[0-9a-f]{32}\.[a-z0-9]{1,8}$`），路径由「后端给的 `asset_root` + 该文件名」拼出，构造过的 EPUB 无法表达绝对路径或 `..`；`renderConvertedDocFile` 调 `mdgoSetAssetRoot()`；`buildLazyTauriImg()` 的 `resolveSrc` 在 `buildLocalImgSrc` **之前**接住该 scheme，交给 `convertFileSrc()` |
| G2 | 截断提示不再写死「20 万字符」（与后端脱钩），改为「内容过长，已截断，仅显示开头部分」 |
| G3 | 同模块提供 `mdgoHeadingSlug()` / `mdgoSlugAllocator()`，**逐字镜像** anydoc 的 `gfm_slug` 与 `UniqueIds::claim`（同名后缀 `-1`/`-2`…、空结果回落 `section`）；`extractMarkdownHeadings` 改用 slug 作为标题 id，与后端内链目标对齐 |
| G4 | `generateTOC` 消费 `_pendingEpubToc`：有真目录时用 `_buildEpubTocItems()` 把 `heading_index` 折算成 `headings[n-1].id` 再渲染大纲。**不使用按标题文字模糊匹配** |
| G5 | `renderFile` 对 `CONVERTED_DOC_EXT_SET` 命中项**提前返回**：只用 `basename(relativePath)` 取文件名，不再 `getFile()` 整文件读入 |

---

## 4. 开源方案调研结论（选型依据）

### 4.1 后端 Rust

| 方案 | 许可 | 目录能力 | 依赖增量 | 结论 |
|---|---|---|---|---|
| `anydoc` 0.2.4（已有） | MIT | ❌ 无任何 TOC/nav/NCX 代码 | 已在树内 | ✅ 继续出正文 |
| **`rbook` 0.7.10** | Apache-2.0 | ✅ 完整 NCX + EPUB 3 nav 树（label/depth/href/children/landmarks/page_list），`LinearBehavior` 管 `linear="no"` | ✅ **零新增 crate 版本** | ✅ **选用** |
| DIY `zip` + `roxmltree` | — | 需自写 150–250 行 | 零新增 | ⚖️ 可行但有坑：`roxmltree` 的 `allow_dtd` 默认 `false`，而真实 `toc.ncx`（带公共 DTD）与 `nav.xhtml`（`<!DOCTYPE html>`）**都会解析失败** |
| `epub` 2.1.5（= danigm/epub-rs） | ⛔ GPL-3.0 | ⚠️ 仅 NCX；实测 EPUB 3 书 `toc len = 0` | 新增 zip 3.x + `xml-rs` | ⛔ 排除 |
| `epub-builder` | MPL-2.0 | ❌ 只写不读 | `libzip` C 绑定 | ⛔ 不适用 |
| `epub-parser` | MIT | ⚠️ 仅 NCX | zip 2.1 + quick-xml 0.36 | ⛔ 排除（停更、4★） |

### 4.2 前端 EPUB 阅读器（本次**未采用**）

| 方案 | 许可 | 备注 |
|---|---|---|
| `foliate-js` | MIT | parse-only 约 10.5KB gz；原生 ESM 无需打包器；Tauri v2 阅读器 Readest 在用。但**无任何 release/tag**，作者自述 API 不稳定 |
| `epub.js` | BSD-2 | npm 最后发布 2022-02，master 有提交但未发版，517 open issues |
| `@likecoin/epub-ts` | BSD-2 | epub.js 的活跃 TS 重写 |
| `@readium/navigator` | BSD-3 | **根本不能解析 EPUB 容器**——需 Go/Kotlin 侧预生成 RWPM + positions list + 资源服务器，架构与本项目相反 |

**未采用前端阅读器的关键理由**：

1. **契约冲突**：`document_preview` 与索引**必须**共用 `DocumentLoader`（方案 §4.1/§7.2.1）。
   前端阅读器会绕过该唯一入口 → 同一本书在「检索侧」和「阅读侧」出现两套解析结果。
2. **安全**：**没有任何一个 EPUB 前端库会 sanitize 章节 HTML 或自带 CSP**。
   而本仓 `csp: null`（未配置 CSP），文件渲染路径也**没有** DOMPurify（只有聊天渲染有）。
   引入阅读器就必须先补 CSP + sanitize。
3. **收益/成本**：阅读器的主要收益只是「翻页 + CSS 还原」，却要付出新引擎 + 双份解析真源 +
   Windows 自定义协议风险（`tauri-apps/tauri#11505`：iframe 子资源请求在 Windows 被拒，至今 open）。

---

## 5. 关键实现约束（后续维护必读）

1. **`ConverterInfo::EPUB.version` 必须随产出结构变化递增**。它是转换缓存主键
   （`id@version`）与失效快照的一部分；改了富化行为却不改版本号 → 旧缓存继续命中并返回旧结果。

   > ⚠️ **这条不是理论约束，已经真实踩过一次（详见 §8.3）**：把图片 URL 从
   > `base64(绝对路径)` 改成裸文件名时忘了递增 ver，导致**已打开过的那本书所有图片都不显示**。
   > 只改「后端产出的形态」而没同步这个常量，是本方案最容易犯、且后果最显眼的错误。
   > **判断标准：只要 `convert_epub` 的输出（正文文本 或 `epub_toc` 或 `asset_root`）会变，就升 ver。**
2. **只呈现一个 TOC 源：优先 `nav.xhtml`，回退 `toc.ncx`，绝不合并**。生产者同时提供两者时
   常常互相矛盾（重新生成时间不同 / NCX 是扁平变体），合并会产生重复与幻影条目。
   这也是对 `rbook` 多版本 `TocGroups` 设计的用法约束——它的 `by_kind_version` 可用，
   但只应挑一个版本。
3. **不要按 `playOrder` 排序 NCX**。它是跨 `navMap`+`pageList`+`navList` 的单一全局序列，
   且跳号合法（标准自身示例就是 2,3,5,10,11…）；权威是**文档顺序**。`rbook` 已按文档顺序解析。
4. **`epub:type` 必须按 token 匹配**，不能整串比较（Readium 有真实在跑的 bug：
   `//html:nav[@epub:type='toc']` 会让 `epub:type="toc bodymatter"` 永远找不到）。
5. **NCX 不能靠文件名定位**（`toc.ncx` 纯属约定），应经 `spine/@toc` 或 `media-type` 定位。
6. **`encryption.xml` 不等于 DRM**（也用于字体混淆）；nav/NCX 可能被内容级 DRM 加密，
   此时应**降级到下一个目录源**而不是报错。
7. **资源上限自己兜**：`rbook`/`epub`/`epub-parser` 读取条目都是无上限的
   `read_to_end`/`io::copy`；本模块已加 `MAX_IMAGE_BYTES`(16MiB)、
   `MAX_TOTAL_ASSET_BYTES`(64MiB) 与 `Read::take` 的二次硬上限（声明尺寸可能撒谎）。

---

## 6. 验收

### 6.1 自动化

```bash
cd tauri/src-tauri
cargo test --lib filekind          # 注册表 + 跨层守卫（含 epub 独立 source_kind）
cargo test --lib document::epub    # 新增：重写/目录/映射单测
cargo check                        # 全量类型检查
```

跨层守卫用例 `filekind.rs` 会把「注册表里需转换预览的扩展名」与 `main.html` 的
`CONVERTED_DOC_EXT_SET`、`support.js` 的 `CONVERTED_DOC_RE`、`_EXT_TYPE_MAP` 逐一对齐；
Phase 4 起该守卫的口径是 `source_kind ∈ {office, epub}`。

### 6.2 手工（GUI）

| # | 步骤 | 期望 |
|---|---|---|
| E1 | 把任意 `.epub` 放进库目录并刷新文件树 | 文件出现在树里（扫描与扩展名无关，只做黑名单过滤） |
| E2 | 点击该 epub | 正文区渲染全书文本，标题层级正确；**不出现乱码** |
| E3 | 观察右侧大纲 | 显示**书自己的章节目录**（nav/ncx），而非仅有 `<h1>` 反推结果；点击可跳转 |
| E4 | 含插图的 epub | 图片正常显示（封面/插图），不是 alt 文字 |
| E5 | 一本 30 万字以上的书 | 正文完整，尾部不被截断（G2） |
| E6 | 书内交叉引用 / 脚注链接 | 点击可跳转（G3） |
| E7 | 打开 epub 时观察内存/耗时 | 无「整本读进 JS」的双倍搬运（G5） |

---

## 7. 已知限制（诚实清单）

1. **章节无标题的书，右侧大纲仍为空**。`anydoc` 只为 XHTML 的 `<h1>`–`<h6>` 产标题；
   若一本书用 `<p class="title">` 排版，正文里就没有标题元素，真目录的
   `heading_index` 全部映射失败（本实现会丢弃这类条目，因为它们没有可跳转目标）。
   彻底解决需要「为目录条目合成章节锚点」，属后续工作。
2. **仅流式 Markdown 阅读，无分页/翻页与版式还原**。EPUB 自带的 CSS 只被 anydoc 的
   小型 CSS 子集消费（影响标题层级判定），不做版式复刻。
3. **`linear="no"` 的内容会被并入正文**：`anydoc` 明确选择包含非线性内容
   （「non-linear items are auxiliary but still publication content」），本方案沿用该行为。
4. **图片只在 Tauri 模式下可见**：资源经 Tauri asset 协议提供，浏览器模式
   （`isTauriVisit()` 为假）不会转换这些 URL。
5. **缓存目录不自动清理**：`<cache>/mdgo/epub-assets/<内容哈希>` 会随阅读过的书累积；
   目前没有 LRU 回收（内容哈希命名保证了正确性，只是占空间）。
6. **`index.html` / `index_cdn.html` 未纳入本次改动**，且**不是"漏同步"，而是它们本就与 EPUB 无关**：
   - 二者最后提交分别是 `585d807`（2026-08-28）与 `082bdc3`（2026-08-24），而 `main.html` 是
     `bb78c15`（2026-09-11，Plan B v2 提交本身）；
   - 二者**完全没有** Plan B v2 的文档管线：搜不到 `CONVERTED_DOC_EXT_SET`、
     `renderConvertedDocFile`、`document_preview`、`isMarkdownDoc`（只残留更早的
     `_TOC_EXT_SET`/`isTocSupported`）。也就是说它们连**EPUB 预览本身**都没有，
     不存在"只差 G1–G5"的情况；
   - 二者也**不参与构建**：`vite.config.js` 的 `rollupOptions.input` 只有 `main.html`，
     `tauri.conf.json` 的窗口 `url` 是 `main.html`（对 `index_cdn.html` 只有一条静态拷贝排除规则）。
   - **结论**：这是历史遗留的旧入口，建议**显式归档或从 `main.html` 重新生成**，
     而不是逐条手工回填 Phase 2–4 的改动（两文件已落后约 1,700 行，手工回填风险高于收益）。

---

## 8. 复核（自审）记录：已修与**刻意不修**

交付后做了一轮针对本方案自身的复核，结论如下。

### 8.1 已修

| # | 问题 | 修法 | 守它的用例 |
|---|---|---|---|
| R1 | **图片 URL 污染检索文本**（本轮发现的最实质缺陷）：分块文本取源码行切片 → URL 原样进入 BM25 与 embedding | `pipeline::chunk_document` 对 epub 调 `epub::strip_image_destinations()` | `enriched_image_urls_do_not_leak_into_index_text`（先复现失败、再转绿） |
| R2 | 图片 URL 里塞了**绝对路径**（隐私 + 体积 + 校验面更大） | 改为只放裸文件名，路径由前端用 `asset_root` 拼 | `asset_url_carries_only_a_bare_file_name` + 前端越权用例 |
| R3 | `extract_toc` 为满足 `rbook` 的 `'static` 约束**整本复制**字节（政策上限 200MB） | 新增 `extract_toc_from_path()`，有磁盘路径时直接 `Epub::open`，省掉复制（打不开才回退） | `extract_toc_reads_epub3_nav_with_levels` |
| R4 | 重建 zip 时把**所有**条目重新 Deflate，包括本来就已压缩的 PNG/JPEG（白费 CPU，且常把体积压大） | 按 `entry.compression()` 保留原压缩方式，不可写的方法退化 Deflate | `enrich_exports_image_and_makes_anydoc_emit_real_image` |
| R5 | GFM slug 与 anydoc 在**实体/转义**上的潜在分歧（`A&amp;B` 会算成 `ampb`） | `_mdInlineToPlain` 增加命名/数字实体解码与反斜杠反转义 | Node 对照用例（`A&amp;B` / `A&#38;B` / `A&#x26;B` / `C\*D`） |
| R6 | **R2 引发的线上回归：已打开过的书所有图片都不显示**（见 §8.3） | `ConverterInfo::EPUB.version` 1 → **2**，让旧缓存与旧索引按既有机制自然失效 | 端到端断言（URL 形态合规 + `asset_root` 下文件真实存在） |

### 8.3 事故复盘：改了产出格式却忘了递增转换器版本

**现象**：用户反馈「所有的图片都不能显示了」。

**根因**：§8.1 的 R2 把图片 URL 从 `mdgoasset://local/<base64url(绝对路径)>` 改成
`mdgoasset://local/<文件名>`。**正文里嵌了前端要解析的 URL**，于是这个形态变成了
一个**跨版本契约**，而它有两种被打破的方式（两种都会让**整本书的图片全部消失**）：

- **(a) 版本错配**：前端已更新、Rust 侧没重建。前端新解析器只认新形态，
  而后端仍在产出旧形态 → 解析失败 → 保留原始 `mdgoasset://…` src → webview 加载不了。
  *本次事故的最可能原因*：工作区里**没有** `.mdgo/conversion_cache.sqlite`，
  说明「旧缓存回放」这条路径并未发生，更像是半更新。
- **(b) 旧缓存回放**：转换缓存主键是
  `(source_hash, converter_id, converter_version, options_hash)`；若把形态改了却**没递增
  `ConverterInfo::EPUB.version`**，已打开过的书会继续命中旧行并回放旧形态正文。

**为什么之前没测出来**：单测只断言「Markdown 里含 `ASSET_URL_PREFIX`」——旧形态、新形态都满足，
所以测试是绿的而功能是坏的。**测试断言了「有 URL」，却没断言「这个 URL 前端能解析且磁盘上真有」。**

**修法与加固**（三处，分别对应不同的失败面）：

1. **前端容忍两种形态**：`mdgoResolveAssetSrc()` 先按当前形态（裸文件名）解析，
   失败再按旧形态（base64 → 绝对路径）解析，旧形态仍要求**解码后落在 `asset_root` 内且不含 `..`**。
   效果：**前后端版本错配退化为"照常显示"，而不是用户可见的功能中断**。
   代价是保留约 15 行兼容代码——相对于一次"图片全没了"的事故，这个代价是值得的。
2. **`ConverterInfo::EPUB.version` → `2`**：让旧缓存主键失配（预览立刻重新转换，
   **无需用户重建索引**），同时 `expected_kind_converters()` 里 `epub` 从
   `epub-enhanced@1` 变为 `@2` → 索引侧 `epub` 判为过期并按既有机制重建。
3. **把那条测试升级为端到端一致性断言**：从产出的 Markdown 里取出 URL 载荷，
   校验它符合前端约定的文件名形态（32 位小写十六进制 + ≤8 位小写扩展名），
   并断言 `asset_root + '/' + 文件名` **在磁盘上真实存在**。这样「URL 形态与前端解析器不匹配」
   会在 `cargo test` 就红，而不是等到用户看图。前端解析失败时也**打可定位的 warn**（按种类去重）。

**结论（给后来者的最短教训）**：
本方案的「产出形态」有三处对外契约——正文文本、`epub_toc`、`asset_root`，
其中**正文里嵌了前端要解析的 URL**。因此：

- **任何一处变了都必须递增 `ConverterInfo::EPUB.version`**；
- **凡是"内容里嵌了跨版本协议"的地方，前端都应容忍已知的历史形态**，
  因为前后端不可能原子升级（尤其本项目前端可直接刷新、后端要重新编译）；
- **验证这类改动必须断言"前后端对同一个 URL 的理解一致"**，只断言"URL 存在"是假绿。

### 8.2 已知但**刻意不修**（附理由，避免后人重复踩）

| # | 候选优化 | 为什么不修 |
|---|---|---|
| D1 | **每本 EPUB 在缓存未命中时被 anydoc 解析两次**（`to_markdown_bytes` 内部已 `to_document` 一次，取标题映射又要一次） | 私有 `render` 模块导致无法「一次解析、两处消费」：Markdown 只能由 `to_markdown_bytes` 产出，而块结构只能由 `to_document` 拿到。**替代方案是自己按 XHTML 复刻 anydoc 的标题抽取规则**——那会把「哪些元素算标题」的语义复制一份，anydoc 升级即静默错位。宁要 2× 解析（且**只在缓存未命中时**发生，索引/首预览各一次），也不要一个会悄悄腐坏的正确性契约。若上游开放 `Document → Markdown`，这一项即可消除 |
| D2 | 图片资源目录**无 LRU 回收**（`<cache>/mdgo/epub-assets/<内容哈希>` 随书累积） | 唯一有效做法是删目录，而「正在被阅读的书」与「已废弃的书」在当前设计里**无法区分**（`write_asset` 对已存在文件跳过写入，目录 mtime 不会因再次阅读而更新）。误删的后果是正文里出现坏图。收益（省几十 MB）远小于风险，故留作已知限制（§7.5） |
| D3 | 章节无 `<h1>`–`<h6>` 的书，右侧大纲仍为空（§7.1） | 正解是**按目录条目合成章节锚点**（后端在对应章节起始处插入一个带 id 的空标题），属于新增的正文变换，会再次改变索引文本并需要单独评估对 `chunk_type`/结构的影响。作为独立议题更合适 |
| D4 | 4M 字符预览上限下，超大文档会一次性构建巨大的 DOM（真正的瓶颈已从「IPC 体积」转为「webview 渲染」） | 正解是**分页/流式渲染**（只渲染视口附近章节），属于前端渲染架构改动，与本次「补齐 G1–G5」不是同一件事。当前上限相比改造前的 20 万字符已是 20×，先观察真实体感再决定是否投入 |
| D5 | 图片解析只接入了 `buildLazyTauriImg`（Markdown 预览链路）；AI 聊天/摘要等其他渲染链路若出现 `mdgoasset://` 仍会渲染失败 | 这些链路**拿不到**该 URL：R1 之后索引文本里已无图片 URL，聊天/摘要都基于索引片段。剩余风险仅为「直接把 epub 预览正文复制给模型」这类手工路径，影响面小（表现为一张坏图）。为避免过度扩散改动，暂不铺开，若日后需要则把 scheme 解析下沉到统一的图片解析处 |

