//! Core types shared across the browser: configuration, the document model,
//! command parsing / intent routing, and the backend contract.

pub mod backend;
pub mod bangs;
pub mod config;
pub mod content;
pub mod intent;
pub mod matheval;

pub use backend::Backend;
pub use bangs::{bang_list, bang_search_template, expand_bang};
pub use config::{Config, KeyConfig, SearchConfig, SearchProvider};
pub use content::{Block, Document, DocumentBuilder, Link, Span};
pub use intent::{
    looks_like_query, normalize_url, parse_command, route, search_url, Command, Mode,
    DEFAULT_SEARCH_URL,
};
pub use matheval::eval as math_eval;
