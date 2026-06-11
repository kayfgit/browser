//! Tabs: the Tab/NativeRead content model, opening every tab kind (web,
//! research, no-js, read), the WebView2 build glue, close/reopen/switch/move,
//! webview visibility + bounds, scrolling, history, and the page-feature
//! toggles (adblock/popups/mute/css/js).

use anyhow::Result;
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows};

use crate::panes::{PaneRect, FOCUS_BORDER};
use crate::term::TermSession;
use crate::{
    read_view, session, vim, App, ModeKind, UserEvent, ADBLOCK_JS, BRIDGE_JS, BROWSER_ARGS,
    CARET_JS, CLOSED_CAP, FEATURES_JS, FIND_JS, RESEARCH_JS,
};

/// Where a content webview gets its page from.
pub(crate) enum Source {
    Url(String),
    Html(String),
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
        let url = self.resolve_target(target);
        match self.build_content_webview(Source::Url(url.clone()), false, RESEARCH_JS) {
            Ok(webview) => {
                let tab = Tab {
                    content: TabContent::Web(webview),
                    url,
                    nojs: false,
                    read: false,
                    research: true,
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
                if let Some(session) = self.tabs[i].take_term() {
                    session.shutdown();
                }
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
            "read" => self.start_read(&c.url, false),
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
        let mut builder = WebViewBuilder::new();
        builder = match source {
            Source::Url(u) => builder.with_url(u),
            Source::Html(h) => builder.with_html(h),
        };
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
            // The shell bridge always loads; the adblocker loads too but starts in
            // the shell's current state (baked as `__adblockDefault`), so `:ads` can
            // flip it live (`__setAdblock`) without a reload. `extra_init` (e.g.
            // research-mode DOM pruning) is appended last. All run in the same
            // document-create pass.
            .with_initialization_script({
                let ab = self.adblock;
                // Page-feature toggles start in the shell's current state too, so a
                // tab opened while a toggle is active is already in that state.
                let (p, m, c) = (self.block_popups, self.mute, self.no_css);
                let mut init = format!(
                    "{BRIDGE_JS}\n{FIND_JS}\n{CARET_JS}\n\
                     window.__adblockDefault={ab};\n{ADBLOCK_JS}\n\
                     window.__featureDefaults={{popups:{p},mute:{m},css:{c}}};\n{FEATURES_JS}"
                );
                if !extra_init.is_empty() {
                    init.push('\n');
                    init.push_str(extra_init);
                }
                init
            })
            .with_ipc_handler(move |req| match req.body().as_str() {
                "leave-passthrough" | "insert-escape" | "insert-blur" => {
                    let _ = ipc_proxy.send_event(UserEvent::ExitToNormal);
                }
                "to-passthrough" => {
                    let _ = ipc_proxy.send_event(UserEvent::InsertToPassthrough);
                }
                "page-ready" => {
                    let _ = ipc_proxy.send_event(UserEvent::FocusShell);
                }
                "grab-focus" => {
                    let _ = ipc_proxy.send_event(UserEvent::GrabFocus);
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
                true
            });
        if disable_js {
            builder = builder.with_javascript_disabled();
        }
        builder
            .build_as_child(&*self.window)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Kick off a background readability extraction into the Document model; the
    /// result arrives as a ReadReady/ReadFailed user event so the UI stays
    /// responsive. `replace` swaps the active read tab's doc in place (link-follow
    /// / reload) rather than opening a new tab.
    pub(crate) fn start_read(&mut self, target: &str, replace: bool) {
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
                    Ok(doc) => UserEvent::ReadReady { doc: Box::new(doc), replace },
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
                    Ok(doc) => UserEvent::ReadReady { doc: Box::new(doc), replace },
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
    /// `replace` (`:read -t`), a new read tab is opened.
    pub(crate) fn show_read_document(&mut self, doc: browser_core::Document, replace: bool) {
        let url = doc.url.clone();
        if replace {
            if let Some(i) = self.active {
                // Fast path: active is already a read tab → swap its doc, keep the tab.
                if self.tabs.get(i).is_some_and(|t| t.native().is_some()) {
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
                let tab = native_read_tab(doc, url, true);
                self.place_tab(tab, false);
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
        self.find_reset();
        self.mode = ModeKind::Normal;
        self.refresh_visibility();
        self.window.set_focus();
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

    pub(crate) fn switch_tab(&mut self, delta: i32) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as i32;
        let cur = self.active.unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(n) as usize;
        self.show_tab(next);
    }

    /// Jump directly to a zero-based tab index (bound to keys 1..9).
    pub(crate) fn jump_to(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.show_tab(index);
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
        self.refresh_visibility();
        self.window.set_focus();
    }

    /// Map an x pixel coordinate on the tab bar to a tab index, for click-to-switch.
    /// Mirrors `draw_tab_bar`'s layout (start at x=8, each label `+6` gap), including
    /// its right-edge truncation, so the clickable regions match what's drawn.
    pub(crate) fn tab_at_pixel(&self, px: f64) -> Option<usize> {
        let p = &self.painter;
        let labels = self.tab_labels();
        let limit = self.inner().0 as usize;
        let px = px.max(0.0) as usize;
        let mut x = 8usize;
        for (i, (label, active, _)) in labels.iter().enumerate() {
            let text = if *active {
                format!("[{}:{}]", i + 1, label)
            } else {
                format!(" {}:{} ", i + 1, label)
            };
            let end = x + p.measure(&text) + 6;
            if px >= x && px < end {
                return Some(i);
            }
            x = end;
            if x > limit.saturating_sub(40) {
                break;
            }
        }
        None
    }

    /// Move the active tab one position left (-1) or right (+1).
    pub(crate) fn move_tab(&mut self, delta: i32) {
        let Some(i) = self.active else { return };
        let j = i as i32 + delta;
        if j < 0 || j as usize >= self.tabs.len() {
            return;
        }
        let j = j as usize;
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

    pub(crate) fn history(&mut self, forward: bool) {
        if let Some(wv) = self.active_webview() {
            let js = if forward { "history.forward();" } else { "history.back();" };
            let _ = wv.evaluate_script(js);
        }
    }

    /// Toggle the ad/tracker blocker. Flips the live `on` flag inside every open
    /// web tab via `__setAdblock` (cosmetic hiding + DOM sweep + YouTube skip update
    /// immediately; already-loaded requests aren't undone — reload for a clean slate)
    /// and updates the default baked into newly opened tabs.
    pub(crate) fn toggle_adblock(&mut self) {
        self.adblock = !self.adblock;
        let on = self.adblock;
        for tab in &self.tabs {
            if let Some(wv) = tab.webview() {
                let _ = wv.evaluate_script(&format!("window.__setAdblock&&window.__setAdblock({on})"));
            }
        }
        self.set_status(if on { "adblock ON — ads blocked" } else { "adblock OFF — ads allowed" });
        self.window.request_redraw();
    }

    /// Flip a live page-feature toggle ([`FEATURES_JS`]) on every open web tab via
    /// `__setToggle` (no reload). `name` is `popups` | `mute` | `css`.
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
                self.tabs[i] = Tab {
                    content: TabContent::Web(webview),
                    url,
                    nojs,
                    read: false,
                    research,
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
            if !url.starts_with("browser://") {
                self.start_read(&url, true);
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
    use super::{deproxy_translate, is_translate_proxy, parse_tab_flag};

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
}
