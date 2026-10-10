//! Network-level ad blocking for Servo pages. Servo has no extension support, so instead of
//! uBlock Origin Lite it runs every sub-resource load past the same list engine
//! ([`blocklist`](crate::blocklist)) the redirect guard uses: a match is cancelled, and a
//! `$redirect` match is answered with uBlock's neutered stub (so a player whose ad SDK is
//! blocked still finds the API it expects).
//!
//! Servo asks the view's delegate about every HTTP load, and a load that isn't intercepted
//! simply continues — so dropping `load` means "allow".
use content_security_policy::Destination;
use servo::{WebResourceLoad, WebResourceResponse, WebView};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::blocklist::{classify_request, BlockAction, SharedBlocker};

/// Block or stub out `load` if the lists say so and blocking is on.
pub(super) fn filter(
    on: &Arc<AtomicBool>,
    blocker: &SharedBlocker,
    webview: &WebView,
    load: WebResourceLoad,
) {
    let request = load.request();
    // The page itself is the redirect guard's call, not a filter list's.
    if !on.load(Ordering::Relaxed) || request.is_for_main_frame || exempt(&request.url) {
        return;
    }
    let source = webview.url().map(|u| u.to_string()).unwrap_or_default();
    let url = request.url.clone();
    match classify_request(
        blocker,
        url.as_str(),
        &source,
        request_type(request.destination),
    ) {
        BlockAction::Pass => {}
        BlockAction::Block => load.intercept(WebResourceResponse::new(url)).cancel(),
        BlockAction::Redirect { mime, body } => {
            let mut headers = http::HeaderMap::new();
            if let Ok(value) = http::HeaderValue::from_str(&mime) {
                headers.insert(http::header::CONTENT_TYPE, value);
            }
            let mut stub = load.intercept(WebResourceResponse::new(url).headers(headers));
            stub.send_body_data(body);
            stub.finish();
        }
    }
}

/// YouTube's own requests are never blocked: EasyList/EasyPrivacy list its first-party ad
/// telemetry, and blocking that is exactly what raises YouTube's "ad blockers violate the
/// terms" wall. Its ads are dealt with page-side (`ADBLOCK_JS`) instead.
fn exempt(url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or("");
    ["youtube.com", "youtube-nocookie.com"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// adblock-rust's name for the kind of resource a fetch is for.
fn request_type(destination: Destination) -> &'static str {
    match destination {
        Destination::Script
        | Destination::Worker
        | Destination::SharedWorker
        | Destination::ServiceWorker
        | Destination::AudioWorklet
        | Destination::PaintWorklet => "script",
        Destination::Style | Destination::Xslt => "stylesheet",
        Destination::Image => "image",
        Destination::Font => "font",
        Destination::Audio | Destination::Video | Destination::Track => "media",
        Destination::Frame | Destination::IFrame => "subdocument",
        Destination::Object | Destination::Embed => "object",
        Destination::Document => "document",
        // `fetch()`/XHR carry no destination.
        Destination::None | Destination::Json => "xmlhttprequest",
        Destination::Report => "ping",
        Destination::Manifest | Destination::WebIdentity => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_and_its_subdomains_are_exempt() {
        for (url, exempt_) in [
            ("https://www.youtube.com/api/stats/ads", true),
            ("https://youtube.com/ptracking", true),
            ("https://www.youtube-nocookie.com/embed/x", true),
            ("https://notyoutube.com/ad.js", false),
            ("https://doubleclick.net/ad.js", false),
        ] {
            assert_eq!(exempt(&url::Url::parse(url).unwrap()), exempt_, "{url}");
        }
    }

    #[test]
    fn fetches_without_a_destination_are_xhr() {
        assert_eq!(request_type(Destination::None), "xmlhttprequest");
        assert_eq!(request_type(Destination::IFrame), "subdocument");
        assert_eq!(request_type(Destination::Script), "script");
    }
}
