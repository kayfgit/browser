//! Saved pages — the "keep this for later" list behind `:save` and `:saved`.
//!
//! A bookmark is a URL, a short name, and when it was saved. They live in their own
//! file (`<data>/saved.toml`), deliberately NOT in the [`session`](crate::session)
//! or the [`config`](crate::config): a session is whatever happened to be open (and
//! `:scratch`/profiles swap it wholesale), and `:restore` resets customization to
//! defaults — neither should ever take the saved list with it. Every change writes
//! the file immediately, so quitting with `:quit` (which skips the session write) can't
//! lose one either.
//!
//! The list is presented as a vim picker (`browser://saved`), like `:history` and
//! `:profiles`: Enter opens the row, ⇧Enter opens it in a new tab, `d` deletes the
//! row or the whole visual selection.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::app::now_epoch;
use crate::pages::truncate_name;
use crate::tabs::{TabContent, TabNav};
use crate::{vim, App, Tab};

/// One saved page.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Bookmark {
    /// The address to reopen. The identity of a bookmark — saving the same URL twice
    /// updates the existing entry rather than adding a second one.
    pub(crate) url: String,
    /// What to call it in the picker. Defaults to the URL's short display form
    /// (`chrome::history_display`) when `:save` is given no name.
    #[serde(default)]
    pub(crate) name: String,
    /// When it was saved (Unix-epoch seconds); `0` = unknown, for a hand-written file.
    #[serde(default)]
    pub(crate) at: u64,
}

/// The on-disk shape. TOML needs a named key for an array of tables, so the list
/// hangs off `items` rather than being the document root.
#[derive(Default, Serialize, Deserialize)]
struct SavedFile {
    #[serde(default)]
    items: Vec<Bookmark>,
}

/// How many header lines [`saved_lines`] emits before the first bookmark row, so a
/// buffer row maps to index `row - HEADER`.
pub(crate) const HEADER: usize = 2;

/// `<data>/saved.toml` — beside the session and config.
fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "browser").map(|d| d.data_dir().join("saved.toml"))
}

/// Read the saved pages, newest-first as written. Never fails: a missing or garbled
/// file reads as an empty list rather than blocking startup.
pub(crate) fn load() -> Vec<Bookmark> {
    let Some(path) = path() else {
        return Vec::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<SavedFile>(&s).ok())
        .map(|f| f.items)
        .unwrap_or_default()
}

/// Persist the list (best-effort; failures are ignored, like the session/config writers).
pub(crate) fn store(items: &[Bookmark]) {
    let Some(path) = path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = SavedFile {
        items: items.to_vec(),
    };
    if let Ok(s) = toml::to_string_pretty(&file) {
        let _ = std::fs::write(path, s);
    }
}

/// The `:saved` picker's lines: a header, then `name    url` per saved page in list
/// order (newest first), so buffer row `r` is bookmark `r - HEADER`.
pub(crate) fn saved_lines(items: &[Bookmark]) -> Vec<String> {
    let mut lines = Vec::with_capacity(items.len() + HEADER);
    lines.push(format!(
        "saved — {} page{}    (Enter: open · Shift+Enter: new tab · d: delete · v: select · :save adds this page)",
        items.len(),
        if items.len() == 1 { "" } else { "s" }
    ));
    lines.push(String::new());
    for b in items {
        let name = if b.name.trim().is_empty() {
            crate::chrome::history_display(&b.url)
        } else {
            b.name.trim().to_string()
        };
        lines.push(format!("{:<38}  {}", truncate_name(&name, 38), b.url));
    }
    lines
}

/// Map a `:saved` buffer row to the bookmark index it shows, or `None` for a
/// header/blank row or one past the end.
pub(crate) fn index_at_row(count: usize, row: usize) -> Option<usize> {
    let i = row.checked_sub(HEADER)?;
    (i < count).then_some(i)
}

impl App {
    /// `:save [name]` — keep the current page for later. Re-saving a page that's
    /// already in the list doesn't duplicate it: with a name it renames the entry,
    /// without one it just says so (so a stray second `:save` is harmless).
    pub(crate) fn save_current_page(&mut self, name: &str) -> Result<String, String> {
        let Some(url) = self.current_url().filter(|u| u.starts_with("http")) else {
            return Err(
                "nothing to save — open a page first (internal pages aren't saveable)".to_string(),
            );
        };
        let name = name.trim();
        if let Some(pos) = self.saved.iter().position(|b| b.url == url) {
            if name.is_empty() {
                return Ok(format!("already saved: {}", self.saved[pos].name));
            }
            self.saved[pos].name = name.to_string();
            store(&self.saved);
            self.refresh_saved_page();
            return Ok(format!("renamed to '{name}'"));
        }
        let label = if name.is_empty() {
            crate::chrome::history_display(&url)
        } else {
            name.to_string()
        };
        self.saved.insert(
            0,
            Bookmark {
                url,
                name: label.clone(),
                at: now_epoch(),
            },
        );
        store(&self.saved);
        self.refresh_saved_page();
        Ok(format!("saved '{label}'  —  :saved to open the list"))
    }

    /// `:unsave <name|url>` — drop a saved page. Matches a name exactly first (case-
    /// insensitively), then falls back to a URL match, so both `:unsave docs` and
    /// `:unsave https://…` work.
    pub(crate) fn remove_saved(&mut self, which: &str) -> Result<String, String> {
        let which = which.trim();
        if which.is_empty() {
            return Err("unsave what? — a saved page's name or URL (:saved lists them)".into());
        }
        let pos = self
            .saved
            .iter()
            .position(|b| b.name.trim().eq_ignore_ascii_case(which))
            .or_else(|| self.saved.iter().position(|b| b.url == which))
            .ok_or_else(|| format!("no saved page called '{which}'"))?;
        let gone = self.saved.remove(pos);
        store(&self.saved);
        self.refresh_saved_page();
        Ok(format!("removed '{}'", gone.name))
    }

