//! Tabs: the Tab/NativeRead content model, opening every tab kind (web,
//! research, no-js, read), the WebView2 build glue, close/reopen/switch/move,
//! webview visibility + bounds, scrolling, history, and the page-feature
//! toggles (adblock/mute/css/js).

use anyhow::Result;
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::{
    NewWindowResponse, PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows,
};

use crate::panes::{PaneRect, FOCUS_BORDER};
use crate::term::TermSession;
use crate::{
    read_view, session, vim, AdblockMode, App, ModeKind, UserEvent, ADBLOCK_JS, AD_HOSTS,
    BRIDGE_JS, BROWSER_ARGS, CARET_JS, CLOSED_CAP, FEATURES_JS, FIND_JS, IPC_PRELUDE, RESEARCH_JS,
};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// Where a content webview gets its page from.
pub(crate) enum Source {
    Url(String),
    Html(String),
}

/// Directory of unpacked browser extensions loaded into every content webview (uBlock
/// Origin lives here as `uBlock0.chromium/`). Prefers `<exe dir>\extensions` (the installed
/// layout that `install.ps1` lays down), falling back to the in-repo
/// `crates/desktop/extensions` for `cargo run`. `None` if neither exists — then no
/// extensions load and the browser still runs.
fn ublock_extensions_dir() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let beside = exe.with_file_name("extensions");
        if beside.is_dir() {
            return Some(beside);
        }
    }
    let dev = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("extensions");
    dev.is_dir().then_some(dev)
}

/// What a tab shows. Exactly one of these — the invariants the old
/// quadruple-Option encoding kept by comment are now kept by construction.
pub(crate) enum TabContent {
    /// A WebView2 page (windowed child HWND over the content band).
    Web(WebView),
    /// An engine-free read-mode document, painted by the shell.
    Read(NativeRead),
    /// A read-only vim-style pager (`:error(s)`, `:res`, `:version`).
    Pager(vim::TextBuffer),
    /// A native terminal (alacritty grid + pty-host process).
    Term(TermSession),
    /// A native `:ai` tab: a Groq-backed quick-question surface, painted by the shell.
    Ai(crate::ai::AiState),
    /// An empty split-pane placeholder ("open something" prompt).
    Blank,
}

pub(crate) struct Tab {
    pub(crate) content: TabContent,
    pub(crate) url: String,
    /// Whether this tab was opened with JavaScript disabled (hint mode needs JS).
    pub(crate) nojs: bool,
    /// Whether this is a readability "read mode" tab.
    pub(crate) read: bool,
    /// Whether this is a "research" tab: a normal page (JS on, images kept) with
    /// heavy media/embeds stripped on the fly for a lighter browse.
    pub(crate) research: bool,
    /// This tab slot's back/forward navigation history (`H` / `L`). Shell-managed
    /// rather than delegated to WebView2's own session history, which only spans a
    /// single webview instance — every `:open`/`o`/search rebuilds the webview, so
    /// the engine's history can't see past the current page, and read tabs have no
    /// engine history at all. Carried across in-place rebuilds by [`App::place_tab`].
    pub(crate) nav: TabNav,
}

/// What kind of page a [`NavEntry`] is, so back/forward recreates the same view.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NavKind {
    Web,
    Nojs,
    Research,
    Read,
}

/// One re-openable location in a tab's [`history`](TabNav): the URL plus the tab
/// kind needed to recreate it (`:open` / no-js / research / read).
#[derive(Clone)]
pub(crate) struct NavEntry {
    pub(crate) url: String,
    pub(crate) kind: NavKind,
}

/// A tab slot's back/forward stacks (`H` pops `back`, `L` pops `fwd`).
#[derive(Default)]
pub(crate) struct TabNav {
    /// Pages behind the current one, oldest first; the top is where `H` goes.
    pub(crate) back: Vec<NavEntry>,
    /// Pages ahead of the current one (after an `H`), so `L` returns forward.
    pub(crate) fwd: Vec<NavEntry>,
    /// Skip recording the next web URL change as a history step — it's the just-
    /// opened page's own landing (and any on-load redirect chain), not a navigation
    /// away from a page worth returning to. Set when a web tab is built; consumed by
    /// the first live-URL refresh.
    pub(crate) settling: bool,
}

/// Cap on each back/forward stack so long browsing can't grow them without bound.
const NAV_CAP: usize = 100;

/// The [`NavEntry`] for `tab`'s current page, or `None` if it isn't a re-openable
/// web/read page (terminals, `:ai`, vim pagers and `browser://` internals don't
/// belong in back/forward history).
pub(crate) fn nav_entry(tab: &Tab) -> Option<NavEntry> {
    if tab.url.is_empty() || tab.url.starts_with("browser://") {
        return None;
    }
    let kind = if tab.native().is_some() {
        tab.read.then_some(NavKind::Read)?
    } else if tab.webview().is_some() {
        if tab.research {
            NavKind::Research
        } else if tab.nojs {
            NavKind::Nojs
        } else {
            NavKind::Web
        }
    } else {
        return None;
    };
    Some(NavEntry { url: tab.url.clone(), kind })
}

/// Push `entry` onto a back/forward stack, dropping the oldest if it would exceed
/// [`NAV_CAP`].
pub(crate) fn nav_push(stack: &mut Vec<NavEntry>, entry: NavEntry) {
    stack.push(entry);
    if stack.len() > NAV_CAP {
        stack.remove(0);
    }
}

impl Tab {
    /// A blank tab: no engine, no content. Used to fill a freshly `:split` pane —
    /// it paints an empty "open something" prompt and is replaced in place by the
    /// first `:open`/`:te`/`:read`/… run while it's focused.
    pub(crate) fn blank() -> Tab {
        Tab {
            content: TabContent::Blank,
            url: BLANK_URL.to_string(),
            nojs: false,
            read: false,
            research: false,
            nav: TabNav::default(),
        }
    }

    /// Whether this is a blank (empty) pane placeholder.
    pub(crate) fn is_blank(&self) -> bool {
        matches!(self.content, TabContent::Blank)
    }

    pub(crate) fn webview(&self) -> Option<&WebView> {
        match &self.content {
            TabContent::Web(w) => Some(w),
            _ => None,
        }
    }

    pub(crate) fn native(&self) -> Option<&NativeRead> {
        match &self.content {
            TabContent::Read(nr) => Some(nr),
            _ => None,
        }
    }

    pub(crate) fn native_mut(&mut self) -> Option<&mut NativeRead> {
        match &mut self.content {
            TabContent::Read(nr) => Some(nr),
            _ => None,
        }
    }

    pub(crate) fn vim(&self) -> Option<&vim::TextBuffer> {
        match &self.content {
            TabContent::Pager(b) => Some(b),
            _ => None,
        }
    }

    pub(crate) fn vim_mut(&mut self) -> Option<&mut vim::TextBuffer> {
        match &mut self.content {
            TabContent::Pager(b) => Some(b),
            _ => None,
        }
    }

