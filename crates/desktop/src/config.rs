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
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// All persisted customization. Every field is `#[serde(default)]` so older config
/// files (and a brand-new one) load cleanly, and so a single missing/garbled field
/// never discards the rest.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Config {
    /// Concrete provider for new web tabs. None preserves the legacy WebView2 default.
    #[serde(default)]
    pub(crate) engine: Option<String>,
    /// Command aliases: the word typed after `:` → the command line it expands to
    /// (e.g. `gh` → `open github.com`). Resolved before the built-in verbs in
    /// [`run_command`](crate::App::run_command). Sorted (BTreeMap) for stable
    /// listing/serialization.
    #[serde(default)]
    pub(crate) aliases: BTreeMap<String, String>,
    /// The user's changes to Kagi's bang list: their own bangs (which win over
    /// Kagi's for the same key) and the Kagi ones they switched off.
    #[serde(default)]
    pub(crate) bangs: browser_core::BangOverrides,
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
    /// The browser version last launched, so the first launch after an update can
    /// point at `:news`. State, not customization: [`restore_defaults`](crate::App::restore_defaults) keeps it.
    #[serde(default)]
    pub(crate) seen_version: Option<String>,
    /// Whether the one-time notice about the detailed diagnostic log (early
    /// development, see `journal`) has been shown. State, like `seen_version`.
    #[serde(default)]
    pub(crate) logging_notice_seen: bool,
    /// What the browser does about updates: `notify` or `off`; unset is `auto`
    /// (download in the background, install on quit). See `:update`.
    #[serde(default)]
    pub(crate) update_mode: Option<String>,
    /// Before `update_mode`, `:update off` saved `false` here; still honoured.
    #[serde(default)]
    pub(crate) check_updates: Option<bool>,
    /// When the last update check ran (Unix seconds). State, like
    /// `seen_version`.
    #[serde(default)]
    pub(crate) update_checked_at: Option<u64>,
    /// Where downloads are saved (`:downloads dir`); unset is the Windows Downloads folder.
    #[serde(default)]
    pub(crate) download_dir: Option<PathBuf>,
    /// Plain link clicks open in a glance instead of following the link
    /// (`:glance clicks on`), like Zen browser. Off: only `q` hints and the
    /// right-click menu open one.
    #[serde(default)]
    pub(crate) glance_clicks: bool,
    /// The clickable buttons on the tab strip / sidebar (`:button`), in order. Unset
    /// is [`default_buttons`]; an empty list hides them (`:buttons off`).
    #[serde(default)]
    pub(crate) buttons: Option<Vec<Button>>,
    /// How wide read mode's text column is, in characters (`:readwidth`). Unset is
    /// [`DEFAULT_READ_WIDTH`]; `0` uses the whole pane.
    #[serde(default)]
    pub(crate) read_width: Option<u32>,
    /// The tab strip's optional behaviours (the `[tabs]` table). Each is one of the
    /// [`SETTINGS`], flipped with `:set` or by `:ai`.
    #[serde(default)]
    pub(crate) tabs: TabsConfig,
}

/// The `[tabs]` table. Unset fields take their [`SETTINGS`] default (all on).
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct TabsConfig {
    /// Restored tabs load only when first shown; off loads them all at launch.
    #[serde(default)]
    pub(crate) lazy_load: Option<bool>,
    /// A speaker on tabs playing sound (crossed out while muted).
    #[serde(default)]
    pub(crate) speaker: Option<bool>,
    /// Hovering a tab widens it and shows its close and mute buttons.
    #[serde(default)]
    pub(crate) hover_buttons: Option<bool>,
}

/// One on/off setting: its `key` (`table.field`, as written in `config.toml`), its
/// `default`, and what it does. The single list `:set`, the `:ai` assistant and the
/// help read from: a new feature's switch is a field in [`Config`], an entry here,
/// and an arm in [`Config::slot`] / [`Config::slot_mut`].
pub(crate) struct Setting {
    pub(crate) key: &'static str,
    pub(crate) default: bool,
    pub(crate) desc: &'static str,
}

pub(crate) const SETTINGS: &[Setting] = &[
    Setting {
        key: "tabs.lazy_load",
        default: true,
        desc: "restored tabs load only when first shown (off: every tab loads at launch)",
    },
    Setting {
        key: "tabs.speaker",
        default: true,
        desc: "show a speaker on tabs playing sound, crossed out while muted",
    },
    Setting {
        key: "tabs.hover_buttons",
        default: true,
        desc: "hovering a tab widens it and shows its close and mute buttons",
    },
    Setting {
        key: "term.mouse_reporting",
        default: true,
        desc: "terminal programs that read the mouse (htop, lazygit, vim) get clicks, drags and the wheel; Shift+drag still selects",
    },
    Setting {
        key: "term.text_styles",
        default: true,
        desc: "terminals draw bold, italic, dim, underlined and struck-through text",
    },
    Setting {
        key: "term.cursor_shape",
        default: true,
        desc: "the terminal cursor takes the shape programs ask for (bar, underline) and turns hollow when the terminal doesn't have the keyboard",
    },
    Setting {
        key: "term.cursor_blink",
        default: true,
        desc: "the terminal cursor blinks, unless a program asks for a steady one",
    },
    Setting {
        key: "term.clipboard_write",
        default: true,
        desc: "terminal programs can copy to the clipboard (nvim, tmux, also over SSH)",
    },
    Setting {
        key: "term.visual_bell",
        default: true,
        desc: "the terminal flashes when a program rings the bell",
    },
];

