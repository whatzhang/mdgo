# 知识库功能 Rust 迁移方案

## 架构原则

```
index.html (纯 UI 展示 + Embedding/LLM HTTP 调用)
    └── Tauri invoke IPC
          └── Rust 命令层 (全部重业务逻辑)
                ├── commands/knowledge.rs — 索引/检索/状态
                ├── db/lance.rs — LanceDB 向量存储
                └── db/bm25.rs — Tantivy 全文检索
```

- **Rust 负责**：目录扫描、文件读取、文本分块、Embedding API 调用、LanceDB 存储/检索、BM25 索引/检索、RRF 混合融合、配置读写
- **JS 负责**：UI 渲染、调用 Invoke、流式 LLM 聊天、用户交互
- **无需**：知识库 CRUD（当前目录即知识库）、多 LLM 客户端抽象

---

## 一、Rust 新增依赖

```toml
# Cargo.toml 新增
lancedb = "0.20"             # 向量数据库
tantivy = "0.22"             # BM25 全文检索
lindera = "0.32"             # 中文分词
lindera-tantivy = "0.28"     # Tantivy + Lindera 集成
reqwest = { version = "0.12", features = ["json"] }  # HTTP 客户端（调 Embedding API）
tokio = { version = "1", features = ["full"] }        # 异步运行时
notify = { version = "7", features = ["macos_kqueue"] }  # 文件监听（P4）
uuid = { version = "1", features = ["v4"] }           # 块ID 生成
```

---

## 二、Rust 文件结构

```
tauri/src-tauri/src/
├── lib.rs                          ← 注册 knowledge 命令
├── main.rs
├── commands/
│   ├── mod.rs                      ← 加 pub mod knowledge
│   ├── knowledge.rs                ← 知识库核心命令（新增）
│   └── ... (已有)
└── db/
    ├── mod.rs                      ← pub mod lance; pub mod bm25;
    ├── lance.rs                    ← LanceDB 封装（新增）
    └── bm25.rs                     ← Tantivy 封装（新增）
```

---

## 三、Rust 命令清单

### P0 — 基础骨架

| 命令 | 输入 | 输出 | 说明 |
|------|------|------|------|
| `kb_index` | `{ dirPath, embeddingEndpoint, embeddingToken, embeddingModel, embeddingDimension }` | `KbIndexResult { fileCount, chunkCount, vectorCount, indexedAt }` | **核心全流程**：walkdir 扫描 → 读取文件 → 分块 → reqwest 调 Embedding API → LanceDB 存储 |
| `kb_status` | `{ dirPath }` | `KbStatus { fileCount, chunkCount, vectorCount, indexedAt, status }` | 从 LanceDB 读取索引状态 |
| `kb_search` | `{ dirPath, queryVector, topK }` | `Vec<SearchHit { text, docName, chunkIndex, score }>` | LanceDB 向量检索（余弦/HNSW） |
| `kb_clear` | `{ dirPath }` | `void` | 清除目录的索引数据 |

### P1 — BM25 混合检索

| 命令 | 输入 | 输出 | 说明 |
|------|------|------|------|
| `kb_search_hybrid` | `{ dirPath, queryVector, query, topK }` | `Vec<SearchHit>` | 向量 + BM25 双路 → RRF 融合 |

### P2 — 增量索引

| 命令 | 输入 | 输出 | 说明 |
|------|------|------|------|
| `kb_start_watcher` | `{ dirPath }` | `void` | 启动文件监听，变更时自动增量索引 |
| `kb_stop_watcher` | `{ dirPath }` | `void` | 停止监听 |

### P3 — 配置 

| 命令 | 输入 | 输出 | 说明 |
|------|------|------|------|
| `kb_config_read` | `—` | `AppConfig` | 读取持久化配置（tauri-plugin-store） |
| `kb_config_write` | `AppConfig` | `void` | 写入配置 |

---

## 四、Rust 核心数据结构

```rust
// commands/knowledge.rs

#[derive(Debug, Serialize, Deserialize)]
pub struct KbIndexInput {
    pub dir_path: String,
    pub embedding_endpoint: String,
    pub embedding_token: Option<String>,
    pub embedding_model: String,
    pub embedding_dimension: u32,
}

#[derive(Debug, Serialize)]
pub struct KbIndexResult {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
}

#[derive(Debug, Serialize)]
pub struct KbStatus {
    pub file_count: u32,
    pub chunk_count: u32,
    pub vector_count: u32,
    pub indexed_at: u64,
    pub status: String, // "indexed", "indexing", "error"
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub text: String,
    pub doc_name: String,
    pub chunk_index: u32,
    pub score: f32,
}

#[derive(Debug, Serialize)]
pub struct DocumentChunk {
    pub text: String,
    pub doc_name: String,
    pub chunk_index: u32,
}
```

---

## 五、前端（index.html）变更

### 5.1 JS → 纯 UI + invoke 调用

