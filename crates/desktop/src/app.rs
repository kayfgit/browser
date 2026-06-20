//! The application shell state: [`App`] itself, the cross-thread [`UserEvent`]s,
//! the modal [`ModeKind`], window geometry/zoom/fullscreen, focus reclaim,
//! status/error reporting, wheel routing, teardown, and session save/restore.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tao::event_loop::EventLoopProxy;
use tao::keyboard::ModifiersState;
use tao::window::Window;
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::{Rect, WebView};

use crate::draw::Painter;
use crate::find::FindState;
use crate::hints::NativeHint;
use crate::pages::{now_hms, ErrorEntry, ERROR_LOG_CAP};
use crate::panes::PaneNode;
use crate::tabs::{NativeRead, Tab};
use crate::{read_view, session, BAR_H, BASE_PX, HISTORY_CAP, TAB_BAR_H, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP};

/// Events posted from webview IPC back into the event loop.
pub(crate) enum UserEvent {
    /// Leave passthrough: move focus from the page back to the shell.
    ExitToNormal,
    /// Reclaim keyboard focus for the shell (e.g. after a page finishes loading
    /// and WebView2 has grabbed focus), unless the page should keep focus.
    FocusShell,
    /// The page grabbed keyboard focus (a click or a script `.focus()`) while in
    /// Normal mode — bounce it back so shell keys keep working (SPA focus trap).
    GrabFocus,
    /// A hint was activated (or the page asked to end hint mode).
    ExitHint,
    /// A hint selected an editable element: focus it and enter passthrough.
    HintEdit,
    /// A hint was activated in new-tab mode (`F`): open this URL in a new tab.
    HintOpen(String),
    /// A web pane was clicked (pointerdown): focus the pane under the cursor.
    PaneClick,
    /// A `:read` extraction finished: render this Document in an engine-free read
    /// tab. `replace` swaps the active read tab's doc in place (link-follow/reload)
    /// instead of opening a new tab. `record` adds a back-stack step for the page
    /// being left (false for reloads and `H`/`L` history replays).
    ReadReady { doc: Box<browser_core::Document>, replace: bool, record: bool },
    /// A `:read` extraction failed.
    ReadFailed(String),
    /// Redirect the active tab to this URL (e.g. de-proxying a `translate.goog`
    /// navigation back to the original site).
    Navigate(String),
    /// The blocker stopped a forced/auto redirect (a scripted cross-origin jump
    /// with no user gesture, an ad-host navigation, or a `<meta refresh>` jump) →
    /// note it in the status bar. Carries the target URL that was suppressed.
    RedirectBlocked(String),
    /// The blocker denied a scripted new window / pop-up (a popunder to a scam tab,
    /// `window.open`, or `target=_blank` while adblock is on) → note it in the status
    /// bar. Carries the target URL (or `about:blank` for a shell popunder).
    PopupBlocked(String),
    /// The download guard blocked an executable/installer download → warn in the status
    /// bar. Carries the file name. Toggle `:downloads` to permit such files.
    DownloadBlocked(String),
    /// The network blocklist engine finished compiling and is now live.
    BlocklistReady,
    /// A `:te` command finished: combined output and exit code.
    TermDone { cmd: String, output: String, code: Option<i32> },
    /// Raw output bytes from a terminal's PTY → feed to its native VT engine.
    TermOutput { id: u64, data: Vec<u8> },
    /// The terminal's shell exited (pty-host stdout EOF) → close that tab.
    TermClosed { id: u64 },
    /// Web caret mode yanked a selection → put this text on the clipboard.
    CaretYank(String),
    /// Web caret mode exited (Esc with no selection) → return the shell to Normal.
    CaretExit,
    /// The page entered (`true`) or left (`false`) HTML fullscreen (e.g. YouTube's
    /// fullscreen button) → match the window's fullscreen so the page fills the screen.
    PageFullscreen(bool),
    /// One Groq round finished for the `:ai` tab with this id: a final answer or
    /// tool-calls to run ([`crate::ai::AiStep`]) — or an error. `convo`/`round` carry
    /// the running conversation so [`ai_reply`](App::ai_reply) can continue the
    /// agentic loop (run actions → feed results back → ask again).
    AiReply {
        id: u64,
        convo: Vec<serde_json::Value>,
        round: u32,
        result: Result<crate::ai::AiStep, String>,
    },
    /// The keyboard hook saw Esc while the page held focus in Normal mode (a click
    /// yielded the keyboard to a page control): pull keyboard focus back to the shell.
    ReclaimNormal,
    /// A Normal-mode click hit a page control (button/menu/link): let the page keep
    /// keyboard focus so its popover stays open (don't bounce focus back).
    PageHold,
    /// A Normal-mode click hit a text field: enter passthrough so keys type into the page.
    PageEdit,
    /// A WebView2 browsing-data clear finished (`:clear cookies`/`cache`/`all`).
    /// `label` is the human description of what was cleared (empty = a silent bonus
    /// clear, ignored). `ai_id` is the `:ai` tab that initiated it, if any: the
    /// confirmation goes into that chat, and the status bar is used only when that tab
    /// isn't the one on screen.
    DataCleared { label: String, ai_id: Option<u64> },
    /// The `Ctrl+Alt+Shift+R` keyboard-hook chord (the brick-proof panic button):
    /// reset all customization to defaults, regardless of mode or how keys are bound.
    RestoreDefaults,
    Quit,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ModeKind {
    Normal,
    Command,
    /// The single "type into the content" mode, whatever the content is: a web page
    /// (keys go to the page), a native terminal (keys go to the PTY), or the `:ai`
    /// field. Entered with `i` (or `Ctrl+V`, a hint on an editable element, or a click
    /// in a text field). Leaving: for a web page it's anti-trap — Esc, or clicking out
    /// of a field, returns to Normal (Ctrl+S also works anytime); a terminal keeps Esc
    /// for the shell and leaves on Ctrl+S; the AI field leaves on Esc.
    Passthrough,
    /// hjkl resize the window; Esc exits. Entered with `:resize`.
    Resize,
    /// hjkl move the window across the desktop; Esc exits. Entered with `:move`.
    Move,
    /// Link hints are shown; typed characters select one. Entered with `f`.
    Hint,
    /// Find-in-page: `/` opened a search prompt. Typing searches live; Enter keeps
    /// the highlights and returns to Normal (where `n`/`N` step through matches).
    Find,
    /// Caret/visual browsing on a WEB tab (`v`/`V`): the shell forwards vim motions
    /// to an injected page caret that moves/extends a real DOM Selection; `y` yanks,
    /// `Esc` collapses then exits. (Engine-free read tabs use `NativeRead.caret`.)
    Caret,
}

pub(crate) struct App {
    pub(crate) window: Rc<Window>,
    // Kept alive for the lifetime of `surface`, which is created from it.
    pub(crate) _context: softbuffer::Context<Rc<Window>>,
    pub(crate) surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    pub(crate) painter: Painter,
    pub(crate) proxy: EventLoopProxy<UserEvent>,

