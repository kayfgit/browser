//! Native WebView2 `NavigationStarting` guard — the structural backstop the wry
//! navigation handler can't be. wry hands its `with_navigation_handler` closure only
//! the URL *string*; it throws away the underlying event's `IsUserInitiated` flag,
//! which is the ONE signal that cleanly separates a forced redirect from a real click.
//!
//! So the wry handler in `tabs.rs` is forced to *reconstruct* "did the user want this?"
//! from focus/timing heuristics (`away` / `leaving`), and those are a narrow time
//! window the redirect just steps outside of: the first hop, fired as you click away,
//! gets cancelled (you see "blocked redirect"), then a follow-up `top.location = scam`
//! a beat later — focus back, the window expired — sails through. The page-side JS
//! `location` traps can't catch it either, because a cross-origin player iframe writing
//! `top.location` goes through the native Location setter, bypassing the prototype trap.
//!
//! This guard removes the guesswork. It reads WebView2's own `IsUserInitiated` and
//! cancels any TOP-LEVEL, CROSS-SITE navigation that no user gesture asked for — every
//! hop of a forced redirect is script-driven, so none is user-initiated, so every hop
//! is cancelled, with no time window to slip through and no JS trap to bypass. It's
//! registered AFTER wry's handler, so it runs last and only ever ADDS a cancel (it
//! never un-cancels one wry already made). `:ads` off disables it via `adblock_on`.
//!
//! Legitimate navigations pass: a real link click carries `IsUserInitiated`; `:open`
//! rebuilds the webview (its initial load has no prior origin to be "cross-site" from);
//! and the shell's own programmatic jumps — `H`/`L` history and the translate de-proxy
//! redirect, which WebView2 also reports as not-user-initiated — stamp [`NavIntent`]
//! just before navigating, which the guard honours for a beat.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
use webview2_com::{take_pwstr, NavigationStartingEventHandler};
use windows_core::{BOOL, PWSTR};
use wry::{WebView, WebViewExtWindows};

use crate::UserEvent;

/// Cross-site navigation intent: stamped `Some(now)` the instant something legitimate
/// asks to leave the current site, so the guard knows the next cross-site top jump is
/// wanted rather than a forced redirect. Stamped from two places:
///   * the page, on a TRUSTED gesture landing on a real cross-site link/submit (the
///     `nav-intent` IPC message — the signal a synthetic-click/overlay hijack can't fake);
///   * the shell, before a programmatic jump WebView2 also reports as not-user-initiated
///     — `H`/`L` history, the `translate.goog` de-proxy `load_url`, following a hint.
/// Shared (cloned `Arc`) into every webview's [`install`]ed guard.
pub(crate) type NavIntent = Arc<Mutex<Option<Instant>>>;

/// How long a navigation-intent stamp stays valid. Generous enough to cover a real
/// click's gesture→navigation latency and a short legit redirect chain, short enough
/// that a delayed forced redirect (which fires a beat *after* the gesture) finds no
/// fresh intent and is cancelled.
const INTENT_WINDOW: Duration = Duration::from_millis(2000);

/// Stamp `intent` "a cross-site navigation is wanted now" — call right before the shell
/// drives a programmatic `history.back()`/`forward()`, a de-proxy `load_url`, or a hint
/// follow. (The page stamps the same intent over IPC for trusted link clicks.)
pub(crate) fn mark(intent: &NavIntent) {
    if let Ok(mut g) = intent.lock() {
        *g = Some(Instant::now());
    }
}

/// Install the native redirect guard on a freshly built content webview. Best-effort:
/// if the raw engine handle or the event registration is unavailable, the wry-handler
/// guards still stand, so this never fails the build.
pub(crate) fn install(
    webview: &WebView,
    adblock_on: Arc<AtomicBool>,
    nav_intent: NavIntent,
    proxy: EventLoopProxy<UserEvent>,
) {
    let core = match unsafe { webview.controller().CoreWebView2() } {
        Ok(c) => c,
        Err(_) => return,
    };
    // The current top-frame registrable domain ("youtube.com"), updated on every
    // allowed navigation. `NavigationStarting` only ever fires on the UI thread, so a
    // plain `Rc<RefCell<…>>` is enough — no locking, no cross-thread sharing.
    let current_site: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let handler = NavigationStartingEventHandler::create(Box::new(
        move |_sender: Option<ICoreWebView2>, args| {
            let Some(args) = args else { return Ok(()) };
            let uri = unsafe {
                let mut p = PWSTR::null();
                args.Uri(&mut p)?;
                take_pwstr(p)
            };
            // wry's handler ran first; if it already cancelled, respect that and don't
            // advance our origin past a navigation that isn't going to happen.
            let already_cancelled = unsafe {
                let mut c = BOOL::default();
                args.Cancel(&mut c)?;
                c.as_bool()
            };
            if already_cancelled {
                return Ok(());
            }
            let target = site_of(&uri);
            // Blocker off, or a non-web target (`about:`/`data:`/`blob:`) we can't reason
            // about → defer to the other guards; only advance origin for real web pages.
            if !adblock_on.load(Ordering::Relaxed) || target.is_empty() {
                if !target.is_empty() {
                    *current_site.borrow_mut() = target;
                }
                return Ok(());
            }
            let prev = current_site.borrow().clone();
            let cross_site = !prev.is_empty() && target != prev;
            // The rule: a TOP-LEVEL, CROSS-SITE jump is a forced redirect UNLESS the shell
            // just saw legitimate intent for it. WebView2's `IsUserInitiated` and the
            // window's foreground state are useless here — these scripts hijack your real
            // click (a synthetic `<a>` click, or a transparent overlay over the player), so
            // the jump is reported as foreground AND user-initiated. The only trustworthy
            // signal is whether a TRUSTED gesture actually landed on a real cross-site
            // link/submit — which the page reports as `nav-intent`, stamped into
            // [`nav_intent`]. Shell navigations pass the same way (`:open` rebuilds the
            // webview, so its first load has no prior origin; `H`/`L`, the translate
            // de-proxy, and hint-follow stamp intent). A synthetic click / overlay div
            // carries no such intent, so its redirect is cancelled.
            if cross_site && !recent(&nav_intent) {
                let _ = unsafe { args.SetCancel(true) };
                let _ = proxy.send_event(UserEvent::RedirectBlocked(uri));
                return Ok(());
            }
            *current_site.borrow_mut() = target;
            Ok(())
        },
    ));
    let mut token = 0i64;
    let _ = unsafe { core.add_NavigationStarting(&handler, &mut token) };
}

