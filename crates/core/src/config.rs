//! User configuration loaded from `config.toml`.
//!
//! The file is optional; a missing file yields sane defaults. The `[modes]` table
//! maps a host pattern to a [`Mode`]; patterns may be an exact host
//! (`news.example.com`) or a leading-wildcard (`*.example.com`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::intent::Mode;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Mode used for a bare target when no domain rule matches.
    pub default_mode: Mode,
    /// Host-pattern → mode rules (from the `[modes]` table).
    pub modes: HashMap<String, Mode>,
    pub search: SearchConfig,
    pub keys: KeyConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_mode: Mode::Text,
            modes: HashMap::new(),
            search: SearchConfig::default(),
            keys: KeyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchProvider {
    /// A SearXNG instance exposing the JSON `format=json` API.
    Searxng,
    /// DuckDuckGo's HTML-lite endpoint (https://lite.duckduckgo.com/lite/).
    Ddg,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub provider: SearchProvider,
    /// Base URL for the provider. For SearXNG, the `/search` endpoint.
    pub base_url: String,
    /// Max results to display.
    pub limit: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        // DDG-lite needs no instance to run, so it's the zero-config default.
        SearchConfig {
            provider: SearchProvider::Ddg,
            base_url: "https://lite.duckduckgo.com/lite/".into(),
            limit: 15,
        }
    }
}

/// Single-key bindings for the main viewport. (Command-bar verbs live in `intent`.)
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeyConfig {
    pub command: String, // open the command bar
    pub follow: String,  // follow a link by number
    pub reload: String,
    pub back: String,
    pub forward: String,
    pub quit: String,
    pub search_in_page: String,
    pub down: String,
    pub up: String,
    pub top: String,
    pub bottom: String,
}

impl Default for KeyConfig {
    fn default() -> Self {
        KeyConfig {
            command: ":".into(),
            follow: "f".into(),
            reload: "r".into(),
            back: "H".into(),
            forward: "L".into(),
            quit: "q".into(),
            search_in_page: "/".into(),
            down: "j".into(),
            up: "k".into(),
            top: "g".into(),
            bottom: "G".into(),
        }
    }
}

impl Config {
    /// Load config from an explicit path, or the platform default location.
    /// A missing file is not an error — defaults are returned.
    pub fn load(explicit: Option<&Path>) -> Result<Config> {
        let path = match explicit {
            Some(p) => Some(p.to_path_buf()),
            None => Self::default_path(),
        };
        match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(&p)
                    .with_context(|| format!("reading config {}", p.display()))?;
                let cfg: Config = toml::from_str(&text)
                    .with_context(|| format!("parsing config {}", p.display()))?;
                Ok(cfg)
            }
            _ => Ok(Config::default()),
        }
    }

    /// `%APPDATA%\browser\config.toml` on Windows, XDG config dir elsewhere.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "browser")
            .map(|d| d.config_dir().join("config.toml"))
    }

    /// Resolve the mode for a host, honoring exact then wildcard rules.
    pub fn mode_for_host(&self, host: &str) -> Option<Mode> {
        if let Some(&m) = self.modes.get(host) {
            return Some(m);
        }
        // Try `*.suffix` rules against progressively shorter parent domains.
        let mut rest = host;
        while let Some((_, parent)) = rest.split_once('.') {
            let pattern = format!("*.{parent}");
            if let Some(&m) = self.modes.get(&pattern) {
                return Some(m);
            }
            rest = parent;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_text_mode() {
        let c = Config::default();
        assert_eq!(c.default_mode, Mode::Text);
    }

    #[test]
    fn wildcard_host_rules() {
        let mut c = Config::default();
        c.modes.insert("*.example.com".into(), Mode::Full);
        c.modes.insert("docs.rs".into(), Mode::Text);
        assert_eq!(c.mode_for_host("a.b.example.com"), Some(Mode::Full));
        assert_eq!(c.mode_for_host("docs.rs"), Some(Mode::Text));
        assert_eq!(c.mode_for_host("other.org"), None);
    }

    #[test]
    fn parses_sample_toml() {
        let toml = r#"
            default_mode = "text"
            [modes]
            "youtube.com" = "video"
            "*.reddit.com" = "text"
            [search]
            provider = "ddg"
            base_url = "https://lite.duckduckgo.com/lite/"
            limit = 10
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.default_mode, Mode::Text);
        assert_eq!(c.mode_for_host("youtube.com"), Some(Mode::Video));
        assert_eq!(c.search.limit, 10);
    }
}
