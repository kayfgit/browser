//! WebView2 suspension and memory targets.
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2_19, ICoreWebView2_3, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
};
use webview2_com::TrySuspendCompletedHandler;
use windows_core::Interface;
use wry::{WebView, WebViewExtWindows};

use browser_engine::EngineResult;

/// Suspend one webview to free its renderer memory. The webview MUST already be
/// hidden — `TrySuspend` only suspends a non-visible one. Also drops its memory
/// target to LOW. Best-effort (older runtimes lack these interfaces).
///
/// Also used on the profile-switch engine keepalive
/// ([`hold_engine`](crate::App::hold_engine)), which exists only to keep the browser
/// process up: suspended, it holds the profile open at close to no renderer cost.
pub(crate) fn suspend(webview: &WebView) -> EngineResult {
    unsafe {
        let core = webview.controller().CoreWebView2().map_err(|e| e.to_string())?;
        if let Ok(c19) = core.cast::<ICoreWebView2_19>() {
            let _ = c19.SetMemoryUsageTargetLevel(COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW);
        }
        let c3 = core.cast::<ICoreWebView2_3>().map_err(|e| e.to_string())?;
        {
            // The completion just reports whether the suspend took; we don't act on it.
            let handler = TrySuspendCompletedHandler::create(Box::new(|_hr, _ok| Ok(())));
            c3.TrySuspend(&handler).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Resume a previously suspended webview and restore its memory target to NORMAL.
pub(crate) fn resume(webview: &WebView) -> EngineResult {
    unsafe {
        let core = webview.controller().CoreWebView2().map_err(|e| e.to_string())?;
        let c3 = core.cast::<ICoreWebView2_3>().map_err(|e| e.to_string())?;
        {
            c3.Resume().map_err(|e| e.to_string())?;
        }
        if let Ok(c19) = core.cast::<ICoreWebView2_19>() {
            let _ = c19.SetMemoryUsageTargetLevel(COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL);
        }
    }
    Ok(())
}
