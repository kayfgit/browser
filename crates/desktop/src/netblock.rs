//! Native WebView2 SUB-RESOURCE network blocker — the uBlock-style layer that stops
//! ad/scam scripts, iframes, XHRs and the like from ever LOADING, instead of hiding
//! their result after the fact.
//!
//! Why this exists: the EasyList [`Engine`](crate::blocklist) was only ever consulted
//! for TOP-LEVEL navigations (`navguard` / the wry nav handler). Sub-resources — the
//! `<script>` / `<iframe>` / `fetch` that *inject* fake "Confirme para continuar" /
//! "Instale o Opera GX" dialogs and popunders — were only checked page-side against the
//! tiny hardcoded `AD_HOSTS` list, never the full ~100k-rule engine. So the injector
//! loaded and ran, and the best we could do was sweep up the DOM it produced. uBlock
//! Origin blocks that injector at the NETWORK level against its full lists, which is why
//! the scam "isn't even loaded". This closes that gap.
//!
//! WebView2 has no public wry hook for this, so — like [`navguard`](crate::navguard) —
//! we drop to the raw COM API: register a `WebResourceRequested` handler with an
//! all-context filter, map the request's resource context to an adblock-rust content
//! type, ask the engine, and answer a matched request with an empty 403 so it never
//! reaches the network. Honours `:ads` live via the shared `adblock_on` flag.

#![cfg(windows)]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2Environment, ICoreWebView2WebResourceResponse, ICoreWebView2_2,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
};
use webview2_com::{take_pwstr, NavigationStartingEventHandler, WebResourceRequestedEventHandler};
use windows_core::{w, Interface, PCWSTR, PWSTR};
// `windows061` is the 0.61 `windows` build webview2-com is compiled against (see Cargo.toml):
// its `IStream` is the exact type `CreateWebResourceResponse` wants, so a stream we mint with
// its `SHCreateMemStream` unifies. (Our default `windows` 0.62 would be a distinct type.)
use windows061::Win32::System::Com::IStream;
use windows061::Win32::UI::Shell::SHCreateMemStream;
use wry::{WebView, WebViewExtWindows};

use crate::blocklist::{BlockAction, SharedBlocker};

