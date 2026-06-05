//! browser-desktop — a lightweight, keyboard-driven shell that boots a WebView2
//! engine only when you open a page.
//!
//! The window chrome (welcome screen + command bar) is drawn natively with a
//! pixel buffer, so an idle shell holds NO browser engine. Opening a tab builds
//! a child WebView2 on demand; closing it drops the WebView and frees the
//! renderer immediately.
//!
//! Modes (qutebrowser-style):
//!   * Normal  — shell has focus; command bar works; j/k/space scroll the page.
//!   * Command — typing a `:`-command (entered with `:` or `o`).
//!   * Insert  — `i` hands focus to the page (e.g. to click YouTube); Esc returns.

#![windows_subsystem = "windows"]

use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::thread::JoinHandle;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use std::time::{Duration, Instant};

use tao::event::{ElementState, Event, KeyEvent, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::keyboard::{Key, KeyCode, ModifiersState};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows};

mod draw;
use draw::Painter;

/// Height of the bottom command/status bar, in physical pixels (at zoom 1.0).
const BAR_H: u32 = 28;
/// Height of the top tab bar at zoom 1.0 (only shown with ≥1 tab open).
const TAB_BAR_H: u32 = 24;
/// Native chrome font size in px at zoom 1.0.
const BASE_PX: f32 = 17.0;
/// Terminal (xterm) font size in px at zoom 1.0.
const BASE_TERM_PX: f64 = 16.0;
/// Global zoom bounds and step.
const ZOOM_MIN: f64 = 0.5;
const ZOOM_MAX: f64 = 3.0;
const ZOOM_STEP: f64 = 0.1;

/// Injected into every page. Reads a synchronous `window.__mode` flag (kept in
/// sync by the shell) and, per mode, intercepts exactly the keys the shell owns.
/// In `insert` it takes Escape (leave) and Ctrl+V (to passthrough) and lets the
/// rest type; in `passthrough` it takes only Shift+Escape and lets every other key
/// reach the page. In insert it also reports when focus leaves the editable element,
/// so the shell can drop back to normal when you click away.
const BRIDGE_JS: &str = r#"
(function () {
  if (window.__shellBridge) return;
  window.__shellBridge = true;
  if (typeof window.__mode === 'undefined') window.__mode = 'normal';
  function post(m) { if (window.ipc) window.ipc.postMessage(m); }
  function editable(el) {
    if (!el) return false;
    var tag = el.tagName;
    if (tag === 'TEXTAREA' || tag === 'SELECT') return true;
    if (tag === 'INPUT') {
      var t = (el.getAttribute('type') || 'text').toLowerCase();
      return ['button','submit','reset','checkbox','radio','file','image','range','color','hidden']
        .indexOf(t) === -1;
    }
    return !!el.isContentEditable;
  }
  window.__shellEditable = editable;
  document.addEventListener('keydown', function (e) {
    var m = window.__mode;
    if (m === 'insert') {
      if (e.key === 'Escape' && !e.shiftKey) { e.preventDefault(); e.stopPropagation(); post('insert-escape'); }
      else if (e.ctrlKey && (e.key === 'v' || e.key === 'V')) { e.preventDefault(); e.stopPropagation(); post('to-passthrough'); }
    } else if (m === 'passthrough') {
      if (e.key === 'Escape' && e.shiftKey) { e.preventDefault(); e.stopPropagation(); post('leave-passthrough'); }
    }
  }, true);
  document.addEventListener('focusout', function () {
    if (window.__mode !== 'insert') return;
    setTimeout(function () {
      var a = document.activeElement;
      if (!a || !editable(a)) post('insert-blur');
    }, 0);
  }, true);
  // In Normal mode the shell owns the keyboard. A click — or a script calling
  // .focus() (common on SPAs like YouTube Shorts) — can move OS keyboard focus
  // into the page and lock the user out of shell keys (':' / Esc). Bounce it back
  // to the shell. Throttled so a page that keeps re-grabbing focus can't spin.
  var __lastGrab = 0;
  function grabBack() {
    if (window.__mode && window.__mode !== 'normal') return;
    var now = Date.now();
    if (now - __lastGrab < 200) return;
    __lastGrab = now;
    // Defer past the current gesture so the webview has actually taken focus by
    // the time the shell calls SetFocus to take it back (avoids a focus race).
    setTimeout(function () { post('grab-focus'); }, 0);
  }
  // focusin catches a script .focus(); mousedown catches a plain click on the
  // page body (which takes OS keyboard focus but fires NO focusin, since the body
  // isn't a focusable element) — that body-click case is the common trap.
  document.addEventListener('focusin', grabBack, true);
  document.addEventListener('mousedown', grabBack, true);
  // Tell the shell once the page is up so it can reclaim keyboard focus — works
  // for both URL and with_html content, independent of native load events.
  window.addEventListener('load', function () { post('page-ready'); });
})();
"#;

/// Injected on demand to drive hint mode. Defines `window.__hintShow/Input/Clear`.
/// The shell collects the typed label and calls `__hintInput`; the page filters
/// badges and, on an exact match, clicks the target and reports back via IPC.
const HINT_JS: &str = r#"
(function () {
  if (window.__hintClear) window.__hintClear();
  var chars = "asdfghjkl";
  var sel = "a[href], button, input:not([type=hidden]):not([disabled]), textarea, " +
            "select, [onclick], [role='button'], [role='link'], [tabindex]:not([tabindex='-1'])";
  var els = Array.prototype.slice.call(document.querySelectorAll(sel)).filter(function (el) {
    var r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return false;
    if (r.bottom < 0 || r.right < 0 || r.top > innerHeight || r.left > innerWidth) return false;
    var st = getComputedStyle(el);
    return st.visibility !== 'hidden' && st.display !== 'none';
  });
  function gen(n) {
    if (n === 0) return [];
    var width = 1, cap = chars.length;
    while (cap < n) { width++; cap *= chars.length; }
    var out = [];
    for (var i = 0; i < n; i++) {
      var s = '', x = i;
      for (var w = 0; w < width; w++) { s = chars[x % chars.length] + s; x = Math.floor(x / chars.length); }
      out.push(s);
    }
    return out;
  }
  var labels = gen(els.length);
  var box = document.createElement('div');
  box.id = '__hint_box';
  var map = {};
  for (var i = 0; i < els.length; i++) {
    var r = els[i].getBoundingClientRect();
    var b = document.createElement('span');
    b.textContent = labels[i];
    b.style.cssText = 'position:fixed;left:' + Math.max(0, r.left) + 'px;top:' + Math.max(0, r.top) +
      'px;z-index:2147483647;background:#ffd400;color:#000;font:bold 11px monospace;padding:0 3px;' +
      'border:1px solid #000;border-radius:3px;line-height:14px;pointer-events:none;';
    box.appendChild(b);
    map[labels[i]] = { el: els[i], badge: b };
  }
  document.documentElement.appendChild(box);
  window.__hintMap = map;
  window.__hintClear = function () {
    var x = document.getElementById('__hint_box');
    if (x) x.remove();
    window.__hintMap = null;
  };
  function editable(el) {
    if (!el) return false;
    var tag = el.tagName;
    if (tag === 'TEXTAREA' || tag === 'SELECT') return true;
    if (tag === 'INPUT') {
      var t = (el.getAttribute('type') || 'text').toLowerCase();
      return ['button','submit','reset','checkbox','radio','file','image','range','color','hidden']
        .indexOf(t) === -1;
    }
    return !!el.isContentEditable;
  }
  window.__hintInput = function (s) {
    var m = window.__hintMap; if (!m) return;
    s = (s || '').toLowerCase();
    var exact = null;
    for (var k in m) {
      if (k.indexOf(s) === 0) { m[k].badge.style.display = ''; if (k === s) exact = m[k]; }
      else { m[k].badge.style.display = 'none'; }
    }
    if (exact) {
      var el = exact.el;
      var edit = editable(el);
      window.__hintClear();
      if (edit) {
        // Defer focusing until the shell has handed the webview OS focus, so the
        // field (not the document body) ends up focused; then enter passthrough.
        window.__hintTarget = el;
        if (window.ipc) window.ipc.postMessage('hint-edit');
      } else {
        try { el.focus(); el.click(); } catch (e) {}
        if (window.ipc) window.ipc.postMessage('hint-exit');
      }
    }
  };
})();
"#;

/// Injected into `:research` tabs. Strips the heavy/noisy stuff (video, audio,
/// embeds, ad/social iframes) on document-create and as the page mutates, while
/// leaving images and text intact — a lighter browse for "how do I…" research.
/// Page scripts still run (so SPAs work); this only prunes the DOM after the fact,
/// since wry exposes no sub-resource request blocker to stop the loads outright.
const RESEARCH_JS: &str = r#"
(function () {
  if (window.__researchLite) return;
  window.__researchLite = true;
  var SEL = 'video,audio,iframe,embed,object,track,source';
  function strip(root) {
    try {
      var r = root && root.querySelectorAll ? root : document;
      var hits = r.querySelectorAll(SEL);
      for (var i = 0; i < hits.length; i++) hits[i].remove();
    } catch (e) {}
  }
  strip(document);
  document.addEventListener('DOMContentLoaded', function () { strip(document); });
  function observe() {
    if (!document.documentElement) { setTimeout(observe, 0); return; }
    new MutationObserver(function (muts) {
      for (var i = 0; i < muts.length; i++) {
        var added = muts[i].addedNodes;
        for (var j = 0; j < added.length; j++) {
          var n = added[j];
          if (n.nodeType !== 1) continue;
          if (n.matches && n.matches(SEL)) n.remove();
          else strip(n);
        }
      }
    }).observe(document.documentElement, { childList: true, subtree: true });
  }
  observe();
})();
"#;

/// Events posted from webview IPC back into the event loop.
enum UserEvent {
    /// Leave insert/passthrough: move focus from the page back to the shell.
    ExitToNormal,
    /// Promote insert → passthrough (Ctrl+V while typing); the page keeps focus.
    InsertToPassthrough,
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
    /// A `:read` extraction finished: open a reader tab with this article HTML.
    ReadReady { url: String, title: String, html: String },
    /// A `:read` extraction failed.
    ReadFailed(String),
    /// A `:te` command finished: combined output and exit code.
    TermDone { cmd: String, output: String, code: Option<i32> },
    /// Keystrokes from a terminal's xterm → write to its PTY (routed by tab id).
    TermInput { id: u64, data: String },
    /// xterm reflow → resize the PTY (cols/rows), routed by tab id.
    TermResize { id: u64, cols: u16, rows: u16 },
    /// Output bytes (base64) from a terminal's PTY → feed to its xterm.
    TermOutput { id: u64, data: String },
    /// The terminal page has set up `window.__feed` and is ready to receive
    /// output → flush anything buffered before it loaded.
    TermReady { id: u64 },
    /// The terminal's shell exited (pty-host stdout EOF) → close that tab.
    TermClosed { id: u64 },
    /// Zoom the whole UI by N steps (forwarded from a focused page, e.g. terminal).
    ZoomStep(i32),
    /// Reset zoom to 100% (forwarded from a focused page).
    ZoomReset,
    Quit,
}

