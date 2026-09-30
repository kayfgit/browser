//! DuckDuckGo-style "bang" shortcuts.
//!
//! A `!key` token anywhere in the input redirects the remaining words to a
//! specific site's search (e.g. `!yt lofi` → a YouTube search for "lofi"). With
//! no query after the bang, the site's home page is opened instead. Unknown
//! bangs are ignored so the input falls back to the normal open/search routing.
//!
//! A key is looked up in three places, first match wins: the user's own bangs,
//! the hand-picked built-in table below, then Kagi's list of about 13,000
//! (<https://github.com/kagisearch/bangs>, MIT; the list Helium ships too),
//! compiled into the executable by `build.rs` from `data/kagi-bangs.json`.

use std::collections::BTreeMap;

use crate::intent::search_url;

/// The user's own bangs: key → search URL template (`%s` = the query).
pub type CustomBangs = BTreeMap<String, String>;

/// A single bang: its trigger key(s) (the first is the canonical one), the
/// search-URL template (`%s` = percent-encoded query), the home page to open
/// when no query follows, and a short description for the help listing.
struct Bang {
    keys: &'static [&'static str],
    search: &'static str,
    home: &'static str,
    desc: &'static str,
}

/// The built-in bang table: hand-picked, and ahead of Kagi's list for the same key.
const BANGS: &[Bang] = &[
    Bang {
        keys: &["yt", "youtube"],
        search: "https://www.youtube.com/results?search_query=%s",
        home: "https://www.youtube.com/",
        desc: "YouTube",
    },
    Bang {
        keys: &["osrs"],
        search: "https://oldschool.runescape.wiki/?search=%s",
        home: "https://oldschool.runescape.wiki/",
        desc: "Old School RuneScape Wiki",
    },
    Bang {
        keys: &["rs", "rswiki"],
        search: "https://runescape.wiki/?search=%s",
        home: "https://runescape.wiki/",
        desc: "RuneScape Wiki",
    },
    Bang {
        keys: &["w", "wiki", "wikipedia"],
        search: "https://en.wikipedia.org/w/index.php?search=%s",
        home: "https://en.wikipedia.org/",
        desc: "Wikipedia",
    },
    Bang {
        keys: &["g", "google"],
        search: "https://www.google.com/search?q=%s",
        home: "https://www.google.com/",
        desc: "Google",
    },
    Bang {
        keys: &["ddg"],
        search: "https://duckduckgo.com/?q=%s",
        home: "https://duckduckgo.com/",
        desc: "DuckDuckGo",
    },
    Bang {
        keys: &["gh", "github"],
        search: "https://github.com/search?q=%s&type=repositories",
        home: "https://github.com/",
        desc: "GitHub",
    },
    Bang {
        keys: &["so"],
        search: "https://stackoverflow.com/search?q=%s",
        home: "https://stackoverflow.com/",
        desc: "Stack Overflow",
    },
    Bang {
        keys: &["reddit", "r"],
        search: "https://www.reddit.com/search/?q=%s",
        home: "https://www.reddit.com/",
        desc: "Reddit",
    },
    Bang {
        keys: &["cr", "crates"],
        search: "https://crates.io/search?q=%s",
        home: "https://crates.io/",
        desc: "crates.io",
    },
    Bang {
        keys: &["dr", "docs"],
        search: "https://docs.rs/releases/search?query=%s",
        home: "https://docs.rs/",
        desc: "docs.rs",
    },
    Bang {
        keys: &["mdn"],
        search: "https://developer.mozilla.org/en-US/search?q=%s",
        home: "https://developer.mozilla.org/",
        desc: "MDN Web Docs",
    },
    Bang {
        keys: &["npm"],
        search: "https://www.npmjs.com/search?q=%s",
        home: "https://www.npmjs.com/",
        desc: "npm",
    },
    Bang {
        keys: &["wa"],
        search: "https://www.wolframalpha.com/input?i=%s",
        home: "https://www.wolframalpha.com/",
        desc: "Wolfram Alpha",
    },
    Bang {
        keys: &["maps", "map"],
        search: "https://www.google.com/maps/search/%s",
        home: "https://www.google.com/maps",
        desc: "Google Maps",
    },
    Bang {
        keys: &["a", "amazon"],
        search: "https://www.amazon.com/s?k=%s",
        home: "https://www.amazon.com/",
        desc: "Amazon",
    },
    Bang {
        keys: &["imdb"],
        search: "https://www.imdb.com/find/?q=%s",
        home: "https://www.imdb.com/",
        desc: "IMDb",
    },
    Bang {
        keys: &["tw", "x"],
        search: "https://twitter.com/search?q=%s",
        home: "https://twitter.com/",
        desc: "Twitter / X",
    },
];

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
    /// The built-in table.
    BuiltIn,
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
    BuiltIn(&'static Bang),
    Kagi(usize),
}

