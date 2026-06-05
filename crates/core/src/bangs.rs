//! DuckDuckGo-style "bang" shortcuts.
//!
//! A `!key` token anywhere in the input redirects the remaining words to a
//! specific site's search (e.g. `!yt lofi` → a YouTube search for "lofi"). With
//! no query after the bang, the site's home page is opened instead. Unknown
//! bangs are ignored so the input falls back to the normal open/search routing.

use crate::intent::search_url;

/// A single bang: its trigger key(s) (the first is the canonical one), the
/// search-URL template (`%s` = percent-encoded query), the home page to open
/// when no query follows, and a short description for the help listing.
struct Bang {
    keys: &'static [&'static str],
    search: &'static str,
    home: &'static str,
    desc: &'static str,
}

/// The built-in bang table. Keys are matched case-insensitively.
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

/// Look up a bang by key (case-insensitive).
fn lookup(key: &str) -> Option<&'static Bang> {
    let key = key.to_ascii_lowercase();
    BANGS.iter().find(|b| b.keys.iter().any(|k| *k == key))
}

/// If `input` contains a `!key` token for a known bang, expand it into a URL:
/// the remaining words become the query (→ the bang's search URL), or — when no
/// words remain — the bang's home page. Returns `None` if no known bang token is
/// present, so callers fall back to the normal open/search routing.
pub fn expand_bang(input: &str) -> Option<String> {
    let mut bang: Option<&Bang> = None;
    let mut rest: Vec<&str> = Vec::new();
    for tok in input.split_whitespace() {
        if bang.is_none() {
            if let Some(key) = tok.strip_prefix('!') {
                if let Some(found) = lookup(key) {
                    bang = Some(found);
                    continue;
                }
            }
        }
        rest.push(tok);
    }
    let bang = bang?;
    let query = rest.join(" ");
    if query.is_empty() {
        Some(bang.home.to_string())
    } else {
        Some(search_url(bang.search, &query))
    }
}

/// `(canonical key, description)` for every bang, for the help/commands page.
pub fn bang_list() -> Vec<(&'static str, &'static str)> {
    BANGS.iter().map(|b| (b.keys[0], b.desc)).collect()
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
        assert_eq!(expand_bang("!osrs"), Some("https://oldschool.runescape.wiki/".into()));
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
}