#[derive(Clone, Copy, PartialEq)]
enum ModeKind {
    Normal,
    Command,
    /// Temporary typing in a field. The page types, but the shell still owns
    /// Escape (leave) and Ctrl+V (→ passthrough); auto-exits when focus leaves the
    /// field. Enter: `i` or a hint on an editable element.
    Insert,
    /// Every keystroke goes to the page, no exceptions; persists across clicks and
    /// navigation. Enter: Ctrl+V. Leave: Shift+Esc only.
    Passthrough,
    /// hjkl resize the window; Esc exits. Entered with `:resize`.
    Resize,
    /// hjkl move the window across the desktop; Esc exits. Entered with `:move`.
    Move,
    /// Link hints are shown; typed characters select one. Entered with `f`.
    Hint,
}

/// Where a content webview gets its page from.
enum Source {
    Url(String),
    Html(String),
}

struct Tab {
    webview: WebView,
    url: String,
    /// Whether this tab was opened with JavaScript disabled (hint mode needs JS).
    nojs: bool,
    /// Whether this is a readability "read mode" tab.
    read: bool,
    /// Whether this is a "research" tab: a normal page (JS on, images kept) with
    /// heavy media/embeds stripped on the fly for a lighter browse.
    research: bool,
    /// Present if this tab is an embedded terminal (xterm.js + PTY).
    term: Option<TermSession>,
}

/// A terminal tab's link to its companion `browser-pty-host` process. The ConPTY
/// lives entirely in that process; here we only hold a normal pipe + the process,
/// none of which can deadlock our exit.
struct TermSession {
    id: u64,
    child: Child,
    stdin: ChildStdin,
    /// Kill-on-close job containing the pty-host (and its conhost + shell), so
    /// closing it reaps the whole tree. 0 if jobs are unavailable.
    job: isize,
    reader: Option<JoinHandle<()>>,
    /// Has the xterm page set up `window.__feed` yet? ConPTY emits its init
    /// sequence (including the `ESC[6n` cursor query the terminal MUST answer or
    /// the shell stalls) the instant it spawns — well before the webview loads.
    /// Until the page reports ready we buffer output here instead of dropping it.
    ready: bool,
    pending: Vec<String>,
}

impl TermSession {
    /// Send a framed message to the pty-host: `[kind:u8][len:u32 LE][payload]`.
    fn send(&mut self, kind: u8, payload: &[u8]) {
        let mut header = [0u8; 5];
        header[0] = kind;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let _ = self.stdin.write_all(&header);
        let _ = self.stdin.write_all(payload);
        let _ = self.stdin.flush();
    }

    /// Tear down: closing the job force-kills the pty-host + its conhost + shell;
    /// the reader then EOFs on the (normal) pipe. None of this can hang our process.
    fn shutdown(mut self) {
        #[cfg(windows)]
        if self.job != 0 {
            job::close(self.job);
        }
        drop(self.stdin); // EOF the pty-host's stdin as well
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

/// Windows job-object helpers: confine the pty-host to a kill-on-close job so the
/// OS reaps it (and its descendants) when we close the handle or the browser dies.
#[cfg(windows)]
mod job {
    use core::ffi::c_void;
    use std::mem::size_of;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Create a kill-on-close job and assign `process_handle` to it. Returns the
    /// job handle (as isize) to keep open; 0 on failure.
    pub fn create_for(process_handle: isize) -> isize {
        unsafe {
            let Ok(job) = CreateJobObjectW(None, windows::core::PCWSTR::null()) else {
                return 0;
            };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let _ = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if AssignProcessToJobObject(job, HANDLE(process_handle as *mut c_void)).is_err() {
                let _ = CloseHandle(job);
                return 0;
            }
            job.0 as isize
        }
    }

    pub fn close(job: isize) {
        unsafe {
            let _ = CloseHandle(HANDLE(job as *mut c_void));
        }
    }
}

struct App {
    window: Rc<Window>,
    // Kept alive for the lifetime of `surface`, which is created from it.
    _context: softbuffer::Context<Rc<Window>>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    painter: Painter,
    proxy: EventLoopProxy<UserEvent>,

    mode: ModeKind,
    command: String,
    /// Caret position within `command`, as a byte offset on a char boundary.
    command_cursor: usize,
    /// Selection anchor (byte offset). When `Some` and != cursor, the text between
    /// it and the caret is selected (Shift-movement extends it; typing replaces it).
    command_anchor: Option<usize>,
    /// Accumulated label characters while in Hint mode.
    hint_input: String,
    status: String,
    tabs: Vec<Tab>,
    active: Option<usize>,
    /// Current keyboard modifier state (tracked via ModifiersChanged).
    modifiers: ModifiersState,
    /// When true, new tabs are opened with JavaScript disabled.
    nojs: bool,
    /// Shell command for `:te` (program + args), set via `:config`.
    term_command: Vec<String>,
    /// Search-engine URL template (`%s` = query) for a non-URL `:open`. Defaults
    /// to Google; change it with `:search <template>`.
    search_template: String,
    /// Monotonic id for routing PTY output to the right terminal tab.
    next_term_id: u64,
    /// Global UI zoom factor (1.0 = 100%). Scales native chrome, web content,
    /// and terminal font together.
    zoom: f64,
    /// Blink state for the command-bar cursor (toggled on a timer in Command mode).
    cursor_on: bool,
    quit: bool,
}

/// Tag this process with an explicit AppUserModelID so Windows (taskbar + Task
/// Manager) treats it and every process it spawns as one application. The id is
/// inherited by child processes, which is what collapses the WebView2 engine and
/// pty-host trees under the shell. Best-effort: any failure is ignored.
#[cfg(windows)]
fn set_app_user_model_id() {
    use windows::core::w;
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(w!("kayf.browser.Shell"));
    }
}

/// Put `text` on the system clipboard (best-effort; failures are ignored).
fn clipboard_set(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
}

/// Read UTF-8 text from the system clipboard, or `None` if unavailable/non-text.
fn clipboard_get() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
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

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("browser")
        .with_decorations(false) // no OS title bar; window control is command-driven
        .with_inner_size(tao::dpi::LogicalSize::new(1100.0, 740.0))
        .build(&event_loop)
        .context("creating window")?;
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
        status: String::new(),
        tabs: Vec::new(),
        active: None,
        modifiers: ModifiersState::default(),
        nojs: false,
        term_command: vec!["nu".to_string()],
        search_template: browser_core::DEFAULT_SEARCH_URL.to_string(),
        next_term_id: 0,
        zoom: 1.0,
        cursor_on: true,
        quit: false,
    };

    // Optional: open a page immediately, e.g. `browser-desktop youtube.com`,
    // or run a command, e.g. `browser-desktop ":nojs youtube.com"`.
    if let Some(target) = std::env::args().nth(1) {
        let t = target.trim_start();
        if let Some(cmd) = t.strip_prefix(':') {
            app.run_command(cmd);
        } else {
            app.open_tab(&target, false);
        }
    }

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

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // Command-bar cursor blink: the WaitUntil deadline (set below while in
            // Command mode) wakes us here to flip the cursor and repaint.
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                if app.mode == ModeKind::Command {
                    app.cursor_on = !app.cursor_on;
                    app.window.request_redraw();
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
                WindowEvent::ModifiersChanged(state) => app.modifiers = state,
                WindowEvent::KeyboardInput { event: key, .. } => {
                    if key.state == ElementState::Pressed {
                        app.handle_key(&key);
                        if app.quit {
                            app.teardown();
                            *control_flow = ControlFlow::Exit;
                        }
                    }
                }
                _ => {}
            },
            Event::UserEvent(UserEvent::ExitToNormal) => app.exit_to_normal(),
            Event::UserEvent(UserEvent::InsertToPassthrough) => {
                app.mode = ModeKind::Passthrough;
                app.set_page_mode("passthrough");
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::FocusShell) => {
                match app.mode {
                    // Passthrough persists across navigation: re-assert it on the new
                    // page and keep the page focused.
                    ModeKind::Passthrough => {
                        app.set_page_mode("passthrough");
                        if let Some(wv) = app.active_webview() {
                            let _ = wv.focus();
                        }
                    }
                    // Insert is temporary; a navigation ends the editing context.
                    ModeKind::Insert => {
                        app.mode = ModeKind::Normal;
                        app.window.set_focus();
                        app.window.request_redraw();
                    }
                    _ => app.window.set_focus(),
                }
                // A fresh navigation can reset the page's zoom factor — re-apply.
                app.apply_active_zoom();
                // Track the post-navigation URL in the status bar.
                app.refresh_active_url();
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::GrabFocus) => {
                // Only in Normal mode: the shell owns the keyboard there. In
                // Insert/Passthrough the page legitimately holds focus.
                if app.mode == ModeKind::Normal {
                    app.reclaim_shell_focus();
                }
            }
            Event::UserEvent(UserEvent::ExitHint) => {
                app.hint_input.clear();
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::HintEdit) => {
                // The hint selected a text field: enter insert (temporary typing),
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
            Event::UserEvent(UserEvent::ReadReady { url, title, html }) => {
                app.open_read_tab(&url, &title, &html);
            }
            Event::UserEvent(UserEvent::ReadFailed(e)) => {
                app.status = format!("read failed: {e}");
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::TermDone { cmd, output, code }) => {
                app.show_term_result(&cmd, &output, code);
            }
            Event::UserEvent(UserEvent::TermInput { id, data }) => {
                if let Some(s) = app.term_session_mut(id) {
                    s.send(0, data.as_bytes());
                }
            }
            Event::UserEvent(UserEvent::TermResize { id, cols, rows }) => {
                if let Some(s) = app.term_session_mut(id) {
                    let mut p = [0u8; 4];
                    p[0..2].copy_from_slice(&cols.to_le_bytes());
                    p[2..4].copy_from_slice(&rows.to_le_bytes());
                    s.send(1, &p);
                }
            }
            Event::UserEvent(UserEvent::TermOutput { id, data }) => app.feed_terminal(id, data),
            Event::UserEvent(UserEvent::TermReady { id }) => app.terminal_ready(id),
            Event::UserEvent(UserEvent::ZoomStep(steps)) => app.zoom_by(steps),
            Event::UserEvent(UserEvent::ZoomReset) => app.zoom_reset(),
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
        // While typing a command, keep waking to blink the cursor (unless we're
        // already exiting). Outside Command mode we stay on plain Wait.
        if !matches!(*control_flow, ControlFlow::Exit | ControlFlow::ExitWithCode(_))
            && app.mode == ModeKind::Command
        {
            *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
        }
    });
}

