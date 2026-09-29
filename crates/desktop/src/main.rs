//! browser — a lightweight, keyboard-driven shell that boots a WebView2
//! engine only when you open a page.
//!
//! The window chrome (welcome screen + command bar) is drawn natively with a
//! pixel buffer, so an idle shell holds NO browser engine. Opening a tab builds
//! a child WebView2 on demand; closing it drops the WebView and frees the
//! renderer immediately.
//!
//! Modes (qutebrowser-style):
//!   * Normal      — shell has focus; command bar works; j/k/space scroll the page.
//!   * Command     — typing a `:`-command (entered with `:` or `o`).
//!   * Insert      — `i` / clicking a field types into a page input; Esc, clicking away,
//!     or navigating returns to Normal. Ctrl+V pastes into the field (no promote).
//!   * Passthrough — `Ctrl+V` (or `i` on a terminal / `:ai`) hands every key to the
//!     content; sticky, so it survives clicks/focus/fullscreen/navigation and leaves
//!     only on Ctrl+S or Shift+Esc (a terminal keeps Esc for the shell).

#![windows_subsystem = "windows"]

#[cfg(all(windows, feature = "servo-engine"))]
surfman::declare_surfman!();

use std::rc::Rc;

use anyhow::{Context as _, Result};
use std::time::{Duration, Instant};

use tao::event::{ElementState, Event, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::keyboard::ModifiersState;
use tao::window::WindowBuilder;

mod actions;
mod ai;
mod app;
mod blocklist;
mod bookmarks;
mod bundled_extensions;
mod chrome;
mod commands;
mod config;
mod data;
mod draw;
mod engines;
mod extensions;
mod favicon;
mod find;
mod freeze;
mod hints;
mod keys;
mod khook;
mod markdown;
mod navguard;
mod news;
mod pages;
mod panes;
mod proc_cwd;
mod procmon;
mod profiles;
mod pty_term;
mod read_view;
mod schemes;
mod scripts;
mod session;
mod tabs;
mod term;
mod vim;
// Re-exported so modules (and the Servo engine) can keep using `crate::ADBLOCK_JS`.
use app::{clipboard_get, clipboard_set, AdblockMode, App, ExtInfo, ModeKind, UserEvent};
use commands::COMMANDS;
use draw::Painter;
use find::FindState;
use hints::HintAct;
use pages::commands_document;
pub(crate) use scripts::{
    ADBLOCK_JS, BRIDGE_JS, CARET_JS, FEATURES_JS, FIND_JS, HINT_JS, IPC_PRELUDE, RESEARCH_JS,
};
use tabs::{js_string, parse_open_flags, parse_tab_flag, Source, Tab};
use term::program_exists;

/// Height of the bottom command/status bar, in physical pixels (at zoom 1.0).
const BAR_H: u32 = 28;
/// Height of the top tab bar at zoom 1.0 (only shown with ≥1 tab open).
const TAB_BAR_H: u32 = 24;
/// Native chrome font size in px at zoom 1.0.
const BASE_PX: f32 = 17.0;
/// Global zoom bounds and step.
const ZOOM_MIN: f64 = 0.5;
const ZOOM_MAX: f64 = 3.0;
const ZOOM_STEP: f64 = 0.1;

/// Max visited URLs kept for autocomplete (also the cap persisted in the session).
const HISTORY_CAP: usize = 300;

/// How many recently-closed tabs to remember for `U` / Ctrl+Shift+T (reopen).
const CLOSED_CAP: usize = 20;

/// Left/right padding (px) inside a native terminal tab's content area.
const TERM_PAD: i32 = 4;

/// Tag this process with an explicit AppUserModelID so the taskbar treats it and
/// the processes it spawns as one application. (Task Manager still files WebView2
/// under its own "WebView2 Manager" group: the runtime claims a separate identity.
/// Correct parenting comes from the DPI manifest in `build.rs`.) Best-effort: any
/// failure is ignored.
#[cfg(windows)]
fn set_app_user_model_id() {
    use windows::core::w;
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(w!("kayf.browser.Shell"));
    }
}

