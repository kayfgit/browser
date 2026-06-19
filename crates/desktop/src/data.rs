//! WebView2 browsing-data clearing — the engine-side primitive behind the privacy
//! actions (`:clear cookies` / `cache` / `all`). WebView2 keeps cookies, the disk
//! cache and every kind of web storage in its *profile* (the shared user-data
//! folder), and the only supported way to erase them is the COM
//! `ICoreWebView2Profile2::ClearBrowsingData` async call. So this reaches through
//! any live content webview to that (shared) profile and wipes the requested kinds.
//!
//! It is best-effort and asynchronous: the call returns immediately and the erase
//! completes later, at which point the completion handler posts a
//! [`UserEvent::DataCleared`] so the shell can confirm it in the status bar. All of
//! the open web tabs share one profile, so clearing through *any* one of them clears
//! it for all.

use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Profile2, ICoreWebView2_13, COREWEBVIEW2_BROWSING_DATA_KINDS,
    COREWEBVIEW2_BROWSING_DATA_KINDS_ALL_PROFILE, COREWEBVIEW2_BROWSING_DATA_KINDS_BROWSING_HISTORY,
    COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE, COREWEBVIEW2_BROWSING_DATA_KINDS_COOKIES,
    COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE, COREWEBVIEW2_BROWSING_DATA_KINDS_DOWNLOAD_HISTORY,
};
use webview2_com::ClearBrowsingDataCompletedHandler;
use windows_core::Interface;
use wry::{WebView, WebViewExtWindows};

use crate::UserEvent;

/// A slice of stored browsing data a clear can target, each mapping to one or more
/// WebView2 data-kind flags.
#[derive(Clone, Copy)]
pub(crate) enum DataKind {
    /// The engine's own visited + download history (the shell keeps its own list
    /// separately; see [`App::history`](crate::App::history)).
    History,
    /// Cookies only — sign-ins and sessions.
    Cookies,
    /// The HTTP disk cache plus the Cache-Storage API (cached responses/assets).
    Cache,
    /// The whole profile: cookies, all site storage, cache, autofill, history — a
    /// full wipe (the `:clear all` "fresh profile" option).
    All,
}

impl DataKind {
    fn flags(self) -> COREWEBVIEW2_BROWSING_DATA_KINDS {
        let bits = |a: COREWEBVIEW2_BROWSING_DATA_KINDS, b: COREWEBVIEW2_BROWSING_DATA_KINDS| {
            COREWEBVIEW2_BROWSING_DATA_KINDS(a.0 | b.0)
        };
        match self {
            DataKind::History => bits(
                COREWEBVIEW2_BROWSING_DATA_KINDS_BROWSING_HISTORY,
                COREWEBVIEW2_BROWSING_DATA_KINDS_DOWNLOAD_HISTORY,
            ),
            DataKind::Cookies => COREWEBVIEW2_BROWSING_DATA_KINDS_COOKIES,
            DataKind::Cache => bits(
                COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE,
                COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE,
            ),
            DataKind::All => COREWEBVIEW2_BROWSING_DATA_KINDS_ALL_PROFILE,
        }
    }
}

/// Clear `kind` from the WebView2 profile via `webview`'s engine handle, optionally
/// limited to a `[start, end]` time window (Unix-epoch seconds; `None` = all time).
/// Returns `Err` only when the COM plumbing is unreachable (no engine yet, or a
/// WebView2 runtime too old for the profile API); the erase itself runs
/// asynchronously and reports completion through [`UserEvent::DataCleared`] carrying
/// `label`.
pub(crate) fn clear(
    webview: &WebView,
    kind: DataKind,
    range: Option<(f64, f64)>,
    label: String,
    ai_id: Option<u64>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<(), String> {
    unsafe {
        let core = webview.controller().CoreWebView2().map_err(|e| e.to_string())?;
        let core13: ICoreWebView2_13 = core.cast().map_err(|_| {
            "this WebView2 runtime is too old to clear browsing data".to_string()
        })?;
        let profile = core13.Profile().map_err(|e| e.to_string())?;
        let profile2: ICoreWebView2Profile2 =
            profile.cast().map_err(|e: windows_core::Error| e.to_string())?;
        let handler = ClearBrowsingDataCompletedHandler::create(Box::new(move |_hr| {
            let _ = proxy.send_event(UserEvent::DataCleared { label, ai_id });
            Ok(())
        }));
        match range {
            Some((start, end)) => profile2
                .ClearBrowsingDataInTimeRange(kind.flags(), start, end, &handler)
                .map_err(|e| e.to_string())?,
            None => profile2.ClearBrowsingData(kind.flags(), &handler).map_err(|e| e.to_string())?,
        }
    }
    Ok(())
}
