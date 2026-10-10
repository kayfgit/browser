//! WebView2 native history and navigation guard.
use crate::{
    navguard::{recent, site_of, NavIntent},
    UserEvent,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
use webview2_com::{take_pwstr, ContentLoadingEventHandler, NavigationStartingEventHandler};
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
        let Ok(core) = webview.controller().CoreWebView2() else {
            return false;
        };
        let mut b = BOOL::default();
        let ok = if forward {
            core.CanGoForward(&mut b)
        } else {
            core.CanGoBack(&mut b)
        };
        ok.is_ok() && b.as_bool()
    }
}

/// Drive the webview's own session history one page back/forward (see [`can_go`]).
/// Stamp [`crate::navguard::mark`] first so the native guard lets a cross-site step
/// through. A failed COM call is returned to the shell.
#[cfg(windows)]
pub(crate) fn go(webview: &WebView, forward: bool) -> browser_engine::EngineResult {
    unsafe {
        let core = webview
            .controller()
            .CoreWebView2()
            .map_err(|e| e.to_string())?;
        if forward {
            core.GoForward()
        } else {
            core.GoBack()
        }
        .map_err(|e| e.to_string())
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
    // The current top-frame registrable domain ("youtube.com"): set on every allowed
    // navigation, and corrected to the document that actually loads (below) — a
    // navigation can end in a redirect elsewhere, or in a download that loads nothing.
    // These events only ever fire on the UI thread, so a plain `Rc` is enough.
    let current_site: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    // The navigation this guard last let through. A server redirect continues the same
    // navigation (same id), so it's part of what was already allowed — a GitHub release
    // download answers the click with a redirect to another domain, and so do many
    // mirrors and link shorteners. A forced redirect is a NEW navigation and still needs
    // intent.
    let allowed: Rc<Cell<u64>> = Rc::new(Cell::new(0));
    let loaded_site = current_site.clone();
    let loaded = ContentLoadingEventHandler::create(Box::new(move |core, _| {
        if let Some(core) = core {
            let mut p = PWSTR::null();
            if unsafe { core.Source(&mut p) }.is_ok() {
                let site = site_of(&take_pwstr(p));
                if !site.is_empty() {
                    *loaded_site.borrow_mut() = site;
                }
            }
        }
        Ok(())
    }));
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
            let mut id = 0u64;
            let _ = unsafe { args.NavigationId(&mut id) };
            let redirected = unsafe {
                let mut b = BOOL::default();
                args.IsRedirected(&mut b).is_ok() && b.as_bool()
            };
            if redirected && id == allowed.get() {
                return Ok(());
            }
            // Blocker off, or a non-web target (`about:`/`data:`/`blob:`) we can't reason
            // about → defer to the other guards; only advance origin for real web pages.
            if !adblock_on.load(Ordering::Relaxed) || target.is_empty() {
                if !target.is_empty() {
                    *current_site.borrow_mut() = target;
                }
                allowed.set(id);
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
            allowed.set(id);
            Ok(())
        },
    ));
    let mut token = 0i64;
    let _ = unsafe { core.add_NavigationStarting(&handler, &mut token) };
    let _ = unsafe { core.add_ContentLoading(&loaded, &mut token) };
}
