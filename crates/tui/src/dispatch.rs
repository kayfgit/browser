//! Routes a resolved intent to the right backend. Holds one concrete backend per
//! implemented mode; unimplemented modes return a friendly "roadmap" error.

use anyhow::{anyhow, Result};
use browser_backend_search::SearchBackend;
use browser_backend_text::TextBackend;
use browser_core::{intent, Backend, Config, Document, Mode};

pub struct Dispatcher {
    config: Config,
    text: TextBackend,
    search: SearchBackend,
}

impl Dispatcher {
    pub fn new(config: Config) -> Result<Self> {
        let text = TextBackend::new()?;
        let search = SearchBackend::new(config.search.clone())?;
        Ok(Dispatcher { config, text, search })
    }

    /// Resolve a bare target to a mode using the user's routing rules.
    pub fn resolve_mode(&self, target: &str) -> Mode {
        intent::route(target, &self.config)
    }

    /// Fetch `target` in `mode`, producing a renderable document.
    pub async fn open(&self, mode: Mode, target: &str) -> Result<Document> {
        match mode {
            Mode::Text => self.text.fetch(target).await,
            Mode::Search => self.search.fetch(target).await,
            Mode::Video => Err(anyhow!(
                "video mode is on the roadmap (yt-dlp + mpv) — not wired up yet"
            )),
            Mode::Full => Err(anyhow!(
                "full webview mode is on the roadmap (WebView2) — not wired up yet"
            )),
        }
    }
}
