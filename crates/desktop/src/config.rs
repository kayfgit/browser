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
    /// `:te` terminal appearance overrides (font + colours). Resolved into the live
    /// [`TermStyle`](crate::pty_term::TermStyle) / terminal painter by
    /// [`rebuild_term_style`](crate::App::rebuild_term_style).
    #[serde(default)]
    pub(crate) term: TermConfig,
    /// The active profile's display name (`:profile work`), or `None` for the unnamed
    /// default session. Decides which session file `:w` writes and the next launch
    /// restores. Session STATE rather than customization, so
    /// [`restore_defaults`](crate::App::restore_defaults) preserves it — resetting your
    /// theme must not silently drop you into a different set of tabs.
    #[serde(default)]
    pub(crate) profile: Option<String>,
    /// True while `:scratch` (the clean-slate scratch profile) is active.
    #[serde(default)]
    pub(crate) scratch: bool,
    /// While [`scratch`](Self::scratch): the profile to return to when it's toggled
    /// off (`None` = the default session).
    #[serde(default)]
    pub(crate) scratch_return: Option<String>,
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

/// Appearance overrides for `:te` terminal tabs. Like [`ThemeConfig`], every field is
/// optional (unset keeps the built-in default) and stored as given — the font name and
/// colours are resolved/validated at apply time, never at load time, so a font that was
/// uninstalled or a stale scheme name degrades gracefully instead of breaking startup.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct TermConfig {
    /// Terminal font: an installed font's name (matched against the system/user font
    /// directories) or an explicit .ttf/.otf path. Unset = the UI's monospace font.
    #[serde(default)]
    pub(crate) font: Option<String>,
    /// Terminal font size in px at zoom 1.0 (browser zoom multiplies it). Unset = the
    /// UI font size, so the terminal matches the chrome like before.
    #[serde(default)]
    pub(crate) font_px: Option<f32>,
    /// Named ANSI colour scheme (see [`pty_term::scheme`](crate::pty_term::scheme)):
    /// dracula, gruvbox, nord, … Unset = campbell (the Windows Terminal default).
    #[serde(default)]
    pub(crate) scheme: Option<String>,
    /// Terminal background (name or hex) — overrides the scheme's.
    #[serde(default)]
    pub(crate) bg: Option<String>,
    /// Terminal default text colour (name or hex) — overrides the scheme's.
    #[serde(default)]
    pub(crate) fg: Option<String>,
}

/// Location of the config file (plain TOML in the app data dir, beside the session).
/// `pub(crate)` so `:theme` can open it in an editor for direct editing.
pub(crate) fn config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "browser").map(|d| d.data_dir().join("config.toml"))
}

/// Directory holding downloaded terminal colour schemes (one TOML per scheme),
/// beside the config. Created on first install.
fn schemes_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "browser").map(|d| d.data_dir().join("schemes"))
}

/// A downloaded terminal colour scheme as stored on disk: display name + default
/// fg/bg + the 16 ANSI colours, all as `#rrggbb` strings (human-editable TOML).
#[derive(Serialize, Deserialize)]
pub(crate) struct SchemeFile {
    pub(crate) name: String,
    pub(crate) fg: String,
    pub(crate) bg: String,
    pub(crate) ansi: Vec<String>,
}

/// The canonical on-disk key for a scheme name: lowercase alphanumerics only, so
/// "IBM 3270", "3270-Dark" and "3270 dark" all address the same file.
pub(crate) fn scheme_key(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// Load an installed (downloaded) scheme by name, resolving colours to a live
/// [`TermStyle`](crate::pty_term::TermStyle). `None` if it isn't installed or the
/// file is garbled — the caller falls back to the default style.
pub(crate) fn load_custom_scheme(name: &str) -> Option<crate::pty_term::TermStyle> {
    let path = schemes_dir()?.join(format!("{}.toml", scheme_key(name)));
    let file: SchemeFile = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let mut st = crate::pty_term::TermStyle::default();
    st.fg = crate::draw::parse_color(&file.fg)?;
    st.bg = crate::draw::parse_color(&file.bg)?;
    if file.ansi.len() != 16 {
        return None;
    }
    for (slot, s) in st.ansi.iter_mut().zip(&file.ansi) {
        *slot = crate::draw::parse_color(s)?;
    }
    Some(st)
}

/// Persist a downloaded scheme under its canonical key. Returns the display name.
pub(crate) fn save_custom_scheme(file: &SchemeFile) -> Result<String, String> {
    let dir = schemes_dir().ok_or("no data directory available")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.toml", scheme_key(&file.name)));
    let s = toml::to_string_pretty(file).map_err(|e| e.to_string())?;
    std::fs::write(&path, s).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(file.name.clone())
}

/// Display names of every installed (downloaded) scheme, sorted.
pub(crate) fn custom_scheme_names() -> Vec<String> {
    let Some(dir) = schemes_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let s = std::fs::read_to_string(e.path()).ok()?;
            toml::from_str::<SchemeFile>(&s).ok().map(|f| f.name)
        })
        .collect();
    names.sort();
    names
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
