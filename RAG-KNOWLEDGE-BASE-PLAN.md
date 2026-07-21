# mdgo 知识库 + RAG 系统 — 最终实施方案

> 基于 page-assist 核心能力分析，结合商用知识库（Dify/MaxKB）功能对标，制定的纯 Tauri 桌面端实施方案。
> 全部重业务逻辑迁移至 Rust，前端 JS 仅负责 UI 展示与 LLM 流式聊天。

---

## 一、架构原则

```
index.html (纯 UI 展示 + LLM SSE 流式聊天)
    │  Tauri invoke IPC
    ▼
Rust 命令层 (全部重业务逻辑)
    ├── commands/knowledge.rs — 索引/检索/状态
    ├── db/lance.rs           — LanceDB 向量存储
    ├── db/bm25.rs            — Tantivy BM25 全文检索
    └── commands/config.rs    — 配置持久化
```

**职责划分**：

| 层级     | 负责                                                                                                                                               |
| -------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Rust** | walkdir 目录扫描 → 文件读取 → 分块 → reqwest 调 Embedding API → LanceDB 向量存储 → Tantivy BM25 索引 → RRF 混合检索 → notify 文件监听 → 配置持久化 |
| **JS**   | UI 展示与交互 → invoke Rust 命令 → SSE 流式 LLM 聊天 → RAG 编排（embed query → invoke search → stream LLM)                                         |

---

## 二、系统架构总览

```
┌──────────────────────────────────────────────────────────────────┐
│                   前端 UI (index.html — 纯展示)                    │
│  ┌──────────────┐  ┌──────────────────┐  ┌───────────────────┐  │
│  │ 设置向导      │  │ 聊天面板          │  │ 知识库管理        │  │
│  │ · LLM/Embed  │  │ · 流式 Markdown   │  │ · 当前目录状态    │  │
│  │ · 模型管理    │  │ · 引用卡片+溯源   │  │ · 索引进度       │  │
│  │ · 连接测试    │  │ · 普通对话/RAG    │  │ · 重新索引       │  │
│  │              │  │                   │  │                   │  │
│  └──────────────┘  └──────────────────┘  └───────────────────┘  │
│                             │ Tauri invoke                       │
└─────────────────────────────┬────────────────────────────────────┘
                              │ IPC
┌──────────────────────────────────────────────────────────────────┐
│                     Rust 命令层 (全部重逻辑)                       │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │ commands/knowledge.rs                                    │    │
│  │  ├─ kb_index    — 全流程索引：walkdir → 读取 → 分块      │    │
│  │  │                 → reqwest Embedding → LanceDB+Tantivy │    │
│  │  ├─ kb_search   — 向量检索 (LanceDB)                     │    │
│  │  ├─ kb_search_hybrid — 混合检索 (向量×BM25 → RRF)       │    │
│  │  ├─ kb_status   — 索引状态查询                           │    │
│  │  ├─ kb_clear    — 清除索引                               │    │
│  │  └─ kb_reindex  — 重建索引 (clear + index)               │    │
│  └──────────────────────────────────────────────────────────┘    │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │ db/lance.rs  — LanceDB 封装                              │    │
│  │  ├─ open_table / create_table                             │    │
│  │  ├─ add_chunks (向量写入)                                 │    │
│  │  ├─ search_vectors (余弦/HNSW)                           │    │
│  │  └─ delete / count                                       │    │
│  │                                                          │    │
│  │ db/bm25.rs   — Tantivy BM25 封装                         │    │
│  │  ├─ create_index / open_index                            │    │
│  │  ├─ index_documents (全文写入)                           │    │
│  │  └─ search (关键词检索)                                  │    │
│  └──────────────────────────────────────────────────────────┘    │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │ commands/config.rs    — 配置读写                          │    │
│  │  ├─ kb_config_read                                       │    │
│  │  └─ kb_config_write                                      │    │
│  └──────────────────────────────────────────────────────────┘    │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │ commands/fs_watcher.rs — 文件变更监听 (notify)            │    │
│  │  ├─ kb_start_watcher  — 启动监听 + 自动增量索引           │    │
│  │  └─ kb_stop_watcher   — 停止监听                         │    │
│  └──────────────────────────────────────────────────────────┘    │
└─────────────────────────────┬────────────────────────────────────┘
                              │ HTTP (reqwest — Rust 侧调用)
┌──────────────────────────────────────────────────────────────────┐
│                    LLM / Embedding 服务 (外部)                    │
│  · Ollama / LM Studio / vLLM / Xinference                       │
│  · 云端 OpenAI / Claude / Gemini                                │
│  · Embedding: BGE / text-embedding-3 / Ollama 嵌入               │
└──────────────────────────────────────────────────────────────────┘
```

