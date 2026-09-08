//! The in-process boundary between the browser shell and a page renderer.
//!
//! This is a Rust source interface, not a binary plugin ABI. Views and their services
//! are UI-thread-affine. Dropping a view releases its resources; a shared runtime may
//! finish shutdown asynchronously after its last view closes.
//!
//! Keep this contract grounded in working adapters. Provider discovery, storage
//! contexts, tagged events and external-process transport are separate next steps;
//! see `docs/engine-switching-research.md` in the workspace.

use std::path::Path;

pub type EngineResult<T = ()> = Result<T, String>;
/// Completion runs on the UI thread. A dispatch error means it will not be called.
pub type Completion<T = ()> = Box<dyn FnOnce(EngineResult<T>)>;

/// Child-surface bounds in physical pixels, relative to the shell window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RectPx {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

pub enum Source {
    Url(String),
    Html(String),
}

#[derive(Clone, Debug)]
pub struct ExtensionInfo {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum DataKind {
    /// Engine-owned visited/download history; excludes the shell's own history.
    History,
    /// Cookies, including sign-in sessions.
    Cookies,
    /// HTTP cache and the Cache Storage API.
    Cache,
    /// All browsing data in this view's storage context, including site storage.
    All,
}

/// One live page. No native engine handle or downcast escapes this boundary.
/// Methods dispatch work; success does not imply that a navigation has loaded or
/// a script has completed. Script results will need a separate async operation.
pub trait EngineView {
    fn load_url(&self, url: &str) -> EngineResult;
    fn reload(&self) -> EngineResult;
    fn url(&self) -> EngineResult<String>;
    fn set_bounds(&self, rect: RectPx) -> EngineResult;
    fn set_visible(&self, visible: bool) -> EngineResult;
    fn zoom(&self, factor: f64) -> EngineResult;
    fn focus(&self) -> EngineResult;
    fn focus_parent(&self) -> EngineResult;
    fn evaluate_script(&self, script: &str) -> EngineResult;

    // Optional services are queried explicitly, rather than successful no-ops.
    // Runtime versions can still reject an operation even if its service exists.
    fn history(&self) -> Option<&dyn History> {
        None
    }
    fn extensions(&self) -> Option<&dyn Extensions> {
        None
    }
    fn browsing_data(&self) -> Option<&dyn BrowsingData> {
        None
    }
    fn suspension(&self) -> Option<&dyn Suspension> {
        None
    }
}

pub trait History {
    fn can_go(&self, forward: bool) -> bool;
    fn go(&self, forward: bool) -> EngineResult;
}

/// Extensions belong to the view's storage context, which may be shared by views.
pub trait Extensions {
    fn list(&self, done: Completion<Vec<ExtensionInfo>>) -> EngineResult;
    /// Mutations dispatch asynchronously; acceptance is not a completion report.
    fn set_enabled(&self, id: String, enabled: bool) -> EngineResult;
    fn set_all_enabled(&self, enabled: bool) -> EngineResult;
    fn install_dir(&self, dir: &Path) -> EngineResult;
}

pub trait BrowsingData {
    /// Range is Unix seconds; None means all time. Only `done` confirms erasure.
    fn clear(&self, kind: DataKind, range: Option<(f64, f64)>, done: Completion) -> EngineResult;
}

pub trait Suspension {
    /// Hide the view first. Acceptance does not guarantee suspension: the runtime
    /// may decline it while page work is active.
    fn suspend(&self) -> EngineResult;
    fn resume(&self) -> EngineResult;
}

// Runtime polymorphism is the point of this contract: keep every service object safe.
const _: fn(&dyn EngineView, &dyn History, &dyn Extensions, &dyn BrowsingData, &dyn Suspension) =
    |_, _, _, _, _| {};
