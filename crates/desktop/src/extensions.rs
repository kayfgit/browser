//! WebView2 browser-extension management (Windows only): list the installed extensions
//! and enable/disable them. This backs two things:
//!   * `:extensions` — a vim-pager picker where Enter toggles the extension on that row;
//!   * the `:adblock ubo|native` mutual exclusion — when native blocking is active the
//!     uBlock extension is disabled (and vice-versa) so only one engine runs at a time.
//!
//! Extensions live on the webview's *profile* (`ICoreWebView2Profile7`), which every tab
//! shares, so toggling via any one webview applies browser-wide. The WebView2 calls are
//! async (completion handlers that fire on the UI thread); `list` reports back over
//! [`UserEvent::ExtensionsListed`], while the enable/disable calls are fire-and-forget —
//! the shell updates its own cached list optimistically (see `App`).

#![cfg(windows)]

use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2BrowserExtension, ICoreWebView2BrowserExtensionList, ICoreWebView2Profile7,
    ICoreWebView2_13,
};
use webview2_com::{
    take_pwstr, BrowserExtensionEnableCompletedHandler, ProfileAddBrowserExtensionCompletedHandler,
    ProfileGetBrowserExtensionsCompletedHandler,
};
use windows_core::{Interface, BOOL, PWSTR};
use wry::{WebView, WebViewExtWindows};

use crate::{ExtInfo, UserEvent};

/// The profile-7 handle a webview exposes the extension APIs through. `None` if the raw
/// engine handle or the interface casts aren't available (best-effort, like the other COM
/// glue) — callers then simply do nothing.
fn profile7(webview: &WebView) -> Option<ICoreWebView2Profile7> {
    let core = unsafe { webview.controller().CoreWebView2() }.ok()?;
    let c13 = core.cast::<ICoreWebView2_13>().ok()?;
    let profile = unsafe { c13.Profile() }.ok()?;
    profile.cast::<ICoreWebView2Profile7>().ok()
}

/// Read a `PWSTR`-returning getter into an owned `String` (empty on failure).
fn pwstr_of(f: impl FnOnce(*mut PWSTR) -> windows_core::Result<()>) -> String {
    let mut p = PWSTR::null();
    if f(&mut p).is_ok() {
        take_pwstr(p)
    } else {
        String::new()
    }
}

/// Pull id/name/enabled out of a browser-extension list, synchronously (called inside the
/// async completion handler, on the UI thread).
fn read_list(list: &ICoreWebView2BrowserExtensionList) -> Vec<ExtInfo> {
    let mut out = Vec::new();
    let mut count = 0u32;
    if unsafe { list.Count(&mut count) }.is_err() {
        return out;
    }
    for i in 0..count {
        let Ok(ext) = (unsafe { list.GetValueAtIndex(i) }) else { continue };
        let id = pwstr_of(|p| unsafe { ext.Id(p) });
        let name = pwstr_of(|p| unsafe { ext.Name(p) });
        let mut b = BOOL::default();
        let enabled = unsafe { ext.IsEnabled(&mut b) }.is_ok() && b.as_bool();
        out.push(ExtInfo { id, name, enabled });
    }
    out
}

/// Query the installed extensions; posts [`UserEvent::ExtensionsListed`] when the async
/// call completes (an empty list if the profile handle is unavailable).
pub(crate) fn list(webview: &WebView, proxy: EventLoopProxy<UserEvent>) {
    let Some(profile) = profile7(webview) else {
        let _ = proxy.send_event(UserEvent::ExtensionsListed(Vec::new()));
        return;
    };
    let handler = ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |_hr, list| {
        let items = list.as_ref().map(read_list).unwrap_or_default();
        let _ = proxy.send_event(UserEvent::ExtensionsListed(items));
        Ok(())
    }));
    let _ = unsafe { profile.GetBrowserExtensions(&handler) };
}

/// Enable/disable one extension by id (fire-and-forget: the shell flips its cached copy).
pub(crate) fn set_enabled(webview: &WebView, id: String, enabled: bool) {
    apply(webview, move |_ext, eid| eid == id, enabled);
}

/// Enable/disable EVERY installed extension — the switch the `:adblock` mode uses to make
/// one engine active at a time (disable uBlock while native blocking runs, and back).
pub(crate) fn set_all_enabled(webview: &WebView, enabled: bool) {
    apply(webview, |_, _| true, enabled);
}

/// Add every unpacked extension under `dir` to the shared profile (async, best-effort) —
/// a mirror of wry's own builder-time loading, for the one case that path no longer
/// covers: switching to `Ubo` mode in a session whose webviews were all built in
/// `native`/`off` mode (which deliberately never (re-)add the bundled dir), on a profile
/// that never had the extension installed. Re-adding an already-installed extension
/// doesn't duplicate it — WebView2 keeps one copy and resets it to ENABLED, which is
/// exactly the state the Ubo switch wants anyway.
pub(crate) fn install_dir(webview: &WebView, dir: &std::path::Path) {
    let Some(profile) = profile7(webview) else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = windows_core::HSTRING::from(entry.path().as_os_str());
        let handler = ProfileAddBrowserExtensionCompletedHandler::create(Box::new(|_, _| Ok(())));
        let _ = unsafe { profile.AddBrowserExtension(&path, &handler) };
    }
}

/// Shared body of the enable/disable calls: fetch the list, then `Enable(enabled)` every
/// extension the `want` predicate accepts. All best-effort.
fn apply(
    webview: &WebView,
    want: impl Fn(&ICoreWebView2BrowserExtension, &str) -> bool + 'static,
    enabled: bool,
) {
    let Some(profile) = profile7(webview) else { return };
    let handler = ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |_hr, list| {
        if let Some(list) = list.as_ref() {
            let mut count = 0u32;
            let _ = unsafe { list.Count(&mut count) };
            for i in 0..count {
                let Ok(ext) = (unsafe { list.GetValueAtIndex(i) }) else { continue };
                let id = pwstr_of(|p| unsafe { ext.Id(p) });
                if want(&ext, &id) {
                    let done = BrowserExtensionEnableCompletedHandler::create(Box::new(|_hr| Ok(())));
                    let _ = unsafe { ext.Enable(enabled, &done) };
                }
            }
        }
        Ok(())
    }));
    let _ = unsafe { profile.GetBrowserExtensions(&handler) };
}