    pub(crate) mode: ModeKind,
    pub(crate) command: String,
    /// Caret position within `command`, as a byte offset on a char boundary.
    pub(crate) command_cursor: usize,
    /// Selection anchor (byte offset). When `Some` and != cursor, the text between
    /// it and the caret is selected (Shift-movement extends it; typing replaces it).
    pub(crate) command_anchor: Option<usize>,
    /// Accumulated label characters while in Hint mode.
    pub(crate) hint_input: String,
    /// Whether the current hint will open its target in a NEW tab (entered with
    /// `F`, or set mid-typing when a label char is typed uppercase). Badges render
    /// uppercase as the visual cue. Links only; non-link targets click normally.
    pub(crate) hint_new_tab: bool,
    /// Placed hint labels for an engine-free read tab (web tabs hint via JS).
    pub(crate) native_hints: Vec<NativeHint>,
    pub(crate) status: String,
    /// Whether the current `status` is an error (rendered red instead of dim).
    pub(crate) status_is_error: bool,
    /// When set, the current status is a transient flash (e.g. a background `:ai`
    /// result) that auto-clears once this deadline passes; any normal status update
    /// cancels it. See [`flash_status`](Self::flash_status).
    pub(crate) status_clear_at: Option<Instant>,
    /// The tab that was active just before the `:ai` tab was summoned, so closing the
    /// AI tab returns there (rather than to a stray blank pane). `None` = the welcome
    /// screen. See [`hide_ai_tab`](Self::hide_ai_tab).
    pub(crate) ai_prev_active: Option<usize>,
    /// Session error log: every failure (message + the command that triggered it +
    /// a wall-clock timestamp), newest last. Inspected with `:error` (latest) and
    /// `:errors` (all), capped to avoid unbounded growth.
    pub(crate) errors: Vec<ErrorEntry>,
    /// The command line currently executing (`:open foo`), so a failure it raises
    /// can be attributed to it in the error log. `None` outside `run_command`.
    pub(crate) current_command: Option<String>,
    /// Find-in-page state (the `/` search and its matches).
    pub(crate) find: FindState,
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: Option<usize>,
    /// Current keyboard modifier state (tracked via ModifiersChanged).
    pub(crate) modifiers: ModifiersState,
    /// When true, new tabs are opened with JavaScript disabled.
    pub(crate) nojs: bool,
    /// When true, the uBlock-style content blocker ([`ADBLOCK_JS`]) is injected
    /// into web tabs. Toggled with `:ads`; persisted across sessions.
    pub(crate) adblock: bool,
    /// A live mirror of [`adblock`](Self::adblock) shared (cloned `Arc`) into every
    /// web tab's navigation handler, so the native top-level redirect guard
    /// (ad-host navigations) honours `:ads` without rebuilding the webviews. Kept in
    /// lock-step via [`set_adblock`](Self::set_adblock).
    pub(crate) adblock_on: Arc<AtomicBool>,
    /// Whether to allow downloads of executable/installer file types. Off by default:
    /// a drive-by `.exe`/`.msi` (the "you almost clicked install" trap) is blocked with
    /// a warning. Toggled with `:downloads`. Shared into every tab's download handler.
    pub(crate) allow_risky_downloads: Arc<AtomicBool>,
    /// Cross-site navigation intent for the native redirect guard
    /// ([`navguard`](crate::navguard)): stamped when something legitimate wants to leave
    /// the current site — the page reports a TRUSTED gesture on a real cross-site
    /// link/submit, or the shell drives `H`/`L` history, the translate de-proxy, or a
    /// hint follow. The guard cancels any cross-site top navigation that carries no fresh
    /// stamp (a forced redirect). Shared (cloned `Arc`) into every web tab.
    pub(crate) nav_intent: crate::navguard::NavIntent,
    /// The uBlock-style network blocklist engine ([`blocklist`](crate::blocklist)),
    /// shared (cloned `Arc`) into every tab's navigation handler. It blocks navigations
    /// to known ad/redirect/malware domains BY NAME — the race-free primary guard, the
    /// way Brave/uBlock do it. `None` until it finishes compiling just after launch.
    pub(crate) blocker: crate::blocklist::SharedBlocker,
    /// Live page-feature toggles ([`FEATURES_JS`]), applied to every web tab without
    /// a reload. `mute` keeps all media muted; `no_css` disables every stylesheet.
    /// Session-only (reset to off each launch). (Pop-up blocking moved under `:ads`.)
    pub(crate) mute: bool,
    pub(crate) no_css: bool,
    /// Shell command for `:te` (program + args), set via `:config`.
    pub(crate) term_command: Vec<String>,
    /// Search-engine URL template (`%s` = query) for a non-URL `:open`. Defaults
    /// to Google; change it with `:search <template>`.
    pub(crate) search_template: String,
    /// Monotonic id for routing PTY output to the right terminal tab.
    pub(crate) next_term_id: u64,
    /// Groq API key for the `:ai` tab, loaded once at startup and persisted after
    /// the user pastes it. `None` until entered (the AI tab shows a key field).
    pub(crate) groq_key: Option<String>,
    /// Monotonic id for routing async Groq replies to the right `:ai` tab.
    pub(crate) next_ai_id: u64,
    /// Model id used by the `:ai` tab (Groq), set with `:model` and persisted.
    pub(crate) ai_model: String,
    /// Past `:ai` conversations (newest last), persisted to disk. `H`/`L` on an AI
    /// tab step through these; new AI tabs start as a fresh draft.
    pub(crate) ai_chats: Vec<Vec<crate::ai::AiMessage>>,
    /// Global UI zoom factor (1.0 = 100%). Scales native chrome, web content,
    /// and terminal font together.
    pub(crate) zoom: f64,
    /// Blink state for the command-bar cursor (toggled on a timer in Command mode).
    pub(crate) cursor_on: bool,
    pub(crate) quit: bool,
    /// Whether `teardown` has already run. It fires from multiple places (window
    /// close, `:q`, then `LoopDestroyed`); without this guard the second call would
    /// re-save the session with the now-cleared tab list, wiping the good snapshot.
    pub(crate) torn_down: bool,
    /// `:res` resource monitor: the previous per-pid (cpu_100ns, io_bytes) sample
    /// for computing CPU%/disk rate, and when that sample was taken. The monitor
    /// always auto-refreshes; `refresh_res` just freezes while text is selected.
    pub(crate) res_prev: std::collections::HashMap<u32, (u64, u64)>,
    pub(crate) res_at: Instant,
    /// Last known cursor position (physical px), tracked for tab-bar drag.
    pub(crate) cursor_pos: (f64, f64),
    /// True while the pointer hovers the command/status bar. In Normal mode the bar
    /// shows the URL shortened to its host; hovering reveals the full address.
    pub(crate) bar_hover: bool,
    /// True while a left-drag is selecting text in the command/find line (mouse
    /// highlight). Set on a press inside the bar, cleared on release.
    pub(crate) bar_dragging: bool,
    /// Horizontal scroll (px) applied to the command line on the last frame, so a
    /// mouse click in the bar can map a pixel x back to a caret byte offset.
    pub(crate) bar_cmd_scroll: i32,
    /// Normal mode, but a click on a page control (button/menu/link) left keyboard
    /// focus in the page so its popover stays open instead of being blurred shut.
    /// While set, the shell stops reclaiming focus and the keyboard hook watches for
    /// Esc to snap control back. Cleared the moment the shell next holds the keyboard.
    pub(crate) page_focus_yielded: bool,
    /// The `:ai` tab id currently running a tool-call, so an async action's
    /// completion (e.g. a WebView2 data wipe) can route its confirmation back to that
    /// chat. Set only for the duration of [`ai_reply`](App::ai_reply)'s dispatch.
    pub(crate) acting_ai: Option<u64>,
    /// Persisted customization (aliases, …) the user/AI tune at runtime. Reset to
    /// defaults by [`restore_defaults`](App::restore_defaults). See [`config`](crate::config).
    pub(crate) config: crate::config::Config,
    /// The live chrome theme, resolved from `config.theme` by
    /// [`rebuild_theme`](Self::rebuild_theme). Read at paint time for the bar
    /// colours/height and accent; reset by [`restore_defaults`](Self::restore_defaults).
    pub(crate) theme: crate::draw::Theme,
    /// When the window last gained focus — used to swallow the stray `Tab` that
    /// Alt+Tab delivers to a focused terminal.
    pub(crate) last_focus_gain: Instant,
    /// Visited URLs, most-recent first (deduped, capped). Drives command-bar
    /// autocomplete for `:open <partial>` and is persisted in the session.
    pub(crate) history: Vec<String>,
    /// Visit time (Unix-epoch seconds) parallel to [`history`](Self::history), same
    /// order/length — kept in lock-step by [`record_history`](Self::record_history)
    /// so `:clear history <period>` can drop only the entries inside a time window.
    /// Restored-from-session entries are stamped `0` (their real time is unknown, so
    /// only an all-time clear removes them).
    pub(crate) history_at: Vec<u64>,
    /// Recently-closed restorable tabs (kind + url), newest last. `U` /
    /// Ctrl+Shift+T pops the most recent and reopens it. Internal pages (the error/
    /// res/version vim tabs, `browser://…`) are never recorded.
    pub(crate) closed_tabs: Vec<session::SavedTab>,
    /// True when the window's fullscreen was triggered by the page entering HTML
    /// fullscreen (YouTube's button), so leaving page fullscreen exits it again —
    /// without disturbing a fullscreen the user set manually with `:f`.
    pub(crate) fs_from_page: bool,
    /// tmux-style pane layout tiling the content area. `None` = a single pane (the
    /// active tab fills the content band, exactly as without splits). `Some(tree)`
    /// once the user `:split`s: leaves reference distinct tab indices, the focused
    /// leaf is the active tab, and Ctrl+W h/j/k/l moves focus between panes.
    pub(crate) split: Option<PaneNode>,
    /// True after Ctrl+W in Normal mode: the next h/j/k/l moves pane focus and
    /// s/v splits (vim-window style). Cleared by the following key.
    pub(crate) pending_window_key: bool,
    /// Cached by `refresh_visibility()`: the focused pane shows a webview, which can
    /// trap keyboard focus on click — arms the fast (300 ms) focus backstop.
    pub(crate) active_pane_is_webview: bool,
    /// Cached by `refresh_visibility()`: a webview is visible in some *non-focused*
    /// pane. It can still steal focus on a stray click, but that's rare — a slow
    /// (1 s) backstop tier is enough, so a focused native pane stays near-idle.
    pub(crate) background_webview_visible: bool,
    /// Scrollback lines kept per terminal (memory scales with it; see
    /// [`pty_term::DEFAULT_SCROLLBACK`]).
    pub(crate) term_scrollback: usize,
    /// Set while an `H`/`L` history replay is reopening a page in place, so the
    /// synchronous navigation paths ([`place_tab`](Self::place_tab)) don't re-record
    /// the page being left (the stacks were already adjusted by [`history`]). The
    /// asynchronous read path is gated separately by `ReadReady.record`.
    pub(crate) nav_replaying: bool,
}

/// Tag this process with an explicit AppUserModelID so Windows (taskbar + Task
/// Manager) treats it and every process it spawns as one application. The id is

/// Put `text` on the system clipboard (best-effort; failures are ignored).
pub(crate) fn clipboard_set(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
}

/// Read UTF-8 text from the system clipboard, or `None` if unavailable/non-text.
pub(crate) fn clipboard_get() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Wall-clock now as Unix-epoch seconds (UTC) — the stamp for visited-history
/// entries and the reference point for `:clear … <period>` time windows.
pub(crate) fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a "quick maths" result: drop the decimal point for whole numbers,
/// otherwise show up to 6 decimal places with trailing zeros trimmed.
pub(crate) fn format_number(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

impl App {

    pub(crate) fn inner(&self) -> (u32, u32) {
        let s = self.window.inner_size();
        (s.width.max(1), s.height.max(1))
    }

    /// Scale a base (zoom-1.0) pixel metric by the current zoom factor.
    pub(crate) fn scaled(&self, base: u32) -> u32 {
        (base as f64 * self.zoom).round().max(1.0) as u32
    }

    /// Whether the window is in borderless fullscreen (`:f` / `:fullscreen`).
    pub(crate) fn is_fullscreen(&self) -> bool {
        self.window.fullscreen().is_some()
    }

    /// Whether the native chrome (tab bar + command/status bar) is hidden right now:
    /// true in fullscreen for an immersive page, EXCEPT while typing a command, so
    /// pressing `:` (or `/`) brings the bars back to interact, then they hide again.
    pub(crate) fn chrome_hidden(&self) -> bool {
        self.is_fullscreen() && !matches!(self.mode, ModeKind::Command | ModeKind::Find)
    }

    /// Command/status bar height at the current zoom (0 when chrome is hidden).
    pub(crate) fn bar_h(&self) -> u32 {
        if self.chrome_hidden() {
            0
        } else {
            // Apply the theme's bar-height multiplier on top of the zoom scaling, with a
            // small floor so a tiny percent can't make the bar unusable.
            let base = self.scaled(BAR_H) as f32;
            ((base * self.theme.bar_scale).round() as u32).max(12)
        }
    }

    /// Re-resolve the live [`Theme`](crate::draw::Theme) from `config.theme` — called at
    /// startup and after any appearance change or `:restore`. A garbled colour string is
    /// ignored per-field (that slot keeps its default), so a bad value never blanks the UI.
    pub(crate) fn rebuild_theme(&mut self) {
        let mut t = crate::draw::Theme::default();
        let tc = &self.config.theme;
        if let Some(pct) = tc.bar_height_pct {
            t.bar_scale = (pct as f32 / 100.0).clamp(0.5, 3.0);
        }
        if let Some(c) = tc.bar_bg.as_deref().and_then(crate::draw::parse_color) {
            t.bar_bg = c;
        }
        if let Some(c) = tc.bar_fg.as_deref().and_then(crate::draw::parse_color) {
            t.bar_fg = c;
        }
        if let Some(c) = tc.accent.as_deref().and_then(crate::draw::parse_color) {
            t.accent = c;
        }
        if let Some(c) = tc.bg.as_deref().and_then(crate::draw::parse_color) {
            t.bg = c;
        }
        self.theme = t;
    }

    /// Tab-bar height: present only while at least one *visible* tab is open and chrome
    /// is shown. The background `:ai` singleton doesn't appear in the strip, so a lone
    /// hidden AI tab leaves the bar at zero height (welcome screen stays clean).
    pub(crate) fn tab_bar_h(&self) -> u32 {
        let no_visible_tabs = !self.tabs.iter().any(|t| t.ai().is_none());
        if no_visible_tabs || self.chrome_hidden() {
            0
        } else {
            self.scaled(TAB_BAR_H)
        }
    }

    /// Bounds for a content webview: full width, between the tab bar and command bar.
    pub(crate) fn content_rect(&self) -> Rect {
        let (w, h) = self.inner();
        let top = self.tab_bar_h();
        Rect {
            position: PhysicalPosition::new(0_i32, top as i32).into(),
            size: PhysicalSize::new(w, h.saturating_sub(top + self.bar_h())).into(),
        }
    }

    pub(crate) fn on_resize(&mut self, _w: u32, _h: u32) {
        self.refresh_visibility();
        self.window.request_redraw();
    }

    /// Re-fit every visible web pane to the current chrome layout and repaint. Called
    /// on transitions that change which bars are visible without resizing the window —
    /// entering/leaving the command bar while fullscreen — so pages grow to fill the
    /// freed space (or shrink to make room for the bar).
    pub(crate) fn relayout_active(&mut self) {
        self.refresh_visibility();
    }

    /// Top/bottom y of the FOCUSED pane (the whole content band when not split) —
    /// drives the active tab's scroll clamp, hint placement, and terminal rows.
    pub(crate) fn content_y_bounds(&self) -> (i32, i32) {
        let r = self.focused_pane_rect();
        (r.y, r.y + r.h)
    }

    /// Visible height of the content band, in px (>= 1).
    pub(crate) fn content_view_h(&self) -> i32 {
        let (top, bottom) = self.content_y_bounds();
        (bottom - top).max(1)
    }

    /// (Re)build the active read tab's native layout when the width, zoom, or the
    /// document itself changed; then clamp the scroll to the new content height.
    /// Cheap no-op when the cache is still valid (called every frame).
    pub(crate) fn refresh_read_layout(&mut self) {
        let px = self.painter.px();
        // Lay out every visible read pane to ITS pane width (so a split read tab
        // wraps to its column); with no split this is just the active read tab.
        let (panes, _) = self.pane_layout();
        for (tab, rect) in panes {
            let is_read = matches!(self.tabs.get(tab), Some(t) if t.native().is_some());
            if !is_read {
                continue;
            }
            let cw = rect.w;
            let view = rect.h;
            // Split borrow: `painter` and this tab's `native` are disjoint fields.
            let painter = &self.painter;
            let nr = self.tabs[tab].native_mut().unwrap();
            if !nr.dirty && nr.layout_w == cw && (nr.layout_px - px).abs() < f32::EPSILON {
                continue;
            }
            // Leave an 8px margin on each side (matches the draw offset).
            nr.layout = read_view::layout(&nr.doc, cw - 16, painter);
            nr.layout_w = cw;
            nr.layout_px = px;
            nr.dirty = false;
            // Re-wrapping changed the visual lines: refresh the caret's grid in place,
            // keeping its cursor/selection (clamped) so caret mode survives resize/zoom.
            if let Some(caret) = nr.caret.as_mut() {
                caret.set_lines(nr.layout.text_lines().to_vec());
            }
            let max = (nr.layout.height - view).max(0);
            nr.scroll = nr.scroll.clamp(0, max);
        }
    }

    // --- zoom -----------------------------------------------------------------

    pub(crate) fn zoom_by(&mut self, steps: i32) {
        self.set_zoom(self.zoom + steps as f64 * ZOOM_STEP);
    }

    pub(crate) fn zoom_reset(&mut self) {
        self.set_zoom(1.0);
    }

    /// Set the global zoom and apply it to every layer: the native chrome font
    /// (painter) and each web tab (WebView2 zoom). Native terminals scale with the
    /// painter too — their grid is re-sized to the new cell count on the next draw
    /// (`sync_active_term_size`). Bar/tab-bar heights scale, so the active webview is
    /// re-laid-out to fit between them.
    pub(crate) fn set_zoom(&mut self, factor: f64) {
        let z = ((factor.clamp(ZOOM_MIN, ZOOM_MAX)) * 100.0).round() / 100.0;
        self.zoom = z;
        self.painter.set_px(BASE_PX * z as f32);
        for tab in &self.tabs {
            if let Some(wv) = tab.webview() {
                let _ = wv.zoom(z);
            }
        }
        // Tab/command bars changed height → refit the visible page.
        let rect = self.content_rect();
        if let Some(wv) = self.active_webview() {
            let _ = wv.set_bounds(rect);
        }
        self.set_status(format!("zoom {}%", (z * 100.0).round() as i32));
        self.window.request_redraw();
    }

    /// Pull keyboard focus back to the shell window. After a click, the top-level
    /// window is still foreground (only the *child* webview HWND grabbed keyboard
    /// focus), and tao's `set_focus` is a no-op when already foreground — so we
    /// must `SetFocus` the parent HWND directly to take the keyboard off the child.
    #[cfg(windows)]
    pub(crate) fn reclaim_shell_focus(&self) {
        use tao::platform::windows::WindowExtWindows;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        let hwnd = self.window.hwnd();
        unsafe {
            let _ = SetFocus(Some(HWND(hwnd as *mut core::ffi::c_void)));
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn reclaim_shell_focus(&self) {
        self.window.set_focus();
    }

    /// Backstop for the click-focus trap (TODO #1). In Normal mode the shell must
    /// own the keyboard, but a click can hand keyboard focus to the webview child
    /// and lock the user out of shell keys (`:` / `Esc` / `hjkl`) until they alt-tab.
    /// The injected `BRIDGE_JS` bounces most clicks back immediately, but it misses
    /// cases — clicks inside cross-origin iframes (no `window.ipc` there) and
    /// re-steals within its throttle window. This runs on a low-frequency timer
    /// (only while a web tab is active in Normal mode) and pulls focus back whenever
    /// we're the foreground app yet the shell window doesn't hold keyboard focus.
    /// No-op when we already have focus, so it's idle the rest of the time.
    #[cfg(windows)]
    pub(crate) fn reclaim_focus_tick(&self) {
        use tao::platform::windows::WindowExtWindows;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        // A click on a page control deliberately left focus in the page so its menu
        // stays open; don't fight that. The keyboard hook restores the shell on Esc.
        if self.page_focus_yielded {
            return;
        }
        let hwnd = HWND(self.window.hwnd() as *mut core::ffi::c_void);
        unsafe {
            // Don't fight for focus while the user has switched to another app.
            if GetForegroundWindow() != hwnd {
                return;
            }
            // Already own keyboard focus → nothing to reclaim (the common case).
            if GetFocus() == hwnd {
                return;
            }
            let _ = SetFocus(Some(hwnd));
        }
    }

    #[cfg(not(windows))]
    pub(crate) fn reclaim_focus_tick(&self) {}

    /// The mode code the keyboard hook should run under (see [`crate::khook`]). Only
    /// WEB passthrough (the page holds OS focus, so the hook catches the anti-trap leave
    /// chords even inside cross-origin iframes) and a click-yielded Normal need
    /// interception; terminal/AI passthrough keep shell focus, and everything else maps
    /// to `OTHER` (inert).
    pub(crate) fn hook_mode_code(&self) -> u8 {
        use crate::khook::*;
        match self.mode {
            ModeKind::Passthrough if self.active_webview().is_some() => MODE_PASSTHROUGH,
            ModeKind::Normal if self.page_focus_yielded && self.active_webview().is_some() => {
                MODE_NORMAL_YIELDED
            }
            _ => MODE_OTHER,
        }
    }

    /// Esc was pressed while a page control held the keyboard (yielded Normal): pull
    /// focus back to the shell (which also blurs the page, closing its menu) and clear
    /// the yield so every Normal-mode key works again.
    pub(crate) fn reclaim_from_page(&mut self) {
        self.page_focus_yielded = false;
        self.set_page_mode("normal");
        self.reclaim_shell_focus();
        self.window.request_redraw();
    }

    /// The OS cursor position in window client (physical) pixels — needed for
    /// pane-click routing, since CursorMoved isn't delivered while the pointer is
    /// over a child webview, leaving `cursor_pos` stale there.
    #[cfg(windows)]
    pub(crate) fn cursor_client_pos(&self) -> Option<(i32, i32)> {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut pt = POINT::default();
        unsafe { GetCursorPos(&mut pt).ok()? };
        // `inner_position` is the client area's top-left in screen (physical) pixels,
        // so subtracting it turns the screen cursor into client coordinates — matching
        // the pane rects — without needing the Win32 GDI feature for ScreenToClient.
        let origin = self.window.inner_position().ok()?;
        Some((pt.x - origin.x, pt.y - origin.y))
    }

    #[cfg(not(windows))]
    pub(crate) fn cursor_client_pos(&self) -> Option<(i32, i32)> {
        Some((self.cursor_pos.0 as i32, self.cursor_pos.1 as i32))
    }

    /// Re-assert the current zoom on the active web tab (e.g. after a navigation,
    /// which can reset the WebView2 zoom factor). No-op for terminal tabs.
    pub(crate) fn apply_active_zoom(&self) {
        if let Some(tab) = self.active.and_then(|i| self.tabs.get(i)) {
            if tab.term().is_none() {
                if let Some(wv) = tab.webview() {
                    let _ = wv.zoom(self.zoom);
                }
            }
        }
    }

    // --- tab access -----------------------------------------------------------

    pub(crate) fn active_webview(&self) -> Option<&WebView> {
        self.active.and_then(|i| self.tabs.get(i)).and_then(|t| t.webview())
    }

    /// The first open web tab's engine handle, if any. All web tabs share one
    /// WebView2 profile, so this is enough to reach the profile for data clears
    /// (`:clear cookies`/`cache`/`all`) regardless of which tab is active.
    pub(crate) fn any_webview(&self) -> Option<&WebView> {
        self.tabs.iter().find_map(|t| t.webview())
    }

    /// Mutable access to the active engine-free read tab's state, if any.
    pub(crate) fn active_native_mut(&mut self) -> Option<&mut NativeRead> {
        self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.native_mut())
    }

    /// Whether the active tab is an engine-free `:error`/`:errors` vim tab.
    pub(crate) fn active_is_vim(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.vim().is_some())
    }

    pub(crate) fn active_url(&self) -> Option<&str> {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.url.as_str())
    }

    /// The live URL of the active web tab (from WebView2, so it reflects in-page
    /// navigation), falling back to the stored URL. `None` for terminal tabs.
    pub(crate) fn current_url(&self) -> Option<String> {
        let tab = self.tabs.get(self.active?)?;
        if tab.term().is_some() {
            return None;
        }
        // Engine-free read tab: the stored url is the canonical document URL.
        let Some(wv) = tab.webview() else {
            return Some(tab.url.clone());
        };
        if let Ok(u) = wv.url() {
            if u.starts_with("http") {
                return Some(u);
            }
        }
        Some(tab.url.clone())
    }

    /// Refresh the stored URL of the active web tab from its live WebView2 URL,
    /// so the status bar tracks navigation. Skips terminal/internal/html tabs
    /// (their live URL is a data:/about: URL, not a real address).
    pub(crate) fn refresh_active_url(&mut self) {
        let mut visited = None;
        if let Some(tab) = self.active.and_then(|i| self.tabs.get_mut(i)) {
            if tab.term().is_some() {
                return;
            }
            let Some(wv) = tab.webview() else { return };
            if let Ok(u) = wv.url() {
                if u.starts_with("http") {
                    // A changed live URL is a real in-webview navigation (clicked
                    // link, form submit, redirect): push the page being left onto
                    // the back stack so `H` returns to it — unless this is the open's
                    // own landing (`settling`), which would record a phantom entry.
                    if u != tab.url {
                        if tab.nav.settling {
                            tab.nav.settling = false;
                        } else if let Some(entry) = crate::tabs::nav_entry(tab) {
                            crate::tabs::nav_push(&mut tab.nav.back, entry);
                            tab.nav.fwd.clear();
                        }
                    }
                    tab.url = u.clone();
                    visited = Some(u);
                }
            }
        }
        if let Some(u) = visited {
            self.record_history(&u);
        }
    }

    /// Record a visited URL for autocomplete: move it to the front (most recent),
    /// de-duplicated, and cap the list — keeping [`history_at`](Self::history_at) in
    /// lock-step (same index, same length). Skips internal `browser://` pages.
    pub(crate) fn record_history(&mut self, url: &str) {
        if url.is_empty() || url.starts_with("browser://") {
            return;
        }
        // Drop any existing copy from BOTH vecs at the same index so they stay aligned.
        if let Some(pos) = self.history.iter().position(|u| u == url) {
            self.history.remove(pos);
            if pos < self.history_at.len() {
                self.history_at.remove(pos);
            }
        }
        self.history.insert(0, url.to_string());
        self.history_at.insert(0, now_epoch());
        self.history.truncate(HISTORY_CAP);
        self.history_at.truncate(HISTORY_CAP);
    }

    pub(crate) fn resize_window(&self, dw: i32, dh: i32) {
        let s = self.window.inner_size();
        let w = (s.width as i32 + dw).max(240) as u32;
        let h = (s.height as i32 + dh).max(160) as u32;
        self.window.set_inner_size(tao::dpi::PhysicalSize::new(w, h));
    }

    pub(crate) fn move_window(&self, dx: i32, dy: i32) {
        if let Ok(p) = self.window.outer_position() {
            self.window
                .set_outer_position(tao::dpi::PhysicalPosition::new(p.x + dx, p.y + dy));
        }
    }

    pub(crate) fn toggle_fullscreen(&mut self) {
        use tao::window::Fullscreen;
        if self.window.fullscreen().is_some() {
            self.window.set_fullscreen(None);
        } else {
            self.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        // A manual toggle owns the fullscreen state — clear the page-initiated flag
        // so a later page fs-exit doesn't fight it.
        self.fs_from_page = false;
        // Entering fullscreen hides the bars (Normal mode); leaving restores them.
        // The window resize usually relayouts, but do it explicitly so the page
        // refits even if the inner size didn't change.
        self.relayout_active();
    }

    /// Sync the window's fullscreen to the page's HTML fullscreen (YouTube's button).
    /// Entering fullscreens the window (so the page fills the screen and the bars
    /// hide); leaving exits — but only if WE entered it for the page, so a manual
    /// `:f` fullscreen isn't undone when an unrelated element leaves fullscreen.
    pub(crate) fn set_page_fullscreen(&mut self, on: bool) {
        use tao::window::Fullscreen;
        if on {
            if !self.is_fullscreen() {
                self.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                self.fs_from_page = true;
                self.relayout_active();
            }
        } else if self.fs_from_page {
            self.window.set_fullscreen(None);
            self.fs_from_page = false;
            self.relayout_active();
        }
    }

    // --- commands -------------------------------------------------------------

    /// Set the adblock flag, keeping the shared atomic (read by every tab's
    /// navigation handler) in lock-step with the canonical bool.
    pub(crate) fn set_adblock(&mut self, on: bool) {
        self.adblock = on;
        self.adblock_on.store(on, Ordering::Relaxed);
    }

    /// Set an informational status message (rendered dim). Clears the error flag and
    /// cancels any pending transient-flash auto-clear.
    pub(crate) fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = false;
        self.status_clear_at = None;
    }

    /// Show a transient status that auto-clears after ~2s — a fire-and-forget
    /// confirmation, e.g. a background `:ai` result you'll glance at. Any later status
    /// update replaces it and cancels the timer.
    pub(crate) fn flash_status(&mut self, msg: impl Into<String>) {
        self.set_status(msg);
        self.status_clear_at = Some(Instant::now() + Duration::from_secs(2));
    }

    /// Show a persistent warning (red), without logging it to `:errors` — used for a
    /// background `:ai` failure whose detail already lives in the chat.
    pub(crate) fn warn_status(&mut self, msg: impl Into<String>) {
        self.set_status(msg);
        self.status_is_error = true;
    }

    /// Clear a transient status flash once its deadline has passed. Called on the
    /// event-loop timer wake; a no-op until then or when no flash is pending.
    pub(crate) fn expire_status_flash(&mut self) {
        if self.status_clear_at.is_some_and(|t| Instant::now() >= t) {
            self.clear_status();
            self.window.request_redraw();
        }
    }

    /// "Quick maths": if the command-bar line is an arithmetic expression, return
    /// its formatted result. Gated on the presence of a maths operator so plain
    /// inputs (a lone number, a URL, a command) don't show a spurious result.
    pub(crate) fn math_preview(&self) -> Option<String> {
        let line = self.command.trim();
        if !line.contains(['+', '-', '*', '/', '%', '^']) {
            return None;
        }
        browser_core::math_eval(line).map(format_number)
    }

    /// Clear the status line.
    pub(crate) fn clear_status(&mut self) {
        self.status.clear();
        self.status_is_error = false;
        self.status_clear_at = None;
    }

    /// Record a failure: show it in the status bar (red) and append it to the
    /// session error log for `:error`/`:errors`.
    pub(crate) fn set_error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.errors.push(ErrorEntry {
            time: now_hms(),
            command: self.current_command.clone(),
            message: msg.clone(),
        });
        if self.errors.len() > ERROR_LOG_CAP {
            let overflow = self.errors.len() - ERROR_LOG_CAP;
            self.errors.drain(0..overflow);
        }
        self.status = msg;
        self.status_is_error = true;
        self.status_clear_at = None;
    }

