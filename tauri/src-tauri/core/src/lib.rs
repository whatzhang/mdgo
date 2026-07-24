pub mod db;
pub mod config;
pub mod types;

mod chat_types;
mod embedding;
mod indexer;
mod watcher;

pub use chat_types::{ChatMessage, ChatMessageSource, ChatSession, ChatSessionSearchResult};
pub use config::{ConfigStore, IndexerConfig};
pub use db::bm25::Bm25Index;
pub use db::lance::{DocumentChunk, LanceStore, SearchHit};
pub use db::utils::{
    build_document_chunks, get_bm25_dir, get_chat_bm25_dir, get_data_dir, get_model_dir,
    call_embedding, split_text, IgnoreMatcher, KbProgress, LOCAL_EMBEDDING_DIMENSION,
};
pub use embedding::call_embedding_parallel;
pub use indexer::Indexer;
pub use types::{IndexMeta, KbIndexResult, KbStatus};
pub use watcher::WatcherService;