/// Parse a setting's value as typed: `on`/`off` (and `true`/`false`, `yes`/`no`,
/// `1`/`0`), or `default` (→ `None`, back to the built-in default).
pub(crate) fn parse_switch(value: &str) -> Result<Option<bool>, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(Some(true)),
        "off" | "false" | "no" | "0" => Ok(Some(false)),
        "default" | "reset" => Ok(None),
        other => Err(format!("{other:?} isn't on, off or default")),
    }
}

impl Config {
    /// The stored value of setting `key` (`Some(None)` when unset), or `None` for an
    /// unknown key.
    fn slot(&self, key: &str) -> Option<Option<bool>> {
        Some(match key {
            "tabs.lazy_load" => self.tabs.lazy_load,
            "tabs.speaker" => self.tabs.speaker,
            "tabs.hover_buttons" => self.tabs.hover_buttons,
            "term.mouse_reporting" => self.term.mouse_reporting,
            "term.text_styles" => self.term.text_styles,
            "term.cursor_shape" => self.term.cursor_shape,
            "term.cursor_blink" => self.term.cursor_blink,
            "term.clipboard_write" => self.term.clipboard_write,
            "term.visual_bell" => self.term.visual_bell,
            _ => return None,
        })
    }

    fn slot_mut(&mut self, key: &str) -> Option<&mut Option<bool>> {
        Some(match key {
            "tabs.lazy_load" => &mut self.tabs.lazy_load,
            "tabs.speaker" => &mut self.tabs.speaker,
            "tabs.hover_buttons" => &mut self.tabs.hover_buttons,
            "term.mouse_reporting" => &mut self.term.mouse_reporting,
            "term.text_styles" => &mut self.term.text_styles,
            "term.cursor_shape" => &mut self.term.cursor_shape,
            "term.cursor_blink" => &mut self.term.cursor_blink,
            "term.clipboard_write" => &mut self.term.clipboard_write,
            "term.visual_bell" => &mut self.term.visual_bell,
            _ => return None,
        })
    }

    /// Setting `key`'s effective value: what's stored, else its default. Unknown keys
    /// read as on, so a typo in the code can't silently switch a feature off (the
    /// test below keeps every key known).
    pub(crate) fn setting(&self, key: &str) -> bool {
        let default = SETTINGS
            .iter()
            .find(|s| s.key == key)
            .is_none_or(|s| s.default);
        self.slot(key).flatten().unwrap_or(default)
    }