    pub(crate) fn term(&self) -> Option<&TermSession> {
        match &self.content {
            TabContent::Term(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn ai(&self) -> Option<&crate::ai::AiState> {
        match &self.content {
            TabContent::Ai(a) => Some(a),
            _ => None,
        }
    }

    pub(crate) fn ai_mut(&mut self) -> Option<&mut crate::ai::AiState> {
        match &mut self.content {
            TabContent::Ai(a) => Some(a),
            _ => None,
        }
    }

    pub(crate) fn term_mut(&mut self) -> Option<&mut TermSession> {
        match &mut self.content {
            TabContent::Term(s) => Some(s),
            _ => None,
        }
    }

    /// Detach the terminal session (for shutdown), leaving a blank tab behind.
    pub(crate) fn take_term(&mut self) -> Option<TermSession> {
        if !matches!(self.content, TabContent::Term(_)) {
            return None;
        }
        match std::mem::replace(&mut self.content, TabContent::Blank) {
            TabContent::Term(s) => Some(s),
            _ => unreachable!(),
        }
    }
}

/// Sentinel URL for a blank pane (see [`Tab::blank`]).
pub(crate) const BLANK_URL: &str = "browser://blank";

/// State for an engine-free read tab: the extracted document, the vertical scroll
/// offset, and a cache of the laid-out lines (recomputed when the width/zoom
/// changes or the document is replaced by following a link).
pub(crate) struct NativeRead {
    pub(crate) doc: browser_core::Document,
    /// Top-of-viewport scroll offset in pixels (>= 0).
    pub(crate) scroll: i32,
    pub(crate) layout: read_view::Layout,
    /// Content width / font px the cached layout was built at; `dirty` forces a
    /// rebuild after the document is swapped (the width/px may be unchanged).
    pub(crate) layout_w: i32,
    pub(crate) layout_px: f32,
    pub(crate) dirty: bool,
    /// Vim caret/visual-selection state, when caret mode is active (`v`/`V`). Its
    /// `lines` mirror `layout.text_lines()` so a row maps 1:1 to a visual line;
    /// `None` means plain scroll mode.
    pub(crate) caret: Option<vim::TextBuffer>,
}

/// Strip a leading `-t` / `--tab` flag from a command argument, returning
/// `(open_in_new_tab, remaining_target)`. Only matches the flag as a whole token, so
/// a target like `-test` or a URL is left untouched.
pub(crate) fn parse_tab_flag(rest: &str) -> (bool, &str) {
    for flag in ["-t", "--tab"] {
        if let Some(r) = rest.strip_prefix(flag) {
            if r.is_empty() || r.starts_with(char::is_whitespace) {
                return (true, r.trim_start());
            }
        }
    }
    (false, rest)
}

/// Build an engine-free native tab rendering `doc` (read mode when `read`). The
/// layout is left empty/dirty so it's laid out on the next draw at the live width.
pub(crate) fn native_read_tab(doc: browser_core::Document, url: String, read: bool) -> Tab {
    Tab {
        url,
        nojs: false,
        read,
        research: false,
        nav: TabNav::default(),
        content: TabContent::Read(NativeRead {
            doc,
            scroll: 0,
            layout: read_view::Layout {
                lines: Vec::new(),
                line_h: 1,
                height: 0,
                text: Vec::new(),
            },
            layout_w: -1,
            layout_px: 0.0,
            dirty: true,
            caret: None,
        }),
    }
}

impl App {
    /// Open a page from a target (URL or query). `new_tab` opens it as a new tab;
    /// otherwise it replaces the active tab in place (`:open` default, `o`). With no
    /// active tab the two are equivalent (a fresh tab is created).
    pub(crate) fn open_tab(&mut self, target: &str, disable_js: bool, new_tab: bool) {
        // Frozen: refuse to spin up a new web engine — the whole point is to NOT
        // process web content. Native pages (:read/:te/:res/…) stay available.
        if self.frozen {
            self.set_status("browser is frozen — :unfreeze first");
            return;
        }
        let url = self.resolve_target(target);
        self.record_history(&url);
        match self.build_content_webview(Source::Url(url.clone()), disable_js, "") {
            Ok(webview) => {
                let tab = Tab {
                    content: TabContent::Web(webview),
                    url,
                    nojs: disable_js,
                    read: false,
                    research: false,
                    // The first live-URL refresh after this open is the page's own
                    // landing, not a navigation worth a back-stack entry.
                    nav: TabNav { settling: true, ..TabNav::default() },
                };
                self.place_tab(tab, new_tab);
                // Keep the keyboard on the shell; the page-load handler re-asserts
                // this once navigation finishes (which is when focus tends to move).
                self.window.set_focus();
                self.set_status(if disable_js { "(no-js)" } else { "" });
            }
            Err(e) => self.set_error(format!("failed to open: {e:#}")),
        }
    }

    /// Open a "research" tab: like `:open` (URL or → search engine), but JS-on with
    /// the [`RESEARCH_JS`] pruner injected so video/audio/embeds are stripped while
    /// images and text stay. A lighter browse for "how do I…" lookups. `new_tab` as
    /// in [`open_tab`].
    pub(crate) fn open_research(&mut self, target: &str, new_tab: bool) {
        if self.frozen {
            self.set_status("browser is frozen — :unfreeze first");
            return;
        }
        let url = self.resolve_target(target);
        match self.build_content_webview(Source::Url(url.clone()), false, RESEARCH_JS) {
            Ok(webview) => {
                let tab = Tab {
                    content: TabContent::Web(webview),
                    url,
                    nojs: false,
                    read: false,
                    research: true,
                    nav: TabNav { settling: true, ..TabNav::default() },
                };
                self.place_tab(tab, new_tab);
                self.window.set_focus();
                self.set_status("(research — media stripped)");
            }
            Err(e) => self.set_error(format!("failed to open: {e:#}")),
        }
    }

    /// Insert `tab`, either as a NEW tab at the end (`new_tab`, or when nothing is
    /// active) or REPLACING the active tab in place (`:open`/`o` default). The tab it
    /// evicts is recorded on the closed-tab stack (so `U` can bring it back) and its
    /// terminal, if any, is shut down deterministically first.
    ///
    /// Under a split, new content ALWAYS lands in the focused pane (in place), since
    /// a background tab would have no pane to show it — `new_tab` is ignored there.
    pub(crate) fn place_tab(&mut self, tab: Tab, new_tab: bool) {
        let replace = (!new_tab || self.split.is_some()) && self.active.is_some();
        match self.active {
            Some(i) if replace && i < self.tabs.len() => {
                self.record_closed(i);
                // Carry this slot's back/forward stacks across the rebuild. Unless
                // this replacement IS a back/forward replay (`nav_replaying`), the
                // page we're leaving becomes the new top of the back stack so `H`
                // returns to it, and any forward history is truncated.
                let mut back = std::mem::take(&mut self.tabs[i].nav.back);
                let mut fwd = std::mem::take(&mut self.tabs[i].nav.fwd);
                if !self.nav_replaying {
                    if let Some(entry) = nav_entry(&self.tabs[i]) {
                        nav_push(&mut back, entry);
                        fwd.clear();
                    }
                }
                if let Some(session) = self.tabs[i].take_term() {
                    session.shutdown();
                }
                let mut tab = tab;
                tab.nav.back = back;
                tab.nav.fwd = fwd;
                self.tabs[i] = tab;
                self.active = Some(i);
            }
            _ => {
                self.tabs.push(tab);
                self.active = Some(self.tabs.len() - 1);
            }
        }
        self.find_reset();
        self.refresh_visibility();
        self.window.request_redraw();
    }

    /// Record tab `i` on the closed-tab stack so `U` / Ctrl+Shift+T can reopen it.
    /// Mirrors the session's restorable-tab rules: live `browser://…` pages and the
    /// error/res vim pagers are session-specific and skipped.
    pub(crate) fn record_closed(&mut self, i: usize) {
        let Some(t) = self.tabs.get(i) else { return };
        if t.url.starts_with("browser://") || t.vim().is_some() {
            return;
        }
        let kind = if t.term().is_some() {
            "term"
        } else if t.read || t.native().is_some() {
            "read"
        } else if t.research {
            "research"
        } else if t.nojs {
            "nojs"
        } else {
            "open"
        };
        self.closed_tabs.push(session::SavedTab { kind: kind.to_string(), url: t.url.clone() });
        if self.closed_tabs.len() > CLOSED_CAP {
            self.closed_tabs.remove(0);
        }
    }

    /// Reopen the most recently closed tab (`U` / Ctrl+Shift+T), as a new tab. Read
    /// tabs are re-fetched and terminals reopened fresh, matching session restore.
    pub(crate) fn reopen_closed(&mut self) {
        let Some(c) = self.closed_tabs.pop() else {
            self.set_status("no closed tab to reopen");
            return;
        };
        match c.kind.as_str() {
            "term" => self.open_terminal(),
            "read" => self.start_read(&c.url, false, true),
            "research" => self.open_research(&c.url, true),
            "nojs" => self.open_tab(&c.url, true, true),
            _ => self.open_tab(&c.url, false, true),
        }
    }

    /// Build a child webview from either a URL or an inline HTML document, with
    /// the full shell bridge (keybindings, focus reclaim, hint mode).
    pub(crate) fn build_content_webview(
        &self,
        source: Source,
        disable_js: bool,
        extra_init: &str,
    ) -> Result<WebView> {
        let ipc_proxy = self.proxy.clone();
        let load_proxy = self.proxy.clone();
        let nav_proxy = self.proxy.clone();
        // Shared with the App so `:ads` toggles the native redirect guard live.
        let adblock_on = self.adblock_on.clone();
        let popup_adblock = self.adblock_on.clone();
        let popup_proxy = self.proxy.clone();
        // The popup guard consults the same trusted-gesture stamp and blocklist engine the
        // navigation guards do, to tell a real "open in new tab" from a scripted popunder.
        let popup_intent = self.nav_intent.clone();
        let popup_blocker = self.blocker.clone();
        // This tab's current top origin, so the blocklist's `$third-party` rules resolve
        // against the right source on each navigation. `cur_top` is the full top-frame
        // URL, used by the sub-resource blocker to avoid 403-ing the main document.
        let cur_origin = Arc::new(Mutex::new(String::new()));
        let nav_origin = cur_origin.clone();
        let cur_top = Arc::new(Mutex::new(String::new()));
        let nav_top = cur_top.clone();
        // Dedicated clones for the sub-resource (WebResourceRequested) blocker, since the
        // closures below consume their own.
        let net_blocker = self.blocker.clone();
        let net_adblock = self.adblock_on.clone();
        // Page-reported "a TRUSTED gesture landed on a real cross-site link/submit" — the
        // one signal that a cross-site top navigation is genuinely wanted. Stamped here,
        // directly (no event-loop hop), so it lands before the navigation it authorises
        // reaches the native guard. Shared (cloned `Arc`) with that guard.
        let intent_set = self.nav_intent.clone();
        // The uBlock-style domain blocklist engine — the race-free primary redirect guard.
        let blocker = self.blocker.clone();
        // Download guard: block executable/installer types unless the user opted in.
        let dl_allow = self.allow_risky_downloads.clone();
        let dl_proxy = self.proxy.clone();
        let mut builder = WebViewBuilder::new();
        builder = match source {
            Source::Url(u) => builder.with_url(u),
            Source::Html(h) => builder.with_html(h),
        };
        // Load uBlock Origin (any unpacked extension in the dir) into WebView2's own
        // Chromium engine. The extension does network + cosmetic + scriptlet ad-blocking
        // natively — far more capable than a hand-rolled blocker, and it doesn't depend on
        // the WebResourceRequested path. Extensions are per-profile and off by default;
        // every content webview shares the default profile and enables them the same way
        // (wry requires that consistency). Absent dir → simply no extensions.
        if let Some(ext_dir) = ublock_extensions_dir() {
            builder = builder.with_browser_extensions_enabled(true).with_extensions_path(ext_dir);
        }
        builder = builder
            .with_bounds(self.content_rect())
            .with_focused(false)
            // Disable Chromium's built-in accelerators (Shift+Esc task manager,
            // Ctrl+F/P, F12, …) so our own keybindings own the keyboard. Standard
            // editing keys (Ctrl+C/V/X) are unaffected.
            .with_browser_accelerator_keys(false)
            // Browser process flags — see BROWSER_ARGS. MUST match every other
            // webview (terminal included) or WebView2 creation fails with 0x8007139F.
            .with_additional_browser_args(BROWSER_ARGS)
            // The page-side blocker (cosmetic hiding + popunder/redirect-intent) is
            // injected into EVERY frame (for_main_only = false), not just the top
            // document: scummy sites drive popunders from cross-origin player/ad iframes,
            // which a main-frame-only injection would leave unguarded. (Network blocking
            // of the ad scripts themselves is native — see `netblock`.) It starts in the
            // shell's current state (baked as `__adblockDefault`), so `:ads` flips it live
            // in the top frame via `__setAdblock` (sub-frames adopt it on reload).
            // `window.ipc` is absent in sub-frames, so the blocker's status reports are
            // main-frame-only (guarded), but the neutering itself is universal.
            .with_initialization_script_for_main_only(
                {
                    let ab = self.adblock;
                    format!("{IPC_PRELUDE}\nwindow.__adblockDefault={ab};\n{ADBLOCK_JS}")
                },
                false,
            )
            // The shell bridge (keybindings, focus reclaim, hint mode) and the page-
            // feature toggles stay MAIN-FRAME-ONLY — focus/IPC plumbing must not run
            // per iframe. They start in the shell's current state too, so a tab opened
            // while a toggle is active is already in that state. `extra_init` (e.g.
            // research-mode DOM pruning) is appended last.
            .with_initialization_script({
                let (m, c, v) = (self.mute, self.no_css, self.no_video);
                let mut init = format!(
                    "{IPC_PRELUDE}\n{BRIDGE_JS}\n{FIND_JS}\n{CARET_JS}\n\
                     window.__featureDefaults={{mute:{m},css:{c},video:{v}}};\n{FEATURES_JS}"
                );
                if !extra_init.is_empty() {
                    init.push('\n');
                    init.push_str(extra_init);
                }
                init
            })
            .with_ipc_handler(move |req| match req.body().as_str() {
                // A trusted click/keypress on a real cross-site link or submit control:
                // authorise the cross-site top navigation it's about to trigger. Stamped
                // directly so it beats that navigation to the native guard.
                "nav-intent" => {
                    if let Ok(mut g) = intent_set.lock() {
                        *g = Some(std::time::Instant::now());
                    }
                }
                "leave-passthrough" => {
                    let _ = ipc_proxy.send_event(UserEvent::ExitToNormal);
                }
                "page-ready" => {
                    let _ = ipc_proxy.send_event(UserEvent::FocusShell);
                }
                "grab-focus" => {
                    let _ = ipc_proxy.send_event(UserEvent::GrabFocus);
                }
                "page-hold" => {
                    let _ = ipc_proxy.send_event(UserEvent::PageHold);
                }
                "page-edit" => {
                    let _ = ipc_proxy.send_event(UserEvent::PageEdit);
                }
                "pane-click" => {
                    let _ = ipc_proxy.send_event(UserEvent::PaneClick);
                }
                "hint-exit" => {
                    let _ = ipc_proxy.send_event(UserEvent::ExitHint);
                }
                "hint-edit" => {
                    let _ = ipc_proxy.send_event(UserEvent::HintEdit);
                }
                "caret-exit" => {
                    let _ = ipc_proxy.send_event(UserEvent::CaretExit);
                }
                "fs-enter" => {
                    let _ = ipc_proxy.send_event(UserEvent::PageFullscreen(true));
                }
                "fs-exit" => {
                    let _ = ipc_proxy.send_event(UserEvent::PageFullscreen(false));
                }
                body => {
                    // Web caret-mode yanked a selection: `caret-yank:<text>`.
                    if let Some(text) = body.strip_prefix("caret-yank:") {
                        let _ = ipc_proxy.send_event(UserEvent::CaretYank(text.to_string()));
                    // A hint in new-tab mode resolved to a link: `hint-open:<href>`.
                    } else if let Some(href) = body.strip_prefix("hint-open:") {
                        let _ = ipc_proxy.send_event(UserEvent::HintOpen(href.to_string()));
                    // The page blocker neutered a scripted pop-up. `popup-blocked:<url>`.
                    } else if let Some(url) = body.strip_prefix("popup-blocked:") {
                        let _ = ipc_proxy.send_event(UserEvent::PopupBlocked(url.to_string()));
                    // The pointer moved onto/off a link: `link-hover:<href>` (empty = off).
                    } else if let Some(href) = body.strip_prefix("link-hover:") {
                        let _ = ipc_proxy.send_event(UserEvent::LinkHover(href.to_string()));
                    }
                }
            })
            // Backup focus reclaim for non-JS tabs (page-ready covers JS tabs).
            .with_on_page_load_handler(move |event, _url| {
                if matches!(event, PageLoadEvent::Finished) {
                    let _ = load_proxy.send_event(UserEvent::FocusShell);
                }
            })
            // Kill auto-translate: a foreign-language site (or a saved/clicked link)
            // can land us on Google's `*.translate.goog` proxy, which mangles the URL
            // and rewrites the page. Cancel any such navigation and load the original
            // (de-proxied) URL instead, so we always show the real page.
            .with_navigation_handler(move |url| {
                if is_translate_proxy(&url) {
                    if let Some(original) = deproxy_translate(&url) {
                        let _ = nav_proxy.send_event(UserEvent::Navigate(original));
                        return false;
                    }
                }
                // Cancel any top-level navigation to a known ad/redirect/malware DOMAIN,
                // however it was triggered (scripted redirect, `<meta refresh>`, server
                // 3xx). The EasyList-style engine blocks by name like uBlock; `url_is_ad_host`
                // is the tiny always-on fallback for the beat before the engine compiles.
                // Forced redirects to UNLISTED domains (the cross-site hijack vector) are
                // caught separately by the native intent-gate guard (see `navguard`).
                // Honours `:ads` live.
                if adblock_on.load(Ordering::Relaxed) {
                    let src = nav_origin.lock().unwrap().clone();
                    if crate::blocklist::blocks_navigation(&blocker, &url, &src)
                        || url_is_ad_host(&url)
                    {
                        let _ = nav_proxy.send_event(UserEvent::RedirectBlocked(url));
                        return false;
                    }
                }
                // Remember the current top origin so the blocklist's `$third-party` rules
                // resolve against the right source on the next navigation, and the full
                // top URL so the sub-resource blocker can tell the main document apart
                // from sub-frames.
                let origin = origin_of(&url);
                if !origin.is_empty() {
                    *nav_origin.lock().unwrap() = origin;
                    *nav_top.lock().unwrap() = url.clone();
                }
                true
            })
            // Popunder / forced-popup guard, uBlock-Origin-style. Scummy sites spawn scam
            // tabs via `window.open` (often an `about:blank` shell they then navigate) or
            // `target=_blank` on ANY click; this native handler fires for every frame and
            // every variant. Rather than deny EVERY new window (which also killed real
            // "open in new tab" clicks — e.g. Google results), we decide like uBO does:
            // a new window is legitimate only when a TRUSTED user gesture just landed on a
            // link (the same `nav-intent` stamp the redirect guard trusts) AND the
            // destination isn't a known ad/scam domain. Real clicks carry that gesture, so
            // they now re-open as a shell-managed tab; scripted popunders (timers/onload,
            // or synthetic clicks aimed at an ad domain) carry no gesture or hit the
            // blocklist, so they stay blocked. Either way the native OS window is suppressed
            // — new tabs here live in our tab strip, not as OS popups. `:ads` off restores
            // normal popups. (The page-side `window.open` neuter is a further backstop.)
            .with_new_window_req_handler(move |url, _features| {
                if !popup_adblock.load(Ordering::Relaxed) {
                    return NewWindowResponse::Allow;
                }
                let wanted = crate::navguard::recent(&popup_intent)
                    && !crate::blocklist::blocks_navigation(&popup_blocker, &url, "")
                    && !url_is_ad_host(&url);
                let _ = popup_proxy.send_event(if wanted {
                    UserEvent::OpenPopupTab(url)
                } else {
                    UserEvent::PopupBlocked(url)
                });
                NewWindowResponse::Deny
            })
            // Drive-by install guard. A scam page (or a redirect we missed) can kick off
            // a download of an `.exe`/`.msi`/etc. that a careless click would run. Block
            // executable/installer types by default and warn loudly; everything else
            // (zip, pdf, images, media …) downloads normally. `:downloads` opts in.
            .with_download_started_handler(move |url, path| {
                if dl_allow.load(Ordering::Relaxed) || !is_risky_download(&url, path) {
                    return true;
                }
                let name = download_name(&url, path);
                let _ = dl_proxy.send_event(UserEvent::DownloadBlocked(name));
                false
            });
        if disable_js {
            builder = builder.with_javascript_disabled();
        }
        let webview =
            builder.build_as_child(&*self.window).map_err(|e| anyhow::anyhow!("{e}"))?;
        // The native redirect guard: cancels forced (non-user-initiated) cross-site top
        // navigations via WebView2's own `IsUserInitiated` — the structural fix the
        // URL-only wry handler above can't be. Best-effort; the wry guards still stand.
        crate::navguard::install(
            &webview,
            self.adblock_on.clone(),
            self.nav_intent.clone(),
            self.proxy.clone(),
        );
        // The uBlock-style sub-resource network blocker: runs the full EasyList engine
        // over every script/iframe/XHR so ad/scam injectors never load (Windows only).
        #[cfg(windows)]
        crate::netblock::install(&webview, net_blocker, net_adblock, cur_origin, cur_top);
        // wry loads bundled extensions ENABLED on every new webview; if we're not in uBlock
        // mode, disable them here so the current `:adblock` choice holds for this tab too.
        #[cfg(windows)]
        if self.adblock_mode != AdblockMode::Ubo {
            crate::extensions::set_all_enabled(&webview, false);
        }
        Ok(webview)
    }

    /// Kick off a background readability extraction into the Document model; the
    /// result arrives as a ReadReady/ReadFailed user event so the UI stays
    /// responsive. `replace` swaps the active read tab's doc in place (link-follow
    /// / reload) rather than opening a new tab. `record` adds a back-stack step for
    /// the page being left (false for reloads and `H`/`L` history replays).
    pub(crate) fn start_read(&mut self, target: &str, replace: bool, record: bool) {
        let proxy = self.proxy.clone();
        // A plain query (no `!bang`, not a URL) is run through the SEARCH backend,
        // which returns a clean, followable results document — readability can't parse
        // a live search-results page (it errors "failed to grab the article"). A bang
        // or a real address is fetched + run through readability as before.
        let bang = browser_core::expand_bang(target);
        if bang.is_none() && browser_core::looks_like_query(target) {
            let query = target.to_string();
            let config = browser_core::SearchConfig::default(); // DDG-lite, zero-config
            self.set_status(format!("searching {query} …"));
            std::thread::spawn(move || {
                let event = match browser_backend_search::search_blocking(&query, config) {
                    Ok(doc) => UserEvent::ReadReady { doc: Box::new(doc), replace, record },
                    Err(e) => UserEvent::ReadFailed(format!("{e:#}")),
                };
                let _ = proxy.send_event(event);
            });
        } else {
            let url = bang
                .or_else(|| browser_core::normalize_url(target))
                .unwrap_or_else(|| target.to_string());
            self.set_status(format!("reading {url} …"));
            std::thread::spawn(move || {
                let event = match browser_backend_text::fetch_document_blocking(&url) {
                    Ok(doc) => UserEvent::ReadReady { doc: Box::new(doc), replace, record },
                    Err(e) => UserEvent::ReadFailed(format!("{e:#}")),
                };
                let _ = proxy.send_event(event);
            });
        }
        self.window.request_redraw();
    }

    /// Render an extracted Document in an engine-free read tab (no WebView2).
    /// `replace` means "open in the current tab": if the active tab is ALREADY a read
    /// tab its document is swapped in place (link-follow/reload, keeping the tab); if
    /// it's some other tab type, that tab is replaced with a fresh read tab. Without
    /// `replace` (`:read -t`), a new read tab is opened. `record` adds a back-stack
    /// step for the page being left (false for reloads and `H`/`L` history replays).
    pub(crate) fn show_read_document(&mut self, doc: browser_core::Document, replace: bool, record: bool) {
        let url = doc.url.clone();
        if replace {
            if let Some(i) = self.active {
                // Fast path: active is already a read tab → swap its doc, keep the tab.
                if self.tabs.get(i).is_some_and(|t| t.native().is_some()) {
                    // The page we're leaving becomes the new top of the back stack.
                    if record {
                        if let Some(entry) = nav_entry(&self.tabs[i]) {
                            nav_push(&mut self.tabs[i].nav.back, entry);
                            self.tabs[i].nav.fwd.clear();
                        }
                    }
                    let nr = self.tabs[i].native_mut().unwrap();
                    nr.doc = doc;
                    nr.scroll = 0;
                    nr.dirty = true;
                    nr.caret = None; // the text changed; drop any caret/selection
                    self.tabs[i].url = url;
                    self.clear_status();
                    self.window.request_redraw();
                    return;
                }
                // Active is a web/terminal tab → replace it in place with a read tab.
                // `place_tab` records the page being left, gated by `nav_replaying`.
                let tab = native_read_tab(doc, url, true);
                let prev = self.nav_replaying;
                self.nav_replaying = !record;
                self.place_tab(tab, false);
                self.nav_replaying = prev;
                self.window.set_focus();
                self.clear_status();
                return;
            }
        }
        self.push_native_tab(doc, url, true);
    }

    /// Open a new engine-free native tab rendering `doc` (no WebView2 process). Used
    /// by read mode (`read = true`, tinted green + `f` hint) and the `:error(s)`
    /// pages (`read = false`). Activates and focuses the new tab.
    pub(crate) fn push_native_tab(&mut self, doc: browser_core::Document, url: String, read: bool) {
        // place_tab is split-aware: a new tab normally, or the focused pane in place.
        self.place_tab(native_read_tab(doc, url, read), true);
        self.window.set_focus();
        self.clear_status();
    }

    pub(crate) fn close_active(&mut self) {
        // The AI tab is a persistent background singleton: `x` HIDES it (switches to a
        // content tab, keeping the conversation alive) rather than destroying it.
        if self.active_is_ai() {
            self.hide_ai_tab();
            return;
        }
        let Some(i) = self.active else {
            self.set_status("no tab to close");
            return;
        };
        // Remember it so `U` / Ctrl+Shift+T can reopen it (internal pages are skipped
        // by record_closed). Shut a terminal down deterministically (kill shell, close
        // PTY, join reader) before dropping the tab; dropping the WebView frees the renderer.
        self.record_closed(i);
        if let Some(session) = self.tabs[i].take_term() {
            session.shutdown();
        }
        // Drop the tab and fix the pane tree (prune its leaf, collapse to single-pane
        // when one remains); focus the surviving pane.
        self.active = self.drop_tab(i);
        // Closing a tab must never surface the background AI singleton: if the survivor
        // we'd land on is the AI tab, step away to a real tab (or the welcome screen).
        if self.active.and_then(|a| self.tabs.get(a)).is_some_and(|t| t.ai().is_some()) {
            self.focus_away_from_ai();
        }
        self.find_reset();
        self.mode = ModeKind::Normal;
        self.refresh_visibility();
        self.window.set_focus();
    }

    /// Hide the AI tab (the `x` action on it): step focus away so the chat stays alive
    /// in the background, never destroying it. Returns to the tab we came from when the
    /// AI tab was summoned, or — if the AI tab is the only one left — to the welcome
    /// screen (`active = None`), exactly as if no tab were open.
    fn hide_ai_tab(&mut self) {
        self.focus_away_from_ai();
        self.mode = ModeKind::Normal;
        self.find_reset();
        self.window.set_focus();
        self.window.request_redraw();
    }

    /// Point `active` at a visible (non-AI) tab without destroying the AI singleton:
    /// prefer the tab active before the AI tab was summoned, else the first visible
    /// tab, else the welcome screen (`None`) when only the background AI tab remains.
    fn focus_away_from_ai(&mut self) {
        let prev = self
            .ai_prev_active
            .filter(|&i| self.tabs.get(i).is_some_and(|t| t.ai().is_none()));
        self.ai_prev_active = None;
        match prev.or_else(|| self.visible_tab_indices().first().copied()) {
            Some(idx) => self.show_tab(idx),
            None => {
                // Only the hidden AI tab is left: fall back to the welcome screen.
                self.active = None;
                self.refresh_visibility();
            }
        }
    }

    /// Remove tab `i` from `tabs` and repair the pane tree: prune its leaf if it was
    /// in a pane, shift higher leaf indices down, and collapse to a single pane (no
    /// split) when only one leaf remains. Returns the tab index that should take
    /// focus if the *focused* pane was the one closed (else the caller keeps its own
    /// active tab, adjusted for the shift). `None` when no tabs remain.
    pub(crate) fn drop_tab(&mut self, i: usize) -> Option<usize> {
        self.tabs.remove(i);
        let collapsed_focus = if let Some(tree) = self.split.take() {
            match tree.prune(i) {
                Some(mut t) => {
                    t.shift_after_remove(i);
                    let mut leaves = Vec::new();
                    t.leaves(&mut leaves);
                    let focus = t.first_leaf();
                    self.split = (leaves.len() > 1).then_some(t);
                    Some(focus)
                }
                None => None, // the pruned leaf was the whole tree
            }
        } else {
            None
        };
        if self.tabs.is_empty() {
            return None;
        }
        Some(collapsed_focus.unwrap_or_else(|| i.min(self.tabs.len() - 1)))
    }

    /// Real indices of the tabs that appear in the strip and are reachable by tab
    /// navigation: every tab EXCEPT the `:ai` singleton, which is a background tab
    /// surfaced only by `:ai`. Order matches `tabs`. Navigation/click/`tab_labels`
    /// all index through this so the AI tab is invisible to the normal tab flow.
    pub(crate) fn visible_tab_indices(&self) -> Vec<usize> {
        self.tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| t.ai().is_none())
            .map(|(i, _)| i)
            .collect()
    }

    pub(crate) fn switch_tab(&mut self, delta: i32) {
        let visible = self.visible_tab_indices();
        if visible.is_empty() {
            return;
        }
        let cur = self.active.unwrap_or(visible[0]);
        // Position of the active tab among the visible ones; if the AI tab is up, start
        // from the first visible tab so n/p step onto real content.
        let pos = visible.iter().position(|&i| i == cur).unwrap_or(0) as i32;
        let n = visible.len() as i32;
        let next = visible[(pos + delta).rem_euclid(n) as usize];
        self.show_tab(next);
    }

    /// Jump directly to a zero-based *visible* tab position (bound to keys 1..9) —
    /// the AI singleton isn't counted, so the digits match the strip.
    pub(crate) fn jump_to(&mut self, index: usize) {
        if let Some(&real) = self.visible_tab_indices().get(index) {
            self.show_tab(real);
        }
    }

    /// Show tab `target` as the active one. With no split it simply becomes active;
    /// while split it's loaded into the FOCUSED pane (swapping panes if it's already
    /// shown in another), so the layout never points at a tab outside the tiling.
    pub(crate) fn show_tab(&mut self, target: usize) {
        if self.split.is_some() {
            let Some(cur) = self.active else { return };
            if cur != target {
                if let Some(tree) = self.split.as_mut() {
                    let mut leaves = Vec::new();
                    tree.leaves(&mut leaves);
                    if leaves.contains(&target) {
                        tree.swap_leaves(cur, target);
                    } else {
                        tree.retarget(cur, target);
                    }
                }
            }
        }
        self.active = Some(target);
        self.find_reset();
        // The previous tab's hovered-link readout doesn't belong to the new tab.
        self.hover_link = None;
        self.refresh_visibility();
        self.window.set_focus();
    }

    /// Map an x pixel on the tab bar to a zero-based *visible* tab position (what
    /// [`jump_to`](App::jump_to) takes), for click-to-switch. Mirrors `draw_tab_bar`'s
    /// layout and numbering, which also skip the background AI tab.
    pub(crate) fn tab_at_pixel(&self, px: f64) -> Option<usize> {
        let p = &self.painter;
        let labels = self.tab_labels();
        let limit = self.inner().0 as usize;
        let px = px.max(0.0) as usize;
        let mut x = 8usize;
        for (pos, (label, active, _)) in labels.iter().enumerate() {
            let text = if *active {
                format!("[{}:{}]", pos + 1, label)
            } else {
                format!(" {}:{} ", pos + 1, label)
            };
            let end = x + p.measure(&text) + 6;
            if px >= x && px < end {
                return Some(pos);
            }
            x = end;
            if x > limit.saturating_sub(40) {
                break;
            }
        }
        None
    }

    /// Move the active tab one position left (-1) or right (+1) among the visible
    /// tabs (the background AI tab is skipped, so it never wedges the order).
    pub(crate) fn move_tab(&mut self, delta: i32) {
        let Some(i) = self.active else { return };
        let visible = self.visible_tab_indices();
        let Some(pos) = visible.iter().position(|&t| t == i) else { return };
        let target = pos as i32 + delta;
        if target < 0 || target as usize >= visible.len() {
            return;
        }
        let j = visible[target as usize];
        self.tabs.swap(i, j);
        // Keep each pane showing the same content after the index swap.
        if let Some(tree) = self.split.as_mut() {
            tree.swap_leaves(i, j);
        }
        self.active = Some(j);
        self.refresh_visibility();
    }

    pub(crate) fn refresh_visibility(&mut self) {
        // A web tab is visible iff it occupies a pane; position it at that pane's
        // rect. With no split this is just the active tab filling the band.
        let (panes, _) = self.pane_layout();
        let active = self.active;
        let split = self.split.is_some();
        for (i, tab) in self.tabs.iter().enumerate() {
            let Some(wv) = tab.webview() else { continue };
            // Frozen: every web tab stays hidden (and suspended) regardless of layout,
            // so switching tabs can't un-hide one. `:unfreeze` clears the flag.
            if self.frozen {
                let _ = wv.set_visible(false);
                continue;
            }
            match panes.iter().find(|(t, _)| *t == i) {
                Some((_, r)) => {
                    let mut rect = *r;
                    // Inset the FOCUSED web pane by the border width so the accent
                    // border we paint on our surface shows around it (the webview HWND
                    // otherwise covers it). Native panes draw their border on top.
                    if split && Some(i) == active {
                        let b = FOCUS_BORDER;
                        rect = PaneRect {
                            x: rect.x + b,
                            y: rect.y + b,
                            w: (rect.w - 2 * b).max(1),
                            h: (rect.h - 2 * b).max(1),
                        };
                    }
                    let _ = wv.set_visible(true);
                    let _ = wv.set_bounds(wry_rect(rect));
                }
                None => {
                    let _ = wv.set_visible(false);
                }
            }
        }
        // Cache the focus-backstop facts for the event-loop timer tail. Every layout
        // mutation (open/close/switch/split/move/resize/restore) funnels through here,
        // so the idle path can read two bools instead of walking the pane tree.
        self.active_pane_is_webview = active
            .and_then(|i| self.tabs.get(i))
            .is_some_and(|t| t.webview().is_some());
        self.background_webview_visible = panes.iter().any(|(t, _)| {
            Some(*t) != active && self.tabs.get(*t).is_some_and(|tab| tab.webview().is_some())
        });
        self.window.request_redraw();
    }

    /// Half the visible content height, in px (the Ctrl+D / Ctrl+U scroll step).
    pub(crate) fn half_page(&self) -> i32 {
        let (_, h) = self.inner();
        ((h as i32 - self.bar_h() as i32).max(40)) / 2
    }

    pub(crate) fn scroll(&mut self, dy: i32) {
        // Engine-free read tab: move the native scroll offset, clamped to content.
        let view = self.content_view_h();
        if let Some(nr) = self.active_native_mut() {
            let max = (nr.layout.height - view).max(0);
            nr.scroll = (nr.scroll + dy).clamp(0, max);
            self.window.request_redraw();
            return;
        }
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script(&format!("window.scrollBy(0,{dy});"));
        }
    }

    /// Jump to the top (`g`) or bottom (`G`) of the active page/document.
    pub(crate) fn scroll_edge(&mut self, bottom: bool) {
        let view = self.content_view_h();
        if let Some(nr) = self.active_native_mut() {
            nr.scroll = if bottom { (nr.layout.height - view).max(0) } else { 0 };
            self.window.request_redraw();
            return;
        }
        if let Some(wv) = self.active_webview() {
            let js = if bottom {
                "window.scrollTo(0,document.body.scrollHeight);"
            } else {
                "window.scrollTo(0,0);"
            };
            let _ = wv.evaluate_script(js);
        }
    }

    /// `H` (back) / `L` (forward): walk the active tab's shell-managed history. The
    /// page being left moves onto the opposite stack, then the target is reopened in
    /// place. Unlike WebView2's own `history.back()`, this spans webview rebuilds
    /// (every `:open`/search makes a fresh engine with no prior history) and covers
    /// read tabs (which have no engine history at all) — so it goes back the full way.
    pub(crate) fn history(&mut self, forward: bool) {
        let Some(i) = self.active else { return };
        // Nothing to go to?
        let empty = {
            let Some(tab) = self.tabs.get(i) else { return };
            if forward { tab.nav.fwd.is_empty() } else { tab.nav.back.is_empty() }
        };
        if empty {
            self.set_status(if forward { "no forward history" } else { "no back history" });
            return;
        }
        // Fast path: if this is a web tab and the ENGINE's own session history can make
        // this exact step (the adjacent page was a clicked link/form within the current
        // webview instance), let it — instant and cached, with scroll/form state intact,
        // no reload. Crossing a `:open`/search rebuild boundary (or a read tab) has no
        // engine entry to restore, so it falls through to reopening the page.
        let engine = self
            .tabs
            .get(i)
            .and_then(|t| t.webview())
            .is_some_and(|wv| crate::navguard::can_go(wv, forward));

        // Pop the target and move the current page onto the opposite stack so the
        // reverse key returns to it.
        let tab = self.tabs.get_mut(i).unwrap();
        let entry = if forward { tab.nav.fwd.pop() } else { tab.nav.back.pop() }.unwrap();
        if let Some(cur) = nav_entry(tab) {
            if forward {
                nav_push(&mut tab.nav.back, cur);
            } else {
                nav_push(&mut tab.nav.fwd, cur);
            }
        }

        if engine {
            // Pre-set the URL and settle so the engine nav's page-load doesn't record
            // the step a second time into the back stack.
            tab.url = entry.url;
            tab.nav.settling = true;
            crate::navguard::mark(&self.nav_intent);
            if let Some(wv) = self.tabs.get(i).and_then(|t| t.webview()) {
                crate::navguard::go(wv, forward);
            }
            self.window.request_redraw();
        } else {
            self.open_entry(entry);
        }
    }

    /// Reopen a [`NavEntry`] in place as part of an `H`/`L` replay: the stacks were
    /// already adjusted by [`history`](Self::history), so recording is suppressed
    /// (`nav_replaying` for the synchronous web/place_tab paths; `record = false`
    /// threaded through the asynchronous read path).
    fn open_entry(&mut self, entry: NavEntry) {
        match entry.kind {
            NavKind::Web => {
                self.nav_replaying = true;
                self.open_tab(&entry.url, false, false);
                self.nav_replaying = false;
            }
            NavKind::Nojs => {
                self.nav_replaying = true;
                self.open_tab(&entry.url, true, false);
                self.nav_replaying = false;
            }
            NavKind::Research => {
                self.nav_replaying = true;
                self.open_research(&entry.url, false);
                self.nav_replaying = false;
            }
            NavKind::Read => self.start_read(&entry.url, true, false),
        }
    }

    /// Bare `:ads`/`:adblock` — a quick on/off toggle: off when a blocker is active,
    /// otherwise back to the default uBlock Origin engine. Use `:adblock native|ubo|off`
    /// to pick a specific engine.
    pub(crate) fn toggle_adblock(&mut self) {
        let mode = if self.adblock_mode == AdblockMode::Off {
            AdblockMode::Ubo
        } else {
            AdblockMode::Off
        };
        self.set_adblock_mode(mode);
    }

    /// Switch the active ad blocker, keeping the two engines mutually exclusive so only one
    /// runs at a time. The native guards all honour the shared `adblock_on` flag, so flipping
    /// it (plus the page-side `__setAdblock`) turns the whole native stack — `netblock`, the
    /// redirect/popup guards, and `ADBLOCK_JS` — on or off in one move; the uBlock extension
    /// is enabled/disabled profile-wide via [`extensions`](crate::extensions). Persisted on
    /// the next session write; re-applied to newly built webviews (see `build_content_webview`).
    pub(crate) fn set_adblock_mode(&mut self, mode: AdblockMode) {
        self.adblock_mode = mode;
        let native_on = mode == AdblockMode::Native;
        let ext_on = mode == AdblockMode::Ubo;
        self.set_adblock(native_on); // keeps the native guards' shared flag in lock-step
        for tab in &self.tabs {
            if let Some(wv) = tab.webview() {
                let _ =
                    wv.evaluate_script(&format!("window.__setAdblock&&window.__setAdblock({native_on})"));
                #[cfg(windows)]
                crate::extensions::set_all_enabled(wv, ext_on);
            }
        }
        self.set_status(match mode {
            AdblockMode::Ubo => "adblock: uBlock Origin (extension) — native blocker off",
            AdblockMode::Native => "adblock: native blocker — uBlock extension off",
            AdblockMode::Off => "adblock OFF — no blocking",
        });
        self.window.request_redraw();
    }

    /// Toggle whether executable/installer downloads (`.exe`, `.msi`, …) are allowed.
    /// Off by default so a drive-by install is blocked; flip it on to grab a real one.
    pub(crate) fn toggle_downloads(&mut self) {
        let on = !self.allow_risky_downloads.load(Ordering::Relaxed);
        self.allow_risky_downloads.store(on, Ordering::Relaxed);
        self.set_status(if on {
            "downloads ON — executable/installer files allowed"
        } else {
            "downloads OFF — executable/installer files blocked"
        });
        self.window.request_redraw();
    }

    /// Flip a live page-feature toggle ([`FEATURES_JS`]) on every open web tab via
    /// `__setToggle` (no reload). `name` is `mute` | `css` | `video`.
    pub(crate) fn broadcast_toggle(&self, name: &str, on: bool) {
        for tab in &self.tabs {
            if let Some(wv) = tab.webview() {
                let _ = wv.evaluate_script(&format!(
                    "window.__setToggle&&window.__setToggle('{name}',{on})"
                ));
            }
        }
    }

    /// Toggle JavaScript globally. Unlike the page-feature toggles, JS can only be
    /// switched per-webview at build time, so this rebuilds the ACTIVE web tab with
    /// the new setting (a reload) and applies to newly opened tabs; background tabs
    /// adopt it when next reloaded.
    pub(crate) fn toggle_js(&mut self) {
        self.nojs = !self.nojs;
        let on = !self.nojs;
        self.set_status(format!(
            "JavaScript {} (this tab reloaded; applies to new tabs)",
            if on { "ON" } else { "OFF" }
        ));
        let Some(i) = self.active else { return };
        let Some(t) = self.tabs.get(i) else { return };
        if t.webview().is_none() {
            return; // only web tabs have JS to toggle
        }
        let url = t.url.clone();
        let research = t.research;
        let extra = if research { RESEARCH_JS } else { "" };
        match self.build_content_webview(Source::Url(url.clone()), self.nojs, extra) {
            Ok(webview) => {
                let nojs = self.nojs;
                if let Some(session) = self.tabs[i].take_term() {
                    session.shutdown();
                }
                // A reload, not a navigation: keep the slot's back/forward history,
                // and settle so the reloaded landing isn't recorded as a new step.
                let mut nav = std::mem::take(&mut self.tabs[i].nav);
                nav.settling = true;
                self.tabs[i] = Tab {
                    content: TabContent::Web(webview),
                    url,
                    nojs,
                    read: false,
                    research,
                    nav,
                };
                self.refresh_visibility();
                self.window.set_focus();
            }
            Err(e) => self.set_error(format!("failed to reload: {e:#}")),
        }
    }

    /// Reload the active tab: re-extract for an engine-free read tab, else reload
    /// the webview.
    pub(crate) fn reload_active(&mut self) {
        if let Some(url) = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.native())
            .map(|nr| nr.doc.url.clone())
        {
            // Internal native pages (e.g. `:error`) have nothing to re-extract.
            // A reload isn't a navigation, so it adds no back-stack step.
            if !url.starts_with("browser://") {
                self.start_read(&url, true, false);
            }
            return;
        }
        if let Some(wv) = self.active_webview() {
            let _ = wv.reload();
        }
    }
}

