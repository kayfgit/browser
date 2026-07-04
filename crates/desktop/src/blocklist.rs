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
use adblock::resources::Resource;
use adblock::Engine;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use tao::event_loop::EventLoopProxy;

use crate::UserEvent;

const SUPPLEMENT: &str = include_str!("../assets/blocklist-extra.txt");
const EASYLIST: &str = include_str!("../assets/easylist.txt");
const EASYPRIVACY: &str = include_str!("../assets/easyprivacy.txt");
const YOYO: &str = include_str!("../assets/yoyo.txt");
/// uBlock Origin's own filter lists (network rules only — cosmetic/`+js` lines stripped to
/// match the others), folded into native blocking. These carry the `$redirect` surrogate
/// rules (video-ad SDKs like `google-ima.js`, analytics/`apstag` stubs) and extra
/// badware/privacy coverage that EasyList/EasyPrivacy lack. See `assets/` for refresh steps.
const UBLOCK: &str = include_str!("../assets/ublock-filters.txt");
/// uBO's web-accessible "neutered surrogate" resources — the harmless stubs served in place
/// of a blocked ad/tracker script so the page doesn't break (a no-op IMA SDK still exposes
/// the API a video player calls, etc.). A `Vec<Resource>` in adblock-rust's own JSON shape,
/// loaded into the engine so `$redirect` rules resolve to a body instead of a bare 403.
const RESOURCES: &str = include_str!("../assets/adblock-resources.json");

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
        for text in [SUPPLEMENT, EASYLIST, EASYPRIVACY, YOYO, UBLOCK] {
            let rules: Vec<String> = text.lines().map(str::to_string).collect();
            set.add_filters(&rules, ParseOptions::default());
        }
        let mut engine = Engine::from_filter_set(set, true);
        // Load the surrogate library so `$redirect` rules resolve to a neutered stub body.
        // Best-effort: a malformed/absent resource file just means redirect rules fall back
        // to plain blocking (a 403), so this never fails the engine build.
        match serde_json::from_str::<Vec<Resource>>(RESOURCES) {
            Ok(resources) => engine.use_resources(resources),
            Err(e) => eprintln!("adblock: failed to parse redirect resources: {e}"),
        }
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
    blocks_request(blocker, url, source, "document")
}

/// Whether a SUB-RESOURCE request — a script, iframe, image, XHR/fetch, … of type
/// `request_type` (an adblock-rust content-type string: `script`, `subdocument`,
/// `xmlhttprequest`, `image`, …) — to `url` from page `source` is blocked by the list
/// engine. This is the uBlock-style network layer: it's what stops an ad/scam injector
/// script (and the fake-dialog / popunder code it carries) from ever LOADING, rather
/// than hiding the result after the fact. `source` is the top page's origin/URL so
/// `$third-party` and `$domain=` rules resolve; empty falls back to the URL itself.
/// Returns `false` while the engine is still building.
pub(crate) fn blocks_request(
    blocker: &SharedBlocker,
    url: &str,
    source: &str,
    request_type: &str,
) -> bool {
    let Ok(guard) = blocker.read() else {
        return false;
    };
    let Some(engine) = guard.as_ref() else {
        return false;
    };
    let src = if source.is_empty() { url } else { source };
    match Request::new(url, src, request_type) {
        Ok(req) => engine.check_network_request(&req).matched,
        Err(_) => false,
    }
}

/// What the sub-resource blocker should do with a request, once the engine has weighed in.
pub(crate) enum BlockAction {
    /// No matching rule — let the request reach the network untouched.
    Pass,
    /// A blocking rule matched with no surrogate — answer with an empty 403.
    Block,
    /// A `$redirect`/`$redirect-rule` matched — replace the response body with a neutered
    /// surrogate: serve `body` (decoded from the engine's `data:` URL) as `mime`, 200 OK.
    Redirect { mime: String, body: Vec<u8> },
}

