//! Engine-free logic shared across the browser: the document model read mode
//! renders, bangs, quick maths, and telling a search query from an address.

pub mod bangs;
pub mod content;
pub mod intent;
pub mod matheval;

pub use bangs::{
    bang_search_template, expand_bang_with, find_bang, kagi_bang, normalize_custom_bang,
    search_bangs, BangInfo, BangOverrides, BangSource, KAGI_VERSION,
};
pub use content::{Block, Document, DocumentBuilder, Link, Span};
pub use intent::{looks_like_query, normalize_url, search_url, DEFAULT_SEARCH_URL};
pub use matheval::eval as math_eval;
