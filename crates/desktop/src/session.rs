//! Session persistence: save the open tabs + UI state on exit and restore them
//! on the next launch, so reopening the browser comes back exactly as it was.
//!
//! Stored as TOML next to the config dir. Only restorable tabs are saved — web
//! pages (open/nojs/research), engine-free read tabs (re-fetched), and terminals
//! (reopened fresh). Internal `browser://` pages and the `:error(s)` log are
//! session-specific and skipped.
//!
//! The same [`Session`] shape is also what a **profile** is (`:saveprofile work`):
//! the unnamed session lives in `session.toml`, each named profile in
//! `profiles/<key>.toml`, and the `:scratch` stash in `scratch.toml`. See
//! [`crate::profiles`] for the switching logic.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A persisted browsing session.
#[derive(Serialize, Deserialize)]
pub struct Session {
    /// Display name of the profile this file holds, as the user typed it
    /// (`:saveprofile Work Stuff`). Empty for the unnamed default session and for
    /// files written before profiles existed; the file NAME is the canonical key
    /// (see [`profile_key`]), this is only what's shown in `:profiles`.
    #[serde(default)]
    pub name: String,
    pub zoom: f64,
    /// Web-content zoom (page scale inside web tabs), separate from the chrome
    /// [`zoom`](Self::zoom). `default` (1.0) for sessions written before it existed.
    #[serde(default = "default_content_zoom")]
    pub content_zoom: f64,
    pub nojs: bool,
    /// Whether the pages' scrollbars are hidden (`:scrollbar`). Unlike the other
    /// page-feature toggles (mute/css/video — deliberately session-only), hiding
    /// scrollbars is a lasting preference, so it survives `:wq`. `default` (shown)
    /// for sessions written before it existed.
    #[serde(default)]
    pub no_scrollbar: bool,
    /// Legacy field: whether the (native) ad blocker was on. Superseded by
    /// [`adblock_mode`](Self::adblock_mode); still written/read for older builds.
    #[serde(default = "default_adblock")]
    pub adblock: bool,
    /// Which ad blocker is active: `"ubo"` (default), `"native"`, or `"off"`. Defaults to
    /// `"ubo"` so sessions written before this field existed adopt the new default engine.
    #[serde(default = "default_adblock_mode")]
    pub adblock_mode: String,
    /// The engine a bare `:ads` switches back ON — the last one that was running before
    /// blocking was turned off, so `native` → off → `native`. Same spellings as
    /// [`adblock_mode`](Self::adblock_mode) minus `"off"`; defaults to `"ubo"`.
    #[serde(default = "default_adblock_mode")]
    pub adblock_prev: String,
    pub search_template: String,
    pub term_command: Vec<String>,
    /// Index of the focused tab within `tabs`.
    pub active: usize,
    /// Visited URLs (most-recent first) for command-bar autocomplete. Must stay
    /// before the `window`/`tabs` tables (TOML: values precede tables).
    #[serde(default)]
    pub history: Vec<String>,
    /// Visit times (Unix-epoch seconds) parallel to `history`. `default` (empty) for
    /// sessions written before time-stamping; realigned to `history` on load, with
    /// missing entries treated as time-unknown. Keep adjacent to `history` and before
    /// the `window`/`tabs` tables (TOML: scalar arrays precede tables).
    #[serde(default)]
    pub history_at: Vec<u64>,
    /// The pane/split layout: one encoded string per tab-strip window (see
    /// `panes::encode_window`), with leaf numbers indexing into `tabs`. Empty for sessions
    /// without splits or written by older builds — then each tab restores standalone. Must
    /// stay before the `window`/`tabs` tables (TOML: values precede tables).
    #[serde(default)]
    pub windows: Vec<String>,
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
    /// `term` only: the shell's last known working directory (via shell-integration
    /// OSC 9;9 / OSC 7 — a WSL path re-enters WSL on restore). Empty = unknown,
    /// the shell reopens in its default directory.
    #[serde(default)]
    pub cwd: String,
}

/// Serde default for [`Session::content_zoom`]: 100% (no page scaling) for
/// sessions written before content zoom was split from the chrome zoom.
fn default_content_zoom() -> f64 {
    1.0
}

/// Serde default for [`Session::adblock`]: blocking is on unless a session
/// explicitly records it off.
fn default_adblock() -> bool {
    true
}

/// Serde default for [`Session::adblock_mode`]: the uBlock Origin extension.
fn default_adblock_mode() -> String {
    "ubo".to_string()
}

/// The app data directory: `%APPDATA%\browser\data` on Windows, the XDG data dir
/// elsewhere. Everything below lives in it.
fn data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "browser").map(|d| d.data_dir().to_path_buf())
}

/// `<data>/session.toml` — the unnamed default session (the one you get when no
/// profile is active).
pub fn session_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("session.toml"))
}

/// `<data>/profiles` — one `<key>.toml` per named profile.
fn profiles_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("profiles"))
}

/// `<data>/scratch.toml` — where `:scratch` parks the layout it wiped, and where
/// the scratch layout itself is written when you leave it (so an accidental toggle
/// is recoverable). Deliberately NOT in `profiles/`, so it can't collide with a
/// user profile or show up in `:profiles`.
pub fn scratch_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("scratch.toml"))
}

