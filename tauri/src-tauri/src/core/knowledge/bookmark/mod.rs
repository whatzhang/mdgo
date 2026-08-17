//! Bookmark 知识资产实体层（mdgo 第一个非文件 Knowledge Asset）。
//!
//! 数据只依赖**一张 SQLite 表** `bookmarks`（知识库级 `.mdgo/mdgo.db`，per-library）：
//! - 无独立任务队列表：后台 Worker 直接按 `status='pending'` 认领驱动流水线；
//! - 无 FTS 倒排表：检索用 LIKE 直接扫 `bookmarks`（检索目的仅为 URL/摘要/分类/标签）；
//! - 向量存 LanceDB `bookmark_vectors` 独立表（见 `vector.rs`），与 SQLite 解耦。
//!
//! 状态为简单三态（非状态机）：
//! - `pending`：已入库，等待后台处理（抓取 → LLM 总结 → embedding）
//! - `ready`：处理完成（抓取+总结+向量均成功）
//! - `failed`：抓取失败（死链）或 LLM 总结失败，**终态**，不再入向量库
//!
//! 边界：Bookmark 是 Knowledge Asset，不是管理对象；Agent 只负责理解和查询
//! （`search_bookmarks` / `get_bookmark`），导入由 UI 完成。

pub mod enrichment;
pub mod importer;
pub mod repository;
pub mod search;
pub mod tree;
pub mod vector;

use rusqlite::Connection;

// ─── 状态（简单三态） ───
pub const STATUS_PENDING: &str = "pending"; // 已入库，等待后台处理
pub const STATUS_READY: &str = "ready";     // 抓取+总结+向量全部完成
pub const STATUS_FAILED: &str = "failed";   // 死链或总结失败（终态）

// ─── 数据模型 ───

/// 书签实体（与表结构一一对应，供命令层/工具层序列化）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Bookmark {
    pub id: String,
    pub url: String,
    pub canonical_url: Option<String>,
    pub title: Option<String>,
    pub browser_folder: Option<String>,
    pub added_at: Option<i64>,
    pub source_file: Option<String>,
    pub category: Option<String>,
    pub summary: Option<String>,
    pub tags: Option<String>,
    pub raw_content: Option<String>,
    pub embedding_text: Option<String>,
    pub status: String,
    pub dead: bool,
    pub last_error: Option<String>,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 导入条目（前端 `parseBookmarkHtml` 解析后经命令传入的结构化 JSON）
#[derive(Debug, Clone, serde::Deserialize)]
pub struct BookmarkEntry {
    pub url: String,
    pub title: Option<String>,
    pub folder: Option<String>,
    pub added_at: Option<i64>,
}

/// 导入结果统计（命令返回给 UI 提示）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BookmarkImportStats {
    pub inserted: usize,
    pub skipped: usize,
    pub failed: usize,
    pub total: usize,
}

/// 检索命中（工具返回的精简字段；检索目的：URL + 摘要 + 分类 + 标签）
#[derive(Debug, Clone, serde::Serialize)]
pub struct BookmarkSearchHit {
    pub id: String,
    pub title: Option<String>,
    pub url: String,
    pub summary: Option<String>,
    pub tags: Option<String>,
    pub category: Option<String>,
    pub status: String,
    pub dead: bool,
}

/// 书签数量统计（UI 统计卡）
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BookmarkStats {
    pub total: i64,
    pub ready: i64,
    pub pending: i64,
    pub failed: i64,
    pub dead: i64,
}

/// BookmarkStore：单连接（WAL），AppState 层以 `Arc<Mutex<BookmarkStore>>` 共享。
pub struct BookmarkStore {
    conn: Connection,
    dir_path: String,
}