---

## 三、完整文件结构

### 3.1 Rust 侧 (tauri/src-tauri/) — 新增/修改

```
tauri/src-tauri/
├── Cargo.toml                        ← 新增依赖: lancedb, tantivy, lindera,
│                                         reqwest, notify, uuid
├── src/
│   ├── main.rs
│   ├── lib.rs                        ← 注册 knowledge, config 命令
│   ├── commands/
│   │   ├── mod.rs                    ← 注册 knowledge, config 模块
│   │   ├── knowledge.rs              ← 核心：索引/检索/状态命令
│   │   └── config.rs                 ← 配置读写
│   └── db/
│       ├── mod.rs
│       ├── lance.rs                  ← LanceDB 封装
│       └── bm25.rs                   ← Tantivy BM25 封装
```

### 3.2 前端 TS 侧 (tauri/src/) — 简化

```
tauri/src/
├── types/
│   └── index.ts                     ← 共用类型定义 (与 Rust 侧 struct 对应)
│
├── llm/
│   └── chat.ts                      ← SSE 流式聊天 (保留现有实现)
│
└── rag/
    └── chain.ts                     ← RAG 编排 (embed → invoke → stream)

index.html                            ← 纯 UI，删除全部 IndexedDB/分块/检索逻辑
```

---

## 四、Rust 核心数据流

### 4.1 索引流程 (`kb_index`)

```
用户点击「索引当前目录」
  → JS: invoke('kb_index', {
        dirPath: "/path/to/dir",
        embeddingEndpoint: "http://host:11434/v1/embeddings",
        embeddingToken: null,
        embeddingModel: "nomic-embed-text",
        embeddingDimension: 768,
      })
  ──────────────────────────────────────────────
  Rust:
    1. walkdir 递归扫描 dirPath
       · 跳过黑名单目录/文件 (isSkipDir/isSkipFile 逻辑)
       · 筛选 KB_SUPPORTED_EXTS (50+ 种格式)
    2. 逐文件读取 → RecursiveCharacterTextSplitter 分块
    3. 分批 Bulk Embedding:
       · for batch in chunks.chunks(20):
         · reqwest POST /v1/embeddings → Vec<Vec<f32>>
    4. LanceDB: create_table + add(chunks, vectors)
    5. Tantivy: create_index + index_documents (全文倒排)
    6. 持久化索引元数据 (fileCount, chunkCount, indexedAt)
  ──────────────────────────────────────────────
  → 返回 KbIndexResult { fileCount, chunkCount, vectorCount, indexedAt }
  → JS: 更新 UI 状态

进度推送:
  Rust → tauri::Emitter::emit("kb-progress", KbProgress { percent, message })
  JS  → listen("kb-progress", updateProgressBar)
```

### 4.2 检索流程 (`kb_search_hybrid`)

```
用户输入问题 (RAG 模式)
  → JS: embedTextsApi([query]) → queryVector
  → JS: invoke('kb_search_hybrid', {
        dirPath: "/path/to/dir",
        queryVector: [0.1, 0.2, ...],
        query: "什么是 RAG 技术？",   // 用于 BM25
        topK: 10,
      })
  ──────────────────────────────────────────────
  Rust (并行):
    ┌─ LanceDB 向量检索 ── top_k=20
    │   table.search(queryVector).limit(20).execute()
    │   → Vec<SearchHit { text, docName, score }>
    │
    └─ Tantivy BM25 检索 ── top_k=20
        index.search(query, LinderaAnalyzer)
        → Vec<SearchHit { text, docName, score }>
    │
    ▼ RRF 融合
    fused = rrf_merge(vec_hits, bm25_hits, k=60)
    → 取 topK
  ──────────────────────────────────────────────
  → 返回 Vec<SearchHit>
  → JS: 构建 context → streamLlmWithContext(text, context, sources)
```

