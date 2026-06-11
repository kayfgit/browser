//! The engine contract: the seam between the browser shell and whatever renders
//! pages. A shell hosts engines only through [`EngineFactory`] / [`EngineView`],
//! so engines are swappable per tab at runtime (`:engine`) and new engines are
//! drop-in crates. The normative, language-neutral description of this contract
//! lives in `docs/ENGINE.md` — keep the two in sync; the doc is what a future
//! out-of-process host or full rewrite implements.
//!
//! Two rendering modes exist (see [`EngineCaps`]):
//!  * **windowed** — the engine owns an OS child surface (e.g. WebView2's HWND);
//!    the shell positions it with [`EngineView::set_bounds`] and paints nothing.
//!  * **painted** — the engine draws into the shell's pixel buffer each frame via
//!    [`EngineView::paint`], using only the [`Frame`] handed to it.
//!
//! Threading: a view lives and dies on the UI thread (it is deliberately NOT
//! `Send`). Only [`EventSink`] and [`NavPolicy`] are `Send + Sync`, so engines
//! may deliver events from any thread.

use std::any::Any;
use std::sync::Arc;

use anyhow::Result;
use raw_window_handle::HasWindowHandle;

/// Identifies one live engine view. Allocated by the shell, never reused within
/// a session; events carry it so stale events from dropped views can be ignored.
pub type EngineId = u64;

/// A rectangle in physical pixels, in the shell window's client coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RectPx {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// What an engine can do. The shell greys out or reroutes features per tab based
/// on this; every capability has a defined degradation (see ENGINE.md §4).
#[derive(Clone, Copy, Debug, Default)]
pub struct EngineCaps {
    /// Owns an OS child surface; `set_bounds` moves it. Mutually exclusive with
    /// `shell_paints`.
    pub windowed: bool,
    /// The shell must call [`EngineView::paint`] for it every frame.
    pub shell_paints: bool,
    /// `eval` executes script in the page; engines without it no-op.
    pub eval_js: bool,
    /// `back()`/`forward()` are meaningful (a real history stack exists).
    pub history: bool,
    /// `set_zoom` scales inside the engine (painted engines usually scale with
    /// the shell's font size instead).
    pub page_zoom: bool,
    /// `find` works (native counting or async page highlights).
    pub find: bool,
    /// `hint` works (link hints).
    pub hints: bool,
    /// `caret` works (caret/visual browsing with yank).
    pub caret: bool,
    /// Can take real OS keyboard focus, enabling Insert/Passthrough modes.
    pub insert_passthrough: bool,
}

/// Where a view gets its initial page from.
pub enum Source {
    Url(String),
    Html(String),
}

/// Live-toggleable page features. Engines without an implementation ignore them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    Adblock,
    BlockPopups,
    Mute,
    NoCss,
}

/// The feature switch positions a view starts with.
#[derive(Clone, Copy, Debug, Default)]
pub struct FeatureFlags {
    pub adblock: bool,
    pub block_popups: bool,
    pub mute: bool,
    pub no_css: bool,
}

/// Page-treatment profile baked in at build time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PageProfile {
    #[default]
    Normal,
    /// "Research" mode: heavy media/embeds stripped on the fly.
    Research,
}

/// Everything a factory needs to build a view.
pub struct BuildOpts {
    pub source: Source,
    pub js_enabled: bool,
    pub bounds: RectPx,
    pub zoom: f64,
    /// Whether the view should take OS focus immediately (normally false — the
    /// shell keeps the keyboard).
    pub focused: bool,
    pub profile: PageProfile,
    pub features: FeatureFlags,
}

/// The shell's modal input state, as far as an engine needs to know it. Engines
/// hosting their own input (windowed ones) use this to decide which keys to
/// surface as [`EngineEvent`]s instead of consuming (ENGINE.md §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Insert,
    Passthrough,
}

