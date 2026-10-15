//! Pop-up windows as shell panes. A page's `window.open` with a size (Gmail's and
//! Google's sign-in windows, "share" dialogs) or a blank window its script fills in
//! needs to stay connected to the page that opened it: the sign-in reports back
//! through `window.opener`. Opening the address in a fresh tab loses that link, and
//! letting WebView2 open its own OS window puts the pop-up outside the shell, where
//! none of the keys, hints or vim motions reach it.
//!
//! So the new-window handler builds a regular shell view on the opener's own WebView2
//! environment and hands it to the engine as the pop-up (`NewWindowResponse::Create`).
//! WebView2 then loads the pop-up into it, opener and all. The handler can't reach the
//! `App`, so the view waits here until [`UserEvent::PopupPane`] lets the shell adopt it
//! as a narrow split beside its opener ([`App::adopt_popups`](crate::App::adopt_popups)).
//! When the pop-up closes itself (`window.close()`, as a finished sign-in does), its
//! pane closes too.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use browser_engine::{EngineView, RectPx, Source, StorageMode, ViewId, ViewIdentity};
use tao::event_loop::EventLoopProxy;
use tao::window::Window;
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
use webview2_com::WindowCloseRequestedEventHandler;
use wry::{NewWindowFeatures, WebViewExtWindows};

use super::super::PageEventProxy;
use super::BuildOptions;
use crate::tabs::PageState;
use crate::UserEvent;

/// A pop-up built as a shell view, waiting for the shell to give it a pane.
pub(crate) struct PendingPopup {
    pub(crate) view: Box<dyn EngineView>,
    pub(crate) page: PageState,
    /// The view whose page opened it; the pane goes beside that one.
    pub(crate) opener: ViewId,
    /// The width the page asked for, in CSS pixels (`None` for a blank window).
    pub(crate) width: Option<f64>,
}

thread_local! {
    /// Built pop-ups (or why one couldn't be built) not yet adopted by the shell.
    /// Views are UI-thread objects, so they wait on the UI thread rather than travel
    /// in a `UserEvent`.
    static PENDING: RefCell<Vec<Result<PendingPopup, String>>> = const { RefCell::new(Vec::new()) };
}

