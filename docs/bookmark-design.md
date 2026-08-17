# mdgo Knowledge Asset Extension 设计定稿（Bookmark v2.0 简化重构）

```text
用户点击「导入书签 HTML 文件」（main.html，Tauri 模式）
   │
   ▼
[前端] main.html  _doImportBookmark()
   ├─ FileReader 读文件 → DOMParser 解析
   ├─ parseBookmarkHtml(doc)        ← 遍历 DT/DL/H3/A，构建目录树（ADD_DATE 秒→毫秒）
   ├─ collectBookmarkEntries()      ← 递归展平为 {url, title, folder路径, added_at}[]
   ├─ 超过 5 万条 → 前端弹提示，仅处理前 5 万条，超出丢弃
   └─ __mdgoBookmark.bookmarkImport(dir, entries, file.name)
        │
        ▼
[适配层] adapters/bookmark.js → invoke('bookmark_import', ...)
        │
        ▼
[命令层] commands/bookmark.rs  bookmark_import
        │  AppState.bookmark_store(dir_path)   ← 惰性创建 + HashMap 缓存（与 Worker 共享同一 Arc）
        ▼
[实体层] importer.rs  import_entries()
   ├─ 上限截断 50,000 条（后端兜底，前端已提示）
   ├─ BEGIN 事务
   │   └─ normalize_url()   ← 协议白名单 http/https（拒 javascript:/data:/file:），去尾斜杠
   │        ├─ canonical_url 已存在 → 直接跳过（不更新、不重新入队）【去重规则】
   │        └─ 不存在 → 插入（status=pending）
   ├─ COMMIT
   └─ 仅一张 SQLite 表 bookmarks（无任务队列表、无 FTS 表）
        │（Worker 按 status='pending' 认领）
        ▼
[后台] enrichment.rs EnrichmentWorker（setup 时 spawn，每 500ms tick，批 256 条）
   ┌──────────── 流水线（单库逐批，任一环节失败即终态 failed）────────────┐
   │ 阶段A 并发抓取（64 并发）                                            │
   │   ├─ SSRF 校验（拒内网/回环，逐跳重定向跟随）+ 10s 超时 + 2MB 上限    │
   │   ├─ readability → ammonia → htmd 转 markdown（降级 scraper）        │
   │   ├─ 成功 → 写 raw_content（仅用于 LLM 总结分类标签）                │
   │   └─ 失败 → status=failed, dead=1（后端死链识别），终态               │
   │ 阶段B LLM 总结（summary/category/tags 一次产出）                     │
   │   ├─ 成功 → 写 summary/category/tags                                │
   │   └─ 失败 → status=failed（不再后续、不入向量库），终态               │
   │ 阶段C 批量 embedding（Title+Summary+Tags → ONNX，spawn_blocking）    │
   │   ├─ embedding 文本顺序 Tags→Category→Title→Summary(截断300字)       │
   │   │    （防 BGE 512 token 截断把末尾 tags 丢掉）                     │
   │   ├─ 增量 upsert LanceDB bookmark_vectors（按 bookmark_id 覆盖）     │
   │   └─ status=ready（可检索：LIKE ∪ 向量补位）                         │
   └────────────────────────────────────────────────────────────────────┘
```

## 设计要点（相对 v1.x 的变化）

1. **单表**：仅 `bookmarks` 一张 SQLite 表；删除 `bookmark_jobs`（队列由
   `status='pending'` 直接驱动）、`bookmarks_fts`（检索用 LIKE 直扫）。
2. **三态状态（非状态机）**：`pending` / `ready` / `failed`。失败即终态，
   不再重试、不再入向量库；崩溃遗留的 pending 下次启动自动重新处理。
3. **去重规则**：以 `canonical_url` 为键，**已存在直接跳过**（不更新、不重新入队）。
4. **死链识别（后端）**：抓取阶段失败（HTTP 错误/超时/网络/DNS/SSRF 拒绝）→
   `dead=1` + `status=failed`；树/列表/Agent 详情均带死链标记。
5. **embedding 一致性**：`embedding_text` 列与向量库 `text` 列写入同一字符串；
   文本顺序 Tags 置前 + Summary 截断，修复 BGE 512 token 截断导致向量缺 tags 的问题。
6. **检索目的**：仅 URL + summary + category + tags；raw_content 只服务 LLM 总结。
7. **5 万上限**：前端弹提示 + 截断前 5 万条，后端兜底同样截断。
8. **删除调试残留日志**（逐条打印抓取正文的 log::info）。
9. 开发阶段，不做旧数据兼容（直接删旧表即可，schema 变更由新库自然生效）。

## 数据表结构（bookmarks）

```sql
CREATE TABLE IF NOT EXISTS bookmarks (
    id             TEXT PRIMARY KEY,
    url            TEXT NOT NULL,
    canonical_url  TEXT NOT NULL UNIQUE,      -- 归一化 URL（去重键）
    title          TEXT,
    browser_folder TEXT,
    added_at       INTEGER,
    source_file    TEXT,
    category       TEXT,                      -- LLM 分类
    summary        TEXT,                      -- LLM 摘要
    tags           TEXT,                      -- LLM 标签（JSON 数组字符串）
    raw_content    TEXT,                      -- 抓取正文（仅用于 LLM 总结）
    embedding_text TEXT,                      -- 实际送入 embedding 的文本（与向量库一致）
    status         TEXT NOT NULL DEFAULT 'pending',  -- pending | ready | failed
    dead           INTEGER NOT NULL DEFAULT 0,       -- 死链标记
    last_error     TEXT,
    revision       INTEGER NOT NULL DEFAULT 1,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
```

## 命令面（Tauri）

- `bookmark_import` / `bookmark_list` / `bookmark_search` / `bookmark_stat` /
  `bookmark_get` / `bookmark_tree`
- 已删除：`bookmark_update`、`bookmark_archive`（UI 无调用点，非核心业务）

## 边界

- Agent 只经 `search_bookmarks` / `get_bookmark` 只读访问（`core/agent/tools`）；
  导入是 UI 行为，不暴露给 Agent。
- 浏览器模式（index.html / index_cdn.html）为独立 JSON 预览实现，与 Tauri 模式完全分离。
