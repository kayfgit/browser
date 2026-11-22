//! Pop-up views an engine built, waiting for the shell to give them a pane.
//!
//! An engine's new-window handler can't reach the `App`, and views are UI-thread
//! objects, so they wait here (on the UI thread) instead of travelling in a
//! `UserEvent`. [`App::adopt_popups`](crate::App::adopt_popups) drains the queue.

use std::cell::RefCell;

use browser_engine::{EngineView, ViewId};

use crate::tabs::PageState;

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
    static PENDING: RefCell<Vec<Result<PendingPopup, String>>> = const { RefCell::new(Vec::new()) };
}

/// Queue a built pop-up (or the reason it couldn't be built) for the shell.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn push(popup: Result<PendingPopup, String>) {
    PENDING.with(|p| p.borrow_mut().push(popup));
}

/// Every pop-up built since the last call, in order.
pub(crate) fn take() -> Vec<Result<PendingPopup, String>> {
    PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()))
}
