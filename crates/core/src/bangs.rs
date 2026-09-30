//! DuckDuckGo-style "bang" shortcuts.
//!
//! A `!key` token anywhere in the input redirects the remaining words to a
//! specific site's search (e.g. `!yt lofi` → a YouTube search for "lofi"). With
//! no query after the bang, the site's home page is opened instead. Unknown
//! bangs are ignored so the input falls back to the normal open/search routing.
//!
//! The bangs are Kagi's list (<https://github.com/kagisearch/bangs>, MIT; the one
//! Helium ships too), about 13,000 keys compiled into the executable by `build.rs`
//! from `data/kagi-bangs.json`. On top of it, [`BangOverrides`] holds the user's own
//! bangs, which win over Kagi's for the same key, and the Kagi ones they switched off.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::intent::search_url;

/// The user's changes to Kagi's list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BangOverrides {
    /// Kagi bangs switched off (`:unbang`), by key.
    pub disabled: BTreeSet<String>,
    /// The user's own bangs: key (without `!`) → search URL with `%s` for the query.
    pub custom: BTreeMap<String, String>,
}

mod kagi {
    include!(concat!(env!("OUT_DIR"), "/kagi_bangs.rs"));

    // Kagi's format flags. Without `fmt` in the source, all of them apply.
    /// With no query, open the site's root instead of the template's path.
    const OPEN_BASE_PATH: u8 = 1;
    /// With no query, open the snap domain (`ad`) if the bang has one.
    const OPEN_SNAP_DOMAIN: u8 = 2;
    /// Percent-encode the query.
    const URL_ENCODE_PLACEHOLDER: u8 = 4;
    /// Encode spaces as `+` rather than `%20`.
    const URL_ENCODE_SPACE_TO_PLUS: u8 = 8;

    fn text(at: u32, len: u32) -> &'static str {
        &BLOB[at as usize..(at + len) as usize]
    }

    /// The index of the bang with this (lowercase) trigger.
    pub(super) fn find(key: &str) -> Option<usize> {
        TRIGGERS
            .binary_search_by(|&(at, len, _)| text(at, len).cmp(key))
            .ok()
            .map(|i| TRIGGERS[i].2 as usize)
    }

    pub(super) fn name(bang: usize) -> &'static str {
        let (at, len, ..) = BANGS[bang];
        text(at, len)
    }

    pub(super) fn template(bang: usize) -> &'static str {
        let (_, _, at, len, ..) = BANGS[bang];
        text(at, len)
    }

    pub(super) fn count() -> usize {
        TRIGGERS.len()
    }

    /// Every trigger with its bang, in trigger order.
    pub(super) fn triggers() -> impl Iterator<Item = (&'static str, usize)> {
        TRIGGERS
            .iter()
            .map(|&(at, len, bang)| (text(at, len), bang as usize))
    }

    /// The URL for `query` (empty: the bang's home page), following its flags.
    pub(super) fn expand(bang: usize, query: &str) -> String {
        let (_, _, url_at, url_len, snap_at, snap_len, flags) = BANGS[bang];
        let template = text(url_at, url_len);
        if query.is_empty() {
            let snap = text(snap_at, snap_len);
            if flags & OPEN_SNAP_DOMAIN != 0 && !snap.is_empty() {
                return format!("https://{snap}/");
            }
            if flags & OPEN_BASE_PATH != 0 {
                return super::origin(template) + "/";
            }
        }
        let encoded = if flags & URL_ENCODE_PLACEHOLDER != 0 {
            let form: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
            if flags & URL_ENCODE_SPACE_TO_PLUS != 0 {
                form
            } else {
                form.replace('+', "%20")
            }
        } else if flags & URL_ENCODE_SPACE_TO_PLUS != 0 {
            query.replace(' ', "+")
        } else {
            query.to_string()
        };
        template.replace("{{{s}}}", &encoded)
    }
}

pub use kagi::KAGI_VERSION;

/// How many triggers the bundled Kagi list has.
pub fn kagi_bang_count() -> usize {
    kagi::count()
}

/// `scheme://host` of a URL template.
fn origin(url: &str) -> String {
    let rest_at = url.find("://").map_or(0, |i| i + 3);
    let host_end = url[rest_at..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |i| rest_at + i);
    url[..host_end].to_string()
}

/// Where a bang is defined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BangSource {
    /// Added by the user (`:bang`).
    Custom,
    /// Kagi's list.
    Kagi,
}

/// A bang found by [`find_bang`] or [`search_bangs`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BangInfo {
    /// Its keys, the one that matched first.
    pub triggers: Vec<String>,
    /// The site or service it searches.
    pub name: String,
    /// Its search URL template, with `%s` for the query.
    pub search: String,
    pub source: BangSource,
}

enum Found<'a> {
    Custom(&'a str),
    Kagi(usize),
}

