//! `:news` — what changed in each release, from the changelog compiled into this
//! build. release-please writes a version's notes into `CHANGELOG.md` before tagging
//! it, so every release build carries its own notes. Shown as an engine-free read tab
//! (scroll, find, select, `f` to follow a commit link), and announced once in the
//! status bar on the first launch after an update.

use browser_core::Document;

use crate::App;

const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// The address `:news` tabs show (and reload from).
pub(crate) const NEWS_URL: &str = "browser://news";

/// The changelog as a read-view document, newest release first.
pub(crate) fn document() -> Document {
    // The file's own "# Changelog" title is replaced by the tab's.
    let body = CHANGELOG
        .trim_start()
        .strip_prefix("# Changelog")
        .unwrap_or(CHANGELOG);
    let mut doc = crate::markdown::to_document(body, NEWS_URL);
    doc.title = format!("What's new in browser {}", env!("CARGO_PKG_VERSION"));
    doc
}

/// The status line to show on the first launch of a version other than the last one
/// seen, recording that it has now been seen. `None` when nothing changed.
pub(crate) fn note_launch(seen: &mut Option<String>) -> Option<String> {
    let current = env!("CARGO_PKG_VERSION");
    if seen.as_deref() == Some(current) {
        return None;
    }
    *seen = Some(current.to_string());
    Some(format!("browser {current} — :news for what's new"))
}

impl App {
    /// `:news` — open the changelog in this tab (a new one with `-t`).
    pub(crate) fn open_news(&mut self, new_tab: bool) {
        self.show_read_document(document(), !new_tab, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_changelog_renders_with_versions_and_links() {
        let doc = document();
        assert!(doc.title.contains(env!("CARGO_PKG_VERSION")));
        assert!(!doc.blocks.is_empty());
        assert!(!doc.links.is_empty(), "commit links should be followable");
    }

    #[test]
    fn the_update_note_shows_once_per_version() {
        let mut seen = None;
        assert!(note_launch(&mut seen).is_some());
        assert_eq!(seen.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert!(note_launch(&mut seen).is_none());
        let mut old = Some("0.0.1".to_string());
        assert!(note_launch(&mut old).is_some());
    }
}
