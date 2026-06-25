//! `:freeze` — minimize RAM while keeping every tab open. WebView2 runs a renderer
//! process per tab, so a handful of tabs can hold hundreds of MB. Freezing hides
//! every web tab and asks WebView2 to **suspend** it
//! ([`ICoreWebView2_3::TrySuspend`], which frees the renderer's memory) and drop its
//! **memory-usage target** to LOW. `:unfreeze` resumes them. Handy when the machine
//! is under memory pressure but you don't want to lose your tabs.
//!
//! Both calls reach through wry to the engine COM handle (the same door `data.rs`
//! uses). They're best-effort: a WebView2 runtime too old for these interfaces just
//! leaves the tab as-is. The shell-side `App.frozen` flag is what
//! [`refresh_visibility`](crate::App::refresh_visibility) consults to keep the
//! webviews hidden, so the freeze sticks even as tabs are switched.

use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2_19, ICoreWebView2_3, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
    COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
};
use webview2_com::TrySuspendCompletedHandler;
use windows_core::Interface;
use wry::{WebView, WebViewExtWindows};

use crate::App;

/// Suspend one webview to free its renderer memory. The webview MUST already be
/// hidden — `TrySuspend` only suspends a non-visible one. Also drops its memory
/// target to LOW. Best-effort (older runtimes lack these interfaces).
fn suspend(webview: &WebView) {
    unsafe {
        let Ok(core) = webview.controller().CoreWebView2() else { return };
        if let Ok(c19) = core.cast::<ICoreWebView2_19>() {
            let _ = c19.SetMemoryUsageTargetLevel(COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW);
        }
        if let Ok(c3) = core.cast::<ICoreWebView2_3>() {
            // The completion just reports whether the suspend took; we don't act on it.
            let handler = TrySuspendCompletedHandler::create(Box::new(|_hr, _ok| Ok(())));
            let _ = c3.TrySuspend(&handler);
        }
    }
}

/// Resume a previously suspended webview and restore its memory target to NORMAL.
fn resume(webview: &WebView) {
    unsafe {
        let Ok(core) = webview.controller().CoreWebView2() else { return };
        if let Ok(c3) = core.cast::<ICoreWebView2_3>() {
            let _ = c3.Resume();
        }
        if let Ok(c19) = core.cast::<ICoreWebView2_19>() {
            let _ = c19.SetMemoryUsageTargetLevel(COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL);
        }
    }
}

impl App {
    /// `:freeze` — hide and suspend every web tab so the browser holds the least RAM
    /// possible while staying open. Idempotent; reports if there are no web tabs to
    /// suspend (read/terminal/AI tabs are already engine-free).
    pub(crate) fn freeze(&mut self) {
        if self.frozen {
            self.set_status("already frozen — :unfreeze to resume");
            return;
        }
        let mut n = 0usize;
        for tab in &self.tabs {
            if let Some(wv) = tab.webview() {
                // Hide first: TrySuspend only suspends a non-visible webview.
                let _ = wv.set_visible(false);
                suspend(wv);
                n += 1;
            }
        }
        self.frozen = true;
        // refresh_visibility honours `frozen` by keeping every webview hidden, and
        // the draw path paints the frozen notice over the (now empty) content band.
        self.refresh_visibility();
        if n == 0 {
            self.set_status("frozen — no web tabs to suspend (RAM already minimal)");
        } else {
            let plural = if n == 1 { "tab" } else { "tabs" };
            self.set_status(format!("frozen {n} web {plural} — :unfreeze to resume"));
        }
        self.window.request_redraw();
    }

    /// `:unfreeze` — resume every suspended web tab and return to normal rendering.
    pub(crate) fn unfreeze(&mut self) {
        if !self.frozen {
            self.set_status("not frozen");
            return;
        }
        for tab in &self.tabs {
            if let Some(wv) = tab.webview() {
                resume(wv);
            }
        }
        self.frozen = false;
        self.refresh_visibility(); // re-shows the active web pane(s)
        self.set_status("unfrozen");
        self.window.request_redraw();
    }
}