/// Install the sub-resource blocker on a freshly built content webview. Best-effort: if
/// the raw engine handle, the environment cast, or the filter/event registration is
/// unavailable, we simply don't block sub-resources here — the page-side `ADBLOCK_JS`
/// network shim and the navigation guards still stand, so this never fails the build.
///
/// `source_origin` is the tab's live top-frame origin (shared with the nav handler, which
/// keeps it current) so `$third-party`/`$domain=` rules resolve; `top_url` is the live
/// top document URL, used only to make sure we never 403 the main document itself.
pub(crate) fn install(
    webview: &WebView,
    blocker: SharedBlocker,
    adblock_on: Arc<AtomicBool>,
    source_origin: Arc<Mutex<String>>,
    top_url: Arc<Mutex<String>>,
) {
    let core = match unsafe { webview.controller().CoreWebView2() } {
        Ok(c) => c,
        Err(_) => return,
    };
    // The environment is what mints the empty "blocked" response we hand back.
    let env: ICoreWebView2Environment =
        match core.cast::<ICoreWebView2_2>().and_then(|w2| unsafe { w2.Environment() }) {
            Ok(e) => e,
            Err(_) => return,
        };
    // Only intercept the contexts we actually block (see `FILTER_CONTEXTS`). High-frequency
    // MEDIA (video/audio segments — the bulk of traffic on the streaming sites this browser
    // targets) and FONT are deliberately left out, so they never reach this UI-thread
    // handler at all — the per-segment cost was the whole reason to narrow this.
    for ctx in FILTER_CONTEXTS {
        if unsafe { core.AddWebResourceRequestedFilter(w!("*"), ctx) }.is_err() {
            return;
        }
    }
    install_youtube_filter_gate(&core);
    let handler = WebResourceRequestedEventHandler::create(Box::new(
        move |_sender: Option<ICoreWebView2>, args| {
            let Some(args) = args else { return Ok(()) };
            // `:ads` off → don't touch any request.
            if !adblock_on.load(Ordering::Relaxed) {
                return Ok(());
            }
            let (uri, ctx) = unsafe {
                let req = args.Request()?;
                let mut p = PWSTR::null();
                req.Uri(&mut p)?;
                let uri = take_pwstr(p);
                let mut ctx = COREWEBVIEW2_WEB_RESOURCE_CONTEXT::default();
                args.ResourceContext(&mut ctx)?;
                (uri, ctx)
            };
            // YouTube's OWN first-party ad telemetry (`/api/stats/ads`, `/ptracking`,
            // `/pagead/`, `/get_midroll_`) MUST reach the network: if it fails, YouTube's
            // anti-adblock detector serves the "ad blockers violate ToS" enforcement wall.
            // The ads are killed instead by pruning the player-response JSON (see
            // `ADBLOCK_JS`). EasyList/EasyPrivacy DO list those endpoints, so the full
            // engine would block them — exempt YouTube's own hosts here, mirroring the
            // deliberate carve-out in `AD_HOSTS`. (Third-party trackers ON youtube.com are
            // still blocked; only youtube.com's own requests are spared.)
            if is_youtube_host(&uri) {
                return Ok(());
            }
            let Some(req_type) = request_type(ctx) else { return Ok(()) };
            // Never 403 the main document — it was already vetted by the nav handler, and
            // blanking it would break the page. A DOCUMENT-context request whose URL is
            // the current top URL is that main document; a sub-frame's document URL
            // differs, so genuine ad iframes are still caught (as `subdocument`).
            if req_type == "subdocument" && top_url.lock().is_ok_and(|t| *t == uri) {
                return Ok(());
            }
            let src = source_origin
                .lock()
                .ok()
                .map(|g| g.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| uri.clone());
            match crate::blocklist::classify_request(&blocker, &uri, &src, req_type) {
                // Untouched — reaches the network normally.
                BlockAction::Pass => {}
                // Plain block: answer with an empty 403. `None` content → an empty body; the
                // trait bound fixes the (0.61) `IStream` type, so bare `None` is the correct
                // optional-COM-param form.
                BlockAction::Block => {
                    if let Ok(resp) =
                        unsafe { env.CreateWebResourceResponse(None, 403, w!("Blocked"), w!("")) }
                    {
                        let _ = unsafe { args.SetResponse(&resp) };
                    }
                }
                // uBlock-style surrogate: serve the neutered stub body so the page keeps
                // working (the blocked script's API still exists, just does nothing). If the
                // stream/response can't be built we leave the request alone rather than break
                // the page — the original would load, but that's strictly safer than a 403 here.
                BlockAction::Redirect { mime, body } => {
                    if let Some(resp) = build_surrogate_response(&env, &mime, &body) {
                        let _ = unsafe { args.SetResponse(&resp) };
                    }
                }
            }
            Ok(())
        },
    ));
    let mut token = 0i64;
    let _ = unsafe { core.add_WebResourceRequested(&handler, &mut token) };
}

/// Un-register the [`HYDRATION_CONTEXTS`] filters while the top frame is YouTube, and put
/// them back on the way out.
///
/// The `is_youtube_host` carve-out in the handler above spares YouTube's own requests from
/// being BLOCKED — but they're still INTERCEPTED, and interception alone is the problem.
/// Routing a request through this UI-thread handler is enough to stall YouTube's lazy
/// hydration: that's what left a fresh watch page stuck at `readyState==loading` with the
/// player up but comments never filling in, and it's why the sub-resource blocker had to be
/// gated to `:adblock native` in the first place. In native mode the same stall is still
/// there, and it hits every part of the UI that fetches itself on demand — clicking the
/// account avatar fires a `/youtubei/v1/account/account_menu` request and opens the menu
/// when it answers, so a stalled fetch reads as a button that simply does nothing.
///
/// So on YouTube we stop intercepting exactly the request classes its UI hydrates through —
/// fetch/XHR and sub-documents (the account menu's `accounts.google.com` / `ogs.google.com`
/// frames) — and keep intercepting scripts, images, stylesheets, websockets and pings, which
/// is where third-party ad/tracker blocking actually earns its keep. Little is given up:
/// YouTube's own requests were exempt anyway, and its ads are killed by pruning the
/// player-response JSON page-side (see `ADBLOCK_JS`), not here.
///
/// `NavigationStarting` on the core webview fires for TOP-frame navigations only, before any
/// sub-resource is requested, so the filter set is always right for the page being loaded.
/// SPA navigations within YouTube fire nothing — and need to, since the state is already
/// correct. Best-effort throughout: a failed add/remove just leaves the filters as they were.
fn install_youtube_filter_gate(core: &ICoreWebView2) {
    // UI-thread only (NavigationStarting never fires anywhere else), so a plain `Cell`
    // is enough to remember whether the hydration filters are currently registered.
    let installed = Rc::new(Cell::new(true));
    // The closure's own handle on the webview (the parameter stays free for the
    // registration call below; the event's `sender` isn't guaranteed non-null).
    let target = core.clone();
    let handler = NavigationStartingEventHandler::create(Box::new(
        move |_sender: Option<ICoreWebView2>, args| {
            let Some(args) = args else { return Ok(()) };
            let uri = unsafe {
                let mut p = PWSTR::null();
                args.Uri(&mut p)?;
                take_pwstr(p)
            };
            let want = !is_youtube_host(&uri);
            if want == installed.get() {
                return Ok(());
            }
            for ctx in HYDRATION_CONTEXTS {
                let _ = unsafe {
                    if want {
                        target.AddWebResourceRequestedFilter(w!("*"), ctx)
                    } else {
                        target.RemoveWebResourceRequestedFilter(w!("*"), ctx)
                    }
                };
            }
            installed.set(want);
            Ok(())
        },
    ));
    let mut token = 0i64;
    let _ = unsafe { core.add_NavigationStarting(&handler, &mut token) };
}

