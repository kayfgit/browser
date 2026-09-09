//! WebView2 native history and navigation guard.
use crate::{
    navguard::{recent, site_of, NavIntent},
    UserEvent,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
use webview2_com::{take_pwstr, NavigationStartingEventHandler};
use windows_core::{BOOL, PWSTR};
use wry::{WebView, WebViewExtWindows};

/// Whether the webview's OWN session history can step back/forward one page — i.e.
/// the adjacent page was navigated to WITHIN this webview instance (a clicked link
/// or form submit), so the engine can restore it from cache instantly (scroll and
/// form state intact) instead of the shell reopening it. False once a `:open`/search
/// rebuilt the webview past that boundary, where only the shell's stack can reach.
#[cfg(windows)]
pub(crate) fn can_go(webview: &WebView, forward: bool) -> bool {
    unsafe {
        let Ok(core) = webview.controller().CoreWebView2() else { return false };
        let mut b = BOOL::default();
        let ok = if forward { core.CanGoForward(&mut b) } else { core.CanGoBack(&mut b) };
        ok.is_ok() && b.as_bool()
    }
}

/// Drive the webview's own session history one page back/forward (see [`can_go`]).
/// Stamp [`crate::navguard::mark`] first so the native guard lets a cross-site step
/// through. A failed COM call is returned to the shell.
#[cfg(windows)]
pub(crate) fn go(webview: &WebView, forward: bool) -> browser_engine::EngineResult {
    unsafe {
        let core = webview.controller().CoreWebView2().map_err(|e| e.to_string())?;
        if forward { core.GoForward() } else { core.GoBack() }.map_err(|e| e.to_string())
    }
}

/// Install the native redirect guard on a freshly built content webview. Best-effort:
/// if the raw engine handle or the event registration is unavailable, the wry-handler
/// guards still stand, so this never fails the build.
pub(crate) fn install(
    webview: &WebView,
    adblock_on: Arc<AtomicBool>,
    nav_intent: NavIntent,
    proxy: crate::engines::PageEventProxy,
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