### 4.3 状态查询 (`kb_status`)

```
用户进入知识库面板
  → JS: invoke('kb_status', { dirPath })
  → Rust: 从 LanceDB 元数据表读取
  → 返回 KbStatus { fileCount, chunkCount, vectorCount, indexedAt, status }
  → JS: 更新 UI
```

---

## 五、Rust 核心数据结构

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
    pub status: String, // "indexed" | "indexing" | "error"
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub text: String,
    pub doc_name: String,
    pub chunk_index: u32,
    pub score: f32,
}

#[derive(Debug, Serialize)]
pub struct KbProgress {
    pub percent: u8,     // 0-100
    pub message: String,
}
```

```rust
// db/lance.rs

pub struct LanceStore {
    uri: String,           // {app_data_dir}/lancedb/{dir_hash}/
}

impl LanceStore {
    pub async fn open(uri: &str) -> Result<Self>;
    pub async fn create_table(&self, dimension: u32) -> Result<Table>;
    pub async fn add_chunks(&self, chunks: &[DocumentChunk], vectors: &[Vec<f32>]) -> Result<()>;
    pub async fn search_vectors(&self, query: &[f32], top_k: u32) -> Result<Vec<SearchHit>>;
    pub async fn count_vectors(&self) -> Result<u64>;
    pub async fn clear(&self) -> Result<()>;
    pub async fn delete_document(&self, doc_name: &str) -> Result<()>;
}
```

```rust
// db/bm25.rs

pub struct Bm25Index {
    index_path: String,    // {app_data_dir}/tantivy/{dir_hash}/
    index: Index,
}

impl Bm25Index {
    pub fn open(path: &str) -> Result<Self>;
    pub fn create(path: &str) -> Result<Self>;
    pub fn add_documents(&self, chunks: &[DocumentChunk]) -> Result<()>;
    pub fn search(&self, query: &str, top_k: u32) -> Result<Vec<SearchHit>>;
    pub fn clear(&self) -> Result<()>;
}
```

---

## 六、前端 JS 变更清单

### 6.1 删除（已迁移到 Rust）

| 模块          | 函数/变量                                      | 删除原因                                      |
| ------------- | ---------------------------------------------- | --------------------------------------------- |
| **向量存储**  | `getVectorDb()`                                | → Rust LanceDB                                |
|               | `saveVectors()`                                | → `lance.rs add_chunks`                       |
|               | `searchLocalVectors()`                         | → `kb_search_hybrid`                          |
|               | `clearDirVectors()`                            | → `lance.rs clear`                            |
|               | `getDirIndexMeta()`                            | → `kb_status`                                 |
|               | `cosineSimilarity()`                           | → LanceDB 内置                                |
| **文件处理**  | `readFileContent()`                            | → Rust `walkdir` + `fs::read_to_string`       |
|               | `splitIntoChunks()`                            | → Rust `RecursiveCharacterTextSplitter`       |
|               | `KB_SUPPORTED_EXTS`                            | → Rust 常量                                   |
| **索引**      | `indexCurrentDir()` 中的 JS 扫描/分块/嵌入     | → Rust `kb_index`                             |
|               | `refreshKnowledgeStatus()` 中的 IndexedDB 读取 | → Rust `kb_status`                            |
| **Embedding** | `embedTextsApi()`                              | → Rust `reqwest` 调用 (JS 仅 invoke 时传参数) |

### 6.2 保留（UI + 编排）

| 模块         | 函数                       | 用途                                                 |
| ------------ | -------------------------- | ---------------------------------------------------- |
| **LLM 聊天** | `sendLlmQuery()`           | SSE 流式聊天 (HTTP 直连)                             |
|              | `streamLlmWithContext()`   | RAG 上下文+流式回答                                  |
| **RAG 编排** | `sendRagQuery()`           | embed → invoke `kb_search_hybrid` → stream LLM       |
| **UI 更新**  | `refreshKnowledgeStatus()` | invoke `kb_status` → 更新 DOM                        |
|              | `indexCurrentDir()`        | invoke `kb_index` + 监听 `kb-progress` 事件 → 进度条 |

### 6.3 索引流程（JS → Rust）

```
用户点击「索引当前目录」
  → JS: invoke('kb_index', {
        dirPath, embeddingEndpoint, embeddingToken, embeddingModel, embeddingDimension
      })
  → Rust 处理全部索引逻辑
  → Rust 推送 Tauri Event: "kb-progress" { percent, message }
  → JS: listen("kb-progress", (e) => {
        progressBar.style.width = e.payload.percent + '%';
        progressText.textContent = e.payload.message;
      })
  → Rust 完成 → 返回 KbIndexResult
  → JS: refreshKnowledgeStatus()