/// The subset of [`FILTER_CONTEXTS`] a site fetches its own UI through, and so the subset
/// whose interception can stall that UI. Dropped while the top frame is YouTube — see
/// [`install_youtube_filter_gate`].
const HYDRATION_CONTEXTS: [COREWEBVIEW2_WEB_RESOURCE_CONTEXT; 3] = [
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT,
];

/// Build a 200-OK WebView2 response that serves `body` as `mime` — the neutered surrogate
/// for a `$redirect` rule. The body is copied into a COM memory stream (`SHCreateMemStream`,
/// whose `IStream` is the type [`CreateWebResourceResponse`](ICoreWebView2Environment) wants),
/// and a single `Content-Type` header tells the page how to interpret it. `None` on any COM
/// failure, in which case the caller leaves the request alone (safer than a broken page).
fn build_surrogate_response(
    env: &ICoreWebView2Environment,
    mime: &str,
    body: &[u8],
) -> Option<ICoreWebView2WebResourceResponse> {
    let stream: IStream = unsafe { SHCreateMemStream(Some(body)) }?;
    // NUL-terminated wide string; must outlive the call (PCWSTR borrows it).
    let headers: Vec<u16> = format!("Content-Type: {mime}\0").encode_utf16().collect();
    unsafe { env.CreateWebResourceResponse(&stream, 200, w!("OK"), PCWSTR(headers.as_ptr())) }.ok()
}

/// Whether `url`'s host is one of YouTube's own properties — requests we must NOT
/// network-block (see the carve-out in the handler). A cheap substring pre-filter keeps
/// it free on the overwhelming majority of requests, which never mention YouTube.
fn is_youtube_host(url: &str) -> bool {
    if !url.contains("youtube") {
        return false;
    }
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| {
            h == "youtube.com"
                || h.ends_with(".youtube.com")
                || h == "youtube-nocookie.com"
                || h.ends_with(".youtube-nocookie.com")
        })
}

/// The resource contexts the blocker registers a filter for — exactly those
/// [`request_type`] maps to a blockable adblock-rust type. MEDIA and FONT are omitted on
/// purpose (see [`install`]): video/audio segments are extremely high-frequency and
/// blocking ad fonts is near-useless, so keeping them off the handler is a pure win.
const FILTER_CONTEXTS: [COREWEBVIEW2_WEB_RESOURCE_CONTEXT; 8] = [
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING,
];

/// Map a WebView2 resource context to the adblock-rust content-type string its filter
/// rules are keyed on. `None` for anything we don't network-block — in practice only the
/// contexts in [`FILTER_CONTEXTS`] ever reach here (MEDIA/FONT/etc. are filtered out
/// upstream), so this is the authoritative per-context decision. DOCUMENT maps to `subdocument`:
/// the only document request we ever see here that we'd want to block is a sub-frame's
/// (the main document is skipped by the `top_url` guard above), and ad-frame rules use
/// `$subdocument`.
fn request_type(ctx: COREWEBVIEW2_WEB_RESOURCE_CONTEXT) -> Option<&'static str> {
    Some(if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT {
        "script"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST
        || ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH
    {
        "xmlhttprequest"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT {
        "subdocument"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE {
        "image"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET {
        "stylesheet"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET {
        "websocket"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING {
        "ping"
    } else {
        return None;
    })
}
