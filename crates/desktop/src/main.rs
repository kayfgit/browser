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
use std::time::Instant;

use tao::event_loop::EventLoopBuilder;
use tao::keyboard::ModifiersState;
use tao::window::WindowBuilder;

mod actions;
mod adblock;
mod ai;
mod app;
mod bangs;
mod blocklist;
mod bookmarks;
mod bundled_extensions;
mod chrome;
mod cmdline;
mod commands;
mod config;
mod data;
mod devtools;
mod draw;
mod engines;
mod events;
mod extensions;
mod favicon;
mod find;
mod freeze;
mod hints;
mod keys;
mod layout;
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
mod shellkeys;
mod status;
mod tabs;
mod term;
mod update;
mod vim;
mod visited;
// Re-exported so modules (and the Servo engine) can keep using `crate::ADBLOCK_JS`.
use adblock::AdblockMode;
use app::{clipboard_get, clipboard_set, App, ExtInfo, ModeKind, UserEvent};
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
        cmdline: cmdline::LineEdit::default(),
        hint_input: String::new(),
        hint_act: HintAct::Follow,
        native_hints: Vec::new(),
        status: status::Status::default(),
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
        adblock: adblock::Adblock::default(),
        term_drag: None,
        term_clicks: None,
        extension_request: 0,
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
        term_resize: term::ResizeDebounce::default(),
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
        visited: visited::Visited::default(),
        saved: bookmarks::load(),
        closed_tabs: Vec::new(),
        fs_from_page: false,
        windows: Vec::new(),
        pane_focus: panes::PaneFocus::default(),
        pending_prefix: None,
        layout_history: layout::LayoutHistory::default(),
        update_available: None,
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
        app.check_for_update_at_launch();
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

    window.request_redraw();

    event_loop.run(move |event, target, control_flow| {
        engines::with_window_target(target, || events::handle(&mut app, event, control_flow))
    });
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