fn resolve<'a>(key: &str, user: &'a BangOverrides) -> Option<Found<'a>> {
    let key = key.to_lowercase();
    if let Some(template) = user.custom.get(&key) {
        return Some(Found::Custom(template));
    }
    if user.disabled.contains(&key) {
        return None;
    }
    kagi::find(&key).map(Found::Kagi)
}

impl Found<'_> {
    fn url(&self, query: &str) -> String {
        match self {
            Found::Custom(template) if query.is_empty() => origin(template) + "/",
            Found::Custom(template) => search_url(template, query),
            Found::Kagi(bang) => kagi::expand(*bang, query),
        }
    }
}

/// If `input` contains a `!key` token for a known bang, expand it into a URL: the
/// remaining words become the query (→ the bang's search URL), or — when no words
/// remain — the bang's home page. Returns `None` if no known bang token is present,
/// so callers fall back to the normal open/search routing.
pub fn expand_bang(input: &str) -> Option<String> {
    expand_bang_with(input, &BangOverrides::default())
}

/// [`expand_bang`] with the user's changes applied.
pub fn expand_bang_with(input: &str, user: &BangOverrides) -> Option<String> {
    let mut found = None;
    let mut rest: Vec<&str> = Vec::new();
    for tok in input.split_whitespace() {
        if found.is_none() {
            if let Some(key) = tok.strip_prefix('!') {
                if let Some(bang) = resolve(key, user) {
                    found = Some(bang);
                    continue;
                }
            }
        }
        rest.push(tok);
    }
    Some(found?.url(&rest.join(" ")))
}

/// What `!key` does, with the user's changes applied: its name, template and where
/// it's defined.
pub fn find_bang(key: &str, user: &BangOverrides) -> Option<BangInfo> {
    let key = key.trim_start_matches('!').to_lowercase();
    Some(match resolve(&key, user)? {
        Found::Custom(template) => BangInfo {
            triggers: vec![key],
            name: crate::intent::host_of(template).unwrap_or_default(),
            search: template.to_string(),
            source: BangSource::Custom,
        },
        Found::Kagi(bang) => kagi_info(bang, &key),
    })
}

/// Kagi's own `!key`, whatever the user changed.
pub fn kagi_bang(key: &str) -> Option<BangInfo> {
    find_bang(key, &BangOverrides::default())
}

fn kagi_info(bang: usize, first: &str) -> BangInfo {
    let mut triggers = vec![first.to_string()];
    triggers.extend(
        kagi::triggers()
            .filter(|&(t, b)| b == bang && t != first)
            .map(|(t, _)| t.to_string()),
    );
    BangInfo {
        triggers,
        name: kagi::name(bang).to_string(),
        search: kagi::template(bang).replace("{{{s}}}", "%s"),
        source: BangSource::Kagi,
    }
}

/// The bangs whose key or name contains `word` (case-insensitive; every bang for an
/// empty `word`), at most `limit`: the user's own first, then Kagi's, an exact key
/// match first. Kagi bangs the user switched off, or replaced with their own, are
/// left out.
pub fn search_bangs(word: &str, user: &BangOverrides, limit: usize) -> Vec<BangInfo> {
    let word = word.trim().trim_start_matches('!').to_lowercase();
    let hit = |key: &str, name: &str| key.contains(&word) || name.to_lowercase().contains(&word);
    let mut out: Vec<BangInfo> = user
        .custom
        .iter()
        .filter(|(key, template)| hit(key, template))
        .filter_map(|(key, _)| find_bang(key, user))
        .collect();
    // Kagi: each matching bang once, with its live keys; exact key first.
    let mut matched: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    let mut exact = None;
    for (key, bang) in kagi::triggers() {
        if user.disabled.contains(key) || user.custom.contains_key(key) {
            continue;
        }
        if key == word {
            exact = Some(bang);
        }
        if hit(key, kagi::name(bang)) {
            matched.entry(bang).or_default().push(key);
        }
    }
    let order = exact
        .into_iter()
        .chain(matched.keys().copied().filter(|&b| Some(b) != exact));
    for bang in order {
        if out.len() >= limit {
            break;
        }
        let mut triggers: Vec<String> = matched[&bang].iter().map(|t| t.to_string()).collect();
        if let Some(i) = triggers.iter().position(|t| *t == word) {
            triggers.swap(0, i);
        }
        out.push(BangInfo {
            triggers,
            name: kagi::name(bang).to_string(),
            search: kagi::template(bang).replace("{{{s}}}", "%s"),
            source: BangSource::Kagi,
        });
    }
    out.truncate(limit);
    out
}

