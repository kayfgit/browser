//! Session persistence: save the open tabs + UI state on exit and restore them
//! on the next launch, so reopening the browser comes back exactly as it was.
//!
//! Stored as TOML next to the config dir. Only restorable tabs are saved — web
//! pages (open/nojs/research), engine-free read tabs (re-fetched), and terminals
//! (reopened fresh). Internal `browser://` pages and the `:error(s)` log are
//! session-specific and skipped.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A persisted browsing session.
#[derive(Serialize, Deserialize)]
pub struct Session {
    pub zoom: f64,
    pub nojs: bool,
    pub search_template: String,
    pub term_command: Vec<String>,
    /// Index of the focused tab within `tabs`.
    pub active: usize,
    /// Visited URLs (most-recent first) for command-bar autocomplete. Must stay
    /// before the `window`/`tabs` tables (TOML: values precede tables).
    #[serde(default)]
    pub history: Vec<String>,
    /// Last window geometry (outer position + inner size). `None` for sessions
    /// written before this was tracked.
    #[serde(default)]
    pub window: Option<WindowGeom>,
    // NOTE: keep this last — TOML requires array-of-tables fields after scalars/tables.
    pub tabs: Vec<SavedTab>,
}

/// Saved window placement: outer position `(x, y)` and inner size `(w, h)`, in
/// physical pixels.
#[derive(Serialize, Deserialize)]
pub struct WindowGeom {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// One saved tab: how it was opened (`open` | `nojs` | `research` | `read` |
/// `term`) and the address to reopen it at (unused for `term`).
#[derive(Serialize, Deserialize)]
pub struct SavedTab {
    pub kind: String,
    #[serde(default)]
    pub url: String,
}

/// `%APPDATA%\browser\session.toml` on Windows, the XDG data dir elsewhere.
fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "browser").map(|d| d.data_dir().join("session.toml"))
}

/// Write the session to disk (best-effort; failures are ignored).
pub fn save(session: &Session) {
    let Some(path) = path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = toml::to_string(session) {
        let _ = std::fs::write(&path, text);
    }
}

/// Read the saved session, or `None` if there isn't one / it can't be parsed.
pub fn load() -> Option<Session> {
    let text = std::fs::read_to_string(path()?).ok()?;
    toml::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip() {
        // The `tabs` array-of-tables must serialize after the scalar fields, or
        // TOML rejects it — this guards that ordering.
        let s = Session {
            zoom: 1.2,
            nojs: true,
            search_template: "https://example.com/?q=%s".into(),
            term_command: vec!["nu".into()],
            active: 1,
            history: vec!["https://example.com/".into()],
            window: Some(WindowGeom { x: 40, y: 60, w: 1280, h: 800 }),
            tabs: vec![
                SavedTab { kind: "open".into(), url: "https://a.test/".into() },
                SavedTab { kind: "term".into(), url: String::new() },
            ],
        };
        let text = toml::to_string(&s).expect("serialize");
        let back: Session = toml::from_str(&text).expect("deserialize");
        assert_eq!(back.tabs.len(), 2);
        assert_eq!(back.active, 1);
        let g = back.window.expect("window geom");
        assert_eq!((g.x, g.y, g.w, g.h), (40, 60, 1280, 800));
        assert_eq!(back.tabs[0].url, "https://a.test/");
        assert_eq!(back.tabs[1].kind, "term");
    }
}