/// Encode `s` as a JavaScript string literal (quoted, with control/quote/`<`
/// escaped) for safe interpolation into an `evaluate_script` call.
pub(crate) fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Whether `url`'s HOST matches a known ad/tracker host in [`AD_HOSTS`] — the native
/// half of the forced-redirect guard. Only host-style entries are considered (those
/// without a `/`); the path-fragment entries (e.g. `/pagead/`) describe sub-resource
/// URLs the page-side blocker handles, not top-level destinations, so matching them
/// against a host would never fire anyway. Substring match (mirrors the page-side
/// `blocked()`), so `adservice.google.` catches `adservice.google.com`.
pub(crate) fn url_is_ad_host(url: &str) -> bool {
    let Some(host) = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase))
    else {
        return false;
    };
    AD_HOSTS
        .iter()
        .any(|h| !h.contains('/') && host.contains(h))
}

/// The ascii origin (`scheme://host[:port]`) of an http(s) `url`, or `""` when it can't
/// be parsed, has no host, or isn't http(s). Non-web schemes (`about:`/`data:`/`file:`)
/// deliberately yield `""` so they never read as a "different origin" in the blurred-
/// redirect check — we only block jumps between real sites.
fn origin_of(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) if u.host().is_some() && matches!(u.scheme(), "http" | "https") => {
            u.origin().ascii_serialization()
        }
        _ => String::new(),
    }
}

