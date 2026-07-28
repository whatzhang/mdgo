pub mod db;
pub mod config;
pub mod types;

mod chat_types;
mod embedding;
mod indexer;
mod watcher;

pub use chat_types::{ChatMessage, ChatMessageSource, ChatSession, ChatSessionSearchResult};
pub use config::{ConfigStore, IndexerConfig};
pub use db::lance::SearchHit;
pub use db::utils::{call_embedding_query, KbProgress};
pub use indexer::Indexer;
pub use types::{KbIndexResult, KbStatus};
pub use watcher::WatcherService;