impl App {
    // --- geometry -------------------------------------------------------------

    fn inner(&self) -> (u32, u32) {
        let s = self.window.inner_size();
        (s.width.max(1), s.height.max(1))
    }

    /// Scale a base (zoom-1.0) pixel metric by the current zoom factor.
    fn scaled(&self, base: u32) -> u32 {
        (base as f64 * self.zoom).round().max(1.0) as u32
    }

    /// Command/status bar height at the current zoom.
    fn bar_h(&self) -> u32 {
        self.scaled(BAR_H)
    }

    /// Tab-bar height: present only while at least one tab is open.
    fn tab_bar_h(&self) -> u32 {
        if self.tabs.is_empty() {
            0
        } else {
            self.scaled(TAB_BAR_H)
        }
    }

    /// Bounds for a content webview: full width, between the tab bar and command bar.
    fn content_rect(&self) -> Rect {
        let (w, h) = self.inner();
        let top = self.tab_bar_h();
        Rect {
            position: PhysicalPosition::new(0_i32, top as i32).into(),
            size: PhysicalSize::new(w, h.saturating_sub(top + self.bar_h())).into(),
        }
    }

    fn on_resize(&mut self, _w: u32, _h: u32) {
        let rect = self.content_rect();
        if let Some(wv) = self.active_webview() {
            let _ = wv.set_bounds(rect);
        }
        self.window.request_redraw();
    }

    // --- zoom -----------------------------------------------------------------

    fn zoom_by(&mut self, steps: i32) {
        self.set_zoom(self.zoom + steps as f64 * ZOOM_STEP);
    }

    fn zoom_reset(&mut self) {
        self.set_zoom(1.0);
    }

    /// Set the global zoom and apply it to every layer at once: the native chrome
    /// font (painter), each web tab (WebView2 zoom factor), and each terminal tab
    /// (xterm font). Bar/tab-bar heights scale too, so the active webview is
    /// re-laid-out to fit between them.
    fn set_zoom(&mut self, factor: f64) {
        let z = ((factor.clamp(ZOOM_MIN, ZOOM_MAX)) * 100.0).round() / 100.0;
        self.zoom = z;
        self.painter.set_px(BASE_PX * z as f32);
        let term_px = (BASE_TERM_PX * z).round();
        for tab in &self.tabs {
            if tab.term.is_some() {
                let _ = tab
                    .webview
                    .evaluate_script(&format!("window.__setZoom&&window.__setZoom({term_px})"));
            } else {
                let _ = tab.webview.zoom(z);
            }
        }
        // Tab/command bars changed height → refit the visible page.
        let rect = self.content_rect();
        if let Some(wv) = self.active_webview() {
            let _ = wv.set_bounds(rect);
        }
        self.status = format!("zoom {}%", (z * 100.0).round() as i32);
        self.window.request_redraw();
    }

    /// Pull keyboard focus back to the shell window. After a click, the top-level
    /// window is still foreground (only the *child* webview HWND grabbed keyboard
    /// focus), and tao's `set_focus` is a no-op when already foreground — so we
    /// must `SetFocus` the parent HWND directly to take the keyboard off the child.
    #[cfg(windows)]
    fn reclaim_shell_focus(&self) {
        use tao::platform::windows::WindowExtWindows;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        let hwnd = self.window.hwnd();
        unsafe {
            let _ = SetFocus(Some(HWND(hwnd as *mut core::ffi::c_void)));
        }
    }

    #[cfg(not(windows))]
    fn reclaim_shell_focus(&self) {
        self.window.set_focus();
    }

    /// Re-assert the current zoom on the active web tab (e.g. after a navigation,
    /// which can reset the WebView2 zoom factor). No-op for terminal tabs.
    fn apply_active_zoom(&self) {
        if let Some(tab) = self.active.and_then(|i| self.tabs.get(i)) {
            if tab.term.is_none() {
                let _ = tab.webview.zoom(self.zoom);
            }
        }
    }

    // --- tab access -----------------------------------------------------------