/// Whether a navigation-intent stamp landed within [`INTENT_WINDOW`].
fn recent(intent: &NavIntent) -> bool {
    intent.lock().ok().and_then(|g| *g).is_some_and(|t| t.elapsed() < INTENT_WINDOW)
}

/// The registrable domain ("eTLD+1") of an http(s) URL, lower-cased — e.g.
/// `https://m.youtube.com/watch` → `youtube.com`. `""` for non-web schemes or
/// unparseable input. Comparing by *site* (not full origin) is what lets the guard tell
/// a real cross-site jump (`animepahe.pw` → `searchapp.space`) from a benign same-site
/// one (apex ↔ `www.` ↔ `m.`, http ↔ https), so it never fights a site's own subdomains.
fn site_of(url: &str) -> String {
    let Ok(u) = url::Url::parse(url) else { return String::new() };
    if !matches!(u.scheme(), "http" | "https") {
        return String::new();
    }
    match u.host() {
        Some(url::Host::Domain(d)) => registrable_domain(&d.to_ascii_lowercase()),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => String::new(),
    }
}

/// Reduce a host to its registrable domain. Two labels by default (`a.b.com` →
/// `b.com`); three when the last two are a known multi-level public suffix (`a.co.uk` →
/// `a.co.uk`), so a scam on a ccTLD second level isn't waved through as "same site" as
/// its victim. Not a full public-suffix list — [`MULTI_SUFFIXES`] covers the common
/// ones; an unlisted multi-level TLD over-groups (treats two real sites as one), which
/// can only *miss* a block, never cause a false one.
fn registrable_domain(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').filter(|s| !s.is_empty()).collect();
    let n = labels.len();
    if n <= 2 {
        return labels.join(".");
    }
    let last_two = format!("{}.{}", labels[n - 2], labels[n - 1]);
    let take = if MULTI_SUFFIXES.contains(&last_two.as_str()) { 3 } else { 2 };
    labels[n - take..].join(".")
}

/// Common two-label public suffixes, so `foo.co.uk` resolves to `foo.co.uk` (not the
/// bare `co.uk`). Not exhaustive — just the ccTLD second levels seen most in practice.
const MULTI_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "gov.uk", "ac.uk", "co.jp", "or.jp", "ne.jp", "co.kr", "co.in",
    "co.nz", "co.za", "com.au", "net.au", "org.au", "com.br", "com.mx", "com.ar",
    "com.tr", "com.cn", "com.tw", "com.hk", "com.sg", "com.my", "com.ph", "com.pk",
    "com.ua", "com.pl", "com.ng", "com.eg", "com.sa", "co.id", "co.il", "co.th",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_of_collapses_subdomains_but_separates_real_sites() {
        // apex / www / m / scheme all resolve to one site → no false cross-site block.
        assert_eq!(site_of("https://youtube.com/"), "youtube.com");
        assert_eq!(site_of("https://www.youtube.com/watch?v=1"), "youtube.com");
        assert_eq!(site_of("http://m.youtube.com/"), "youtube.com");
        // A genuine forced-redirect jump is a different registrable domain.
        assert_ne!(site_of("https://animepahe.pw/play"), site_of("https://searchapp.space/?z=3"));
        // Multi-level ccTLDs keep the third label so two .co.uk sites stay distinct.
        assert_eq!(site_of("https://news.bbc.co.uk/x"), "bbc.co.uk");
        assert_ne!(site_of("https://bbc.co.uk/"), site_of("https://evil.co.uk/"));
    }

    #[test]
    fn site_of_ignores_non_web_and_junk() {
        assert_eq!(site_of("about:blank"), "");
        assert_eq!(site_of("data:text/html,hi"), "");
        assert_eq!(site_of("blob:https://x.test/abc"), "");
        assert_eq!(site_of("not a url"), "");
        // IP literals compare by the literal itself.
        assert_eq!(site_of("http://127.0.0.1:8080/x"), "127.0.0.1");
    }
}