/// Executable/installer extensions a drive-by download must not run unprompted.
const RISKY_DOWNLOAD_EXTS: &[&str] = &[
    "exe", "msi", "msix", "msp", "scr", "bat", "cmd", "com", "cpl", "pif", "jar", "apk",
    "app", "dmg", "pkg", "deb", "rpm", "vbs", "vbe", "jse", "wsf", "wsh", "ps1", "psm1",
    "reg", "hta", "lnk", "gadget", "inf", "scf", "appx", "appxbundle",
];

/// The lower-cased file extension of a download, from the suggested save `path` first
/// (most reliable) then the URL's last path segment.
fn download_ext(url: &str, path: &std::path::Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .or_else(|| {
            let seg = url::Url::parse(url).ok()?.path_segments()?.next_back()?.to_string();
            seg.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase())
        })
}

/// Whether a download targets an executable/installer file type (see
/// [`RISKY_DOWNLOAD_EXTS`]).
fn is_risky_download(url: &str, path: &std::path::Path) -> bool {
    download_ext(url, path).is_some_and(|e| RISKY_DOWNLOAD_EXTS.contains(&e.as_str()))
}

/// A short human label for a blocked download: the save-path file name, else the URL's
/// last path segment, else the raw URL.
fn download_name(url: &str, path: &std::path::Path) -> String {
    if let Some(n) = path.file_name().and_then(|n| n.to_str()) {
        if !n.is_empty() {
            return n.to_string();
        }
    }
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments()?.next_back().map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| url.to_string())
}

