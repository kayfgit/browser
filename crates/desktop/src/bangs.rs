//! Changing the bang list and the `:bangs` page. The bangs are Kagi's list; the user
//! can add their own (`:bang`), switch Kagi's off (`:unbang`) and undo either
//! (`:resetbangs`). Lookup itself lives in `browser_core::bangs`.

use browser_core::{BangInfo, BangSource};

use crate::tabs::{Tab, TabContent, TabNav};
use crate::{vim, App};

/// The most rows `:bangs <word>` lists.
const SEARCH_LIMIT: usize = 500;

impl App {
    /// Expand a `!key` in an open/search target, with the user's changes applied.
    pub(crate) fn expand_bang(&self, target: &str) -> Option<String> {
        browser_core::expand_bang_with(target, &self.config.bangs)
    }

    /// Add (or replace) one of the user's bangs and persist it.
    pub(crate) fn set_bang(&mut self, key: &str, url: &str) -> Result<String, String> {
        let (key, url) = browser_core::normalize_custom_bang(key, url)?;
        let replaced = browser_core::kagi_bang(&key)
            .map(|b| {
                format!(
                    " (instead of Kagi's {}; :resetbangs {key} brings it back)",
                    b.name
                )
            })
            .unwrap_or_default();
        self.config.bangs.disabled.remove(&key);
        self.config.bangs.custom.insert(key.clone(), url.clone());
        crate::config::save(&self.config);
        Ok(format!("bang !{key} → {url}{replaced}"))
    }

    /// Remove a bang: one of the user's own, or switch off one of Kagi's.
    pub(crate) fn remove_bang(&mut self, key: &str) -> Result<String, String> {
        let key = key.trim().trim_start_matches('!').to_lowercase();
        let kagi = browser_core::kagi_bang(&key);
        let msg = if self.config.bangs.custom.remove(&key).is_some() {
            match &kagi {
                Some(b) if !self.config.bangs.disabled.contains(&key) => {
                    format!("removed your !{key}; it's Kagi's {} again", b.name)
                }
                _ => format!("removed your !{key}"),
            }
        } else if let Some(b) = kagi.filter(|_| !self.config.bangs.disabled.contains(&key)) {
            self.config.bangs.disabled.insert(key.clone());
            format!(
                "switched off !{key} ({}) — :resetbangs {key} brings it back",
                b.name
            )
        } else {
            return Err(format!("no bang !{key}"));
        };
        crate::config::save(&self.config);
        Ok(msg)
    }

    /// Undo changes to the bangs: for `key`, or all of them when it's empty. Brings
    /// back switched-off Kagi bangs and removes the user's own.
    pub(crate) fn reset_bangs(&mut self, key: &str) -> Result<String, String> {
        let key = key.trim().trim_start_matches('!').to_lowercase();
        let bangs = &mut self.config.bangs;
        let msg = if key.is_empty() {
            let (mine, off) = (bangs.custom.len(), bangs.disabled.len());
            if mine + off == 0 {
                return Ok("the bangs are already Kagi's list, unchanged".into());
            }
            *bangs = Default::default();
            format!("bangs reset to Kagi's list: brought back {off}, removed {mine} of yours")
        } else {
            let was_mine = bangs.custom.remove(&key).is_some();
            let was_off = bangs.disabled.remove(&key);
            if !was_mine && !was_off {
                return Err(format!("!{key} hasn't been changed"));
            }
            match browser_core::kagi_bang(&key) {
                Some(b) => format!("!{key} is Kagi's {} again", b.name),
                None => format!("removed your !{key}"),
            }
        };
        crate::config::save(&self.config);
        Ok(msg)
    }

    /// `:bang <key>` — say what a bang does.
    pub(crate) fn describe_bang(&mut self, key: &str) {
        let key = key.trim_start_matches('!');
        match browser_core::find_bang(key, &self.config.bangs) {
            Some(b) => self.set_status(format!(
                "!{} → {} ({}): {}",
                b.triggers[0],
                b.name,
                source_label(b.source),
                b.search
            )),
            None if self.config.bangs.disabled.contains(&key.to_lowercase()) => self.set_status(
                format!("!{key} is switched off — :resetbangs {key} brings it back"),
            ),
            None => self.set_status(format!(
                "no bang !{key} — :bang {key} <url with %s> adds one"
            )),
        }
    }

    /// `:bangs [word]` — every bang, or those whose key or name contains `word`, in a
    /// vim tab (where `/` searches too).
    pub(crate) fn open_bangs_page(&mut self, word: &str) {
        let word = word.trim();
        let limit = if word.is_empty() {
            usize::MAX
        } else {
            SEARCH_LIMIT
        };
        let found = browser_core::search_bangs(word, &self.config.bangs, limit);
        let disabled: Vec<&str> = self
            .config
            .bangs
            .disabled
            .iter()
            .map(String::as_str)
            .collect();
        let lines = bang_lines(word, &found, &disabled);
        self.place_tab(
            Tab {
                id: crate::layout::TabId::new(),
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
        BangSource::Kagi => "Kagi",
    }
}

/// The `:bangs` page: a header, then one row per bang — its keys, name, where it's
/// defined and its search URL.
fn bang_lines(word: &str, found: &[BangInfo], disabled: &[&str]) -> Vec<String> {
    let title = if word.is_empty() {
        format!(
            "bangs — {} (Kagi's list {}, and yours)    (/ searches · :bangs <word> filters · \
             :bang <key> <url with %s> adds · :unbang <key> removes · :resetbangs undoes)",
            found.len(),
            browser_core::KAGI_VERSION,
        )
    } else if found.len() >= SEARCH_LIMIT {
        format!("bangs matching \"{word}\" — the first {SEARCH_LIMIT}")
    } else {
        format!("bangs matching \"{word}\" — {}", found.len())
    };
    let mut lines = vec![title];
    if !disabled.is_empty() {
        let keys: Vec<String> = disabled.iter().map(|k| format!("!{k}")).collect();
        lines.push(format!(
            "switched off: {}    (:resetbangs <key> brings one back)",
            keys.join(" ")
        ));
    }
    lines.push(String::new());
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
        let mut mine = browser_core::BangOverrides::default();
        mine.custom.insert(
            "osrs".into(),
            "https://oldschool.runescape.wiki/?search=%s".into(),
        );
        mine.disabled.insert("yt".into());
        let found = browser_core::search_bangs("osrs", &mine, 10);
        let lines = bang_lines("osrs", &found, &["yt"]);
        assert!(lines[0].starts_with("bangs matching \"osrs\""));
        assert!(lines[1].starts_with("switched off: !yt"));
        assert!(lines[3].starts_with("!osrs"), "{}", lines[3]);
        assert!(lines[3].contains("[yours]"), "{}", lines[3]);
        assert!(bang_lines("zzzznothing", &[], &[])
            .last()
            .unwrap()
            .starts_with("no bangs match"));
    }
}