impl BookmarkStore {
    /// 打开知识库级数据库（`{dir}/.mdgo/mdgo.db`），初始化 bookmark 表。
    pub fn open_for_dir(dir_path: &str, db_path: impl Into<std::path::PathBuf>) -> Result<Self, String> {
        let db_path = db_path.into();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建书签数据目录失败: {}", e))?;
        }
        let conn = Connection::open(&db_path).map_err(|e| format!("打开书签数据库失败: {}", e))?;
        crate::core::db::pool::apply_pragmas(&conn)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn,
            dir_path: dir_path.to_string(),
        })
    }

    /// 幂等建表（仅一张 `bookmarks` 表；开发阶段不迁移旧结构）
    fn init_schema(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bookmarks (
                id             TEXT PRIMARY KEY,
                url            TEXT NOT NULL,
                canonical_url  TEXT NOT NULL UNIQUE,
                title          TEXT,
                browser_folder TEXT,
                added_at       INTEGER,
                source_file    TEXT,
                category       TEXT,
                summary        TEXT,
                tags           TEXT,
                raw_content    TEXT,
                embedding_text TEXT,
                status         TEXT NOT NULL DEFAULT 'pending',
                dead           INTEGER NOT NULL DEFAULT 0,
                last_error     TEXT,
                revision       INTEGER NOT NULL DEFAULT 1,
                created_at     INTEGER NOT NULL,
                updated_at     INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_bookmarks_status ON bookmarks(status);
            CREATE INDEX IF NOT EXISTS idx_bookmarks_folder ON bookmarks(browser_folder);
            CREATE INDEX IF NOT EXISTS idx_bookmarks_category ON bookmarks(category);
            ",
        )
        .map_err(|e| format!("初始化书签数据表失败: {}", e))
    }

    /// 当前实例绑定的知识库目录
    pub fn dir_path(&self) -> &str {
        &self.dir_path
    }

    /// 行 → Bookmark
    fn row_to_bookmark(row: &rusqlite::Row<'_>) -> rusqlite::Result<Bookmark> {
        Ok(Bookmark {
            id: row.get(0)?,
            url: row.get(1)?,
            canonical_url: row.get(2)?,
            title: row.get(3)?,
            browser_folder: row.get(4)?,
            added_at: row.get(5)?,
            source_file: row.get(6)?,
            category: row.get(7)?,
            summary: row.get(8)?,
            tags: row.get(9)?,
            raw_content: row.get(10)?,
            embedding_text: row.get(11)?,
            status: row.get(12)?,
            dead: row.get::<_, i64>(13)? != 0,
            last_error: row.get(14)?,
            revision: row.get(15)?,
            created_at: row.get(16)?,
            updated_at: row.get(17)?,
        })
    }

    /// SELECT 列清单（与 `row_to_bookmark` 的列序一致）
    const COLS: &'static str = "id,url,canonical_url,title,browser_folder,added_at,source_file,\
        category,summary,tags,raw_content,embedding_text,status,dead,last_error,revision,\
        created_at,updated_at";

    /// 当前毫秒时间戳
    pub fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// 生成新 id（bm_<毫秒>_<随机后缀>）
    pub fn new_id() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("bm_{}_{}", Self::now_ms(), nanos % 1_000_000)
    }
}

/// URL 规范化 + 协议白名单（http/https，拒绝 javascript:/data:/file: 等）。
/// 幂等去重键：host 小写、省略默认端口、去 fragment、去尾斜杠（保留根路径 `/`）。
pub fn normalize_url(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("书签 URL 为空".to_string());
    }
    let sl = s.to_lowercase();
    let scheme = if sl.starts_with("https://") {
        "https"
    } else if sl.starts_with("http://") {
        "http"
    } else {
        let shown: String = s.chars().take(32).collect();
        return Err(format!("不支持的 URL 协议（仅允许 http/https）: {}", shown));
    };
    if s.len() <= scheme.len() + 3 {
        return Err("URL 缺少主机名".to_string());
    }
    let prefix_len = scheme.len() + 3; // "://"
    let after = &s[prefix_len..];
    // 分离 authority(host[:port]) 与 path/query/fragment
    let cut = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority_raw = &after[..cut];
    let rest_raw = &after[cut..];
    if authority_raw.is_empty() {
        return Err("URL 缺少主机名".to_string());
    }
    // host 小写 + 省略默认端口
    let host = strip_default_port(&authority_raw.to_lowercase(), scheme);
    // 去掉 fragment（fragment 不影响目标内容，避免 `#a` 与 `#b` 重复入库）
    let rest = match rest_raw.find('#') {
        Some(i) => &rest_raw[..i],
        None => rest_raw,
    };
    let base = format!("{}://{}", scheme, host);
    let mut out = format!("{}{}", base, rest);
    // 去尾斜杠（幂等去重键；`scheme://host` 根部不带多余 `/`）
    while out.len() > base.len() && out.ends_with('/') {
        out.pop();
    }
    Ok(out)
}