/// Check and normalize a bang the user adds: the key without `!`, lowercase, made of
/// letters, digits, `.`, `-` and `_`; the URL http(s) with `%s` (or Kagi's `{{{s}}}`)
/// where the query goes. Returns `(key, template)`.
pub fn normalize_custom_bang(key: &str, url: &str) -> Result<(String, String), String> {
    let key = key.trim().trim_start_matches('!').to_lowercase();
    if key.is_empty() || key.chars().count() > 40 {
        return Err("a bang needs a short key, like osrs".into());
    }
    if !key
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(format!(
            "'{key}' can only use letters, digits, '.', '-' and '_'"
        ));
    }
    let url = url.trim().replace("{{{s}}}", "%s");
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("the bang's URL must start with https:// (or http://)".into());
    }
    if !url.contains("%s") {
        return Err(
            "put %s in the URL where the search goes, e.g. https://example.com/?q=%s".into(),
        );
    }
    Ok((key, url))
}

/// The search-URL template (`%s` = query) for Kagi's `!key` — lets `:search <name>`
/// (e.g. `:search ddg`, `:search g`) reuse the bang list instead of needing a full
/// `%s` URL. Returns `None` for an unknown key.
pub fn bang_search_template(key: &str) -> Option<String> {
    kagi_bang(key).map(|b| b.search)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(custom: &[(&str, &str)], disabled: &[&str]) -> BangOverrides {
        BangOverrides {
            custom: custom
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            disabled: disabled.iter().map(|k| k.to_string()).collect(),
        }
    }

    #[test]
    fn expands_leading_bang_with_query() {
        assert_eq!(
            expand_bang("!yt lofi beats"),
            Some("https://www.youtube.com/results?search_query=lofi+beats".into())
        );
    }

    #[test]
    fn bang_without_query_opens_home() {
        assert_eq!(
            expand_bang("!osrs"),
            Some("https://oldschool.runescape.wiki/".into())
        );
        assert_eq!(
            expand_bang("!mdn"),
            Some("https://developer.mozilla.org/".into())
        );
    }

    #[test]
    fn bang_can_trail_the_query() {
        let url = expand_bang("dragon scimitar !osrs").unwrap();
        assert!(
            url.starts_with("https://oldschool.runescape.wiki/?search=dragon+scimitar"),
            "{url}"
        );
    }

    #[test]
    fn unknown_or_absent_bang_is_none() {
        assert_eq!(expand_bang("!zzzznotabang something"), None);
        assert_eq!(expand_bang("just a search"), None);
        assert_eq!(expand_bang("example.com"), None);
    }

    #[test]
    fn keys_are_case_insensitive() {
        assert!(expand_bang("!YT cats").is_some());
    }

    #[test]
    fn the_whole_kagi_list_is_there() {
        assert!(kagi_bang_count() > 10_000);
        let mdn = expand_bang("!mdn array map").unwrap();
        assert!(mdn.starts_with("https://developer.mozilla.org/"), "{mdn}");
        assert!(mdn.contains("array+map"), "{mdn}");
        assert_eq!(kagi_bang("hn").unwrap().source, BangSource::Kagi);
    }

    #[test]
    fn your_bangs_win_and_switched_off_ones_are_gone() {
        let mine = user(&[("w", "https://wiki.example/find?q=%s")], &["yt"]);
        assert_eq!(
            expand_bang_with("!w two words", &mine),
            Some("https://wiki.example/find?q=two+words".into())
        );
        assert_eq!(
            expand_bang_with("!W", &mine),
            Some("https://wiki.example/".into())
        );
        assert_eq!(find_bang("!w", &mine).unwrap().source, BangSource::Custom);
        assert_eq!(expand_bang_with("!yt cats", &mine), None);
        assert!(
            kagi_bang("yt").is_some(),
            "Kagi's own is still known for a reset"
        );
    }

    #[test]
    fn search_lists_exact_keys_first_and_everything_when_empty() {
        let found = search_bangs("mdn", &BangOverrides::default(), 50);
        assert_eq!(found[0].triggers[0], "mdn");
        assert!(search_bangs("youtube", &BangOverrides::default(), 5).len() <= 5);
        let all = search_bangs("", &BangOverrides::default(), usize::MAX);
        assert!(all.len() > 10_000);
        let without_yt = search_bangs("", &user(&[], &["yt"]), usize::MAX);
        assert!(without_yt
            .iter()
            .all(|b| !b.triggers.contains(&"yt".to_string())));
    }

    #[test]
    fn custom_bangs_are_checked_and_normalized() {
        assert_eq!(
            normalize_custom_bang("!OSRS", "https://oldschool.runescape.wiki/?search={{{s}}}"),
            Ok((
                "osrs".into(),
                "https://oldschool.runescape.wiki/?search=%s".into()
            ))
        );
        assert!(normalize_custom_bang("a b", "https://x.test/?q=%s").is_err());
        assert!(normalize_custom_bang("x", "https://x.test/").is_err());
        assert!(normalize_custom_bang("x", "javascript:alert(%s)").is_err());
    }

    #[test]
    fn origin_keeps_scheme_and_host() {
        assert_eq!(origin("https://a.test/search?q=%s"), "https://a.test");
        assert_eq!(origin("https://a.test?q=%s"), "https://a.test");
    }
}