/// Decide what to do with a sub-resource request, distinguishing a plain block from a
/// `$redirect` surrogate. This is [`blocks_request`]'s richer sibling: where that returns a
/// bool (used by the page/nav guards, which can only allow or deny), this also surfaces the
/// surrogate body so the network blocker can hand a harmless stub to the page — uBlock
/// Origin's trick for killing an ad/tracker script without breaking the code that expects
/// its API. Returns [`BlockAction::Pass`] while the engine is still building.
pub(crate) fn classify_request(
    blocker: &SharedBlocker,
    url: &str,
    source: &str,
    request_type: &str,
) -> BlockAction {
    let Ok(guard) = blocker.read() else {
        return BlockAction::Pass;
    };
    let Some(engine) = guard.as_ref() else {
        return BlockAction::Pass;
    };
    let src = if source.is_empty() { url } else { source };
    let Ok(req) = Request::new(url, src, request_type) else {
        return BlockAction::Pass;
    };
    let result = engine.check_network_request(&req);
    // A surrogate takes priority over a bare block: it both stops the original request and
    // gives the page a working stub. The engine only populates `redirect` when it actually
    // applies (a `$redirect` match, or a `$redirect-rule` whose companion block also matched).
    if let Some(data_url) = result.redirect {
        if let Some((mime, body)) = decode_data_url(&data_url) {
            return BlockAction::Redirect { mime, body };
        }
    }
    if result.matched {
        BlockAction::Block
    } else {
        BlockAction::Pass
    }
}

/// Split a `data:<mime>;base64,<payload>` URL (the form adblock-rust returns for a redirect
/// surrogate) into its MIME type and decoded bytes. `None` if it isn't a base64 data URL.
fn decode_data_url(data_url: &str) -> Option<(String, Vec<u8>)> {
    let rest = data_url.strip_prefix("data:")?;
    let (mime, b64) = rest.split_once(";base64,")?;
    let body = STANDARD.decode(b64).ok()?;
    Some((mime.to_string(), body))
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
    fn bundled_redirect_resources_parse() {
        // The vendored surrogate library must deserialize into adblock-rust's `Resource`
        // shape, or every `$redirect` rule silently degrades to a plain 403.
        let resources: Vec<Resource> =
            serde_json::from_str(RESOURCES).expect("adblock-resources.json must parse");
        assert!(!resources.is_empty());
        // The aliases the bundled rules actually reference must be resolvable.
        let names: Vec<&str> =
            resources.iter().flat_map(|r| std::iter::once(r.name.as_str()).chain(r.aliases.iter().map(String::as_str))).collect();
        for needed in ["noopjs", "google-ima.js", "google-analytics_analytics.js"] {
            assert!(names.contains(&needed), "missing surrogate: {needed}");
        }
    }

    #[test]
    fn ublock_redirect_rule_serves_surrogate_stub() {
        // End-to-end: a uBO `$redirect=google-ima.js` rule on a real ad-SDK URL must come
        // back as a Redirect surrogate (decoded body + mime), not a bare Block — that's what
        // keeps a video player working instead of 403-ing its IMA SDK.
        let mut set = FilterSet::new(false);
        let rules: Vec<String> = UBLOCK.lines().map(str::to_string).collect();
        set.add_filters(&rules, ParseOptions::default());
        let mut engine = Engine::from_filter_set(set, true);
        engine.use_resources(serde_json::from_str::<Vec<Resource>>(RESOURCES).unwrap());
        let blocker: SharedBlocker = Arc::new(RwLock::new(Some(engine)));

        let action = classify_request(
            &blocker,
            "https://imasdk.googleapis.com/js/sdkloader/ima3.js",
            "https://juegos.eleconomista.es/",
            "script",
        );
        match action {
            BlockAction::Redirect { mime, body } => {
                assert_eq!(mime, "application/javascript");
                assert!(!body.is_empty(), "surrogate body should be the stub script");
            }
            _ => panic!("expected a redirect surrogate for the IMA SDK request"),
        }
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