/// 去掉 scheme 的默认端口（http:80 / https:443），其它端口保留。
fn strip_default_port(auth: &str, scheme: &str) -> String {
    // 含 '[' 视为 IPv6（端口带括号无歧义），仅当 rfind(':') 后是纯数字才当端口
    if auth.contains('[') {
        return auth.to_string();
    }
    if let Some(idx) = auth.rfind(':') {
        let port = &auth[idx + 1..];
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            let default = if scheme == "https" { "443" } else { "80" };
            if port == default {
                return auth[..idx].to_string();
            }
        }
    }
    auth.to_string()
}

/// LIKE 通配符转义（`%`/`_`/`\`），配合 `ESCAPE '\'` 使用，防用户输入放大匹配范围。
pub fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp() -> (tempfile::TempDir, BookmarkStore) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let store = BookmarkStore::open_for_dir(
            dir.path().to_str().unwrap(),
            dir.path().join("test.db"),
        )
        .expect("打开测试库失败");
        (dir, store)
    }

    #[test]
    fn normalize_url_rejects_bad_protocols() {
        assert!(normalize_url("javascript:alert(1)").is_err());
        assert!(normalize_url("data:text/html,<b>x</b>").is_err());
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert_eq!(normalize_url("https://a.com/").unwrap(), "https://a.com");
        assert_eq!(normalize_url("http://b.com///").unwrap(), "http://b.com");
    }

    #[test]
    fn normalize_url_host_lowercase_and_default_port_and_fragment() {
        // host 小写（不同大小写归一为同一去重键）
        assert_eq!(normalize_url("https://A.COM/path").unwrap(), "https://a.com/path");
        // 省略默认端口
        assert_eq!(normalize_url("https://a.com:443/x").unwrap(), "https://a.com/x");
        assert_eq!(normalize_url("http://a.com:80/x").unwrap(), "http://a.com/x");
        // 保留非默认端口
        assert_eq!(normalize_url("http://a.com:8080/x").unwrap(), "http://a.com:8080/x");
        // 去 fragment（`#锚` 不改变目标内容，避免重复入库）
        assert_eq!(normalize_url("https://a.com/x#sec").unwrap(), "https://a.com/x");
        // 含 query 时去尾斜杠不破坏 query
        assert_eq!(normalize_url("https://a.com/x/?q=1#t").unwrap(), "https://a.com/x/?q=1");
    }

    #[test]
    fn store_init_and_insert() {
        let (_dir, store) = open_temp();
        let b = Bookmark {
            id: "bm_1".into(),
            url: "https://example.com".into(),
            canonical_url: Some("https://example.com".into()),
            title: Some("Example".into()),
            browser_folder: Some("AI/Agent".into()),
            added_at: Some(1),
            source_file: Some("bookmarks.html".into()),
            category: None,
            summary: None,
            tags: None,
            raw_content: None,
            embedding_text: None,
            status: STATUS_PENDING.into(),
            dead: false,
            last_error: None,
            revision: 1,
            created_at: 1,
            updated_at: 1,
        };
        store.insert(&b).unwrap();
        let got = store.get("bm_1").unwrap().expect("存在");
        assert_eq!(got.title.as_deref(), Some("Example"));
        assert_eq!(got.status, STATUS_PENDING);
        // 重复插入 → Err（主键约束）
        assert!(store.insert(&b).is_err());
    }
}