fn resolve<'a>(key: &str, custom: &'a CustomBangs) -> Option<Found<'a>> {
    let key = key.to_lowercase();
    if let Some(template) = custom.get(&key) {
        return Some(Found::Custom(template));
    }
    if let Some(bang) = BANGS.iter().find(|b| b.keys.contains(&key.as_str())) {
        return Some(Found::BuiltIn(bang));
    }
    kagi::find(&key).map(Found::Kagi)
}

impl Found<'_> {
    fn url(&self, query: &str) -> String {
        match self {
            Found::Custom(template) if query.is_empty() => origin(template) + "/",
            Found::Custom(template) => search_url(template, query),
            Found::BuiltIn(bang) if query.is_empty() => bang.home.to_string(),
            Found::BuiltIn(bang) => search_url(bang.search, query),
            Found::Kagi(bang) => kagi::expand(*bang, query),
        }
    }
}

/// If `input` contains a `!key` token for a known bang, expand it into a URL: the
/// remaining words become the query (→ the bang's search URL), or — when no words
/// remain — the bang's home page. Returns `None` if no known bang token is present,
/// so callers fall back to the normal open/search routing.
pub fn expand_bang(input: &str) -> Option<String> {
    expand_bang_with(input, &CustomBangs::new())
}

/// [`expand_bang`] with the user's own bangs, which take precedence.
pub fn expand_bang_with(input: &str, custom: &CustomBangs) -> Option<String> {
    let mut found = None;
    let mut rest: Vec<&str> = Vec::new();
    for tok in input.split_whitespace() {
        if found.is_none() {
            if let Some(key) = tok.strip_prefix('!') {
                if let Some(bang) = resolve(key, custom) {
                    found = Some(bang);
                    continue;
                }
            }
        }
        rest.push(tok);
    }
    Some(found?.url(&rest.join(" ")))
}

/// What `!key` does: its name, template and where it's defined.
pub fn find_bang(key: &str, custom: &CustomBangs) -> Option<BangInfo> {
    let key = key.trim_start_matches('!').to_lowercase();
    Some(match resolve(&key, custom)? {
        Found::Custom(template) => BangInfo {
            triggers: vec![key],
            name: crate::intent::host_of(template).unwrap_or_default(),
            search: template.to_string(),
            source: BangSource::Custom,
        },
        Found::BuiltIn(bang) => BangInfo {
            triggers: bang.keys.iter().map(|k| k.to_string()).collect(),
            name: bang.desc.to_string(),
            search: bang.search.to_string(),
            source: BangSource::BuiltIn,
        },
        Found::Kagi(bang) => BangInfo {
            triggers: kagi_triggers(bang, &key),
            name: kagi::name(bang).to_string(),
            search: kagi::template(bang).replace("{{{s}}}", "%s"),
            source: BangSource::Kagi,
        },
    })
}

/// Kagi bang `bang`'s triggers, `first` leading.
fn kagi_triggers(bang: usize, first: &str) -> Vec<String> {
    let mut keys = vec![first.to_string()];
    keys.extend(
        kagi::triggers()
            .filter(|&(t, b)| b == bang && t != first)
            .map(|(t, _)| t.to_string()),
    );
    keys
}