/// Every pop-up built since the last call, in order.
pub(crate) fn take() -> Vec<Result<PendingPopup, String>> {
    PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

/// What a view needs to build pop-ups like itself: an owned copy of its
/// [`BuildOptions`], taken when the view is built.
#[derive(Clone)]
pub(super) struct Seed {
    parent: Rc<Window>,
    opener: ViewId,
    extra_init: String,
    disable_js: bool,
    mute: bool,
    no_css: bool,
    no_video: bool,
    no_scrollbar: bool,
    proxy: EventLoopProxy<UserEvent>,
    adblock_on: Arc<AtomicBool>,
    blocker: crate::blocklist::SharedBlocker,
}

impl Seed {
    pub(super) fn new(parent: &Rc<Window>, opts: &BuildOptions<'_>, opener: ViewId) -> Seed {
        Seed {
            parent: parent.clone(),
            opener,
            extra_init: opts.extra_init.to_string(),
            disable_js: opts.disable_js,
            mute: opts.mute,
            no_css: opts.no_css,
            no_video: opts.no_video,
            no_scrollbar: opts.no_scrollbar,
            proxy: opts.proxy.clone(),
            adblock_on: opts.adblock_on.clone(),
            blocker: opts.blocker.clone(),
        }
    }

    /// Build the pop-up's view on the opener's environment, queue it for the shell, and
    /// return the WebView2 object to hand back as the new window. The view must not
    /// navigate anywhere itself: WebView2 loads the pop-up into it.
    pub(super) fn build(&self, features: &NewWindowFeatures) -> Result<ICoreWebView2, String> {
        let opts = BuildOptions {
            // Never loaded: `build_view` doesn't navigate, the pop-up arrives by itself.
            source: Source::Html(String::new()),
            disable_js: self.disable_js,
            extra_init: &self.extra_init,
            storage: StorageMode::Persistent,
            bounds: RectPx {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            adblock: self.adblock_on.load(std::sync::atomic::Ordering::Relaxed),
            mute: self.mute,
            no_css: self.no_css,
            no_video: self.no_video,
            no_scrollbar: self.no_scrollbar,
            proxy: self.proxy.clone(),
            adblock_on: self.adblock_on.clone(),
            blocker: self.blocker.clone(),
        };
        let identity = ViewIdentity {
            id: ViewId::allocate(),
            provider: "webview2".into(),
            storage: StorageMode::Persistent,
        };
        let id = identity.id;
        let built = super::build_view(
            &self.parent,
            &opts,
            identity,
            Some(features.opener.environment.clone()),
        )
        .map_err(|e| format!("opening the pop-up in a pane: {e:#}"));
        let (view, page) = match built {
            Ok(built) => built,
            Err(e) => {
                PENDING.with(|p| p.borrow_mut().push(Err(e.clone())));
                let _ = self.proxy.send_event(UserEvent::PopupPane);
                return Err(e);
            }
        };
        let core = view.inner.webview();
        close_with_page(&core, PageEventProxy::new(self.proxy.clone(), id));
        PENDING.with(|p| {
            p.borrow_mut().push(Ok(PendingPopup {
                view: Box::new(view),
                page,
                opener: self.opener,
                width: features.size.map(|s| s.width),
            }))
        });
        let _ = self.proxy.send_event(UserEvent::PopupPane);
        Ok(core)
    }
}

/// Close the pop-up's pane when its page closes the window (`window.close()`). wry
/// destroys the view's host window at that point, so the pane would otherwise stay
/// behind, black.
fn close_with_page(core: &ICoreWebView2, proxy: PageEventProxy) {
    let handler = WindowCloseRequestedEventHandler::create(Box::new(move |_, _| {
        let _ = proxy.send_event(UserEvent::PopupClosed);
        Ok(())
    }));
    let mut token = 0i64;
    let _ = unsafe { core.add_WindowCloseRequested(&handler, &mut token) };
}

/// What WebView2 itself said about the latest new-window request: its address and
/// whether a real click or key press started it (`IsUserInitiated`). The engine
/// decides that, not the page, and it holds for a button inside an iframe too (the
/// "Sign in with Google" button is one) — where the page-side gesture report can't
/// reach. wry's handler doesn't pass it on, so this second handler records it; it
/// runs before wry's decision, which wry defers to the message loop.
pub(super) type Gesture = Rc<RefCell<Option<(String, bool)>>>;

/// Record [`Gesture`]s for `webview`'s new-window requests.
pub(super) fn watch_gestures(webview: &wry::WebView, gesture: Gesture) {
    use webview2_com::NewWindowRequestedEventHandler;
    use windows_core061::{BOOL, PWSTR};
    let Ok(core) = (unsafe { webview.controller().CoreWebView2() }) else {
        return;
    };
    let handler = NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        let uri = unsafe {
            let mut p = PWSTR::null();
            args.Uri(&mut p)?;
            webview2_com::take_pwstr(p)
        };
        let mut user = BOOL::default();
        let _ = unsafe { args.IsUserInitiated(&mut user) };
        *gesture.borrow_mut() = Some((uri, user.as_bool()));
        Ok(())
    }));
    let mut token = 0i64;
    let _ = unsafe { core.add_NewWindowRequested(&handler, &mut token) };
}

/// Whether ad blocking should let a pop-up through: a click on a real link (`linked`,
/// the navigation guard's gesture stamp), or a sized window a real click or key press
/// opened — a sign-in window. Scam pop-unders also ride your clicks, but they want a
/// whole tab or window, not a small sized one; and either way the address must not be
/// a known ad/scam domain (`ad_domain`).
pub(super) fn wanted_with_adblock(linked: bool, sized: bool, user: bool, ad_domain: bool) -> bool {
    (linked || (sized && user)) && !ad_domain
}

#[cfg(test)]
mod tests {
    use super::wanted_with_adblock;

    #[test]
    fn sign_in_windows_pass_ad_blocking_but_popunders_do_not() {
        // A sign-in button: a sized window opened by a real click.
        assert!(wanted_with_adblock(false, true, true, false));
        // A link the user clicked opens as before.
        assert!(wanted_with_adblock(true, false, true, false));
        // A pop-under riding a click anywhere wants a full window: blocked.
        assert!(!wanted_with_adblock(false, false, true, false));
        // A sized window nobody clicked for (a timer, onload): blocked.
        assert!(!wanted_with_adblock(false, true, false, false));
        // Anything going to a known ad/scam domain: blocked.
        assert!(!wanted_with_adblock(true, true, true, true));
    }
}
