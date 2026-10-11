//! WebView2 profile data clearing.
use browser_engine::{Completion, DataKind};
use webview2_com::ClearBrowsingDataCompletedHandler;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Profile2, ICoreWebView2_13, COREWEBVIEW2_BROWSING_DATA_KINDS,
    COREWEBVIEW2_BROWSING_DATA_KINDS_ALL_PROFILE,
    COREWEBVIEW2_BROWSING_DATA_KINDS_BROWSING_HISTORY,
    COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE, COREWEBVIEW2_BROWSING_DATA_KINDS_COOKIES,
    COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE, COREWEBVIEW2_BROWSING_DATA_KINDS_DOWNLOAD_HISTORY,
};
use windows_core061::Interface;
use wry::{WebView, WebViewExtWindows};

fn flags(kind: DataKind) -> COREWEBVIEW2_BROWSING_DATA_KINDS {
    let bits = |a: COREWEBVIEW2_BROWSING_DATA_KINDS, b: COREWEBVIEW2_BROWSING_DATA_KINDS| {
        COREWEBVIEW2_BROWSING_DATA_KINDS(a.0 | b.0)
    };
    match kind {
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
pub(crate) fn clear(
    webview: &WebView,
    kind: DataKind,
    range: Option<(f64, f64)>,
    done: Completion,
) -> Result<(), String> {
    unsafe {
        let core = webview
            .controller()
            .CoreWebView2()
            .map_err(|e| e.to_string())?;
        let core13: ICoreWebView2_13 = core
            .cast()
            .map_err(|_| "this WebView2 runtime is too old to clear browsing data".to_string())?;
        let profile = core13.Profile().map_err(|e| e.to_string())?;
        let profile2: ICoreWebView2Profile2 = profile
            .cast()
            .map_err(|e: windows_core061::Error| e.to_string())?;
        let handler = ClearBrowsingDataCompletedHandler::create(Box::new(move |hr| {
            done(hr.map_err(|e| e.to_string()));
            Ok(())
        }));
        match range {
            Some((start, end)) => profile2
                .ClearBrowsingDataInTimeRange(flags(kind), start, end, &handler)
                .map_err(|e| e.to_string())?,
            None => profile2
                .ClearBrowsingData(flags(kind), &handler)
                .map_err(|e| e.to_string())?,
        }
    }
    Ok(())
}
