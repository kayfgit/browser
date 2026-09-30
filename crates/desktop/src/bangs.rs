//! The user's own bangs and the `:bangs` search page. Bang lookup itself (custom →
//! built-in → Kagi's list) lives in `browser_core::bangs`.

use browser_core::{BangInfo, BangSource};

use crate::tabs::{Tab, TabContent, TabNav};
use crate::{vim, App};

/// The most rows `:bangs <word>` lists.
const SEARCH_LIMIT: usize = 500;

impl App {
    /// Expand a `!key` in an open/search target, with the user's bangs first.
    pub(crate) fn expand_bang(&self, target: &str) -> Option<String> {
        browser_core::expand_bang_with(target, &self.config.bangs)
    }

    /// Add (or replace) one of the user's bangs and persist it.
    pub(crate) fn set_bang(&mut self, key: &str, url: &str) -> Result<String, String> {
        let (key, url) = browser_core::normalize_custom_bang(key, url)?;
        let replaced = browser_core::find_bang(&key, &self.config.bangs)
            .filter(|b| b.source != BangSource::Custom)
            .map(|b| format!(" (instead of {})", b.name))
            .unwrap_or_default();
        self.config.bangs.insert(key.clone(), url.clone());
        crate::config::save(&self.config);
        Ok(format!("bang !{key} → {url}{replaced}"))
    }

    /// Remove one of the user's bangs. Built-in and Kagi bangs can't be removed, only
    /// replaced by one of your own.
    pub(crate) fn remove_bang(&mut self, key: &str) -> Result<String, String> {
        let key = key.trim().trim_start_matches('!').to_lowercase();
        if self.config.bangs.remove(&key).is_none() {
            return Err(format!("!{key} isn't one of your bangs"));
        }
        crate::config::save(&self.config);
        Ok(match browser_core::find_bang(&key, &self.config.bangs) {
            Some(b) => format!("removed your !{key}; it's {} again", b.name),
            None => format!("removed !{key}"),
        })
    }

    /// `:bang <key>` — say what a bang does.
    pub(crate) fn describe_bang(&mut self, key: &str) {
        match browser_core::find_bang(key, &self.config.bangs) {
            Some(b) => self.set_status(format!(
                "!{} → {} ({}): {}",
                b.triggers[0],
                b.name,
                source_label(b.source),
                b.search
            )),
            None => self.set_status(format!(
                "no bang !{} — :bang {} <url with %s> adds one",
                key.trim_start_matches('!'),
                key.trim_start_matches('!')
            )),
        }
    }

    /// `:bangs [word]` — your bangs and the built-in ones, or every bang whose key or
    /// name contains `word`, in a vim tab.
    pub(crate) fn open_bangs_page(&mut self, word: &str) {
        let word = word.trim();
        let found = if word.is_empty() {
            let mut list = browser_core::search_bangs("", &self.config.bangs, usize::MAX);
            list.retain(|b| b.source != BangSource::Kagi);
            list
        } else {
            browser_core::search_bangs(word, &self.config.bangs, SEARCH_LIMIT)
        };
        let lines = bang_lines(word, &found, self.config.bangs.len());
        self.place_tab(
            Tab {
                url: "browser://bangs".into(),
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
}

fn source_label(source: BangSource) -> &'static str {
    match source {
        BangSource::Custom => "yours",
        BangSource::BuiltIn => "built-in",
        BangSource::Kagi => "Kagi",
    }
}

/// The `:bangs` page: a header, then one row per bang — its keys, name, where it's
/// defined and its search URL.
fn bang_lines(word: &str, found: &[BangInfo], yours: usize) -> Vec<String> {
    let mut lines = vec![
        if word.is_empty() {
            format!(
                "bangs — {yours} yours, {} built-in, and {} more from Kagi's list ({})    \
                 (:bangs <word> searches them all · :bang <key> <url with %s> adds yours · \
                 :unbang <key> removes it)",
                browser_core::bang_list().len(),
                browser_core::kagi_bang_count(),
                browser_core::KAGI_VERSION,
            )
        } else if found.len() >= SEARCH_LIMIT {
            format!("bangs matching \"{word}\" — the first {SEARCH_LIMIT}")
        } else {
            format!("bangs matching \"{word}\" — {}", found.len())
        },
        String::new(),
    ];
    let key_width = found
        .iter()
        .map(|b| keys(b).chars().count())
        .max()
        .unwrap_or(0)
        .min(28);
    lines.extend(found.iter().map(|b| {
        format!(
            "{:<key_width$}  {}  [{}]  {}",
            keys(b),
            b.name,
            source_label(b.source),
            b.search
        )
    }));
    if found.is_empty() {
        lines.push(format!("no bangs match \"{word}\""));
    }
    lines
}

fn keys(b: &BangInfo) -> String {
    b.triggers
        .iter()
        .map(|t| format!("!{t}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bangs_page_lists_keys_name_source_and_url() {
        let mut mine = browser_core::CustomBangs::new();
        mine.insert(
            "osrs".into(),
            "https://oldschool.runescape.wiki/?search=%s".into(),
        );
        let found = browser_core::search_bangs("osrs", &mine, 10);
        let lines = bang_lines("osrs", &found, 1);
        assert!(lines[0].starts_with("bangs matching \"osrs\""));
        assert!(lines[2].starts_with("!osrs"), "{}", lines[2]);
        assert!(lines[2].contains("[yours]"), "{}", lines[2]);
        assert!(bang_lines("zzzznothing", &[], 0)
            .last()
            .unwrap()
            .starts_with("no bangs match"));
    }
}
