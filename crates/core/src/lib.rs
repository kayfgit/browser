//! Core types shared across the browser: configuration, the document model,
//! command parsing / intent routing, and the backend contract.

pub mod backend;
pub mod config;
pub mod content;
pub mod intent;

pub use backend::Backend;
pub use config::{Config, KeyConfig, SearchConfig, SearchProvider};
pub use content::{Block, Document, DocumentBuilder, Link, Span};
pub use intent::{normalize_url, parse_command, route, Command, Mode};