    fn active_webview(&self) -> Option<&WebView> {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| &t.webview)
    }

    fn active_url(&self) -> Option<&str> {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.url.as_str())
    }

    /// The live URL of the active web tab (from WebView2, so it reflects in-page
    /// navigation), falling back to the stored URL. `None` for terminal tabs.
    fn current_url(&self) -> Option<String> {
        let tab = self.tabs.get(self.active?)?;
        if tab.term.is_some() {
            return None;
        }
        if let Ok(u) = tab.webview.url() {
            if u.starts_with("http") {
                return Some(u);
            }
        }
        Some(tab.url.clone())
    }

    /// Refresh the stored URL of the active web tab from its live WebView2 URL,
    /// so the status bar tracks navigation. Skips terminal/internal/html tabs
    /// (their live URL is a data:/about: URL, not a real address).
    fn refresh_active_url(&mut self) {
        if let Some(tab) = self.active.and_then(|i| self.tabs.get_mut(i)) {
            if tab.term.is_some() {
                return;
            }
            if let Ok(u) = tab.webview.url() {
                if u.starts_with("http") {
                    tab.url = u;
                }
            }
        }
    }

    // --- input ----------------------------------------------------------------

    fn handle_key(&mut self, key: &KeyEvent) {
        match self.mode {
            ModeKind::Command => self.key_command(key),
            ModeKind::Resize => self.key_resize(key),
            ModeKind::Move => self.key_move(key),
            ModeKind::Hint => self.key_hint(key),
            // In Insert/Passthrough the page normally has OS focus and the injected
            // bridge handles the shell keys; these arms are fallbacks for when the
            // shell still holds focus (e.g. right after entering the mode).
            ModeKind::Insert => {
                if matches!(key.logical_key, Key::Escape) && !self.modifiers.shift_key() {
                    self.exit_to_normal();
                } else if self.modifiers.control_key() && key.physical_key == KeyCode::KeyV {
                    self.enter_passthrough();
                }
            }
            ModeKind::Passthrough => {
                if matches!(key.logical_key, Key::Escape) && self.modifiers.shift_key() {
                    self.exit_to_normal();
                }
            }
            ModeKind::Normal => self.key_normal(key),
        }
        self.window.request_redraw();
    }

    fn key_normal(&mut self, key: &KeyEvent) {
        // Chords (with Ctrl) take precedence over plain keys.
        if self.modifiers.control_key() {
            match key.physical_key {
                KeyCode::KeyV => self.enter_passthrough(),
                // Browser-wide zoom (native chrome + web content + terminal).
                KeyCode::Equal => self.zoom_by(1),
                KeyCode::Minus => self.zoom_by(-1),
                KeyCode::Digit0 => self.zoom_reset(),
                _ => {}
            }
            return;
        }
        let (_, h) = self.inner();
        let page = (h as i32 - self.bar_h() as i32).max(40);
        match &key.logical_key {
            Key::Character(s) => match *s {
                ":" => self.enter_command(""),
                "o" => self.enter_command("open "),
                "j" => self.scroll(80),
                "k" => self.scroll(-80),
                "d" => self.scroll(page / 2),
                "u" => self.scroll(-page / 2),
                "i" => {
                    if self.active_is_term() {
                        self.enter_passthrough();
                    } else {
                        self.enter_insert();
                    }
                }
                "f" => self.enter_hint(),
                "x" => self.close_active(),
                "r" => {
                    if let Some(wv) = self.active_webview() {
                        let _ = wv.reload();
                    }
                }
                "H" => self.history(false),
                "L" => self.history(true),
                "n" => self.switch_tab(1),
                "p" => self.switch_tab(-1),
                "<" => self.move_tab(-1),
                ">" => self.move_tab(1),
                d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
                    let n = (d.as_bytes()[0] - b'0') as usize;
                    if n >= 1 {
                        self.jump_to(n - 1);
                    }
                }
                _ => {}
            },
            Key::ArrowDown => self.scroll(80),
            Key::ArrowUp => self.scroll(-80),
            _ => {}
        }
    }

    fn key_command(&mut self, key: &KeyEvent) {
        let shift = self.modifiers.shift_key();
        // Ctrl chords: clipboard, word movement/deletion, line editing. They take
        // precedence over text input. (Alt+Backspace is handled just below.)
        if self.modifiers.control_key() {
            match key.physical_key {
                // Clipboard. Ctrl+C copies the selection (or, with nothing selected,
                // cancels — the old behavior); Ctrl+X cuts; Ctrl+V pastes.
                KeyCode::KeyC => {
                    if let Some((a, b)) = self.sel_range() {
                        clipboard_set(&self.command[a..b]);
                    } else {
                        self.cancel_command();
                        return;
                    }
                }
                KeyCode::KeyX => {
                    if let Some((a, b)) = self.sel_range() {
                        clipboard_set(&self.command[a..b]);
                        self.delete_selection();
                    }
                }
                KeyCode::KeyV => {
                    if let Some(text) = clipboard_get() {
                        self.cmd_insert(&text);
                    }
                }
                KeyCode::KeyA => {
                    self.command_anchor = Some(0);
                    self.command_cursor = self.command.len();
                }
                // Word-wise caret movement (Ctrl+Shift extends the selection).
                KeyCode::ArrowLeft => {
                    let p = self.prev_word(self.command_cursor);
                    self.move_caret(p, shift);
                }
                KeyCode::ArrowRight => {
                    let p = self.next_word(self.command_cursor);
                    self.move_caret(p, shift);
                }
                // Ctrl+W / Ctrl+Backspace: delete the word before the caret.
                KeyCode::KeyW | KeyCode::Backspace => self.cmd_delete_word(),
                // Ctrl+Delete: delete the word after the caret.
                KeyCode::Delete => self.cmd_delete_word_forward(),
                // Ctrl+U: delete from the caret back to the start of the line.
                KeyCode::KeyU => {
                    self.command.replace_range(0..self.command_cursor, "");
                    self.command_cursor = 0;
                    self.command_anchor = None;
                }
                KeyCode::KeyH => self.cmd_backspace(),
                _ => {}
            }
            self.cursor_on = true;
            return;
        }
        // Alt+Backspace: delete the word before the caret (a common alias).
        if self.modifiers.alt_key() && key.physical_key == KeyCode::Backspace {
            self.cmd_delete_word();
            self.cursor_on = true;
            return;
        }
        match &key.logical_key {
            Key::Enter => {
                let line = std::mem::take(&mut self.command);
                self.command_cursor = 0;
                self.command_anchor = None;
                self.mode = ModeKind::Normal;
                self.run_command(&line);
            }
            Key::Escape => self.cancel_command(),
            Key::Backspace => {
                if !self.delete_selection() {
                    self.cmd_backspace();
                }
            }
            Key::Delete => {
                if !self.delete_selection() {
                    self.cmd_delete_forward();
                }
            }
            // Plain arrow with a selection collapses to that edge; otherwise moves a
            // character. Shift extends (or starts) the selection.
            Key::ArrowLeft => {
                if !shift {
                    if let Some((a, _)) = self.sel_range() {
                        self.command_cursor = a;
                        self.command_anchor = None;
                    } else {
                        self.move_caret(self.prev_char(self.command_cursor), false);
                    }
                } else {
                    self.move_caret(self.prev_char(self.command_cursor), true);
                }
            }
            Key::ArrowRight => {
                if !shift {
                    if let Some((_, b)) = self.sel_range() {
                        self.command_cursor = b;
                        self.command_anchor = None;
                    } else {
                        self.move_caret(self.next_char(self.command_cursor), false);
                    }
                } else {
                    self.move_caret(self.next_char(self.command_cursor), true);
                }
            }
            Key::Home => self.move_caret(0, shift),
            Key::End => self.move_caret(self.command.len(), shift),
            Key::Space => self.cmd_insert(" "),
            Key::Character(s) => self.cmd_insert(s),
            _ => {}
        }
        // Any edit should show the cursor immediately (don't wait for the blink).
        self.cursor_on = true;
    }

    /// Leave the command bar, discarding the line (Esc / Ctrl+C with no selection).
    fn cancel_command(&mut self) {
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        self.mode = ModeKind::Normal;
    }

    /// The current selection as an ordered byte range, or `None` if empty.
    fn sel_range(&self) -> Option<(usize, usize)> {
        let a = self.command_anchor?;
        let c = self.command_cursor;
        (a != c).then(|| (a.min(c), a.max(c)))
    }

    /// Move the caret to `pos`. `extend` keeps/starts a selection (Shift held);
    /// otherwise the selection is dropped. A zero-width selection is normalized away.
    fn move_caret(&mut self, pos: usize, extend: bool) {
        if extend {
            if self.command_anchor.is_none() {
                self.command_anchor = Some(self.command_cursor);
            }
        } else {
            self.command_anchor = None;
        }
        self.command_cursor = pos;
        if self.command_anchor == Some(pos) {
            self.command_anchor = None;
        }
    }

    /// Replace the selection (if any) with `text`, then place the caret after it.
    /// Control characters (e.g. newlines from a paste) are dropped — it's one line.
    fn cmd_insert(&mut self, text: &str) {
        self.delete_selection();
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        self.command.insert_str(self.command_cursor, &clean);
        self.command_cursor += clean.len();
    }

    /// Remove the selection if there is one; returns whether anything was deleted.
    fn delete_selection(&mut self) -> bool {
        if let Some((a, b)) = self.sel_range() {
            self.command.replace_range(a..b, "");
            self.command_cursor = a;
            self.command_anchor = None;
            true
        } else {
            self.command_anchor = None;
            false
        }
    }

    /// Byte offset of the char before `pos` (or `pos` if at the start).
    fn prev_char(&self, pos: usize) -> usize {
        self.command[..pos].char_indices().next_back().map(|(i, _)| i).unwrap_or(pos)
    }

    /// Byte offset just after the char at `pos` (or `pos` if at the end).
    fn next_char(&self, pos: usize) -> usize {
        self.command[pos..].chars().next().map(|c| pos + c.len_utf8()).unwrap_or(pos)
    }

    /// Start of the word before `pos`: skip trailing whitespace, then the word.
    fn prev_word(&self, pos: usize) -> usize {
        let trimmed = self.command[..pos].trim_end_matches(char::is_whitespace);
        trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0)
    }

    /// End of the word after `pos`: skip leading whitespace, then the word.
    fn next_word(&self, pos: usize) -> usize {
        let rest = &self.command[pos..];
        let after_ws = rest.trim_start_matches(char::is_whitespace);
        let ws = rest.len() - after_ws.len();
        let word = after_ws.find(char::is_whitespace).unwrap_or(after_ws.len());
        pos + ws + word
    }

    /// Delete the character before the caret (or the selection, if any).
    fn cmd_backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.prev_char(self.command_cursor);
        if start != self.command_cursor {
            self.command.replace_range(start..self.command_cursor, "");
            self.command_cursor = start;
        }
    }

    /// Delete the character after the caret (or the selection, if any).
    fn cmd_delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.next_char(self.command_cursor);
        self.command.replace_range(self.command_cursor..end, "");
    }

    /// Delete the word before the caret (or the selection, if any).
    fn cmd_delete_word(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.prev_word(self.command_cursor);
        self.command.replace_range(start..self.command_cursor, "");
        self.command_cursor = start;
    }

    /// Delete the word after the caret (or the selection, if any).
    fn cmd_delete_word_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.next_word(self.command_cursor);
        self.command.replace_range(self.command_cursor..end, "");
    }

    fn enter_command(&mut self, prefill: &str) {
        self.mode = ModeKind::Command;
        self.command = prefill.to_string();
        self.command_cursor = self.command.len();
        self.command_anchor = None;
        self.cursor_on = true;
        self.status.clear();
    }

    fn enter_insert(&mut self) {
        if self.active_webview().is_none() {
            self.status = "no page — open one first".into();
            return;
        }
        self.mode = ModeKind::Insert;
        self.set_page_mode("insert");
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus();
        }
    }

    fn enter_passthrough(&mut self) {
        if self.active_webview().is_none() {
            self.status = "no page — open one first".into();
            return;
        }
        self.mode = ModeKind::Passthrough;
        self.set_page_mode("passthrough");
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus();
        }
    }

    /// Push the current mode name into the page so the injected bridge knows which
    /// keys to intercept. Called whenever the mode changes.
    fn set_page_mode(&self, mode: &str) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script(&format!("window.__mode={mode:?}"));
        }
    }

    fn exit_to_normal(&mut self) {
        self.set_page_mode("normal");
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus_parent();
        }
        self.mode = ModeKind::Normal;
        self.window.set_focus();
        self.window.request_redraw();
    }

    // --- hint mode ------------------------------------------------------------

    fn enter_hint(&mut self) {
        let Some(idx) = self.active else {
            self.status = "no page — open one first".into();
            return;
        };
        if self.tabs[idx].nojs {
            self.status = "hint mode needs JavaScript (this tab is no-js)".into();
            return;
        }
        self.hint_input.clear();
        self.mode = ModeKind::Hint;
        let _ = self.tabs[idx].webview.evaluate_script(HINT_JS);
    }

    fn key_hint(&mut self, key: &KeyEvent) {
        match &key.logical_key {
            Key::Escape => self.exit_hint(),
            Key::Backspace => {
                self.hint_input.pop();
                self.hint_send();
            }
            Key::Character(s) => {
                let c = *s;
                if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphabetic()) {
                    self.hint_input.push_str(&c.to_lowercase());
                    self.hint_send();
                }
            }
            _ => {}
        }
    }

    /// Forward the current label string to the page to filter/activate hints.
    fn hint_send(&self) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script(&format!(
                "window.__hintInput&&window.__hintInput({:?})",
                self.hint_input
            ));
        }
    }

    fn exit_hint(&mut self) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script("window.__hintClear&&window.__hintClear()");
        }
        self.hint_input.clear();
        self.mode = ModeKind::Normal;
        self.window.request_redraw();
    }

    // --- window control (resize / move / fullscreen) --------------------------

    fn key_resize(&mut self, key: &KeyEvent) {
        const STEP: i32 = 40;
        match &key.logical_key {
            Key::Escape | Key::Enter => self.mode = ModeKind::Normal,
            Key::Character(s) => match *s {
                "h" => self.resize_window(-STEP, 0),
                "l" => self.resize_window(STEP, 0),
                "j" => self.resize_window(0, STEP),
                "k" => self.resize_window(0, -STEP),
                _ => {}
            },
            _ => {}
        }
    }

    fn key_move(&mut self, key: &KeyEvent) {
        const STEP: i32 = 40;
        match &key.logical_key {
            Key::Escape | Key::Enter => self.mode = ModeKind::Normal,
            Key::Character(s) => match *s {
                "h" => self.move_window(-STEP, 0),
                "l" => self.move_window(STEP, 0),
                "j" => self.move_window(0, STEP),
                "k" => self.move_window(0, -STEP),
                _ => {}
            },
            _ => {}
        }
    }

    fn resize_window(&self, dw: i32, dh: i32) {
        let s = self.window.inner_size();
        let w = (s.width as i32 + dw).max(240) as u32;
        let h = (s.height as i32 + dh).max(160) as u32;
        self.window.set_inner_size(tao::dpi::PhysicalSize::new(w, h));
    }

    fn move_window(&self, dx: i32, dy: i32) {
        if let Ok(p) = self.window.outer_position() {
            self.window
                .set_outer_position(tao::dpi::PhysicalPosition::new(p.x + dx, p.y + dy));
        }
    }

    fn toggle_fullscreen(&self) {
        use tao::window::Fullscreen;
        if self.window.fullscreen().is_some() {
            self.window.set_fullscreen(None);
        } else {
            self.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
    }

    // --- commands -------------------------------------------------------------

    fn run_command(&mut self, line: &str) {
        let line = line.trim();
        let (verb, rest) = match line.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (line, ""),
        };
        match verb {
            "open" | "o" | "tabopen" | "t" => self.open_tab(rest, self.nojs),
            // Edit the current URL: drop into the command bar pre-filled with
            // `<verb> <current url>` so you can tweak and re-open it — `<verb>` is the
            // mode the tab was opened in (open/research/read/nojs) so e.g. editing a
            // `:research` tab re-opens with `:research`, not `:open`. Reads the LIVE
            // document URL (so in-page/SPA navigation gives the real address).
            "edit" | "e" => match self.current_url() {
                Some(url) => {
                    let verb = self.active_reopen_verb();
                    self.enter_command(&format!("{verb} {url}"));
                }
                None => self.status = "no page to edit".into(),
            },
            // Yank (copy) the current URL to the system clipboard.
            "y" | "yank" => match self.current_url() {
                Some(url) => {
                    clipboard_set(&url);
                    self.status = format!("yanked {url}");
                }
                None => self.status = "no url to yank".into(),
            },
            "read" => {
                if rest.is_empty() {
                    self.status = "usage: :read <url>".into();
                } else {
                    self.start_read(rest);
                }
            }
            // Like :open (URL or → search engine) but lighter: JS on, images kept,
            // heavy media/embeds stripped. For "how do I…" / "best way to…" lookups.
            "research" | "rs" => self.open_research(rest),
            "te" | "term" => {
                if rest.is_empty() {
                    self.open_terminal();
                } else {
                    self.run_term(rest);
                }
            }
            "shell" => {
                if rest.is_empty() {
                    self.status = format!("shell = {}", self.term_command.join(" "));
                } else {
                    self.term_command = rest.split_whitespace().map(String::from).collect();
                    self.status = format!("shell set to: {}", self.term_command.join(" "));
                }
            }
            // Customize the search engine used when `:open <query>` isn't a URL.
            // `%s` in the template is replaced with the percent-encoded query.
            "search" => {
                if rest.is_empty() {
                    self.status = format!("search = {}", self.search_template);
                } else {
                    self.search_template = rest.to_string();
                    self.status = format!("search engine set to: {}", self.search_template);
                }
            }
            "nojs" => {
                if rest.is_empty() {
                    self.nojs = !self.nojs;
                    self.status =
                        format!("new tabs: JavaScript {}", if self.nojs { "OFF" } else { "ON" });
                } else {
                    self.open_tab(rest, true);
                }
            }
            "close" | "tabclose" | "bd" => self.close_active(),
            "quit" | "q" => self.quit = true,
            "reload" | "r" => {
                if let Some(wv) = self.active_webview() {
                    let _ = wv.reload();
                }
            }
            "next" | "tabnext" | "tn" => self.switch_tab(1),
            "prev" | "tabprev" | "tp" => self.switch_tab(-1),
            "back" => self.history(false),
            "forward" => self.history(true),
            "f" | "fullscreen" => self.toggle_fullscreen(),
            "resize" => {
                self.mode = ModeKind::Resize;
                self.status.clear();
            }
            "move" => {
                self.mode = ModeKind::Move;
                self.status.clear();
            }
            "commands" | "help" => self.open_local_page("commands", commands_document()),
            "version" => self.open_local_page("version", version_document()),
            "" => {}
            other => self.status = format!("unknown command: {other}"),
        }
    }

    /// Turn a command-bar target into a URL the way `:open` does: a bare query
    /// (spaces, or no scheme/dot like `rustlang`) — or anything that won't parse as
    /// a URL — goes to the configured search engine; a real address opens directly.
    fn resolve_target(&self, target: &str) -> String {
        if browser_core::looks_like_query(target) {
            browser_core::search_url(&self.search_template, target)
        } else {
            browser_core::normalize_url(target)
                .unwrap_or_else(|| browser_core::search_url(&self.search_template, target))
        }
    }

    fn open_tab(&mut self, target: &str, disable_js: bool) {
        let url = self.resolve_target(target);
        match self.build_content_webview(Source::Url(url.clone()), disable_js, "") {
            Ok(webview) => {
                self.tabs.push(Tab {
                    webview,
                    url: url.clone(),
                    nojs: disable_js,
                    read: false,
                    research: false,
                    term: None,
                });
                self.active = Some(self.tabs.len() - 1);
                self.refresh_visibility();
                // Keep the keyboard on the shell; the page-load handler re-asserts
                // this once navigation finishes (which is when focus tends to move).
                self.window.set_focus();
                self.status = if disable_js { "(no-js)".into() } else { String::new() };
            }
            Err(e) => self.status = format!("failed to open: {e:#}"),
        }
    }

    /// Open a "research" tab: like `:open` (URL or → search engine), but JS-on with
    /// the [`RESEARCH_JS`] pruner injected so video/audio/embeds are stripped while
    /// images and text stay. A lighter browse for "how do I…" lookups.
    fn open_research(&mut self, target: &str) {
        let url = self.resolve_target(target);
        match self.build_content_webview(Source::Url(url.clone()), false, RESEARCH_JS) {
            Ok(webview) => {
                self.tabs.push(Tab {
                    webview,
                    url: url.clone(),
                    nojs: false,
                    read: false,
                    research: true,
                    term: None,
                });
                self.active = Some(self.tabs.len() - 1);
                self.refresh_visibility();
                self.window.set_focus();
                self.status = "(research — media stripped)".into();
            }
            Err(e) => self.status = format!("failed to open: {e:#}"),
        }
    }

    /// Build a child webview from either a URL or an inline HTML document, with
    /// the full shell bridge (keybindings, focus reclaim, hint mode).
    fn build_content_webview(
        &self,
        source: Source,
        disable_js: bool,
        extra_init: &str,
    ) -> Result<WebView> {
        let ipc_proxy = self.proxy.clone();
        let load_proxy = self.proxy.clone();
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
            // Browser process flags. This overrides wry's default arg string, so we
            // re-include its defaults (mini-menu / PDF UI / SmartScreen off, plus
            // gesture-free autoplay) and add `Translate,msAutoTranslate` to kill the
            // "translate this page?" bar that Edge pops on foreign-language pages.
            .with_additional_browser_args(
                "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,Translate,\
                 msAutoTranslate --autoplay-policy=no-user-gesture-required",
            )
            // The shell bridge always loads; `extra_init` (e.g. research-mode DOM
            // pruning) is appended so it runs in the same document-create pass.
            .with_initialization_script(if extra_init.is_empty() {
                BRIDGE_JS.to_string()
            } else {
                format!("{BRIDGE_JS}\n{extra_init}")
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
                "hint-exit" => {
                    let _ = ipc_proxy.send_event(UserEvent::ExitHint);
                }
                "hint-edit" => {
                    let _ = ipc_proxy.send_event(UserEvent::HintEdit);
                }
                _ => {}
            })
            // Backup focus reclaim for non-JS tabs (page-ready covers JS tabs).
            .with_on_page_load_handler(move |event, _url| {
                if matches!(event, PageLoadEvent::Finished) {
                    let _ = load_proxy.send_event(UserEvent::FocusShell);
                }
            });
        if disable_js {
            builder = builder.with_javascript_disabled();
        }
        builder
            .build_as_child(&*self.window)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Kick off a background readability extraction; the result arrives as a
    /// ReadReady/ReadFailed user event so the UI stays responsive.
    fn start_read(&mut self, target: &str) {
        self.status = format!("reading {target} …");
        let proxy = self.proxy.clone();
        let target = target.to_string();
        std::thread::spawn(move || {
            let event = match browser_backend_text::fetch_readable_blocking(&target) {
                Ok(r) => UserEvent::ReadReady { url: r.url, title: r.title, html: r.html },
                Err(e) => UserEvent::ReadFailed(format!("{e:#}")),
            };
            let _ = proxy.send_event(event);
        });
        self.window.request_redraw();
    }

    /// Run a local shell command in the background. Result arrives as TermDone.
    /// Strictly shell-initiated — never reachable from page content.
    fn run_term(&mut self, cmd: &str) {
        self.status = format!("$ {cmd}");
        let proxy = self.proxy.clone();
        let cmd = cmd.to_string();
        std::thread::spawn(move || {
            let (output, code) = exec_command(&cmd);
            let _ = proxy.send_event(UserEvent::TermDone { cmd, output, code });
        });
        self.window.request_redraw();
    }

    fn active_is_term(&self) -> bool {
        self.active
            .and_then(|i| self.tabs.get(i))
            .map(|t| t.term.is_some())
            .unwrap_or(false)
    }

    /// Open an embedded terminal tab. The ConPTY + shell run in a companion
    /// `browser-pty-host` process (so they can't deadlock our exit); we bridge
    /// keystrokes/resize to its stdin and its stdout (PTY output) back to xterm.
    fn open_terminal(&mut self) {
        let shell = if self.term_command.is_empty() {
            vec!["cmd".to_string()]
        } else {
            self.term_command.clone()
        };

        let Some(host) = pty_host_path() else {
            self.status = "could not locate browser-pty-host".into();
            return;
        };

        let mut command = Command::new(&host);
        command
            .arg("80")
            .arg("24")
            .args(&shell)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // NOTE: do NOT pass CREATE_NO_WINDOW here. A console-less host can fail
        // to back its ConPTY (the shell starts but no output flows — a terminal
        // stuck at a blinking cursor). The console popup is suppressed instead by
        // building browser-pty-host as a GUI-subsystem binary.
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.status = format!("failed to start pty-host: {e}");
                return;
            }
        };

        // Confine the pty-host (and its conhost + shell) to a kill-on-close job so
        // closing the handle — or the browser dying — reaps the whole tree.
        #[cfg(windows)]
        let job = {
            use std::os::windows::io::AsRawHandle;
            job::create_for(child.as_raw_handle() as isize)
        };
        #[cfg(not(windows))]
        let job = 0isize;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let id = self.next_term_id;
        self.next_term_id += 1;

        let webview = match self.build_terminal_webview(id) {
            Ok(wv) => wv,
            Err(e) => {
                self.status = format!("terminal webview: {e:#}");
                let _ = child.kill();
                return;
            }
        };

        // Pump the pty-host's stdout (raw PTY output) to the UI thread → xterm.
        let proxy = self.proxy.clone();
        let reader_handle = std::thread::spawn(move || {
            let mut stdout = stdout;
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = proxy.send_event(UserEvent::TermClosed { id });
                        break;
                    }
                    Ok(n) => {
                        let data = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                        if proxy.send_event(UserEvent::TermOutput { id, data }).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        self.tabs.push(Tab {
            webview,
            url: format!("term: {}", shell[0]),
            nojs: false,
            read: false,
            research: false,
            term: Some(TermSession {
                id,
                child,
                stdin,
                job,
                reader: Some(reader_handle),
                ready: false,
                pending: Vec::new(),
            }),
        });
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.mode = ModeKind::Passthrough;
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus();
        }
        self.status = "terminal — Shift+Esc returns to the shell".into();
    }

    /// Build the terminal webview. The PTY handles live in the [`TermSession`]; the
    /// page only relays keystrokes/resizes/leave via IPC events (tagged with `id`).
    fn build_terminal_webview(&self, id: u64) -> Result<WebView> {
        let proxy = self.proxy.clone();
        WebViewBuilder::new()
            .with_html(terminal_page())
            .with_bounds(self.content_rect())
            .with_focused(false)
            .with_browser_accelerator_keys(false)
            .with_ipc_handler(move |req| {
                let body = req.body().as_str();
                // Exact-match control messages MUST be checked before the prefix
                // messages: "ready" starts with 'r', so the resize ('r') branch
                // would otherwise swallow it and the terminal would never flush.
                match body {
                    "ready" => {
                        let _ = proxy.send_event(UserEvent::TermReady { id });
                    }
                    "zoom+" => {
                        let _ = proxy.send_event(UserEvent::ZoomStep(1));
                    }
                    "zoom-" => {
                        let _ = proxy.send_event(UserEvent::ZoomStep(-1));
                    }
                    "zoom0" => {
                        let _ = proxy.send_event(UserEvent::ZoomReset);
                    }
                    "leave-passthrough" => {
                        let _ = proxy.send_event(UserEvent::ExitToNormal);
                    }
                    _ if body.starts_with('i') => {
                        let _ = proxy
                            .send_event(UserEvent::TermInput { id, data: body[1..].to_string() });
                    }
                    _ if body.starts_with('r') => {
                        if let Some((c, r)) = body[1..].split_once(',') {
                            if let (Ok(cols), Ok(rows)) = (c.parse::<u16>(), r.parse::<u16>()) {
                                let _ = proxy.send_event(UserEvent::TermResize { id, cols, rows });
                            }
                        }
                    }
                    _ => {}
                }
            })
            .build_as_child(&*self.window)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    fn term_session_mut(&mut self, id: u64) -> Option<&mut TermSession> {
        self.tabs.iter_mut().find_map(|t| t.term.as_mut().filter(|s| s.id == id))
    }

    /// Feed a chunk of PTY output (base64) to the terminal's xterm. Before the
    /// page reports ready, `window.__feed` doesn't exist yet, so buffer instead
    /// of dropping (dropping the early `ESC[6n` would stall the shell forever).
    fn feed_terminal(&mut self, id: u64, data: String) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.term.as_ref().map(|s| s.id) == Some(id))
        else {
            return;
        };
        let Some(session) = tab.term.as_mut() else { return };
        if session.ready {
            let _ = tab.webview.evaluate_script(&format!("window.__feed(\"{data}\")"));
        } else {
            session.pending.push(data);
        }
    }

    /// The xterm page finished initializing `window.__feed`: mark it ready and
    /// flush everything buffered while it was loading (in arrival order).
    fn terminal_ready(&mut self, id: u64) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.term.as_ref().map(|s| s.id) == Some(id))
        else {
            return;
        };
        let Some(session) = tab.term.as_mut() else { return };
        session.ready = true;
        let pending = std::mem::take(&mut session.pending);
        for data in pending {
            let _ = tab.webview.evaluate_script(&format!("window.__feed(\"{data}\")"));
        }
        // Adopt the current global zoom (the page starts at 100%).
        if (self.zoom - 1.0).abs() > f64::EPSILON {
            let term_px = (BASE_TERM_PX * self.zoom).round();
            if let Some(tab) =
                self.tabs.iter().find(|t| t.term.as_ref().map(|s| s.id) == Some(id))
            {
                let _ = tab.webview.evaluate_script(&format!("window.__setZoom({term_px})"));
            }
        }
    }

    /// Present a finished command vim-style: the result replaces the command-bar
    /// text (collapsed to one line).
    fn show_term_result(&mut self, _cmd: &str, output: &str, code: Option<i32>) {
        let trimmed = output.trim();
        self.status = if trimmed.is_empty() {
            let codestr = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            format!("(exit {codestr})")
        } else {
            trimmed.replace(['\r', '\n'], " ")
        };
        self.window.request_redraw();
    }

    /// Open a reader tab from already-extracted article HTML.
    fn open_read_tab(&mut self, url: &str, title: &str, article_html: &str) {
        let doc = read_document(url, title, article_html);
        // The article carries a strict CSP (no scripts/images/media/fonts) and the
        // reader CSS hides any media, so a read tab is truly text-only. Engine JS
        // stays on solely for our host-injected bridge (scroll/focus), which runs
        // regardless of page CSP — there are no *page* scripts left to execute.
        match self.build_content_webview(Source::Html(doc), false, "") {
            Ok(webview) => {
                self.tabs.push(Tab {
                    webview,
                    url: url.to_string(),
                    nojs: false,
                    read: true,
                    research: false,
                    term: None,
                });
                self.active = Some(self.tabs.len() - 1);
                self.refresh_visibility();
                self.window.set_focus();
                self.status.clear();
            }
            Err(e) => self.status = format!("read failed: {e:#}"),
        }
    }

    /// Open an internal HTML page (e.g. `:commands`, `:version`) in a new tab.
    fn open_local_page(&mut self, label: &str, html: String) {
        match self.build_content_webview(Source::Html(html), false, "") {
            Ok(webview) => {
                self.tabs.push(Tab {
                    webview,
                    url: format!("browser://{label}"),
                    nojs: false,
                    read: false,
                    research: false,
                    term: None,
                });
                self.active = Some(self.tabs.len() - 1);
                self.refresh_visibility();
                self.window.set_focus();
                self.status.clear();
            }
            Err(e) => self.status = format!("failed to open {label}: {e:#}"),
        }
    }

    fn close_active(&mut self) {
        let Some(i) = self.active else {
            self.status = "no tab to close".into();
            return;
        };
        // Shut a terminal down deterministically (kill shell, close PTY, join reader)
        // before dropping the tab; dropping the WebView frees the renderer.
        if let Some(session) = self.tabs[i].term.take() {
            session.shutdown();
        }
        let _ = self.tabs.remove(i);
        self.active = if self.tabs.is_empty() {
            None
        } else {
            Some(i.min(self.tabs.len() - 1))
        };
        self.refresh_visibility();
        self.window.set_focus();
    }

    /// Close the tab whose terminal has the given id (its shell exited). Behaves
    /// like `x`, but only disturbs focus/mode if that tab was the active one.
    fn close_term_tab(&mut self, id: u64) {
        let Some(i) = self.tabs.iter().position(|t| t.term.as_ref().map(|s| s.id) == Some(id))
        else {
            return;
        };
        let was_active = self.active == Some(i);
        if let Some(session) = self.tabs[i].term.take() {
            session.shutdown();
        }
        self.tabs.remove(i);
        self.active = if self.tabs.is_empty() {
            None
        } else {
            let a = self.active.unwrap_or(0);
            Some(if a > i { a - 1 } else { a.min(self.tabs.len() - 1) })
        };
        if was_active {
            self.mode = ModeKind::Normal;
            self.window.set_focus();
        }
        self.refresh_visibility();
    }

    fn switch_tab(&mut self, delta: i32) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as i32;
        let cur = self.active.unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(n) as usize;
        self.active = Some(next);
        self.refresh_visibility();
        self.window.set_focus();
    }

    /// Jump directly to a zero-based tab index (bound to keys 1..9).
    fn jump_to(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = Some(index);
            self.refresh_visibility();
            self.window.set_focus();
        }
    }

    /// Move the active tab one position left (-1) or right (+1).
    fn move_tab(&mut self, delta: i32) {
        let Some(i) = self.active else { return };
        let j = i as i32 + delta;
        if j < 0 || j as usize >= self.tabs.len() {
            return;
        }
        let j = j as usize;
        self.tabs.swap(i, j);
        self.active = Some(j);
        self.refresh_visibility();
    }

    fn refresh_visibility(&mut self) {
        let rect = self.content_rect();
        for (i, tab) in self.tabs.iter().enumerate() {
            let visible = Some(i) == self.active;
            let _ = tab.webview.set_visible(visible);
            if visible {
                let _ = tab.webview.set_bounds(rect);
                if tab.term.is_some() {
                    // Re-fit xterm to the new size; it reports back to resize the PTY.
                    let _ = tab.webview.evaluate_script("window.__fit&&window.__fit()");
                }
            }
        }
        self.window.request_redraw();
    }

    /// Shut down terminals (kill shells, close PTYs, join readers) and drop every
    /// webview before exiting — so WebView2 processes and ConPTYs close cleanly
    /// rather than leaving a stuck thread that deadlocks process teardown.
    fn teardown(&mut self) {
        for tab in &mut self.tabs {
            if let Some(session) = tab.term.take() {
                session.shutdown();
            }
        }
        self.tabs.clear();
        self.active = None;
    }

    fn scroll(&mut self, dy: i32) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script(&format!("window.scrollBy(0,{dy});"));
        }
    }

    fn history(&mut self, forward: bool) {
        if let Some(wv) = self.active_webview() {
            let js = if forward { "history.forward();" } else { "history.back();" };
            let _ = wv.evaluate_script(js);
        }
    }

    // --- rendering ------------------------------------------------------------

    fn draw(&mut self) -> Result<()> {
        let (w, h) = self.inner();
        // Gather all dynamic text + zoom-scaled metrics up front, while we can
        // still borrow &self.
        let tab_labels = self.tab_labels();
        let welcome = self.active.is_none();
        let bar_h = self.bar_h() as usize;
        let tab_h = self.tab_bar_h() as usize;
        // Command bar: draw the `:`-line with horizontal scroll-to-caret so editing
        // a long URL (e.g. after `:edit`, caret parked at the end) keeps the caret —
        // and the tail of the URL — visible. `cmd` is Some((line, scroll_px)) in
        // Command mode and replaces the normal segment list; `caret` is the lit
        // block cursor (x, width) already shifted by the same scroll.
        const MARGIN: i32 = 8;
        let cw = ((self.zoom * 2.0).round() as i32).max(2);
        let (segments, cmd, caret, sel) = if self.mode == ModeKind::Command {
            let line = format!(":{}", self.command);
            let prefix = format!(":{}", &self.command[..self.command_cursor]);
            let caret_un = MARGIN + self.painter.measure(&prefix) as i32;
            // Keep the caret a hair inside the right edge; scroll only when it would
            // otherwise fall off the end of the bar.
            let right_bound = (w as i32 - cw - 2).max(MARGIN);
            let scroll = (caret_un - right_bound).max(0);
            let caret = if self.cursor_on {
                Some(((caret_un - scroll) as usize, cw as usize))
            } else {
                None
            };
            // Selection highlight rect (x range), in the same scrolled coordinates,
            // clipped to the left margin.
            let sel = self.sel_range().map(|(a, b)| {
                let x_of = |k: usize| {
                    MARGIN - scroll + self.painter.measure(&format!(":{}", &self.command[..k])) as i32
                };
                (x_of(a).max(MARGIN).max(0) as usize, x_of(b).max(0) as usize)
            });
            (Vec::new(), Some((line, scroll)), caret, sel)
        } else {
            (self.bar_segments(), None, None, None)
        };

        self.surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .map_err(|e| anyhow::anyhow!("resize: {e}"))?;
        let mut buf = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("buffer: {e}"))?;

        // `p` is a disjoint field borrow from `self.surface`, so it coexists with `buf`.
        let p = &self.painter;
        let (wz, hz) = (w as usize, h as usize);
        let bar_top = hz.saturating_sub(bar_h);

        let baseline = bar_top + (bar_h * 2 / 3);
        // Draw the opaque command bar; called LAST so nothing bleeds through it.
        let draw_bar = |buf: &mut [u32]| {
            draw::fill_band(buf, wz, hz, bar_top, hz, draw::BAR_BG);
            if let Some((text, scroll)) = &cmd {
                // Selection highlight first, so the text paints on top of it.
                if let Some((sx0, sx1)) = sel {
                    let lh = p.line_height();
                    let y0 = baseline.saturating_sub(lh * 3 / 4);
                    let y1 = (baseline + lh / 6).min(hz);
                    draw::fill_rect(buf, wz, hz, sx0, y0, sx1, y1, draw::SEL);
                }
                // Command line, scrolled left by `scroll` px; clip at the left
                // margin so scrolled-off text doesn't bleed into the edge.
                p.text_clipped(buf, wz, hz, MARGIN - *scroll, baseline, text, draw::BAR_FG, MARGIN);
            } else {
                let mut x = 8;
                for (text, color) in &segments {
                    x = p.text(buf, wz, hz, x, baseline, text, *color) + 6;
                }
            }
            if let Some((cx, cw)) = caret {
                let lh = p.line_height();
                let y0 = baseline.saturating_sub(lh * 3 / 4);
                let y1 = (baseline + lh / 6).min(hz);
                draw::fill_rect(buf, wz, hz, cx, y0, cx + cw, y1, draw::BAR_FG);
            }
        };

        if welcome {
            // No engine running: paint the welcome screen, THEN the bar on top so a
            // long welcome list can't bleed into the command bar.
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, draw::BG);
            draw_welcome(p, &mut buf, wz, hz, self.zoom as f32);
            draw_bar(&mut buf);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else {
            draw_bar(&mut buf);
            // A webview covers the middle; redraw only the top tab bar and the
            // bottom command bar so we never paint over the live page.
            draw::fill_band(&mut buf, wz, hz, 0, tab_h, draw::BAR_BG);
            draw_tab_bar(p, &mut buf, wz, tab_h, &tab_labels);
            let top = softbuffer::Rect {
                x: 0,
                y: 0,
                width: NonZeroU32::new(w).unwrap(),
                height: NonZeroU32::new(tab_h.max(1) as u32).unwrap(),
            };
            let bottom = softbuffer::Rect {
                x: 0,
                y: bar_top as u32,
                width: NonZeroU32::new(w).unwrap(),
                height: NonZeroU32::new(bar_h.max(1) as u32).unwrap(),
            };
            buf.present_with_damage(&[top, bottom])
                .map_err(|e| anyhow::anyhow!("present: {e}"))?;
        }
        Ok(())
    }

    /// (label, is_active, color) for each open tab, in order.
    fn tab_labels(&self) -> Vec<(String, bool, draw::Rgb)> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let active = Some(i) == self.active;
                let color = if t.term.is_some() {
                    draw::TERM
                } else if t.read {
                    draw::READ
                } else if t.research {
                    draw::RESEARCH
                } else if active {
                    draw::ACCENT
                } else {
                    draw::DIM
                };
                (short_label(&t.url), active, color)
            })
            .collect()
    }

    /// Whether the active tab is a read-mode tab.
    fn active_is_read(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.read).unwrap_or(false)
    }

    /// Whether the active tab is a research-mode tab.
    fn active_is_research(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.research).unwrap_or(false)
    }

    /// The command verb that re-opens the active tab in its own mode, for `:edit`.
    fn active_reopen_verb(&self) -> &'static str {
        match self.active.and_then(|i| self.tabs.get(i)) {
            Some(t) if t.research => "research",
            Some(t) if t.read => "read",
            Some(t) if t.nojs => "nojs",
            _ => "open",
        }
    }

    /// Build the bar as a sequence of (text, color) segments drawn left to right.
    fn bar_segments(&self) -> Vec<(String, draw::Rgb)> {
        match self.mode {
            // The blinking caret is drawn separately (at the byte cursor), so the
            // text segment is just the literal command line.
            ModeKind::Command => vec![(format!(":{}", self.command), draw::BAR_FG)],
            ModeKind::Resize => vec![
                ("[RESIZE]".into(), draw::ACCENT),
                ("  hjkl resize window · Esc done".into(), draw::DIM),
            ],
            ModeKind::Move => vec![
                ("[MOVE]".into(), draw::ACCENT),
                ("  hjkl move window · Esc done".into(), draw::DIM),
            ],
            ModeKind::Hint => vec![
                ("[HINT]".into(), draw::ACCENT),
                (format!(" {}", self.hint_input), draw::BAR_FG),
                ("   type a label · Esc cancel".into(), draw::DIM),
            ],
            ModeKind::Insert => {
                let url = self.active_url().unwrap_or("").to_string();
                vec![
                    ("[INSERT]".into(), draw::ACCENT),
                    (url, draw::BAR_FG),
                    ("   (Esc normal · Ctrl+V passthrough)".into(), draw::DIM),
                ]
            }
            ModeKind::Passthrough => {
                let url = self.active_url().unwrap_or("").to_string();
                vec![
                    ("[PASS]".into(), draw::ACCENT),
                    (url, draw::BAR_FG),
                    ("   (Shift+Esc to exit)".into(), draw::DIM),
                ]
            }
            ModeKind::Normal => {
                let label = match self.active_url() {
                    Some(url) => {
                        let n = self.tabs.len();
                        let i = self.active.map(|i| i + 1).unwrap_or(0);
                        format!("{i}/{n}  {url}")
                    }
                    None => ":open <url>  (or press o)".to_string(),
                };
                let mut segs = vec![("[N]".into(), draw::ACCENT), (label, draw::BAR_FG)];
                if self.active_is_read() {
                    segs.push(("   [read]".into(), draw::READ));
                }
                if self.active_is_research() {
                    segs.push(("   [research]".into(), draw::RESEARCH));
                }
                if self.active_is_term() {
                    segs.push(("   [term]".into(), draw::TERM));
                }
                if self.nojs {
                    segs.push(("   [no-js]".into(), draw::ACCENT));
                }
                if !self.status.is_empty() {
                    segs.push((format!("   {}", self.status), draw::DIM));
                }
                segs
            }
        }
    }
}