/// The canonical on-disk key for a profile name: lowercase, spaces folded to `-`,
/// and anything that isn't alphanumeric/`-`/`_` dropped — so "Work Stuff", "work
/// stuff" and "Work-Stuff" all address the same profile. Empty if the name has no
/// usable characters (the caller rejects that).
pub fn profile_key(name: &str) -> String {
    let mut key = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            key.push(c.to_ascii_lowercase());
        } else if (c == '-' || c == '_' || c.is_whitespace()) && !key.ends_with('-') {
            key.push('-');
        }
    }
    key.trim_matches('-').to_string()
}

/// Path of the named profile's file, or `None` if the name is unusable / there's
/// no data dir.
pub fn profile_path(name: &str) -> Option<PathBuf> {
    let key = profile_key(name);
    if key.is_empty() {
        return None;
    }
    profiles_dir().map(|d| d.join(format!("{key}.toml")))
}

/// Whether a profile with this name exists on disk.
pub fn profile_exists(name: &str) -> bool {
    profile_path(name).is_some_and(|p| p.is_file())
}

/// Every saved profile's display name, sorted. A file whose `name` is empty (an
/// older or hand-made file) falls back to its filename key, so nothing is hidden.
pub fn list_profiles() -> Vec<String> {
    let Some(dir) = profiles_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
        .filter_map(|e| {
            let stem = e.path().file_stem()?.to_string_lossy().into_owned();
            let name = std::fs::read_to_string(e.path())
                .ok()
                .and_then(|s| toml::from_str::<Session>(&s).ok())
                .map(|s| s.name)
                .filter(|n| !n.trim().is_empty());
            Some(name.unwrap_or(stem))
        })
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// Delete a profile's file. `true` if one was there.
pub fn delete_profile(name: &str) -> bool {
    profile_path(name).is_some_and(|p| std::fs::remove_file(p).is_ok())
}

/// Write a session to an explicit path, creating the directory (best-effort;
/// failures are ignored, like the config writer).
pub fn save_to(path: &std::path::Path, session: &Session) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = toml::to_string(session) {
        let _ = std::fs::write(path, text);
    }
}

/// Read a session from an explicit path, or `None` if it isn't there / won't parse.
pub fn load_from(path: &std::path::Path) -> Option<Session> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip() {
        // The `tabs` array-of-tables must serialize after the scalar fields, or
        // TOML rejects it — this guards that ordering.
        let s = Session {
            name: "Work".into(),
            zoom: 1.2,
            content_zoom: 1.1,
            nojs: true,
            no_scrollbar: true,
            adblock: true,
            adblock_mode: "ubo".into(),
            adblock_prev: "native".into(),
            search_template: "https://example.com/?q=%s".into(),
            term_command: vec!["nu".into()],
            active: 1,
            history: vec!["https://example.com/".into()],
            history_at: vec![1_700_000_000],
            windows: vec!["R0.5000(0|1)".into()],
            window: Some(WindowGeom { x: 40, y: 60, w: 1280, h: 800 }),
            tabs: vec![
                SavedTab { kind: "open".into(), url: "https://a.test/".into(), cwd: String::new() },
                SavedTab {
                    kind: "term".into(),
                    url: String::new(),
                    cwd: "C:\\projects\\browser".into(),
                },
            ],
        };
        let text = toml::to_string(&s).expect("serialize");
        let back: Session = toml::from_str(&text).expect("deserialize");
        assert_eq!(back.name, "Work");
        assert!(back.no_scrollbar);
        assert_eq!(back.adblock_prev, "native");
        assert_eq!(back.tabs.len(), 2);
        assert_eq!(back.active, 1);
        let g = back.window.expect("window geom");
        assert_eq!((g.x, g.y, g.w, g.h), (40, 60, 1280, 800));
        assert_eq!(back.tabs[0].url, "https://a.test/");
        assert_eq!(back.tabs[1].kind, "term");
        assert_eq!(back.tabs[1].cwd, "C:\\projects\\browser");
        assert_eq!(back.windows, vec!["R0.5000(0|1)".to_string()]);
    }

    #[test]
    fn saved_tab_cwd_defaults_for_old_sessions() {
        // Sessions written before terminal-cwd tracking have no `cwd` key.
        let tab: SavedTab = toml::from_str("kind = \"term\"").expect("deserialize");
        assert_eq!(tab.cwd, "");
    }

    #[test]
    fn profile_key_folds_case_spaces_and_punctuation() {
        // Everything a user might type for the same profile lands on one file.
        assert_eq!(profile_key("Work"), "work");
        assert_eq!(profile_key("Work Stuff"), "work-stuff");
        assert_eq!(profile_key("  work   stuff  "), "work-stuff");
        assert_eq!(profile_key("Work-Stuff"), "work-stuff");
        assert_eq!(profile_key("work_stuff"), "work-stuff");
        // Path separators and other punctuation can't escape the profiles dir.
        assert_eq!(profile_key("../../etc/passwd"), "etcpasswd");
        assert_eq!(profile_key("a/b"), "ab");
        // A name with nothing usable in it has no key (rejected by the caller).
        assert_eq!(profile_key("!!!"), "");
        assert_eq!(profile_key(""), "");
    }
}