/// Events an engine reports back to the shell. Every event is tagged with the
/// originating [`EngineId`]; the shell drops events whose id no longer matches a
/// live view, and honors active-tab-only events only from the active tab's view.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    /// The page finished loading (shell reclaims focus, re-asserts zoom, refreshes
    /// the displayed URL).
    LoadFinished,
    /// The document title changed (tab label refresh).
    TitleChanged(String),
    /// The engine asks the shell to navigate this tab elsewhere (e.g. a policy
    /// redirect). The shell decides; the engine must not navigate itself.
    NavigateRequest(String),
    /// Leave Insert/Passthrough back to Normal (page Esc, input blur, …).
    ExitToNormal,
    /// Promote Insert → Passthrough (Ctrl+V typed inside the page).
    InsertToPassthrough,
    /// The page stole keyboard focus while the shell was in Normal mode.
    GrabFocus,
    /// Pointer pressed inside the engine's surface (split-pane focus routing).
    PaneClick,
    /// Hint mode ended page-side.
    HintExit,
    /// A hint picked an editable element (shell enters Insert).
    HintEdit,
    /// A new-tab hint resolved to this URL.
    HintOpen(String),
    /// Caret mode yanked this text (shell owns the clipboard).
    CaretYank(String),
    /// Caret mode exited page-side.
    CaretExit,
    /// The page entered/left HTML fullscreen (e.g. a video player).
    Fullscreen(bool),
}

/// Cloneable, thread-safe channel from engines to the shell. The shell builds it
/// (wrapping its event-loop proxy) and hands it to factories.
#[derive(Clone)]
pub struct EventSink(Arc<dyn Fn(EngineId, EngineEvent) + Send + Sync>);

impl EventSink {
    pub fn new(f: impl Fn(EngineId, EngineEvent) + Send + Sync + 'static) -> Self {
        EventSink(Arc::new(f))
    }

    pub fn send(&self, id: EngineId, event: EngineEvent) {
        (self.0)(id, event);
    }
}

/// Synchronous navigation filter, consulted before a navigation is allowed.
/// Returns `false` to cancel. Shell policy stays in the shell: a canceling
/// policy may follow up by sending the corrected URL through other channels.
#[derive(Clone)]
pub struct NavPolicy(Arc<dyn Fn(&str) -> bool + Send + Sync>);

impl NavPolicy {
    pub fn new(f: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        NavPolicy(Arc::new(f))
    }

    /// Build a policy that allows every navigation.
    pub fn allow_all() -> Self {
        NavPolicy(Arc::new(|_| true))
    }

    pub fn allows(&self, url: &str) -> bool {
        (self.0)(url)
    }
}

/// Text metrics + glyph drawing the shell provides to painted engines, so they
/// can draw without knowing the shell's rasterizer.
pub trait GlyphPaint {
    /// Width of `s` in px at the current font size.
    fn measure(&self, s: &str) -> i32;
    /// Horizontal advance of one char, in (fractional) px.
    fn advance(&self, ch: char) -> f32;
    /// Line height in px.
    fn line_height(&self) -> i32;
    /// The current font size in px (scales with shell zoom).
    fn font_px(&self) -> f32;
    /// Draw `s` with its baseline at `baseline`, clipped to `clip`; returns the
    /// new pen x.
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        buf: &mut [u32],
        buf_w: usize,
        buf_h: usize,
        x: i32,
        baseline: i32,
        s: &str,
        color: (u8, u8, u8),
        clip: RectPx,
    ) -> i32;
}

/// One frame handed to a painted engine. The engine MUST NOT write outside
/// `clip` (its pane rect).
pub struct Frame<'a> {
    pub buf: &'a mut [u32],
    pub buf_w: usize,
    pub buf_h: usize,
    pub clip: RectPx,
    pub glyphs: &'a dyn GlyphPaint,
    /// Whether this view sits in the focused pane (gates caret/find overlays).
    pub focused: bool,
}