/// Whether `url` points at Google Translate's page-proxy (`*.translate.goog`).
pub(crate) fn is_translate_proxy(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.ends_with(".translate.goog")))
        .unwrap_or(false)
}

/// Reverse a `*.translate.goog` proxy URL back to the original site. Google encodes
/// the host (`.`→`-`, `-`→`--`) and appends `_x_tr_*` query params; we decode the
/// host, keep the path and any genuine query params, and drop the `_x_tr_*` ones.
/// Returns `None` if `url` isn't a translate-proxy URL we can decode.
pub(crate) fn deproxy_translate(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let encoded = parsed.host_str()?.strip_suffix(".translate.goog")?;
    // '--' is a literal '-'; a lone '-' is a '.'.
    let mut decoded = String::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' {
            if chars.peek() == Some(&'-') {
                chars.next();
                decoded.push('-');
            } else {
                decoded.push('.');
            }
        } else {
            decoded.push(c);
        }
    }
    let mut out = parsed.clone();
    out.set_host(Some(&decoded)).ok()?;
    // The proxy serves over its own host on 443; the original default port applies.
    let _ = out.set_port(None);
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("_x_tr"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        out.set_query(None);
    } else {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &kept {
            ser.append_pair(k, v);
        }
        out.set_query(Some(&ser.finish()));
    }
    Some(out.to_string())
}