fn main() -> Result<()> {
    // Give this process a single explicit AppUserModelID *before* anything is
    // spawned. Child processes inherit it at creation time, so every descendant —
    // the on-demand WebView2 engine and its renderer/GPU/utility processes, the
    // browser-pty-host companion, its conhost + shell — shares one identity and
    // Task Manager groups the whole tree under a single "browser" entry instead of
    // scattering the WebView2 manager and friends as separate top-level apps.
    #[cfg(windows)]
    set_app_user_model_id();

    // Release builds load uBlock Origin Lite from a copy unpacked out of the
    // executable; get that done before the first web tab needs it.
    if !cfg!(debug_assertions) {
        bundled_extensions::prepare_in_background();
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Decide up front whether we're restoring a session: only with no CLI target
    // and outside headless test runs. Load it here so the saved window geometry can
    // be applied at build time (no visible jump from the default size). Always the
    // LIVE session — profiles are snapshots you load on purpose, never a thing the
    // browser silently boots into — except in `:scratch`, whose own file is live for
    // as long as the detour lasts (quitting inside it and relaunching isn't the same
    // as ENTERING it, which always starts clean). The config knows which, so it has
    // to be read before the window exists.
    // `--scratch` boots this RUN into a throwaway slate: the config pointer is set in
    // memory only (so the next ordinary launch returns to the real profile), and every
    // write is redirected to `scratch-cli.toml` — not the live session, not a profile,
    // and not the `:scratch` slate either. The safe way to poke at a dev build while
    // real data is set up. Note you can't get here by typing `:scratch` after launch:
    // that parks whatever is on screen INTO `session.toml` first (`profiles::switch_to`
    // step 1), which is exactly what a throwaway run must not do.
    let mut cli_arg = None;
    let mut cli_scratch = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--scratch" => cli_scratch = true,
            _ if cli_arg.is_none() => cli_arg = Some(a),
            _ => {}
        }
    }
    let is_test = std::env::var("BROWSER_TEST_QUIT_MS").is_ok();
    let mut cfg = config::load();
    if cli_scratch {
        // In-memory only: never `config::save`d here, so the next ordinary launch
        // comes back to the profile the user actually left off in.
        cfg.scratch = true;
        cfg.scratch_return = cfg.profile.take();
    }
    let restore = if cli_arg.is_none() && !is_test {
        let path = if cli_scratch {
            session::cli_scratch_path()
        } else if cfg.scratch {
            session::scratch_path()
        } else {
            session::session_path()
        };
        path.as_deref().and_then(session::load_from)
    } else {
        None
    };

    let mut builder = WindowBuilder::new()
        .with_title("browser")
        .with_decorations(false); // no OS title bar; window control is command-driven
    builder = match restore.as_ref().and_then(|s| s.window.as_ref()) {
        Some(g) => builder
            .with_inner_size(tao::dpi::PhysicalSize::new(g.w, g.h))
            .with_position(tao::dpi::PhysicalPosition::new(g.x, g.y)),
        None => builder.with_inner_size(tao::dpi::LogicalSize::new(1100.0, 740.0)),
    };
    let window = builder.build(&event_loop).context("creating window")?;
    let window = Rc::new(window);

    let context = softbuffer::Context::new(window.clone())
        .map_err(|e| anyhow::anyhow!("softbuffer context: {e}"))?;
    let surface = softbuffer::Surface::new(&context, window.clone())
        .map_err(|e| anyhow::anyhow!("softbuffer surface: {e}"))?;

    let painter = Painter::new(BASE_PX).context("loading font")?;

    let mut app = App {
        window: window.clone(),
        _context: context,
        surface,
        painter,
        proxy,
        mode: ModeKind::Normal,
        command: String::new(),
        command_cursor: 0,
        command_anchor: None,
        hint_input: String::new(),
        hint_act: HintAct::Follow,
        native_hints: Vec::new(),
        status: String::new(),
        status_is_error: false,
        status_color: None,
        status_clear_at: None,
        ai_prev_active: None,
        errors: Vec::new(),
        current_command: None,
        find: FindState::default(),
        tabs: Vec::new(),
        active: None,
        modifiers: ModifiersState::default(),
        nojs: false,
        // Blocking on by default: uBO Lite (network) plus the native layers, which cover
        // different halves of the job (see `AdblockMode`). Session restore may override.
        adblock_mode: AdblockMode::Ubo,
        adblock_prev: AdblockMode::Ubo,
        term_drag: None,
        term_clicks: None,
        extension_request: 0,
        adblock: true,
        adblock_on: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        allow_risky_downloads: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        blocker: blocklist::new_shared(),
        mute: false,
        no_css: false,
        no_video: false,
        no_scrollbar: false,
        term_command: term::default_shell(),
        search_template: browser_core::DEFAULT_SEARCH_URL.to_string(),
        next_term_id: 0,
        groq_key: ai::load_key(),
        next_ai_id: 0,
        ai_model: ai::load_model().unwrap_or_else(|| ai::DEFAULT_MODEL.to_string()),
        ai_chats: ai::load_chats(),
        zoom: 1.0,
        content_zoom: 1.0,
        cursor_on: true,
        term_resize_want: Vec::new(),
        term_resize_want_at: None,
        quit: false,
        torn_down: false,
        res_prev: std::collections::HashMap::new(),
        res_at: Instant::now(),
        cursor_pos: (0.0, 0.0),
        bar_hover: false,
        hover_link: None,
        bar_dragging: false,
        bar_cmd_scroll: 0,
        page_focus_yielded: false,
        page_gesture_at: None,
        acting_ai: None,
        config: cfg,
        theme: draw::Theme::default(),
        last_focus_gain: Instant::now(),
        history: Vec::new(),
        history_at: Vec::new(),
        saved: bookmarks::load(),
        closed_tabs: Vec::new(),
        fs_from_page: false,
        windows: Vec::new(),
        pane_focus: panes::PaneFocus::default(),
        pending_window_key: false,
        pending_window_at: Instant::now(),
        pending_yank_key: false,
        pending_yank_at: Instant::now(),
        cli_scratch,
        pane_resize_at: Instant::now(),
        pane_move_orig: None,
        active_pane_is_webview: false,
        background_webview_visible: false,
        term_scrollback: pty_term::DEFAULT_SCROLLBACK,
        term_style: pty_term::TermStyle::default(),
        term_painter: None,
        nav_replaying: false,
        term_find_pending: None,
        engine_keepalive: None,
        config_edit_term: None,
        installing_scheme: None,
        term_last_find: None,
        frozen: false,
    };
    // Resolve the persisted appearance overrides into the live chrome theme and
    // terminal style (colours + optional custom terminal font).
    app.rebuild_theme();
    app.rebuild_term_style();

    // Compile the ad/redirect blocklist engine off-thread; it goes live a beat after
    // launch (BlocklistReady), and navigations use the timing heuristic until then.
    blocklist::spawn_build(app.blocker.clone(), app.proxy.clone());

    // Optional: open a page immediately, e.g. `browser youtube.com`,
    // or run a command, e.g. `browser ":nojs youtube.com"`. An explicit
    // CLI target takes precedence over (and skips) session restore. With no
    // argument, restore the previous session's tabs + UI state (window geometry was
    // already applied at build time above).
    engines::with_window_target(&event_loop, || match cli_arg {
        Some(target) => {
            let t = target.trim_start();
            if let Some(cmd) = t.strip_prefix(':') {
                app.run_command(cmd);
            } else {
                app.open_tab(&target, false, true);
            }
        }
        None => {
            if let Some(s) = restore {
                app.restore_session(s);
            }
        }
    });

    // First launch of a new version: point at what changed. Not in throwaway runs,
    // which must not write the real config.
    if !cli_scratch && !is_test {
        if let Some(note) = news::note_launch(&mut app.config.seen_version) {
            config::save(&app.config);
            app.set_status(note);
        }
    }

    #[cfg(all(windows, feature = "servo-engine"))]
    engines::with_window_target(&event_loop, || engines::servo::smoke::start(&mut app))?;

    // Test hook: auto-quit after N ms so cleanup can be verified headlessly.
    if let Ok(ms) = std::env::var("BROWSER_TEST_QUIT_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            let proxy = app.proxy.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                let _ = proxy.send_event(UserEvent::Quit);
            });
        }
    }

    // Install the low-level keyboard hook so the shell can always reclaim control
    // (leave passthrough/insert, or snap back from a click that yielded focus to the
    // page) regardless of which HWND/iframe holds keyboard focus. Must run on the
    // event-loop thread (here) so the hook proc fires from its message pump.
    #[cfg(windows)]
    {
        use tao::platform::windows::WindowExtWindows;
        khook::install(app.window.hwnd() as isize, app.proxy.clone());
    }

    window.request_redraw();

    event_loop.run(move |event, target, control_flow| engines::with_window_target(target, || {
        #[cfg(all(windows, feature = "servo-engine"))]
        let event = engines::servo::intercept(&mut app, event).unwrap_or(Event::UserEvent(UserEvent::Servo(engines::servo::Event::Wake)));

        let event = match event {
            Event::UserEvent(UserEvent::Engine { view, event }) => {
                Event::UserEvent(app.route_engine_event(view, *event).unwrap_or(UserEvent::Redraw))
            }
            event => event,
        };
        *control_flow = ControlFlow::Wait;
        match event {
            // Command-bar cursor blink: the WaitUntil deadline (set below while in
            // Command mode) wakes us here to flip the cursor and repaint.
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                // A transient status flash (e.g. a finished background `:ai`) times out.
                app.expire_status_flash();
                // Advance the tab strip's loading sweep. Mode-independent: a page can
                // load while you're typing a command or in passthrough.
                if app.any_tab_loading() {
                    app.window.request_redraw();
                }
                // A held-back terminal resize (zoom/drag burst settling): repaint —
                // the draw's sync_active_term_size applies it once the target settles.
                if app.term_resize_want_at.is_some() {
                    app.window.request_redraw();
                }
                // Repeatable pane-resize auto-leaves after a spell of no resize key, so a
                // later j/k (meant to scroll) doesn't silently resize.
                if app.mode == ModeKind::PaneResize
                    && app.pane_resize_at.elapsed() >= crate::app::PANE_RESIZE_TIMEOUT
                {
                    app.mode = ModeKind::Normal;
                    app.clear_status();
                    app.window.request_redraw();
                }
                if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
                    app.cursor_on = !app.cursor_on;
                    app.window.request_redraw();
                } else if matches!(app.mode, ModeKind::Normal | ModeKind::Scroll | ModeKind::ScrollCaret)
                    || (app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll)
                {
                    // Read-mode caret: blink its block cursor.
                    if app.read_caret_active() {
                        app.cursor_on = !app.cursor_on;
                        app.window.request_redraw();
                    }
                    // Focus backstop: reclaim keyboard focus if a click handed it to
                    // the webview (see reclaim_focus_tick).
                    app.reclaim_focus_tick();
                    // Live `:res` monitor: re-sample on the tick.
                    if app.active_is_res() {
                        app.refresh_res();
                        app.window.request_redraw();
                    }
                }
            }
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    app.teardown();
                    *control_flow = ControlFlow::Exit;
                }
                WindowEvent::Resized(size) => {
                    app.on_resize(size.width, size.height);
                }
                WindowEvent::ModifiersChanged(state) => {
                    app.modifiers = state;
                    app.on_modifiers_changed();
                }
                WindowEvent::Focused(focused) => {
                    if focused {
                        app.last_focus_gain = Instant::now();
                        // Let the keyboard hook swallow the Alt+Tab straggler `Tab` on a
                        // focused web page too (the terminal is guarded in `key_term`).
                        khook::note_focus_gain();
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    app.cursor_pos = (position.x, position.y);
                    // Left-drag inside the command line extends the selection.
                    app.bar_drag(position.x);
                    // Left-drag inside a terminal pane extends ITS selection.
                    app.term_select_drag(position.x, position.y);
                    // Hovering the command bar reveals the full (vs. shortened) URL.
                    let (_, h) = app.inner();
                    let over_bar =
                        app.bar_h() > 0 && position.y >= (h as f64 - app.bar_h() as f64);
                    if over_bar != app.bar_hover {
                        app.bar_hover = over_bar;
                        if app.mode == ModeKind::Normal {
                            app.window.request_redraw();
                        }
                    }
                }
                // The cursor left the parent client area — including moving up onto the
                // child webview, which swallows CursorMoved. Drop the bar hover so the
                // URL collapses back to its short form instead of getting stuck full.
                WindowEvent::CursorLeft { .. } => {
                    // A release over the child webview can be missed by the parent, so
                    // also end any in-progress drag-select here.
                    app.bar_dragging = false;
                    // Same for a terminal drag — but drop it WITHOUT copying: leaving
                    // the client area (often just crossing onto a sibling web pane's
                    // HWND) isn't a release, and shouldn't clobber the clipboard. What
                    // was selected stays highlighted and yankable.
                    app.term_drag = None;
                    if app.bar_hover {
                        app.bar_hover = false;
                        if app.mode == ModeKind::Normal {
                            app.window.request_redraw();
                        }
                    }
                }
                // Clicks on the top tab bar: hit a tab label to switch to it, or
                // drag the borderless window by the empty strip (QoL — like a title
                // bar). The webview owns clicks below the bar.
                WindowEvent::MouseInput {
                    state: ElementState::Released,
                    button: MouseButton::Left,
                    ..
                } => {
                    app.bar_dragging = false;
                    // Ends a terminal drag-select and copies what it covered.
                    app.term_select_end();
                }
                // Right-press on a natively-drawn surface (terminal, :read, a vim
                // pager, the command line): copy that surface's selection. Web panes
                // handle their own right-click in the page's context menu.
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Right,
                    ..
                } => app.right_click_copy(app.cursor_pos.0, app.cursor_pos.1),
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    ..
                } => {
                    let (_, h) = app.inner();
                    let bar_top = h as f64 - app.bar_h() as f64;
                    if app.bar_h() > 0 && app.cursor_pos.1 >= bar_top {
                        // Click the command/status bar: edit the URL or place the caret.
                        app.bar_click(app.cursor_pos.0);
                    } else if !app.tabs.is_empty() && app.cursor_pos.1 < app.tab_bar_h() as f64 {
                        if let Some(i) = app.tab_at_pixel(app.cursor_pos.0) {
                            app.jump_to(i);
                            app.window.request_redraw();
                        } else {
                            let _ = app.window.drag_window();
                        }
                    } else if let Some((tab, rect)) =
                        app.pane_at_pixel(app.cursor_pos.0, app.cursor_pos.1)
                    {
                        // A press on a (native) pane below the tab bar. Web panes
                        // consume the click in their own HWND, so this only fires for
                        // terminal/read/vim/blank panes; the web half of the same
                        // gesture arrives as `PaneClick`. Both go through
                        // `focus_pane_click`, which carries passthrough across the move
                        // and hands the keyboard to whichever pane now owns it.
                        if app.is_split() {
                            app.focus_pane_click(tab);
                        }
                        // Then start a drag-selection if that pane is a terminal (also
                        // when unsplit — one terminal filling the band still selects).
                        app.term_select_start(tab, rect, app.cursor_pos.0, app.cursor_pos.1);
                    }
                }
                // Mouse wheel: scroll the native-drawn content under the cursor (the
                // terminal's scrollback, or a `:read` document). Web tabs receive the
                // wheel directly via their child window, so they never reach here.
                WindowEvent::MouseWheel { delta, .. } => {
                    let dy = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y as f64,
                        MouseScrollDelta::PixelDelta(pos) => pos.y / 40.0,
                        _ => 0.0,
                    };
                    if dy != 0.0 {
                        app.on_wheel(dy);
                    }
                }
                WindowEvent::KeyboardInput { event: key, .. }
                    if key.state == ElementState::Pressed =>
                {
                    app.handle_key(&key);
                    if app.quit {
                        app.teardown();
                        *control_flow = ControlFlow::Exit;
                    }
                }
                _ => {}
            },
            Event::UserEvent(UserEvent::ExitToNormal) => app.exit_to_normal(),
            Event::UserEvent(UserEvent::SyncAdblock) => app.broadcast_adblock(),
            Event::UserEvent(UserEvent::FocusShell) => {
                match app.mode {
                    ModeKind::Hint if app.hint_act == HintAct::Scroll => {
                        app.exit_hint();
                        app.reclaim_shell_focus();
                    }
                    // Passthrough persists across navigation: re-assert it on the new
                    // page and keep the page focused.
                    ModeKind::Passthrough => {
                        app.set_page_mode("passthrough");
                        if let Some(wv) = app.active_webview() {
                            let _ = wv.focus();
                        }
                    }
                    // Insert, Caret and Scroll are tied to the old page's DOM; navigation ends
                    // them (the field/caret is gone on the new document).
                    ModeKind::Insert | ModeKind::Caret | ModeKind::Scroll | ModeKind::ScrollCaret => {
                        app.set_page_mode("normal");
                        app.mode = ModeKind::Normal;
                        app.reclaim_shell_focus();
                        app.window.request_redraw();
                    }
                    // Reclaim KEYBOARD focus from the webview child, but never steal the
                    // FOREGROUND: this fires on every page-load/`page-ready`, and a busy
                    // site (ads, video, redirects) that finishes loading after you've
                    // alt-tabbed away must not pop the window back to the front.
                    // `SetFocus` no-ops when we aren't the foreground app, so leaving is
                    // respected; the periodic `reclaim_focus_tick` restores keyboard
                    // focus when you actually return.
                    _ => app.reclaim_shell_focus(),
                }
                // A navigation (e.g. clicking a link that yielded focus) lands on a
                // fresh page with the shell back in control — end any page-focus yield.
                app.page_focus_yielded = false;
                // The old page's hovered-link readout is stale on a new document.
                app.hover_link = None;
                // A fresh navigation can reset the page's zoom factor — re-apply.
                app.apply_active_zoom();
                // Track the post-navigation URL in the status bar.
                app.refresh_active_url();
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::GrabFocus) => {
                // Only in Normal mode: the shell owns the keyboard there. In
                // Insert/Passthrough the page legitimately holds focus. A bare-area
                // click ends any prior page-focus yield.
                if app.mode == ModeKind::Normal {
                    app.page_focus_yielded = false;
                    // The gesture resolved to "the shell keeps the keyboard": end the
                    // grace so the poll guards this page again immediately.
                    app.page_gesture_at = None;
                    app.reclaim_shell_focus();
                }
            }
            // A click on a page control left focus in the page so its menu stays open.
            Event::UserEvent(UserEvent::PageHold) => {
                if app.mode == ModeKind::Normal && app.active_webview().is_some() {
                    app.page_focus_yielded = true;
                    app.window.request_redraw();
                }
            }
            // A click landed in a text field: enter Insert so typing goes to the page
            // (and clicking away later drops back to Normal on its own).
            Event::UserEvent(UserEvent::PageEdit) => {
                if app.mode == ModeKind::Normal {
                    app.page_focus_yielded = false;
                    app.enter_insert();
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::LinkHover(href)) => {
                // Only the focused tab's hover readout matters; ignore stray reports
                // from a background pane. Empty = the pointer left the link.
                let next = (!href.is_empty()).then_some(href);
                if app.hover_link != next {
                    app.hover_link = next;
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::ReclaimNormal) => app.reclaim_from_page(),
            Event::UserEvent(UserEvent::ReplayToShell(key)) => {
                // Only while still in Normal: a key queued behind one that changed
                // mode (`:` opening the command bar) is already on its way to the
                // shell, which holds focus by then.
                if app.mode == ModeKind::Normal {
                    app.reclaim_from_page();
                    // Update the hook now, so keys typed right behind this one aren't
                    // taken from the page a second time.
                    khook::set_mode(app.hook_mode_code());
                }
                khook::replay(key);
            }
            Event::UserEvent(UserEvent::PaneClick) => {
                // A gesture is under way in the page: hold the focus-reclaim poll off
                // until it has finished and the bridge has said who should keep the
                // keyboard (see `reclaim_focus_tick`).
                app.page_gesture_at = Some(Instant::now());
                // Clicking inside a web pane focuses it (web panes consume the click in
                // their HWND, so this is the only way they reach us). Use the OS cursor
                // position, since CursorMoved isn't delivered over a child webview.
                // `focus_pane_click` only marks the pane active — it must NOT reclaim the
                // keyboard, or it'd yank focus straight back off the page you just
                // clicked, leaving the pane merely "selected" until a second click.
                if app.is_split() {
                    if let Some((x, y)) = app.cursor_client_pos() {
                        if let Some((tab, _)) = app.pane_at_pixel(x as f64, y as f64) {
                            app.focus_pane_click(tab);
                        }
                    }
                }
            }
            Event::UserEvent(UserEvent::ExitHint) => {
                app.hint_input.clear();
                app.hint_act = HintAct::Follow;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::ScrollSelected) => {
                if app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll {
                    app.hint_input.clear();
                    app.hint_act = HintAct::Follow;
                    app.mode = ModeKind::Scroll;
                    app.set_page_mode("scroll");
                    app.reclaim_shell_focus();
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::ScrollExit) => {
                if matches!(app.mode, ModeKind::Scroll | ModeKind::ScrollCaret) {
                    app.exit_to_normal();
                    app.set_status("scroll target is no longer available");
                } else if app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll {
                    app.exit_hint();
                    app.set_status("no scrollable boxes on screen");
                }
            }
            Event::UserEvent(UserEvent::HintEdit) => {
                // The hint selected a text field: enter Insert (type into the page),
                // then focus the field itself within the page.
                app.hint_input.clear();
                app.enter_insert();
                if let Some(wv) = app.active_webview() {
                    let _ = wv.evaluate_script(
                        "window.__hintTarget&&(window.__hintTarget.focus(),window.__hintTarget=null)",
                    );
                }
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::HintOpen(url)) => {
                // The page already cleared its badges; just reset shell hint state
                // and open the link in a fresh tab.
                app.hint_input.clear();
                app.hint_act = HintAct::Follow;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.open_tab(&url, app.nojs, true);
            }
            Event::UserEvent(UserEvent::HintCopy(url)) => {
                // Copy mode (`yf`): the page already cleared its badges — reset the
                // shell's hint state, take the keyboard back, and yank the address.
                app.hint_input.clear();
                app.hint_act = HintAct::Follow;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.copy_text(&url);
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::ReadReady { doc, replace, record }) => {
                app.show_read_document(*doc, replace, record);
            }
            Event::UserEvent(UserEvent::ReadFailed(e)) => {
                app.set_error(format!("read failed: {e}"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::Navigate(url)) => {
                if let Some(i) = app.active {
                    // Shell-driven `load_url` reads as not-user-initiated; stamp intent so
                    // the native guard lets this de-proxy redirect through.
                    if let Some(wv) = app.tabs.get(i).and_then(|t| t.webview()) {
                        let _ = wv.load_url(&url);
                    }
                    // Reflect the de-proxied address in the status bar right away
                    // (the live URL refresh on page-load will confirm it).
                    if let Some(t) = app.tabs.get_mut(i) {
                        t.url = url;
                    }
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::RedirectBlocked(url)) => {
                // Show the destination we refused, truncated so a long tracking URL
                // doesn't blow out the status bar.
                let short: String = url.chars().take(80).collect();
                app.set_status(format!("blocked redirect → {short}"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::PopupBlocked(url)) => {
                let short: String = url.chars().take(80).collect();
                app.set_status(format!("blocked pop-up → {short}  (:ads off to allow)"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::OpenPopupTab(url)) => {
                // A real new-tab click the popup guard cleared: re-open it as a managed
                // tab (new tabs here live in our tab strip, not as OS popups).
                app.open_tab(&url, app.nojs, true);
            }
            Event::UserEvent(UserEvent::BlocklistReady) => {
                // Quiet by default (don't clobber a useful status); the engine simply
                // starts catching navigations from here on.
            }
            Event::UserEvent(UserEvent::ExtensionsListed { request, view, result }) => {
                if request == app.extension_request && app.view_by_id(view).is_some() {
                    match result {
                        Ok(items) => app.show_extensions_page(view, items),
                        Err(error) => app.set_error(error),
                    }
                }
            }
            Event::UserEvent(UserEvent::DownloadBlocked(name)) => {
                let short: String = name.chars().take(60).collect();
                app.set_error(format!(
                    "blocked download of {short} — executable/installer. :downloads to allow"
                ));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::DataCleared { label, ai_id, result }) => {
                // Announce failures, including those from the bonus engine-history
                // clear. An empty label only suppresses successful bonus reports.
                if let Err(error) = result {
                    let target = if label.is_empty() { "engine history" } else { &label };
                    let message = format!("could not clear {target}: {error}");
                    let shown_in_chat = ai_id.is_some_and(|id| app.ai_note(id, &message));
                    if !shown_in_chat {
                        app.set_error(message);
                        app.window.request_redraw();
                    }
                } else if !label.is_empty() {
                    // If an :ai tab asked for it, confirm in that chat; fall back to the
                    // status bar only when that tab isn't the one on screen.
                    let shown_in_chat = ai_id.is_some_and(|id| app.ai_action_done(id, &label));
                    if !shown_in_chat {
                        app.set_status(format!("cleared {label}"));
                        app.window.request_redraw();
                    }
                }
            }
            Event::UserEvent(UserEvent::SchemeInstalled { ai_id, result }) => {
                app.finish_scheme_install(ai_id, result);
            }
            Event::UserEvent(UserEvent::RestoreDefaults) => {
                // The Ctrl+Alt+Shift+R panic chord (caught by the keyboard hook below
                // the keybind layer). Route through the same action so it's one path.
                app.run_action("restore", serde_json::json!({}));
            }
            Event::UserEvent(UserEvent::TermDone { cmd, output, code }) => {
                app.show_term_result(&cmd, &output, code);
            }
            Event::UserEvent(UserEvent::TermOutput { id, data }) => app.feed_terminal(id, &data),
            Event::UserEvent(UserEvent::CaretYank(text)) => {
                let n = text.chars().count();
                clipboard_set(&text);
                if app.mode == ModeKind::Caret {
                    app.mode = ModeKind::Normal;
                } else if app.mode == ModeKind::ScrollCaret {
                    app.mode = ModeKind::Scroll;
                }
                app.set_status(format!("yanked {n} chars"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::ClipCopy(text)) => {
                app.copy_text(&text);
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::CaretExit) => {
                if matches!(app.mode, ModeKind::Caret | ModeKind::ScrollCaret) {
                    app.mode = if app.mode == ModeKind::ScrollCaret {
                        ModeKind::Scroll
                    } else {
                        ModeKind::Normal
                    };
                    app.clear_status();
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::AiReply { id, convo, round, result }) => {
                app.ai_reply(id, convo, round, result)
            }
            Event::UserEvent(UserEvent::PageFullscreen(on)) => app.set_page_fullscreen(on),
            // An SPA navigation changed the page URL without a document load: sync the
            // stored URL (and, for real steps, the H/L back stack) + repaint the bar.
            Event::UserEvent(UserEvent::UrlChanged { record }) => {
                app.refresh_active_url_record(record);
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::Redraw) => app.window.request_redraw(),
            Event::UserEvent(UserEvent::TermClosed { id }) => app.close_term_tab(id),
            Event::UserEvent(UserEvent::Quit) => {
                app.teardown();
                *control_flow = ControlFlow::Exit;
            }
            Event::LoopDestroyed => app.teardown(),
            Event::RedrawRequested(_) => {
                if let Err(e) = app.draw() {
                    eprintln!("draw error: {e}");
                }
            }
            _ => {}
        }
        // Keep the keyboard hook's view of the mode current, so it intercepts the
        // right chords (leave passthrough/insert, Esc out of a page-focus yield).
        #[cfg(all(windows, feature = "servo-engine"))]
        let servo_smoke = engines::servo::smoke::tick(&mut app);
        khook::set_mode(app.hook_mode_code());
        if app.quit { app.teardown(); *control_flow = ControlFlow::Exit; }

        // While typing a command, keep waking to blink the cursor (unless we're
        // already exiting). Outside Command mode we stay on plain Wait.
        if !matches!(*control_flow, ControlFlow::Exit | ControlFlow::ExitWithCode(_)) {
            if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
                // Blink the command-bar cursor.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
            } else if app.mode == ModeKind::Normal && app.read_caret_active() {
                // Blink the read-mode caret's block cursor.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
            } else if app.active_pane_is_webview
                && (matches!(app.mode, ModeKind::Normal | ModeKind::Scroll | ModeKind::ScrollCaret)
                    || (app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll))
            {
                // Poll to keep keyboard focus on the shell while the FOCUSED pane is a
                // web tab (the click-focus backstop).
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(300));
            } else if app.mode == ModeKind::Normal && app.background_webview_visible {
                // A web pane is merely visible beside a focused terminal/read pane: it
                // can still trap the keyboard on a stray click, but that's rare — tick
                // slowly so working in the native pane stays near-idle. Fully idle
                // otherwise — zero wakeups on welcome/read/term-only layouts.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1000));
            } else if app.mode == ModeKind::Normal && app.active_is_res() {
                // Auto-refresh the live resource monitor about once a second.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1000));
            } else if app.mode == ModeKind::PaneResize {
                // Wake at the resize-mode idle deadline so it can auto-exit.
                *control_flow =
                    ControlFlow::WaitUntil(app.pane_resize_at + crate::app::PANE_RESIZE_TIMEOUT);
            }
            // A page is loading: wake fast enough for the tab strip's progress sweep to
            // read as motion. Merged (not chained onto the mode branches above) because
            // a load can be in flight in any mode.
            if app.any_tab_loading() {
                let deadline = Instant::now() + Duration::from_millis(60);
                let next = match *control_flow {
                    ControlFlow::WaitUntil(t) => t.min(deadline),
                    _ => deadline,
                };
                *control_flow = ControlFlow::WaitUntil(next);
            }
            // A pending status-flash auto-clear: wake at its deadline (or sooner, if
            // another timer above already wins).
            if let Some(clear_at) = app.status_clear_at {
                let next = match *control_flow {
                    ControlFlow::WaitUntil(t) => t.min(clear_at),
                    _ => clear_at,
                };
                *control_flow = ControlFlow::WaitUntil(next);
            }
            // A held-back terminal resize: wake when its settle window closes.
            if let Some(at) = app.term_resize_want_at {
                let deadline = at + crate::app::TERM_RESIZE_DEBOUNCE;
                let next = match *control_flow {
                    ControlFlow::WaitUntil(t) => t.min(deadline),
                    _ => deadline,
                };
                *control_flow = ControlFlow::WaitUntil(next);
            }
            #[cfg(all(windows, feature = "servo-engine"))]
            if servo_smoke { *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(50)); }
            #[cfg(all(windows, feature = "servo-engine"))]
            if let Some(deadline) = engines::servo::tick(&app) {
                let next = match *control_flow { ControlFlow::WaitUntil(t) => t.min(deadline), _ => deadline };
                *control_flow = ControlFlow::WaitUntil(next);
            }
        }
    }));
}
#[cfg(test)]
mod tests {
    use super::vim::{Key, TextBuffer};

    fn type_keys(b: &mut TextBuffer, chars: &str) -> Option<String> {
        let mut last = None;
        for c in chars.chars() {
            last = b.key(Key::Char(c), 20, 80).yanked;
        }
        last
    }

    #[test]
    fn yank_inside_parens_grabs_the_hresult_token() {
        let mut b = TextBuffer::new(vec!["WindowsError(HRESULT(0x8007139f))".into()]);
        b.cx = 25; // somewhere inside the inner parens
        assert_eq!(type_keys(&mut b, "yi(").as_deref(), Some("0x8007139f"));
    }

    #[test]
    fn yank_inner_word_grabs_the_whole_token() {
        let mut b = TextBuffer::new(vec!["code 0x8007139f here".into()]);
        b.cx = 8; // inside the hex token
        assert_eq!(type_keys(&mut b, "yiw").as_deref(), Some("0x8007139f"));
    }

    #[test]
    fn charwise_visual_selection_yanks_inclusively() {
        let mut b = TextBuffer::new(vec!["HRESULT(0x..)".into()]);
        assert!(b.key(Key::Char('v'), 20, 80).consumed);
        assert_eq!(b.mode_label(), Some("VISUAL"));
        for _ in 0..6 {
            b.key(Key::Char('l'), 20, 80); // cursor 0 -> 6, inclusive of char 6
        }
        let yanked = b.key(Key::Char('y'), 20, 80).yanked;
        assert_eq!(yanked.as_deref(), Some("HRESULT"));
        assert_eq!(b.mode_label(), None); // visual cleared after yank
    }

    #[test]
    fn yy_yanks_the_whole_line() {
        let mut b = TextBuffer::new(vec!["first".into(), "second".into()]);
        b.key(Key::Char('j'), 20, 80); // -> line 1
        assert_eq!(type_keys(&mut b, "yy").as_deref(), Some("second"));
    }

    #[test]
    fn np_swallowed_colon_falls_through() {
        let mut b = TextBuffer::new(vec!["x".into()]);
        // `n`/`p` are swallowed (no tab switch from the pager); `:` falls through.
        assert!(b.key(Key::Char('n'), 20, 80).consumed);
        assert!(b.key(Key::Char('p'), 20, 80).consumed);
        assert!(!b.key(Key::Char(':'), 20, 80).consumed);
    }

    #[test]
    fn find_char_moves_cursor_onto_target() {
        let mut b = TextBuffer::new(vec!["abc(def)ghi".into()]);
        type_keys(&mut b, "f("); // jump to the '('
        assert_eq!(b.cx, 3);
        type_keys(&mut b, "f)"); // jump forward to the ')'
        assert_eq!(b.cx, 7);
        type_keys(&mut b, "F("); // jump back to the '('
        assert_eq!(b.cx, 3);
    }

    #[test]
    fn till_char_stops_before_target() {
        let mut b = TextBuffer::new(vec!["abc(def)".into()]);
        type_keys(&mut b, "t("); // land just before '('
        assert_eq!(b.cx, 2);
    }

    #[test]
    fn repeat_find_with_semicolon_and_comma() {
        let mut b = TextBuffer::new(vec!["a.b.c.d".into()]);
        type_keys(&mut b, "f."); // first '.'
        assert_eq!(b.cx, 1);
        type_keys(&mut b, ";"); // next '.'
        assert_eq!(b.cx, 3);
        type_keys(&mut b, ","); // back to previous '.'
        assert_eq!(b.cx, 1);
    }

    #[test]
    fn yank_find_includes_target_till_excludes_it() {
        let mut b = TextBuffer::new(vec!["key=value;".into()]);
        assert_eq!(type_keys(&mut b, "yf;").as_deref(), Some("key=value;"));
        let mut b2 = TextBuffer::new(vec!["key=value;".into()]);
        assert_eq!(type_keys(&mut b2, "yt;").as_deref(), Some("key=value"));
    }
}