    /// Store setting `key` (`None` = back to its default).
    pub(crate) fn set_setting(&mut self, key: &str, value: Option<bool>) -> Result<(), String> {
        let known = SETTINGS.iter().any(|s| s.key == key);
        match self.slot_mut(key).filter(|_| known) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(format!(
                "no setting {key:?} — try {}",
                SETTINGS
                    .iter()
                    .map(|s| s.key)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn every_setting_is_stored_and_defaults_apply() {
        let mut c = Config::default();
        for s in SETTINGS {
            assert!(c.slot(s.key).is_some(), "{} has no field", s.key);
            assert_eq!(c.setting(s.key), s.default, "{} default", s.key);
            c.set_setting(s.key, Some(!s.default)).unwrap();
            assert_eq!(c.setting(s.key), !s.default, "{} set", s.key);
            c.set_setting(s.key, None).unwrap();
            assert_eq!(c.setting(s.key), s.default, "{} reset", s.key);
        }
        assert!(c.set_setting("tabs.nope", Some(true)).is_err());
    }

    #[test]
    fn settings_round_trip_through_config_toml() {
        let mut c = Config::default();
        c.set_setting("tabs.speaker", Some(false)).unwrap();
        let text = toml::to_string(&c).unwrap();
        assert!(
            text.contains("[tabs]") && text.contains("speaker = false"),
            "{text}"
        );
        let back: Config = toml::from_str(&text).unwrap();
        assert!(!back.setting("tabs.speaker"));
        assert!(back.setting("tabs.lazy_load"));
        // An older config.toml with no [tabs] table keeps every feature on.
        let old: Config = toml::from_str("").unwrap();
        assert!(SETTINGS.iter().all(|s| old.setting(s.key) == s.default));
    }

    #[test]
    fn switches_parse_like_people_type_them() {
        assert_eq!(parse_switch("ON"), Ok(Some(true)));
        assert_eq!(parse_switch("off"), Ok(Some(false)));
        assert_eq!(parse_switch(" default "), Ok(None));
        assert!(parse_switch("maybe").is_err());
    }
}

/// Read mode's default text column: lines past ~90 characters get hard to follow.
pub(crate) const DEFAULT_READ_WIDTH: u32 = 90;

impl Config {
    /// The read view's column width in characters, or `None` for the whole pane.
    pub(crate) fn read_max_chars(&self) -> Option<u32> {
        match self.read_width.unwrap_or(DEFAULT_READ_WIDTH) {
            0 => None,
            n => Some(n),
        }
    }
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
    /// Where the tabs go: `"side"` for a vertical sidebar on the left (`:sidebar`);
    /// unset (or `"top"`) keeps the strip along the top.
    #[serde(default)]
    pub(crate) tabs: Option<String>,
}

/// One clickable button on the tab strip or sidebar: its `label` and the command line
/// it runs (`run`, without the leading `:`) — anything the command bar takes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Button {
    pub(crate) label: String,
    pub(crate) run: String,
}

/// The buttons a fresh config shows: back, forward, reload and new tab.
pub(crate) fn default_buttons() -> Vec<Button> {
    [
        ("←", "back"),
        ("→", "forward"),
        ("↻", "reload"),
        ("+", "tabopen"),
    ]
    .into_iter()
    .map(|(label, run)| Button {
        label: label.into(),
        run: run.into(),
    })
    .collect()
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
    /// Programs that read the mouse (htop, lazygit, vim `mouse=a`) get clicks, drags
    /// and the wheel. One of the [`SETTINGS`].
    #[serde(default)]
    pub(crate) mouse_reporting: Option<bool>,
    /// Bold, italic, dim, underlined and struck-through text is drawn as such.
    #[serde(default)]
    pub(crate) text_styles: Option<bool>,
    /// The cursor takes the shape programs ask for (a bar, an underline) and turns
    /// hollow while the terminal doesn't have the keyboard.
    #[serde(default)]
    pub(crate) cursor_shape: Option<bool>,
    /// The cursor blinks (unless a program asks for a steady one).
    #[serde(default)]
    pub(crate) cursor_blink: Option<bool>,
    /// Programs can copy to the clipboard (OSC 52: nvim, tmux, over SSH too).
    #[serde(default)]
    pub(crate) clipboard_write: Option<bool>,
    /// The terminal flashes when a program rings the bell.
    #[serde(default)]
    pub(crate) visual_bell: Option<bool>,
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
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Load an installed (downloaded) scheme by name, resolving colours to a live
/// [`TermStyle`](crate::pty_term::TermStyle). `None` if it isn't installed or the
/// file is garbled — the caller falls back to the default style.
pub(crate) fn load_custom_scheme(name: &str) -> Option<crate::pty_term::TermStyle> {
    let path = schemes_dir()?.join(format!("{}.toml", scheme_key(name)));
    let file: SchemeFile = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let mut st = crate::pty_term::TermStyle {
        fg: crate::draw::parse_color(&file.fg)?,
        bg: crate::draw::parse_color(&file.bg)?,
        ..Default::default()
    };
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
    let Some(dir) = schemes_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
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
    let Some(path) = config_path() else {
        return Config::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the config (best-effort; failures are ignored, like the session writer).
pub(crate) fn save(cfg: &Config) {
    let Some(path) = config_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = toml::to_string_pretty(&for_disk(cfg)) {
        let _ = std::fs::write(path, s);
    }
}

/// Which profile the config file points at — `(profile, scratch, scratch_return)` —
/// as it was on disk when a `--scratch` run started. That run points the live config
/// at a throwaway scratch slate IN MEMORY ONLY, but every customization (`:theme`,
/// `:alias`, `:bang`, …) re-saves the whole config, which wrote `scratch = true`
/// to disk and sent the next ordinary launch into scratch instead of the user's
/// profile. [`save`] writes these values back in place of the run's own.
static PINNED_POINTER: OnceLock<(Option<String>, bool, Option<String>)> = OnceLock::new();

/// Keep the on-disk profile pointer as it is in `cfg` for the rest of this process
/// (see [`PINNED_POINTER`]). Call before changing it for a `--scratch` run.
pub(crate) fn pin_profile_pointer(cfg: &Config) {
    let _ = PINNED_POINTER.set((cfg.profile.clone(), cfg.scratch, cfg.scratch_return.clone()));
}

/// `cfg` as it should be written: unchanged, unless the profile pointer is pinned.
fn for_disk(cfg: &Config) -> std::borrow::Cow<'_, Config> {
    match PINNED_POINTER.get() {
        Some((profile, scratch, scratch_return)) => {
            let mut c = cfg.clone();
            c.profile = profile.clone();
            c.scratch = *scratch;
            c.scratch_return = scratch_return.clone();
            std::borrow::Cow::Owned(c)
        }
        None => std::borrow::Cow::Borrowed(cfg),
    }
}