    /// Mouse-wheel scroll. `dy_lines` > 0 means the wheel rolled up (toward older
    /// terminal output / the top of a page). Routes to the terminal's scrollback or
    /// the native read-tab scroll offset; web tabs handle the wheel themselves.
    pub(crate) fn on_wheel(&mut self, dy_lines: f64) {
        // Scroll the pane UNDER the cursor (web panes get the wheel via their own
        // HWND, so they never reach here — only native panes and the bars do).
        let Some((tab, rect)) = self.pane_at_pixel(self.cursor_pos.0, self.cursor_pos.1) else {
            return;
        };
        if self.tabs.get(tab).is_some_and(|t| t.term().is_some()) {
            // If the program enabled mouse reporting (vim `mouse=a`, less, tmux…),
            // hand it the wheel so IT scrolls; otherwise page our own scrollback.
            if self.term_mouse_wheel(tab, rect, dy_lines) {
                return;
            }
            let lines = (dy_lines * 3.0).round() as i32;
            if lines != 0 {
                if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) {
                    s.pty.scroll_display(lines);
                    self.window.request_redraw();
                }
            }
            return;
        }
        // Native read pane: scroll its own pixel offset, clamped to its pane height.
        if let Some(nr) = self.tabs.get_mut(tab).and_then(|t| t.native_mut()) {
            let dy = (-dy_lines * 80.0).round() as i32;
            if dy != 0 {
                let max = (nr.layout.height - rect.h).max(0);
                nr.scroll = (nr.scroll + dy).clamp(0, max);
                self.window.request_redraw();
            }
            return;
        }
        // AI pane: move the vim buffer's top by whole lines (and drop "follow" unless
        // we land back at the very bottom).
        let line_h = self.painter.line_height().max(1);
        if let Some(ai) = self.tabs.get_mut(tab).and_then(|t| t.ai_mut()) {
            let rows = (rect.h as usize / line_h).max(1);
            let n = ai.buf.lines.len();
            let max_top = n.saturating_sub(rows) as isize;
            let lines = (-dy_lines * 3.0).round() as isize;
            let top = (ai.buf.top as isize + lines).clamp(0, max_top.max(0)) as usize;
            ai.buf.top = top;
            ai.follow = top as isize >= max_top;
            self.window.request_redraw();
        }
    }

    /// Shut down terminals (kill shells, close PTYs, join readers) and drop every
    /// webview before exiting — so WebView2 processes and ConPTYs close cleanly
    /// rather than leaving a stuck thread that deadlocks process teardown.
    pub(crate) fn teardown(&mut self) {
        // Idempotent: only the first call tears down (see `torn_down`). The session is
        // NOT auto-saved here — saving is explicit (`:w` / `:wq`), vim-style, so just
        // closing the window or `:q` leaves the last written session untouched.
        if self.torn_down {
            return;
        }
        self.torn_down = true;
        for tab in &mut self.tabs {
            if let Some(session) = tab.take_term() {
                session.shutdown();
            }
        }
        self.tabs.clear();
        self.active = None;
    }

    /// Snapshot the open tabs + UI state to disk so the next launch restores them.
    /// Explicit only — run by `:w` / `:wq` (vim-style), never automatically on exit.
    /// Internal pages (`browser://…`, the `:error(s)` log) are session-specific and
    /// skipped. No-op during headless test runs so they don't clobber a real session.
    pub(crate) fn save_session(&self) {
        if std::env::var("BROWSER_TEST_QUIT_MS").is_ok() {
            return;
        }
        let mut tabs = Vec::new();
        let mut active = 0;
        for (i, tab) in self.tabs.iter().enumerate() {
            if tab.url.starts_with("browser://") || tab.vim().is_some() {
                continue;
            }
            let kind = if tab.term().is_some() {
                "term"
            } else if tab.read || tab.native().is_some() {
                "read"
            } else if tab.research {
                "research"
            } else if tab.nojs {
                "nojs"
            } else {
                "open"
            };
            if Some(i) == self.active {
                active = tabs.len();
            }
            tabs.push(session::SavedTab { kind: kind.to_string(), url: tab.url.clone() });
        }
        // Remember the window placement (outer position + inner size) so it reopens
        // exactly where it was.
        let window = self.window.outer_position().ok().map(|p| {
            let s = self.window.inner_size();
            session::WindowGeom { x: p.x, y: p.y, w: s.width, h: s.height }
        });
        session::save(&session::Session {
            zoom: self.zoom,
            nojs: self.nojs,
            adblock: self.adblock,
            search_template: self.search_template.clone(),
            term_command: self.term_command.clone(),
            active,
            history: self.history.clone(),
            history_at: self.history_at.clone(),
            window,
            tabs,
        });
    }

    /// Reopen the tabs + UI state saved by a previous run. Read tabs are re-fetched
    /// (so they may arrive slightly out of order, since fetching is async) and
    /// terminals are reopened fresh.
    pub(crate) fn restore_session(&mut self, s: session::Session) {
        self.search_template = s.search_template;
        if !s.term_command.is_empty() {
            self.term_command = s.term_command;
        }
        self.history = s.history;
        self.history.truncate(HISTORY_CAP);
        // Restore visit times, realigning to the URL list: old sessions (and any
        // length drift) leave entries stamped 0 = "time unknown", so only an
        // all-time `:clear history` removes them, never a windowed clear.
        self.history_at = s.history_at;
        self.history_at.resize(self.history.len(), 0);
        self.nojs = s.nojs;
        self.set_adblock(s.adblock);
        if s.zoom != 1.0 {
            self.set_zoom(s.zoom);
        }
        for tab in &s.tabs {
            // Each restored tab is a NEW tab (push), so they don't replace each other.
            match tab.kind.as_str() {
                "term" => self.open_terminal(),
                "read" => self.start_read(&tab.url, false, true),
                "research" => self.open_research(&tab.url, true),
                "nojs" => self.open_tab(&tab.url, true, true),
                _ => self.open_tab(&tab.url, false, true),
            }
        }
        if !self.tabs.is_empty() {
            self.active = Some(s.active.min(self.tabs.len() - 1));
            self.refresh_visibility();
        }
        self.clear_status();
        self.window.request_redraw();
    }

}
