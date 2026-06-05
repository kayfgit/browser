//! Core types shared across the browser: configuration, the document model,
//! command parsing / intent routing, and the backend contract.

pub mod backend;
pub mod config;
pub mod content;
pub mod intent;

pub use backend::Backend;
pub use config::{Config, KeyConfig, SearchConfig, SearchProvider};
pub use content::{Block, Document, DocumentBuilder, Link, Span};
pub use intent::{
    looks_like_query, normalize_url, parse_command, route, search_url, Command, Mode,
    DEFAULT_SEARCH_URL,
};
