//! Network-level domain blocklist — a Brave-style [adblock-rust] [`Engine`] that matches
//! navigation URLs against EasyList-format filter rules. This is what stops the
//! forced-redirect / popunder DOMAINS (`proceedflow.com`, `searchapp.space`, …) the way
//! uBlock Origin does: by NAME, race-free — not by the timing heuristic in `tabs.rs`,
//! which now only backstops brand-new domains the lists haven't caught yet.
//!
//! The lists (EasyList + EasyPrivacy + Peter Lowe's, network rules only — cosmetic lines
//! were stripped since cosmetic filtering is done page-side) are bundled and compiled
//! into the engine on a BACKGROUND THREAD at startup, so the ~100k-rule parse never
//! hitches the UI. A small curated [`SUPPLEMENT`](self) covers domains not yet upstream.
//!
//! [adblock-rust]: https://github.com/brave/adblock-rust

use std::sync::{Arc, RwLock};

use adblock::lists::{FilterSet, ParseOptions};
use adblock::request::Request;
use adblock::Engine;
use tao::event_loop::EventLoopProxy;

use crate::UserEvent;

const SUPPLEMENT: &str = include_str!("../assets/blocklist-extra.txt");
const EASYLIST: &str = include_str!("../assets/easylist.txt");
const EASYPRIVACY: &str = include_str!("../assets/easyprivacy.txt");
const YOYO: &str = include_str!("../assets/yoyo.txt");

/// The shared, swappable network blocker. `None` until the engine finishes building (a
/// beat after launch); navigations fall back to the heuristic until then. `Engine` is
/// `Send + Sync` here because we build the crate with `single-thread` off (see
/// `Cargo.toml`), so a plain `Arc<RwLock<…>>` shares it across the build thread and the
/// per-webview navigation handlers.
pub(crate) type SharedBlocker = Arc<RwLock<Option<Engine>>>;

pub(crate) fn new_shared() -> SharedBlocker {
    Arc::new(RwLock::new(None))
}

/// Compile the engine off-thread from the bundled lists, store it, and post
/// [`UserEvent::BlocklistReady`] when it's live.
pub(crate) fn spawn_build(blocker: SharedBlocker, proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        let mut set = FilterSet::new(false);
        for text in [SUPPLEMENT, EASYLIST, EASYPRIVACY, YOYO] {
            let rules: Vec<String> = text.lines().map(str::to_string).collect();
            set.add_filters(&rules, ParseOptions::default());
        }
        let engine = Engine::from_filter_set(set, true);
        if let Ok(mut guard) = blocker.write() {
            *guard = Some(engine);
        }
        let _ = proxy.send_event(UserEvent::BlocklistReady);
    });
}

/// Whether a top-level navigation to `url` (from page `source`) is blocked by the list
/// engine. `source` lets `$third-party` rules resolve; an empty source (first load)
/// falls back to the URL itself. Returns `false` while the engine is still building.
pub(crate) fn blocks_navigation(blocker: &SharedBlocker, url: &str, source: &str) -> bool {
    let Ok(guard) = blocker.read() else {
        return false;
    };
    let Some(engine) = guard.as_ref() else {
        return false;
    };
    let src = if source.is_empty() { url } else { source };
    match Request::new(url, src, "document") {
        Ok(req) => engine.check_network_request(&req).matched,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_from(texts: &[&str]) -> SharedBlocker {
        let mut set = FilterSet::new(false);
        for t in texts {
            let rules: Vec<String> = t.lines().map(str::to_string).collect();
            set.add_filters(&rules, ParseOptions::default());
        }
        Arc::new(RwLock::new(Some(Engine::from_filter_set(set, true))))
    }

    #[test]
    fn supplement_blocks_scam_and_popunder_domains() {
        let b = engine_from(&[SUPPLEMENT]);
        // The exact forced-redirect domain from the bug report — and its subdomains.
        assert!(blocks_navigation(&b, "https://proceedflow.com/click?key=abc", "https://animepahe.pw/"));
        assert!(blocks_navigation(&b, "https://www.popads.net/foo", "https://stream.test/"));
        // A normal destination is left alone.
        assert!(!blocks_navigation(&b, "https://example.com/", "https://animepahe.pw/"));
        assert!(!blocks_navigation(&b, "https://github.com/rust-lang/rust", ""));
    }

    #[test]
    fn easylist_blocks_known_redirect_domain() {
        // Proves the bundled EasyList is parsed and that a top-level "document"
        // navigation to a listed domain (`||searchapp.space^`) is actually blocked.
        let b = engine_from(&[EASYLIST]);
        assert!(blocks_navigation(&b, "https://best.searchapp.space/abc?zoneid=3", "https://animepahe.pw/"));
        assert!(!blocks_navigation(&b, "https://en.wikipedia.org/wiki/Rust", "https://animepahe.pw/"));
    }
}
