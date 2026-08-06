pub mod db;
pub mod config;
pub mod types;
pub mod agent;
pub mod skill;

mod chat_types;
mod embedding;
mod indexer;
pub(crate) mod model_download;
mod watcher;

pub use chat_types::{ChatMessage, ChatMessageSource, ChatSession, ChatSessionSearchResult};
pub use config::{ConfigStore, IndexerConfig};
pub use db::lance::SearchHit;
pub use db::utils::{call_embedding_query, KbProgress};
pub use indexer::{Indexer, RetrievalIntent, route_intent};
pub use types::{KbIndexResult, KbStatus};
pub use watcher::WatcherService;