/// Cap captured command output so a runaway command can't balloon memory.
const TERM_OUTPUT_CAP: usize = 200_000;

/// Run `cmd` through the platform shell, returning combined stdout+stderr and the
/// exit code. Blocking — call from a background thread.
fn exec_command(cmd: &str) -> (String, Option<i32>) {
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    match command.output() {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&err);
            }
            if s.len() > TERM_OUTPUT_CAP {
                s.truncate(TERM_OUTPUT_CAP);
                s.push_str("\n… (output truncated)");
            }
            (s, out.status.code())
        }
        Err(e) => (format!("failed to run command: {e}"), None),
    }
}

// Vendored xterm.js (UMD), its CSS, and the fit addon — embedded so the terminal
// works offline with no CDN/CSP issues.
const XTERM_JS: &str = include_str!("../assets/xterm.js");
const XTERM_CSS: &str = include_str!("../assets/xterm.css");
const FIT_JS: &str = include_str!("../assets/addon-fit.js");

/// Page script: wire xterm.js to our IPC bridge. Input → `i<data>`, resize →
/// `r<cols>,<rows>`, Shift+Esc → `leave-passthrough`; `window.__feed`/`__fit` are
/// driven by the shell. (Built with string concat to avoid `format!` brace issues.)
const TERM_INIT: &str = r#"
var DEFAULT_SIZE = 16;
var term = new Terminal({ fontFamily: 'Consolas, monospace', fontSize: DEFAULT_SIZE, cursorBlink: true,
  theme: { background: '#1a1a1a', foreground: '#d6d6d6' } });