/// Convert an internal [`PaneRect`] to the wry `Rect` used for webview bounds.
pub(crate) fn wry_rect(r: PaneRect) -> Rect {
    Rect {
        position: PhysicalPosition::new(r.x, r.y).into(),
        size: PhysicalSize::new(r.w.max(1) as u32, r.h.max(1) as u32).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        deproxy_translate, download_name, is_risky_download, is_translate_proxy, nav_push,
        origin_of, parse_tab_flag, url_is_ad_host, NavEntry, NavKind, NAV_CAP,
    };
    use std::path::Path;

    #[test]
    fn nav_push_caps_the_stack_dropping_the_oldest() {
        let mut stack = Vec::new();
        for i in 0..NAV_CAP + 5 {
            nav_push(&mut stack, NavEntry { url: format!("https://e/{i}"), kind: NavKind::Web });
        }
        // Capped at NAV_CAP, and it's the OLDEST entries that fell off the front.
        assert_eq!(stack.len(), NAV_CAP);
        assert_eq!(stack.first().unwrap().url, "https://e/5");
        assert_eq!(stack.last().unwrap().url, format!("https://e/{}", NAV_CAP + 4));
    }

    #[test]
    fn tab_flag_only_matches_whole_token() {
        // `-t` / `--tab` as a leading token → new tab, flag stripped.
        assert_eq!(parse_tab_flag("-t youtube.com"), (true, "youtube.com"));
        assert_eq!(parse_tab_flag("--tab youtube.com"), (true, "youtube.com"));
        assert_eq!(parse_tab_flag("-t"), (true, ""));
        // No flag → opens in the current tab, target untouched.
        assert_eq!(parse_tab_flag("youtube.com"), (false, "youtube.com"));
        // A target that merely starts with `-t` is NOT the flag.
        assert_eq!(parse_tab_flag("-test query"), (false, "-test query"));
        assert_eq!(parse_tab_flag("rust -t async"), (false, "rust -t async"));
    }

    #[test]
    fn js_string_escapes_quotes_newlines_and_angle() {
        assert_eq!(super::js_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(super::js_string("a\nb"), r#""a\nb""#);
        // `<` becomes the unicode escape < (guard against `</script>`).
        assert!(super::js_string("x<y").contains("\\u003c"));
    }

    #[test]
    fn detects_and_deproxies_translate_urls() {
        let proxied = "https://monsterhunterrise-wiki-fextralife-com.translate.goog/Monster+Hunter+Rise+Wiki?_x_tr_sl=en&_x_tr_tl=pt&_x_tr_hl=pt-BR";
        assert!(is_translate_proxy(proxied));
        assert_eq!(
            deproxy_translate(proxied).as_deref(),
            Some("https://monsterhunterrise.wiki.fextralife.com/Monster+Hunter+Rise+Wiki")
        );
    }

    #[test]
    fn deproxy_keeps_real_query_and_decodes_literal_dashes() {
        // `my--site` decodes to `my-site` (a literal dash); `q=1` is a real param.
        let url = "https://my--site-com.translate.goog/p?q=1&_x_tr_sl=en";
        assert_eq!(deproxy_translate(url).as_deref(), Some("https://my-site.com/p?q=1"));
    }

    #[test]
    fn non_translate_urls_are_left_alone() {
        assert!(!is_translate_proxy("https://example.com/translate"));
        assert_eq!(deproxy_translate("https://example.com/"), None);
    }

    #[test]
    fn ad_host_redirect_guard_matches_hosts_not_paths() {
        // Host-style entries match the URL host (substring, so subdomains count).
        assert!(url_is_ad_host("https://googleads.g.doubleclick.net/pagead/x"));
        assert!(url_is_ad_host("http://ads.pubmatic.com/AdServer"));
        // The `adservice.google.` trailing-dot entry catches the real TLD'd host.
        assert!(url_is_ad_host("https://adservice.google.com/"));
        // Ordinary destinations are never blocked.
        assert!(!url_is_ad_host("https://example.com/article"));
        assert!(!url_is_ad_host("https://news.ycombinator.com/"));
        // Path-fragment entries (e.g. `/pagead/`) must NOT block a benign host that
        // merely has such a path — those are for the page-side sub-resource blocker.
        assert!(!url_is_ad_host("https://example.com/pagead/info"));
        // Garbage / non-http inputs don't panic and don't match.
        assert!(!url_is_ad_host("not a url"));
    }

    #[test]
    fn origin_of_isolates_web_origins() {
        assert_eq!(origin_of("https://animepahe.pw/play/x"), "https://animepahe.pw");
        // Port and scheme are part of the origin; path/query are not.
        assert_eq!(origin_of("http://localhost:8731/a?b=1"), "http://localhost:8731");
        // A cross-origin scam URL has a different origin than the page above.
        assert_ne!(origin_of("https://scam.example/win"), origin_of("https://animepahe.pw/"));
        // Non-web schemes and junk yield "" so they never count as "a different origin".
        assert_eq!(origin_of("about:blank"), "");
        assert_eq!(origin_of("file:///C:/x.html"), "");
        assert_eq!(origin_of("data:text/html,hi"), "");
        assert_eq!(origin_of("nonsense"), "");
    }

    #[test]
    fn download_guard_flags_executables_only() {
        // Executable / installer types are risky (from the save path)…
        assert!(is_risky_download("https://x.test/get", Path::new("setup.exe")));
        assert!(is_risky_download("https://x.test/a", Path::new("pkg.msi")));
        assert!(is_risky_download("https://x.test/a", Path::new("s.BAT"))); // case-insensitive
        // …or inferred from the URL when the save path has no extension.
        assert!(is_risky_download("https://x.test/download/installer.exe", Path::new("installer")));
        // Ordinary downloads are allowed through.
        assert!(!is_risky_download("https://x.test/a", Path::new("movie.mp4")));
        assert!(!is_risky_download("https://x.test/doc.pdf", Path::new("doc.pdf")));
        assert!(!is_risky_download("https://x.test/archive.zip", Path::new("archive.zip")));
    }

    #[test]
    fn download_name_prefers_file_name() {
        assert_eq!(download_name("https://x.test/d", Path::new("C:/Users/me/setup.exe")), "setup.exe");
        // Falls back to the URL's last segment when the path has no file name.
        assert_eq!(download_name("https://x.test/path/evil.exe", Path::new("")), "evil.exe");
    }
}
