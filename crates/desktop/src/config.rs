//! Persistent **customization** config — the user/AI-tunable settings that live
//! apart from the per-run [`session`](crate::session) (open tabs, window geometry).
//! This is the store the AI mutates to "mess with the browser": today it holds
//! command aliases; chrome appearance and keybinds will join it as those become
//! runtime-configurable.
//!
//! It is loaded once at startup, applied at runtime, and re-saved on every change.
//! The safety net is [`App::restore_defaults`](crate::App::restore_defaults) (the
//! `:restore` command and the `Ctrl+Alt+Shift+R` keyboard-hook chord), which resets
//! this whole file to defaults even if a bad customization has made the UI
//! unusable — the hook chord is caught below the keybind layer, so it works no
//! matter how keys get rebound later.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// All persisted customization. Every field is `#[serde(default)]` so older config
/// files (and a brand-new one) load cleanly, and so a single missing/garbled field
/// never discards the rest.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Config {
    /// Command aliases: the word typed after `:` → the command line it expands to
    /// (e.g. `gh` → `open github.com`). Resolved before the built-in verbs in
    /// [`run_command`](crate::App::run_command). Sorted (BTreeMap) for stable
    /// listing/serialization.
    #[serde(default)]
    pub(crate) aliases: BTreeMap<String, String>,
    /// Chrome appearance overrides (bar height + colours). Resolved into the live
    /// [`Theme`](crate::draw::Theme) by [`rebuild_theme`](crate::App::rebuild_theme).
    #[serde(default)]
    pub(crate) theme: ThemeConfig,
}

/// Appearance overrides for the shell chrome — the command/status bar height and the
/// four themable colours. Every field is optional: an unset field keeps the built-in
/// default. Colours are stored exactly as given (a name like `green` or a hex like
/// `#0a2a0a`) and resolved (validated) at apply time, so a future colour name doesn't
/// invalidate an old file.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct ThemeConfig {
    /// Command/status bar height as a percent of the default (100 = default, 125 = 25%
    /// taller). Clamped to a sane range on apply.
    #[serde(default)]
    pub(crate) bar_height_pct: Option<u32>,
    /// Command/status (and tab) bar background.
    #[serde(default)]
    pub(crate) bar_bg: Option<String>,
    /// Command/status bar text.
    #[serde(default)]
    pub(crate) bar_fg: Option<String>,
    /// Accent/highlight (active tab, caret, focused-pane border, mode tags).
    #[serde(default)]
    pub(crate) accent: Option<String>,
    /// Page/welcome background.
    #[serde(default)]
    pub(crate) bg: Option<String>,
}

/// Location of the config file (plain TOML in the app data dir, beside the session).
fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "browser").map(|d| d.data_dir().join("config.toml"))
}

/// Load the saved config, or defaults if there's none / it can't be read or parsed.
/// Never fails: a broken file falls back to defaults rather than blocking startup.
pub(crate) fn load() -> Config {
    let Some(path) = config_path() else { return Config::default() };
    std::fs::read_to_string(path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

/// Persist the config (best-effort; failures are ignored, like the session writer).
pub(crate) fn save(cfg: &Config) {
    let Some(path) = config_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = toml::to_string_pretty(cfg) {
        let _ = std::fs::write(path, s);
    }
}