var fit = new FitAddon.FitAddon();
term.loadAddon(fit);
term.open(document.getElementById('term'));
function refit() { try { fit.fit(); } catch (e) {} }
refit();
term.focus();
window.__fit = refit;
window.__feed = function (b64) {
  term.write(Uint8Array.from(atob(b64), function (c) { return c.charCodeAt(0); }));
};
// The shell drives zoom globally (native chrome + web tabs + terminal together),
// so it pushes an absolute font size here rather than us zooming locally.
window.__setZoom = function (px) { term.options.fontSize = px; refit(); };
term.onData(function (d) { if (window.ipc) window.ipc.postMessage('i' + d); });
term.onResize(function (s) { if (window.ipc) window.ipc.postMessage('r' + s.cols + ',' + s.rows); });
document.addEventListener('keydown', function (e) {
  if (e.key === 'Escape' && e.shiftKey) {
    e.preventDefault(); e.stopPropagation();
    if (window.ipc) window.ipc.postMessage('leave-passthrough');
    return;
  }
  // Ctrl +/-/0 zoom the WHOLE browser — forward to the shell, which scales the
  // native chrome and every tab (including this one via __setZoom).
  if (e.ctrlKey && (e.key === '=' || e.key === '+')) { e.preventDefault(); if (window.ipc) window.ipc.postMessage('zoom+'); return; }
  if (e.ctrlKey && e.key === '-') { e.preventDefault(); if (window.ipc) window.ipc.postMessage('zoom-'); return; }
  if (e.ctrlKey && e.key === '0') { e.preventDefault(); if (window.ipc) window.ipc.postMessage('zoom0'); return; }
}, true);
window.addEventListener('resize', refit);
// __feed and onData are now wired, so the browser can safely flush any output it
// buffered while we loaded (notably ConPTY's ESC[6n, which xterm answers via
// onData — without that reply the shell never prints its prompt).
if (window.ipc) window.ipc.postMessage('ready');
setTimeout(function () { refit(); if (window.ipc) window.ipc.postMessage('r' + term.cols + ',' + term.rows); }, 0);
"#;