```

---

## 七、检索链路设计

```
用户问题
    │
    ▼
[Step 1] JS: Embedding API 向量化问题 (HTTP)
    │   返回 queryVector (Vec<f32>)
    │
    ▼
[Step 2] JS: invoke('kb_search_hybrid', { dirPath, queryVector, query, topK })
    │
    ▼
[Step 3] Rust: 双路并行检索
    ├── LanceDB 向量检索 (余弦/HNSW, top_k=20)
    └── Tantivy BM25 关键词检索 (Lindera 分词, top_k=20)
    │
    ▼
[Step 4] Rust: RRF 融合 (k=60)
    │   score = 1/(60 + rank_vec) + 1/(60 + rank_bm25)
    │   取 topK
    │
    ▼
[Step 5] Rust: 返回 Vec<SearchHit>
    │
    ▼
[Step 6] JS: 构建上下文 → SSE 流式 LLM 回答
    │   来源：显示引用卡片 (docName + score)
    │   分级：最高分 < 阈值时提示"未找到相关内容"
    │
    ▼
渲染 Markdown + 引用溯源
```

---

## 八、实施路线图

| 阶段           | 内容                                                                                           | 交付物                       | 涉及文件                                                                                                     |
| -------------- | ---------------------------------------------------------------------------------------------- | ---------------------------- | ------------------------------------------------------------------------------------------------------------ |
| **P1 骨架**    | Cargo 依赖 + db/lance.rs 骨架 + db/bm25.rs 骨架 + commands/knowledge.rs 命令声明 + lib.rs 注册 | 编译通过，命令可调用         | `Cargo.toml`, `lib.rs`, `commands/mod.rs`, `commands/knowledge.rs`, `db/mod.rs`, `db/lance.rs`, `db/bm25.rs` |
| **P2 索引**    | `kb_index` 全流程：walkdir 扫描 → 读取 → 分块 → reqwest Embedding → LanceDB 存储 + SSE 进度    | 可在 Rust 侧完成目录索引     | `knowledge.rs`, `lance.rs`                                                                                   |
| **P3 检索**    | `kb_search` + `kb_status` + `kb_clear`                                                         | 可查询索引状态和执行向量检索 | `knowledge.rs`, `lance.rs`                                                                                   |
| **P4 JS 适配** | 删除 index.html 中所有旧逻辑（IndexedDB/文件读取/分块/检索），改为 invoke 调用 + RAG 编排      | 完整知识库功能可用           | `index.html`                                                                                                 |
| **P5 BM25**    | `db/bm25.rs` 完整实现 + `kb_search_hybrid` + RRF 融合                                          | 混合检索可用                 | `bm25.rs`, `knowledge.rs`                                                                                    |
| **P6 增强**    | 配置持久化 (`config.rs`) + 文件监听 (`fs_watcher.rs`)                                          | 配置保存 + 增量索引          | `config.rs`, `fs_watcher.rs`                                                                                 |

---

## 九、与现有实现的差异对照

| 项目           | 当前 (JS IndexedDB)                        | 迁移后 (Rust LanceDB)                     |
| -------------- | ------------------------------------------ | ----------------------------------------- |
| 文本分块       | `splitIntoChunks(text, 1000, 200)` — 纯 JS | `RecursiveCharacterTextSplitter` — Rust   |
| 向量存储       | IndexedDB (JS)                             | LanceDB (HNSW 索引, O(log n) 检索)        |
| 向量检索       | 全量余弦相似度 O(n)                        | LanceDB 内置近似搜索                      |
| Embedding 调用 | `fetch()` JS 直连 HTTP                     | `reqwest` Rust 侧 HTTP (统一在索引流程内) |
| 文件扫描       | File System Access API (浏览器)            | `walkdir` (Rust 原生, 多线程)             |
| BM25           | ❌ 不存在                                  | Tantivy + Lindera 中文分词                |
| 混合检索       | ❌ 不存在                                  | 向量 + BM25 → RRF 融合                    |
| 进度反馈       | JS 手动计算                                | Tauri Event SSE 推送                      |
| 配置持久化     | localStorage                               | `tauri-plugin-store` + `config.rs`        |
| 增量同步       | ❌ 不存在                                  | `notify` crate 文件监听                   |

---

## 十、依赖清单

### 10.1 Rust 侧 Cargo.toml

```toml
[dependencies]
# 已有
tauri = { version = "2", features = ["protocol-asset"] }
tauri-plugin-dialog = "2"
tauri-plugin-fs = "2"
tauri-plugin-store = "2"
open = "5"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
walkdir = "2"
mime_guess = "2"
base64 = "0.22"
sysinfo = "0.39.5"
arboard = "3"