/// Find-in-page commands. The shell owns the `/` prompt and `n`/`N`.
pub enum FindCmd<'a> {
    Query(&'a str),
    Next,
    Prev,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindReply {
    /// Search runs in the page; counts unknown (highlights are page-side).
    Async,
    /// Native search: total matches and the 0-based current index.
    Counted { total: usize, current: usize },
    Unsupported,
}

/// Hint-mode commands (`f`/`F`).
pub enum HintCmd<'a> {
    Begin { new_tab: bool },
    Filter { input: &'a str, new_tab: bool },
    Cancel,
}

#[derive(Debug, Clone)]
pub enum HintReply {
    /// Hints run in the page; results arrive as Hint* events.
    Async,
    /// Native hints: this many labelled targets remain (0 ⇒ shell exits hint mode).
    Targets(usize),
    /// A native hint matched exactly: open this URL.
    Open { url: String, new_tab: bool },
    Unsupported,
}

/// Caret/visual-mode commands (`v`/`V`).
pub enum CaretCmd {
    Enter { linewise: bool },
    Key(char),
    Esc,
    Yank,
}

#[derive(Debug, Clone)]
pub enum CaretReply {
    /// Caret runs in the page; results arrive as Caret* events.
    Async,
    Consumed,
    NotConsumed,
    Yanked(String),
    Exited,
    Unsupported,
}

/// Builds views for one engine. Implementations register with the shell's
/// engine table; `name()` is what `:engine <name>` and the config select.
pub trait EngineFactory {
    /// Stable, lowercase identifier: `"webview2"`, `"read"`, later `"servo"`, …
    fn name(&self) -> &'static str;
    fn caps(&self) -> EngineCaps;
    /// Build a view as a child of `parent`. UI thread only.
    fn build(
        &self,
        parent: &dyn HasWindowHandle,
        id: EngineId,
        opts: BuildOpts,
        sink: EventSink,
        nav: NavPolicy,
    ) -> Result<Box<dyn EngineView>>;
}

/// One live page view. UI-thread-affine (not `Send`); dropping it MUST free the
/// renderer (processes, surfaces, threads).
pub trait EngineView {
    fn id(&self) -> EngineId;
    fn caps(&self) -> EngineCaps;

    // --- navigation -----------------------------------------------------------
    fn navigate(&mut self, url: &str);
    fn reload(&mut self);
    /// History steps; no-ops without `caps().history`.
    fn back(&mut self);
    fn forward(&mut self);
    /// The live (post-redirect) URL, if known.
    fn url(&self) -> Option<String>;
    /// The current document title, if known.
    fn title(&self) -> Option<String>;

    // --- geometry / presentation ------------------------------------------------
    /// Windowed: move/resize the child surface. Painted: the viewport for layout
    /// and scroll clamping.
    fn set_bounds(&mut self, rect: RectPx);
    fn set_visible(&mut self, visible: bool);
    fn set_zoom(&mut self, factor: f64);
    /// Hand the engine real OS input focus (entering Insert/Passthrough).
    fn focus(&mut self);
    /// Give focus back to the shell window.
    fn focus_shell(&mut self);
    /// Tell the engine which modal state the shell is in (ENGINE.md §8).
    fn set_input_mode(&mut self, mode: InputMode);

    // --- scripting --------------------------------------------------------------
    /// Run script in the page. No-op without `caps().eval_js`.
    fn eval(&mut self, js: &str);

    // --- scrolling ----------------------------------------------------------------
    fn scroll_by(&mut self, dy_px: i32);
    /// Jump to the top (`false`) or bottom (`true`).
    fn scroll_edge(&mut self, bottom: bool);

    // --- unified feature surface --------------------------------------------------
    fn find(&mut self, cmd: FindCmd<'_>) -> FindReply;
    fn hint(&mut self, cmd: HintCmd<'_>) -> HintReply;
    fn caret(&mut self, cmd: CaretCmd) -> CaretReply;
    fn set_feature(&mut self, feature: Feature, on: bool);

    // --- painted engines ------------------------------------------------------------
    /// Draw this frame. Only called when `caps().shell_paints`.
    fn paint(&mut self, frame: &mut Frame<'_>) {
        let _ = frame;
    }

    /// Transitional escape hatch for the shell while features migrate behind the
    /// trait; scheduled for removal. Do not use in new code.
    fn as_any(&mut self) -> &mut dyn Any;
}

// The whole point of the trait is runtime-swappable engines, so it must stay
// dyn-compatible. This fails to compile if anyone breaks that.
const _: fn(&dyn EngineView, &dyn EngineFactory) = |_, _| {};