/// Assemble the full terminal page (xterm.css + xterm.js + fit + init).
fn terminal_page() -> String {
    let mut s = String::with_capacity(XTERM_JS.len() + XTERM_CSS.len() + 2048);
    s.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><style>");
    s.push_str(XTERM_CSS);
    s.push_str(
        "html,body{margin:0;height:100%;background:#1a1a1a;overflow:hidden}\
         #term{position:fixed;inset:0;padding:4px}\
         .xterm-viewport{overflow-y:hidden!important}\
         .xterm-viewport::-webkit-scrollbar{width:0;height:0;display:none}",
    );
    s.push_str("</style></head><body><div id=\"term\"></div><script>");
    s.push_str(XTERM_JS);
    s.push_str("</script><script>");
    s.push_str(FIT_JS);
    s.push_str("</script><script>");
    s.push_str(TERM_INIT);
    s.push_str("</script></body></html>");
    s
}

/// A clean dark reading stylesheet for read mode.
const READ_CSS: &str = "html{background:#1e1e1e;color:#d0d0d0}body{margin:0}\
main{max-width:760px;margin:48px auto;padding:0 22px;\
font:17px/1.65 -apple-system,Segoe UI,Roboto,sans-serif}\
h1,h2,h3,h4{line-height:1.25;color:#fff}h1{font-size:1.9em}\
a{color:#6cb6ff}img,picture,svg,video,audio,iframe,figure,object,embed{display:none}\
pre,code{font-family:Consolas,monospace;font-size:.92em}\
pre{background:#2a2a2a;padding:12px;overflow:auto;border-radius:6px}\
code{background:#2a2a2a;padding:1px 4px;border-radius:3px}\
pre code{background:none;padding:0}\
blockquote{border-left:3px solid #444;margin:0 0 1em;padding-left:16px;color:#a8a8a8}\
hr{border:none;border-top:1px solid #333}";