**删除**（已迁移到 Rust）：
- `getVectorDb()` / `saveVectors()` / `searchLocalVectors()` / `clearDirVectors()` / `getDirIndexMeta()`
- `cosineSimilarity()`
- `splitIntoChunks()`
- `readFileContent()` 的纯前端逻辑
- `embedTextsApi()` **保留**（JS 调 HTTP API，或改为 Rust 调）

**保留**（UI + 编排）：
- `sendLlmQuery()` — 流式 LLM 聊天（HTTP SSE）
- `sendRagQuery()` — 编排：embedding → `kb_search` → `streamLlmWithContext`
- `refreshKnowledgeStatus()` — invoke `kb_status` → 更新 UI
- `indexCurrentDir()` — invoke `kb_index` → 更新进度

### 5.2 索引流程（JS → Rust）

```
用户点击「索引当前目录」
  → JS: invoke('kb_index', {
        dirPath: dirHandle.name,
        embeddingEndpoint: EMBEDDING_ENDPOINT,
        embeddingToken: EMBEDDING_TOKEN,
        embeddingModel: EMBEDDING_MODEL,
        embeddingDimension: EMBEDDING_DIMENSION,
      })
  → Rust: 
      1. walkdir 扫描 dirPath（跳过黑名单目录/文件）
      2. 逐文件读取 → 分块（递归分隔符分块）
      3. 分批调 Embedding API（reqwest POST /v1/embeddings）
      4. LanceDB 存储（chunks + vectors）
      5. 保存元数据（fileCount, chunkCount, indexedAt）
  → 返回 KbIndexResult
  → JS: 更新 UI
```

### 5.3 RAG 检索流程（JS → Rust）

```
用户输入问题
  → JS: embedTextsApi([query]) → queryVector
  → JS: invoke('kb_search', { dirPath, queryVector, topK: 10 })
  → Rust: LanceDB.table.search(queryVector).limit(10).execute()
  → 返回 Vec<SearchHit>
  → JS: streamLlmWithContext(text, context, sources)
```

### 5.4 通信方式：SSE 进度回调

`kb_index` 是耗时操作，需要通过 Tauri 事件推送进度：

```
Rust → tauri::Emitter::emit("kb-progress", { percent, message })
JS  → listen("kb-progress", (event) => { 更新进度条 })
```

---

## 六、实施阶段

| 阶段 | 内容 | 涉及文件 |
|------|------|---------|
| **Phase 1** (骨架) | Cargo.toml 依赖 + `db/mod.rs` + `db/lance.rs` 骨架 + `commands/knowledge.rs` 命令声明 + `lib.rs` 注册 | 4 个文件 |
| **Phase 2** (索引) | `kb_index` 完整实现：walkdir 扫描 → 读取 → 分块 → Embedding → LanceDB 存储 + SSE 进度 | `knowledge.rs`, `lance.rs` |
| **Phase 3** (检索) | `kb_search` + `kb_status` + `kb_clear` | `knowledge.rs`, `lance.rs` |
| **Phase 4** (JS 适配) | 删除 JS 旧逻辑，改为 invoke 调用 | `index.html` |
| **Phase 5** (BM25) | `db/bm25.rs` + `kb_search_hybrid` + RRF | `bm25.rs`, `knowledge.rs` |
| **Phase 6** (增量) | `kb_start_watcher` + `kb_stop_watcher` | `knowledge.rs`, `fs_watcher.rs` |

---

## 七、与现有实现的关键差异

| 项目 | 当前 (JS) | 迁移后 (Rust) |
|------|-----------|--------------|
| 文本分块 | `splitIntoChunks(text, 1000, 200)` — 纯 JS | `RecursiveCharacterTextSplitter::new(1000, 200)` — Rust |
| 向量存储 | IndexedDB | LanceDB (HNSW 索引，O(log n) 检索) |
| 向量检索 | 全量余弦相似度 O(n) | LanceDB 内置近似搜索 |
| Embedding 调用 | `fetch()` 直接调 HTTP API | `reqwest` 调 HTTP API（Rust 侧） |
| 文件扫描 | File System Access API (浏览器) | `walkdir` (Rust 原生) |
| 配置持久化 | localStorage | `tauri-plugin-store` + `kb_config_read/write` |
| 进度反馈 | JS 手动计算 | Tauri Event SSE 推送 |

---

## 八、注意事项

1. **Embedding Dimension**：不同模型维度不同（bge-m3=1024, text-embedding-3-small=1536），需要在 `kb_index` 输入中指定，LanceDB 建表时固定
2. **LanceDB 存储路径**：`{app_data_dir}/lancedb/{dir_hash}/` — 按目录路径 hash 区分不同索引
3. **支持的文件类型**：与现有 `KB_SUPPORTED_EXTS` 保持一致（50+ 种代码/文档/配置格式）
4. **`.mdgoignore`**：Dify 风格忽略文件，后续支持，当前复用项目已有的 `DIR_RULES/FILE_RULES` 黑名单配置
5. **reqwest 客户端**：设 timeout=30s、连接池、重试 3 次，与前端 `embedTextsApi` 行为一致