# 新增: 向量数据库 + 全文检索 + HTTP + 文件监听
lancedb = "0.20"                    # 向量数据库
tantivy = "0.22"                    # BM25 全文检索
lindera = "0.32"                    # 中文分词
lindera-tantivy = "0.28"           # Lindera → Tantivy 桥接
reqwest = { version = "0.12", features = ["json"] }  # Embedding HTTP 客户端
tokio = { version = "1", features = ["full"] }
notify = { version = "7", features = ["macos_kqueue"] }  # 文件监听
uuid = { version = "1", features = ["v4"] }           # 块 ID 生成
```

### 10.2 前端 package.json

```json
{
  "dependencies": {
    "@tauri-apps/api": "^2",
    "@tauri-apps/plugin-dialog": "^2",
    "@tauri-apps/plugin-fs": "^2",
    "@tauri-apps/plugin-store": "^2"
  }
}
```

（前端依赖大幅简化，无需 pdfjs-dist/mammoth/langchain/tesseract.js/zustand/react 等）

---

## 十一、关键设计决策

| 决策项             | 选择                                 | 理由                                           |
| ------------------ | ------------------------------------ | ---------------------------------------------- |
| **向量数据库**     | LanceDB                              | 嵌入式、高性能列式存储、Rust SDK、无服务器     |
| **关键词检索**     | Tantivy + Lindera                    | 成熟中文分词、Rust 生态最佳 BM25 实现          |
| **混合检索融合**   | RRF (Reciprocal Rank Fusion)         | 简单高效，无参数调优，业界标准方案             |
| **Embedding 调用** | Rust reqwest                         | 所有重逻辑统一在 Rust 侧，JS 无 Embedding 逻辑 |
| **LLM 流式**       | JS SSE (保持现状)                    | 无需多抽象层，JS 直连 HTTP 效率最高            |
| **知识库模型**     | 当前目录即知识库                     | 无需 CRUD，打开目录即用                        |
| **对话历史**       | IndexedDB (保持现状)                 | 简单持久化，无需 SQLite 复杂性                 |
| **进度反馈**       | Tauri Event                          | Rust → JS 实时推送索引进度                     |
| **存储目录**       | `{app_data_dir}/lancedb/{dir_hash}/` | 按目录路径 hash 隔离不同知识库索引             |

---

_文档版本: v3.0 · 重业务逻辑全量迁移至 Rust，JS 仅 UI + LLM 流式_