    /// `:saved` — the saved-page picker in an engine-free vim tab. Refreshed in place
    /// when it's already the active tab (keeping the cursor row), so saving/deleting
    /// from the list updates it without opening a second one.
    pub(crate) fn open_saved_page(&mut self) {
        if self.saved.is_empty() {
            self.set_status("nothing saved yet — :save keeps the page you're on");
            return;
        }
        let lines = saved_lines(&self.saved);
        if self.active_url() == Some("browser://saved") {
            self.set_saved_lines(lines);
            return;
        }
        self.place_tab(
            Tab {
                url: "browser://saved".into(),
                nojs: false,
                read: false,
                research: false,
                private: false,
                nav: TabNav::default(),
                content: TabContent::Pager(vim::TextBuffer::new(lines)),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }

    /// Re-render the `:saved` picker if it happens to be on screen (after a save or
    /// delete made from elsewhere). A no-op otherwise.
    fn refresh_saved_page(&mut self) {
        if self.active_url() != Some("browser://saved") {
            return;
        }
        let lines = saved_lines(&self.saved);
        self.set_saved_lines(lines);
    }

    /// Swap the picker's text in place, keeping the cursor row (clamped) and dropping
    /// any visual selection — the rows it covered may no longer exist.
    fn set_saved_lines(&mut self, lines: Vec<String>) {
        let cy = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.vim())
            .map_or(0, |b| b.cy);
        if let Some(buf) = self
            .active
            .and_then(|i| self.tabs.get_mut(i))
            .and_then(|t| t.vim_mut())
        {
            buf.set_lines(lines);
            buf.anchor = None;
            buf.cy = cy.min(buf.lines.len().saturating_sub(1));
            buf.cx = 0;
        }
        self.window.request_redraw();
    }

    /// Enter on a `:saved` row: open that page — in this tab, or a new one with
    /// ⇧Enter. A no-op on the header/blank rows.
    pub(crate) fn open_saved_entry(&mut self, new_tab: bool) {
        if self.active_url() != Some("browser://saved") {
            return;
        }
        let row = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.vim())
            .map(|b| b.cy);
        let Some(url) = row
            .and_then(|r| index_at_row(self.saved.len(), r))
            .map(|i| self.saved[i].url.clone())
        else {
            return;
        };
        self.open_tab(&url, self.nojs, new_tab);
    }

    /// `d` on the `:saved` picker: remove the page under the cursor — or every page
    /// in the visual selection — and rewrite the file. A no-op on any other tab.
    pub(crate) fn delete_saved_lines(&mut self) {
        if self.active_url() != Some("browser://saved") {
            return;
        }
        let Some(i) = self.active else { return };
        let (lo, hi) = {
            let Some(buf) = self.tabs.get(i).and_then(|t| t.vim()) else {
                return;
            };
            match buf.anchor {
                Some((ay, _)) => (ay.min(buf.cy), ay.max(buf.cy)),
                None => (buf.cy, buf.cy),
            }
        };
        let n = self.saved.len();
        let (Some(first), Some(last)) = (
            index_at_row(n, lo.max(HEADER)),
            index_at_row(n, hi.min(n + HEADER - 1)),
        ) else {
            return;
        };
        let removed = last + 1 - first;
        self.saved.drain(first..=last);
        store(&self.saved);
        let lines = saved_lines(&self.saved);
        self.set_saved_lines(lines);
        self.set_status(format!(
            "removed {removed} saved page{}",
            if removed == 1 { "" } else { "s" }
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<Bookmark> {
        vec![
            Bookmark {
                url: "https://a.test/one".into(),
                name: "One".into(),
                at: 1,
            },
            Bookmark {
                url: "https://b.test/two".into(),
                name: String::new(),
                at: 0,
            },
        ]
    }

    #[test]
    fn picker_rows_follow_the_header() {
        let lines = saved_lines(&items());
        assert!(lines[0].starts_with("saved — 2 pages"));
        assert_eq!(lines.len(), 2 + 2);
        assert!(lines[2].starts_with("One"));
        assert!(lines[2].ends_with("https://a.test/one"));
        // An unnamed entry falls back to the URL's short display form.
        assert!(lines[3].starts_with("b.test/two"));
    }

    #[test]
    fn rows_map_back_to_indices() {
        assert_eq!(index_at_row(2, 0), None); // header
        assert_eq!(index_at_row(2, 1), None); // blank
        assert_eq!(index_at_row(2, 2), Some(0));
        assert_eq!(index_at_row(2, 3), Some(1));
        assert_eq!(index_at_row(2, 4), None); // past the end
        assert_eq!(index_at_row(0, 2), None); // empty list
    }

    #[test]
    fn toml_roundtrip_keeps_every_field() {
        let file = SavedFile { items: items() };
        let text = toml::to_string_pretty(&file).expect("serialize");
        let back: SavedFile = toml::from_str(&text).expect("deserialize");
        assert_eq!(back.items.len(), 2);
        assert_eq!(back.items[0].url, "https://a.test/one");
        assert_eq!(back.items[0].name, "One");
        assert_eq!(back.items[0].at, 1);
        // A hand-written file with just a url loads with the defaults filled in.
        let minimal: SavedFile =
            toml::from_str("[[items]]\nurl = \"https://c.test/\"").expect("deserialize");
        assert_eq!(minimal.items[0].name, "");
        assert_eq!(minimal.items[0].at, 0);
    }
}
