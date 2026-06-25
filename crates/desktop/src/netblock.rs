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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2Environment, ICoreWebView2_2, COREWEBVIEW2_WEB_RESOURCE_CONTEXT,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_DOCUMENT,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FETCH, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_IMAGE, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_SCRIPT,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_STYLESHEET, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET,
    COREWEBVIEW2_WEB_RESOURCE_CONTEXT_XML_HTTP_REQUEST,
};
use webview2_com::{take_pwstr, WebResourceRequestedEventHandler};
use windows_core::{w, Interface, PWSTR};
use wry::{WebView, WebViewExtWindows};

use crate::blocklist::SharedBlocker;

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
    // Route every request (every context) through our handler.
    if unsafe { core.AddWebResourceRequestedFilter(w!("*"), COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL) }
        .is_err()
    {
        return;
    }
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
            if crate::blocklist::blocks_request(&blocker, &uri, &src, req_type) {
                // `None` content → an empty body; the trait bound fixes the (0.61)
                // `IStream` type, so bare `None` is the correct optional-COM-param form.
                if let Ok(resp) =
                    unsafe { env.CreateWebResourceResponse(None, 403, w!("Blocked"), w!("")) }
                {
                    let _ = unsafe { args.SetResponse(&resp) };
                }
            }
            Ok(())
        },
    ));
    let mut token = 0i64;
    let _ = unsafe { core.add_WebResourceRequested(&handler, &mut token) };
}

/// Map a WebView2 resource context to the adblock-rust content-type string its filter
/// rules are keyed on. `None` for contexts we deliberately don't network-block here
/// (text tracks, manifests, CSP reports, …) — letting them through is harmless and
/// avoids mis-typing a request the lists don't target. DOCUMENT maps to `subdocument`:
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
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_FONT {
        "font"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_MEDIA {
        "media"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_WEBSOCKET {
        "websocket"
    } else if ctx == COREWEBVIEW2_WEB_RESOURCE_CONTEXT_PING {
        "ping"
    } else {
        return None;
    })
}