/// Wrap extracted article HTML in a full document with a `<base>` (so relative
/// links/images resolve against the source) and the reading stylesheet.
fn read_document(url: &str, title: &str, article_html: &str) -> String {
    // Strict CSP makes read mode truly text-only: no scripts, images, media or web
    // fonts are fetched (real memory/bandwidth savings), only inline styles for our
    // reader CSS. Our host-injected bridge (scroll/focus) runs via WebView2's
    // document-create hook, which is exempt from page CSP — so navigation keys keep
    // working even though page scripts can't.
    const CSP: &str = "default-src 'none'; img-src 'none'; media-src 'none'; \
                       font-src 'none'; object-src 'none'; connect-src 'none'; \
                       style-src 'unsafe-inline'";
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <base href=\"{url}\"><title>{title}</title><style>{READ_CSS}</style></head>\
         <body><main>{article_html}</main></body></html>"
    )
}

/// Stylesheet for the internal `:commands` / `:version` pages.
const HELP_CSS: &str = "html{background:#1e1e1e;color:#d0d0d0}body{margin:0}\
main{max-width:820px;margin:40px auto;padding:0 22px;\
font:16px/1.6 -apple-system,Segoe UI,Roboto,sans-serif}\
h1{color:#fff;font-size:1.7em;margin:0 0 .2em}h2{color:#6cb6ff;font-size:1.1em;\
margin:1.6em 0 .4em;border-bottom:1px solid #333;padding-bottom:.2em}\
p.sub{color:#888;margin:0 0 1em}table{border-collapse:collapse;width:100%}\
td{padding:3px 10px 3px 0;vertical-align:top}td.k{white-space:nowrap;color:#e6a55e;\
font-family:Consolas,monospace;width:1%}kbd{background:#2a2a2a;border:1px solid #444;\
border-radius:4px;padding:1px 6px;font-family:Consolas,monospace;font-size:.9em;color:#f0f0f0}\
td.d{color:#cfcfcf}";

/// Minimal HTML-escaping for text interpolated into the internal pages.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Render rows of (key, description) into a `<table>`, escaping both columns.
fn help_table(rows: &[(&str, &str)]) -> String {
    let mut s = String::from("<table>");
    for (k, d) in rows {
        s.push_str(&format!(
            "<tr><td class=\"k\">{}</td><td class=\"d\">{}</td></tr>",
            html_escape(k),
            html_escape(d)
        ));
    }
    s.push_str("</table>");
    s
}

/// The `:commands` page: every keybind and command (not customizable yet).
fn commands_document() -> String {
    let normal = help_table(&[
        (":", "open the command bar"),
        ("o", "open a page (prefills “open ”)"),
        ("j / k", "scroll down / up"),
        ("d / u", "scroll half a page down / up"),
        ("i", "insert mode (passthrough on a terminal tab)"),
        ("f", "hint mode — label every link, type the label to follow"),
        ("x", "close the current tab"),
        ("r", "reload the page"),
        ("H / L", "history back / forward"),
        ("n / p", "next / previous tab"),
        ("1 – 9", "jump straight to tab N"),
        ("< / >", "move the current tab left / right"),
        ("Ctrl+V", "passthrough mode (every key to the page)"),
        ("Ctrl +/-/0", "zoom the whole UI in / out / reset"),
    ]);
    let cmdline = help_table(&[
        ("Enter", "run the command"),
        ("Esc / Ctrl+C", "cancel (Ctrl+C copies first if text is selected)"),
        ("Left / Right", "move the caret a character"),
        ("Ctrl+Left / Right", "move the caret a word"),
        ("Home / End", "jump to start / end of line"),
        ("Shift+ movement", "extend the selection (with arrows, Ctrl+arrows, Home/End)"),
        ("Ctrl+A", "select the whole line"),
        ("Ctrl+C / Ctrl+X / Ctrl+V", "copy / cut / paste"),
        ("Backspace / Delete", "delete back / forward (or the selection)"),
        ("Ctrl+W · Ctrl/Alt+Backspace", "delete the previous word"),
        ("Ctrl+Delete", "delete the next word"),
        ("Ctrl+U", "delete to the start of the line"),
    ]);
    let modes = help_table(&[
        ("Insert", "type into a field; Esc or click-away leaves, Ctrl+V → passthrough"),
        ("Passthrough", "every key goes to the page; Shift+Esc leaves"),
        ("Hint", "type a label to follow it; Esc cancels"),
        ("Resize / Move", "hjkl to size / reposition the window; Esc finishes"),
    ]);
    let cmds = help_table(&[
        (":open <url|query> · :o · :t", "open a page (non-URL → search engine)"),
        (":research <url|query> · :rs", "lighter browse: JS on, images kept, media/embeds stripped"),
        (":edit · :e", "edit the current URL (re-opens in the tab's own mode)"),
        (":y · :yank", "copy the current URL to the clipboard"),
        (":read <url>", "reader mode: text only, no JS/images/ads"),
        (":search [template]", "show/set the search engine (%s = query)"),
        (":te", "open a terminal tab"),
        (":te <command>", "run a local command, result in the command bar"),
        (":shell <program>", "set the terminal shell (e.g. :shell nu, :shell bash)"),
        (":nojs", "toggle JavaScript off for new tabs"),
        (":nojs <url>", "open a page with JavaScript disabled"),
        (":close · :bd", "close the current tab"),
        (":reload · :r", "reload"),
        (":tabnext · :tn · :tabprev · :tp", "switch tabs"),
        (":back · :forward", "history navigation"),
        (":f · :fullscreen", "toggle fullscreen"),
        (":resize · :move", "window-control modes (then hjkl, Esc)"),
        (":commands · :help", "this page"),
        (":version", "version and build information"),
        (":quit · :q", "quit the browser"),
    ]);
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>commands</title><style>{HELP_CSS}</style></head><body><main>\
         <h1>Commands &amp; keybindings</h1>\
         <p class=\"sub\">Not customizable yet — these are the built-in bindings.</p>\
         <h2>Normal mode</h2>{normal}\
         <h2>Command-line editing</h2>{cmdline}\
         <h2>Other modes</h2>{modes}\
         <h2>Commands</h2>{cmds}\
         </main></body></html>"
    )
}

/// The `:version` page: build/runtime details about this browser.
fn version_document() -> String {
    let rows = help_table(&[
        ("Name", env!("CARGO_PKG_NAME")),
        ("Version", env!("CARGO_PKG_VERSION")),
        ("Description", env!("CARGO_PKG_DESCRIPTION")),
        ("Authors", env!("CARGO_PKG_AUTHORS")),
        ("Engine", "WebView2 (Chromium) via wry 0.55 — loaded on demand"),
        ("Windowing", "tao 0.35 + softbuffer/fontdue native chrome"),
        ("Terminal", "xterm.js + a browser-pty-host companion (ConPTY)"),
        ("Platform", std::env::consts::OS),
        ("Architecture", std::env::consts::ARCH),
    ]);
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>version</title><style>{HELP_CSS}</style></head><body><main>\
         <h1>{} {}</h1>\
         <p class=\"sub\">{}</p>{rows}\
         <p class=\"sub\" style=\"margin-top:1.6em\">A modal, mode-dispatching browser — \
         only what's needed, when needed.</p>\
         </main></body></html>",
        html_escape(env!("CARGO_PKG_NAME")),
        html_escape(env!("CARGO_PKG_VERSION")),
        html_escape(env!("CARGO_PKG_DESCRIPTION")),
    )
}

/// Locate the companion `browser-pty-host` binary next to our own executable.
fn pty_host_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "browser-pty-host.exe"
    } else {
        "browser-pty-host"
    };
    Some(exe.parent()?.join(name))
}

/// A short tab label: the host without scheme/`www.`, truncated.
fn short_label(url: &str) -> String {
    let s = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = s.split('/').next().unwrap_or(s);
    let host = host.strip_prefix("www.").unwrap_or(host);
    let mut label = host.to_string();
    if label.chars().count() > 22 {
        label = label.chars().take(21).collect::<String>();
        label.push('…');
    }
    label
}

/// Draw the top tab bar: `[1:host]` for the active tab, ` 2:host ` for others.
/// Read-mode tabs are tinted green.
fn draw_tab_bar(p: &Painter, buf: &mut [u32], w: usize, h: usize, labels: &[(String, bool, draw::Rgb)]) {
    let baseline = h * 2 / 3;
    let mut x = 8;
    for (i, (label, active, color)) in labels.iter().enumerate() {
        let color = *color;
        let text = if *active {
            format!("[{}:{}]", i + 1, label)
        } else {
            format!(" {}:{} ", i + 1, label)
        };
        x = p.text(buf, w, h, x, baseline, &text, color) + 6;
        if x > w.saturating_sub(40) {
            p.text(buf, w, h, x, baseline, "…", draw::DIM);
            break;
        }
    }
}

/// Paint the engine-free welcome screen: title + a key/command cheat-sheet.
/// `scale` is the global zoom factor so column offsets track the scaled font.
fn draw_welcome(p: &Painter, buf: &mut [u32], w: usize, h: usize, scale: f32) {
    let lh = p.line_height();
    let mut y = lh * 2;
    let x = (40.0 * scale) as usize;
    let gap = (16.0 * scale) as usize;
    let col = (320.0 * scale) as usize;
    let after = p.text(buf, w, h, x, y, "browser", draw::ACCENT);
    p.text(buf, w, h, after + gap, y, "— lightweight modal shell", draw::DIM);
    y += lh * 2;
    for (keys, desc) in [
        (":open <url>   or   o <url>", "open a page (boots the engine on demand)"),
        (":edit   or   :e", "edit the current URL in the command bar"),
        (":read <url>", "reader mode: extract the article, no JS/ads (green tab)"),
        (":te", "open a terminal tab (xterm.js + your shell; Shift+Esc to leave)"),
        (":te <command>", "run a local command; result shows in the command bar"),
        (":shell <program>", "set the terminal shell (e.g. :shell nu, :shell bash)"),
        ("j / k / Space / d / u", "scroll the page"),
        ("f", "hint mode: label every link, type the label to follow it"),
        ("i", "insert mode: type in a field (Esc leaves, click-away leaves)"),
        ("Ctrl+V", "passthrough: send EVERY key to the page (for ttyd, web apps)"),
        ("Shift+Esc", "leave passthrough (it persists across clicks & links)"),
        (":nojs            ", "toggle JavaScript off for new tabs"),
        (":nojs <url>", "open a page with JavaScript disabled"),
        ("n / p", "next / previous tab"),
        ("1 .. 9", "jump straight to tab N"),
        ("< / >", "move the current tab left / right"),
        ("x", "close the current tab (frees its memory)"),
        ("H / L", "history back / forward"),
        ("Ctrl + / - / 0", "zoom the whole UI in / out / reset"),
        (":f", "toggle fullscreen"),
        (":resize", "resize mode — then hjkl to size, Esc to finish"),
        (":move", "move mode — then hjkl to reposition, Esc to finish"),
        (":commands", "open the full list of commands & keybinds"),
        (":version", "version and build information"),
        (":q", "quit"),
    ] {
        p.text(buf, w, h, x, y, keys, draw::FG);
        p.text(buf, w, h, x + col, y, desc, draw::DIM);
        y += lh + lh / 3;
    }
}