/// Bangs whose key or name contains `word` (case-insensitive), at most `limit`:
/// the user's own, then built-in, then Kagi's, exact key matches first within each.
pub fn search_bangs(word: &str, custom: &CustomBangs, limit: usize) -> Vec<BangInfo> {
    let word = word.trim().trim_start_matches('!').to_lowercase();
    let hit = |key: &str, name: &str| key.contains(&word) || name.to_lowercase().contains(&word);
    let mut out: Vec<BangInfo> = Vec::new();
    for (key, template) in custom {
        if hit(key, template) {
            out.push(find_bang(key, custom).expect("a custom bang resolves"));
        }
    }
    for bang in BANGS {
        if bang.keys.iter().any(|k| hit(k, bang.desc)) {
            out.push(BangInfo {
                triggers: bang.keys.iter().map(|k| k.to_string()).collect(),
                name: bang.desc.to_string(),
                search: bang.search.to_string(),
                source: BangSource::BuiltIn,
            });
        }
    }
    // Kagi: gather each matching bang's triggers, exact key first, then by key order.
    let mut matched: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    let mut exact = None;
    for (key, bang) in kagi::triggers() {
        if key == word {
            exact = Some(bang);
        }
        if hit(key, kagi::name(bang)) {
            matched.entry(bang).or_default().push(key);
        }
    }
    let mut order: Vec<usize> = exact.into_iter().collect();
    order.extend(matched.keys().copied().filter(|&b| Some(b) != exact));
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

/// `(canonical key, description)` for every built-in bang, for the help page.
pub fn bang_list() -> Vec<(&'static str, &'static str)> {
    BANGS.iter().map(|b| (b.keys[0], b.desc)).collect()
}

/// The search-URL template (`%s` = query) for a bang key — lets `:search <name>`
/// (e.g. `:search ddg`, `:search google`) reuse the bang tables instead of needing a
/// full `%s` URL. Returns `None` for an unknown key.
pub fn bang_search_template(key: &str) -> Option<String> {
    find_bang(key, &CustomBangs::new()).map(|b| b.search)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn bang_can_trail_the_query() {
        assert_eq!(
            expand_bang("dragon scimitar !osrs"),
            Some("https://oldschool.runescape.wiki/?search=dragon+scimitar".into())
        );
    }

    #[test]
    fn unknown_or_absent_bang_is_none() {
        assert_eq!(expand_bang("!nope something"), None);
        assert_eq!(expand_bang("just a search"), None);
        assert_eq!(expand_bang("example.com"), None);
    }

    #[test]
    fn keys_are_case_insensitive() {
        assert!(expand_bang("!YT cats").is_some());
    }

    fn custom(pairs: &[(&str, &str)]) -> CustomBangs {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn kagi_bangs_fill_in_behind_the_built_in_ones() {
        assert!(kagi_bang_count() > 10_000);
        // Built in: our template wins over Kagi's for the same key.
        assert_eq!(
            expand_bang("!w rust"),
            Some("https://en.wikipedia.org/w/index.php?search=rust".into())
        );
        // Only in Kagi's list: MDN.
        let mdn = expand_bang("!mdn array map").unwrap();
        assert!(mdn.contains("developer.mozilla.org"), "{mdn}");
        assert!(
            mdn.contains("array+map") || mdn.contains("array%20map"),
            "{mdn}"
        );
    }

    #[test]
    fn kagi_flags_pick_the_home_page_and_space_encoding() {
        let home = expand_bang("!mdn").unwrap();
        assert_eq!(home, "https://developer.mozilla.org/");
        let hn = find_bang("hn", &CustomBangs::new()).unwrap();
        assert_eq!(hn.source, BangSource::Kagi);
    }

    #[test]
    fn custom_bangs_win_and_open_their_site_without_a_query() {
        let mine = custom(&[("w", "https://wiki.example/find?q=%s")]);
        assert_eq!(
            expand_bang_with("!w two words", &mine),
            Some("https://wiki.example/find?q=two+words".into())
        );
        assert_eq!(
            expand_bang_with("!W", &mine),
            Some("https://wiki.example/".into())
        );
        assert_eq!(find_bang("!w", &mine).unwrap().source, BangSource::Custom);
    }

    #[test]
    fn search_lists_exact_keys_first() {
        let found = search_bangs("mdn", &CustomBangs::new(), 50);
        let first = found.iter().find(|b| b.source == BangSource::Kagi).unwrap();
        assert_eq!(first.triggers[0], "mdn");
        assert!(search_bangs("youtube", &CustomBangs::new(), 5).len() <= 5);
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
