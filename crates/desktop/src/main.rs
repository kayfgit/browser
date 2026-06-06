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
mod procmon;
mod read_view;
mod session;
mod vim;
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

/// WebView2 browser-process arguments, applied to EVERY webview we build.
///
/// This MUST be identical across all webviews: WebView2 requires every
/// environment sharing a user-data folder to be created with the same options,
/// or the second creation fails with `ERROR_INVALID_STATE` (HRESULT 0x8007139F).
/// (That's why a `:te` terminal opened after a content tab used to error — the
/// terminal webview had no args while content tabs did.) Overrides wry's default
/// arg string, so we re-include its defaults (mini-menu / PDF UI / SmartScreen off,
/// plus gesture-free autoplay) and add `Translate,msAutoTranslate` to kill Edge's
/// "translate this page?" bar.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,\
     Translate,msAutoTranslate --autoplay-policy=no-user-gesture-required";

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

// Page-side find-in-page: `__find(q)` highlights every match (CSS Custom Highlight
// API — no DOM mutation, so it can't break the page), scrolls to the first, and
// `__findNext`/`__findPrev` move the "current" highlight. `__findClear` removes it.
const FIND_JS: &str = r#"
(function () {
  if (window.__find) return;
  var all = [], cur = -1;
  var hAll = null, hCur = null;
  function ready() {
    if (hAll || typeof Highlight === 'undefined' || !CSS || !CSS.highlights) return;
    hAll = new Highlight(); hCur = new Highlight();
    CSS.highlights.set('bfind', hAll);
    CSS.highlights.set('bfindcur', hCur);
    var st = document.createElement('style');
    st.textContent = '::highlight(bfind){background:#5a5214;color:#fff}' +
                     '::highlight(bfindcur){background:#c8641e;color:#000}';
    (document.head || document.documentElement).appendChild(st);
  }
  function clear() {
    all = []; cur = -1;
    if (hAll) hAll.clear();
    if (hCur) hCur.clear();
  }
  function show() {
    if (!hCur) return;
    hCur.clear();
    if (cur < 0 || cur >= all.length) return;
    hCur.add(all[cur]);
    var r = all[cur].getBoundingClientRect();
    window.scrollBy(0, r.top - window.innerHeight / 2);
  }
  window.__find = function (q) {
    ready();
    clear();
    if (!q || !hAll) return 0;
    var ql = q.toLowerCase();
    var walk = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT, {
      acceptNode: function (n) {
        if (!n.nodeValue || !n.nodeValue.trim()) return NodeFilter.FILTER_REJECT;
        var p = n.parentElement; if (!p) return NodeFilter.FILTER_REJECT;
        var t = p.tagName; if (t === 'SCRIPT' || t === 'STYLE' || t === 'NOSCRIPT') return NodeFilter.FILTER_REJECT;
        var s = getComputedStyle(p);
        if (s.display === 'none' || s.visibility === 'hidden') return NodeFilter.FILTER_REJECT;
        return NodeFilter.FILTER_ACCEPT;
      }
    });
    var n;
    while ((n = walk.nextNode())) {
      var low = n.nodeValue.toLowerCase(), i = 0;
      while ((i = low.indexOf(ql, i)) !== -1) {
        var r = document.createRange();
        r.setStart(n, i); r.setEnd(n, i + ql.length);
        all.push(r); hAll.add(r);
        i += ql.length;
      }
    }
    cur = all.length ? 0 : -1;
    show();
    return all.length;
  };
  window.__findNext = function () { if (all.length) { cur = (cur + 1) % all.length; show(); } };
  window.__findPrev = function () { if (all.length) { cur = (cur - 1 + all.length) % all.length; show(); } };
  window.__findClear = clear;
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
    /// A `:read` extraction finished: render this Document in an engine-free read
    /// tab. `replace` swaps the active read tab's doc in place (link-follow/reload)
    /// instead of opening a new tab.
    ReadReady { doc: Box<browser_core::Document>, replace: bool },
    /// A `:read` extraction failed.
    ReadFailed(String),
    /// Redirect the active tab to this URL (e.g. de-proxying a `translate.goog`
    /// navigation back to the original site).
    Navigate(String),
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
    /// Find-in-page: `/` opened a search prompt. Typing searches live; Enter keeps
    /// the highlights and returns to Normal (where `n`/`N` step through matches).
    Find,
}

/// A find-in-page match on a native (read/vim) tab: a line index plus the char
/// range `[start, end)` within that line's plain text.
struct NativeMatch {
    line: usize,
    start: usize,
    end: usize,
}

/// Find-in-page state, shared across tab types. For web tabs the matches live in
/// the page (injected JS); for native read/vim tabs they live in `matches`.
#[derive(Default)]
struct FindState {
    /// The confirmed query (`n`/`N` navigate it while this is non-empty + `active`).
    query: String,
    /// Whether a search is live (highlights shown, `n`/`N` active).
    active: bool,
    /// Matches on a native tab; empty for web tabs (JS owns those).
    matches: Vec<NativeMatch>,
    /// Index of the current match within `matches`.
    current: usize,
}

/// Where a content webview gets its page from.
enum Source {
    Url(String),
    Html(String),
}

struct Tab {
    /// The engine. `None` for an engine-free read tab (rendered natively from a
    /// `Document` — no WebView2 process at all); `Some` for every other tab.
    webview: Option<WebView>,
    url: String,
    /// Whether this tab was opened with JavaScript disabled (hint mode needs JS).
    nojs: bool,
    /// Whether this is a readability "read mode" tab.
    read: bool,
    /// Whether this is a "research" tab: a normal page (JS on, images kept) with
    /// heavy media/embeds stripped on the fly for a lighter browse.
    research: bool,
    /// Present for an engine-free read tab: the extracted Document + its native
    /// layout/scroll state. Mutually exclusive with `webview`.
    native: Option<NativeRead>,
    /// Present for an engine-free `:error`/`:errors` tab: a read-only vim-style text
    /// buffer over the session error log. Mutually exclusive with `webview`.
    vim: Option<vim::TextBuffer>,
    /// Present if this tab is an embedded terminal (xterm.js + PTY).
    term: Option<TermSession>,
}

/// State for an engine-free read tab: the extracted document, the vertical scroll
/// offset, and a cache of the laid-out lines (recomputed when the width/zoom
/// changes or the document is replaced by following a link).
struct NativeRead {
    doc: browser_core::Document,
    /// Top-of-viewport scroll offset in pixels (>= 0).
    scroll: i32,
    layout: read_view::Layout,
    /// Content width / font px the cached layout was built at; `dirty` forces a
    /// rebuild after the document is swapped (the width/px may be unchanged).
    layout_w: i32,
    layout_px: f32,
    dirty: bool,
    /// Vim caret/visual-selection state, when caret mode is active (`v`/`V`). Its
    /// `lines` mirror `layout.text_lines()` so a row maps 1:1 to a visual line;
    /// `None` means plain scroll mode.
    caret: Option<vim::TextBuffer>,
}

/// One recorded failure: when it happened, the command that triggered it (if
/// known), and the message. Rendered by `:error` / `:errors`.
struct ErrorEntry {
    /// Local wall-clock time, `HH:MM:SS`.
    time: String,
    /// The command line that raised it (e.g. `:open foo`), if any.
    command: Option<String>,
    message: String,
}

/// A placed hint label over a native read link: the typed label and target URL.
struct NativeHint {
    label: String,
    url: String,
    x: i32,
    y: i32,
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
    /// Placed hint labels for an engine-free read tab (web tabs hint via JS).
    native_hints: Vec<NativeHint>,
    status: String,
    /// Whether the current `status` is an error (rendered red instead of dim).
    status_is_error: bool,
    /// Session error log: every failure (message + the command that triggered it +
    /// a wall-clock timestamp), newest last. Inspected with `:error` (latest) and
    /// `:errors` (all), capped to avoid unbounded growth.
    errors: Vec<ErrorEntry>,
    /// The command line currently executing (`:open foo`), so a failure it raises
    /// can be attributed to it in the error log. `None` outside `run_command`.
    current_command: Option<String>,
    /// Find-in-page state (the `/` search and its matches).
    find: FindState,
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
    /// Whether `teardown` has already run. It fires from multiple places (window
    /// close, `:q`, then `LoopDestroyed`); without this guard the second call would
    /// re-save the session with the now-cleared tab list, wiping the good snapshot.
    torn_down: bool,
    /// `:res` resource monitor: the previous per-pid (cpu_100ns, io_bytes) sample
    /// for computing CPU%/disk rate, and when that sample was taken. The monitor
    /// always auto-refreshes; `refresh_res` just freezes while text is selected.
    res_prev: std::collections::HashMap<u32, (u64, u64)>,
    res_at: Instant,
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

/// Format a "quick maths" result: drop the decimal point for whole numbers,
/// otherwise show up to 6 decimal places with trailing zeros trimmed.
fn format_number(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Generate `n` fixed-width, prefix-free hint labels from the home-row charset
/// (matches the web HINT_JS scheme, so the muscle memory is the same).
fn hint_labels(n: usize) -> Vec<String> {
    const CH: &[u8] = b"asdfghjkl";
    if n == 0 {
        return Vec::new();
    }
    let (mut width, mut cap) = (1usize, CH.len());
    while cap < n {
        width += 1;
        cap *= CH.len();
    }
    (0..n)
        .map(|i| {
            let mut buf = vec![0u8; width];
            let mut x = i;
            for w in 0..width {
                buf[width - 1 - w] = CH[x % CH.len()];
                x /= CH.len();
            }
            String::from_utf8(buf).unwrap()
        })
        .collect()
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

    // Decide up front whether we're restoring a session: only with no CLI target
    // and outside headless test runs. Load it here so the saved window geometry can
    // be applied at build time (no visible jump from the default size).
    let cli_arg = std::env::args().nth(1);
    let is_test = std::env::var("BROWSER_TEST_QUIT_MS").is_ok();
    let restore = if cli_arg.is_none() && !is_test { session::load() } else { None };

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
        native_hints: Vec::new(),
        status: String::new(),
        status_is_error: false,
        errors: Vec::new(),
        current_command: None,
        find: FindState::default(),
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
        torn_down: false,
        res_prev: std::collections::HashMap::new(),
        res_at: Instant::now(),
    };

    // Optional: open a page immediately, e.g. `browser-desktop youtube.com`,
    // or run a command, e.g. `browser-desktop ":nojs youtube.com"`. An explicit
    // CLI target takes precedence over (and skips) session restore. With no
    // argument, restore the previous session's tabs + UI state (window geometry was
    // already applied at build time above).
    match cli_arg {
        Some(target) => {
            let t = target.trim_start();
            if let Some(cmd) = t.strip_prefix(':') {
                app.run_command(cmd);
            } else {
                app.open_tab(&target, false);
            }
        }
        None => {
            if let Some(s) = restore {
                app.restore_session(s);
            }
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
                if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
                    app.cursor_on = !app.cursor_on;
                    app.window.request_redraw();
                } else if app.mode == ModeKind::Normal {
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
            Event::UserEvent(UserEvent::ReadReady { doc, replace }) => {
                app.show_read_document(*doc, replace);
            }
            Event::UserEvent(UserEvent::ReadFailed(e)) => {
                app.set_error(format!("read failed: {e}"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::Navigate(url)) => {
                if let Some(i) = app.active {
                    if let Some(wv) = app.tabs.get(i).and_then(|t| t.webview.as_ref()) {
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
        if !matches!(*control_flow, ControlFlow::Exit | ControlFlow::ExitWithCode(_)) {
            if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
                // Blink the command-bar cursor.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
            } else if app.mode == ModeKind::Normal && app.active_has_webview() {
                // Poll to keep keyboard focus on the shell while a web tab is up (the
                // click-focus backstop). Idle otherwise — no wakeups on welcome/read tabs.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(300));
            } else if app.mode == ModeKind::Normal && app.active_is_res() {
                // Auto-refresh the live resource monitor about once a second.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1000));
            }
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

    /// Top/bottom y of the content band (between the tab bar and command bar), px.
    fn content_y_bounds(&self) -> (i32, i32) {
        let (_, h) = self.inner();
        (self.tab_bar_h() as i32, h as i32 - self.bar_h() as i32)
    }

    /// Visible height of the content band, in px (>= 1).
    fn content_view_h(&self) -> i32 {
        let (top, bottom) = self.content_y_bounds();
        (bottom - top).max(1)
    }

    /// (Re)build the active read tab's native layout when the width, zoom, or the
    /// document itself changed; then clamp the scroll to the new content height.
    /// Cheap no-op when the cache is still valid (called every frame).
    fn refresh_read_layout(&mut self) {
        let Some(i) = self.active else { return };
        if self.tabs[i].native.is_none() {
            return;
        }
        let (w, _) = self.inner();
        let cw = w as i32;
        let px = self.painter.px();
        let view = self.content_view_h();
        // Split borrow: `painter` and this tab's `native` are disjoint fields.
        let painter = &self.painter;
        let nr = self.tabs[i].native.as_mut().unwrap();
        if !nr.dirty && nr.layout_w == cw && (nr.layout_px - px).abs() < f32::EPSILON {
            return;
        }
        // Leave an 8px margin on each side (matches the draw offset).
        nr.layout = read_view::layout(&nr.doc, cw - 16, painter);
        nr.layout_w = cw;
        nr.layout_px = px;
        nr.dirty = false;
        // Re-wrapping changed the visual lines: refresh the caret's grid in place,
        // keeping its cursor/selection (clamped) so caret mode survives resize/zoom.
        if let Some(caret) = nr.caret.as_mut() {
            caret.set_lines(nr.layout.text_lines());
        }
        let max = (nr.layout.height - view).max(0);
        nr.scroll = nr.scroll.clamp(0, max);
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
            let Some(wv) = &tab.webview else { continue };
            if tab.term.is_some() {
                let _ = wv.evaluate_script(&format!("window.__setZoom&&window.__setZoom({term_px})"));
            } else {
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
    fn reclaim_focus_tick(&self) {
        use tao::platform::windows::WindowExtWindows;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
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
    fn reclaim_focus_tick(&self) {}

    /// Whether the active tab is a webview (web/research/nojs/terminal) — i.e. one
    /// that can trap keyboard focus on click. Engine-free read/error tabs and the
    /// empty welcome screen can't, so they don't need the focus backstop.
    fn active_has_webview(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.webview.is_some())
    }

    /// Re-assert the current zoom on the active web tab (e.g. after a navigation,
    /// which can reset the WebView2 zoom factor). No-op for terminal tabs.
    fn apply_active_zoom(&self) {
        if let Some(tab) = self.active.and_then(|i| self.tabs.get(i)) {
            if tab.term.is_none() {
                if let Some(wv) = &tab.webview {
                    let _ = wv.zoom(self.zoom);
                }
            }
        }
    }

    // --- tab access -----------------------------------------------------------

    fn active_webview(&self) -> Option<&WebView> {
        self.active.and_then(|i| self.tabs.get(i)).and_then(|t| t.webview.as_ref())
    }

    /// Mutable access to the active engine-free read tab's state, if any.
    fn active_native_mut(&mut self) -> Option<&mut NativeRead> {
        self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.native.as_mut())
    }

    /// Whether the active tab is an engine-free `:error`/`:errors` vim tab.
    fn active_is_vim(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.vim.is_some())
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
        // Engine-free read tab: the stored url is the canonical document URL.
        let Some(wv) = &tab.webview else {
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
    fn refresh_active_url(&mut self) {
        if let Some(tab) = self.active.and_then(|i| self.tabs.get_mut(i)) {
            if tab.term.is_some() {
                return;
            }
            let Some(wv) = &tab.webview else { return };
            if let Ok(u) = wv.url() {
                if u.starts_with("http") {
                    tab.url = u;
                }
            }
        }
    }

    // --- input ----------------------------------------------------------------

    fn handle_key(&mut self, key: &KeyEvent) {
        match self.mode {
            ModeKind::Command | ModeKind::Find => self.key_command(key),
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
        // Once a `/` search is live, `n`/`N` step through matches and Esc clears it
        // (qutebrowser-style) — in every tab type, so this takes precedence over both
        // the vim pager and the normal tab/scroll bindings.
        if self.find.active && !self.modifiers.control_key() {
            match &key.logical_key {
                Key::Character(s) if *s == "n" => {
                    self.find_step(true);
                    return;
                }
                Key::Character(s) if *s == "N" => {
                    self.find_step(false);
                    return;
                }
                Key::Escape => {
                    self.find_clear();
                    return;
                }
                _ => {}
            }
        }
        // A `:error`/`:errors` tab is a read-only vim pager: let it claim the motion/
        // visual/yank keys first; anything it doesn't want (`:`, n/p, x, …) falls
        // through to the normal browser bindings below.
        if self.active_is_vim() && self.key_vim(key) {
            return;
        }
        // Read-mode caret/visual selection (engine-free read tab): once active it
        // claims motion/visual/yank keys; `v`/`V` below enter it.
        if self.read_caret_active() && self.key_read_caret(key) {
            return;
        }
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
                "g" => self.scroll_edge(false),
                "G" => self.scroll_edge(true),
                "/" => self.enter_find(),
                "i" => {
                    if self.active_is_term() {
                        self.enter_passthrough();
                    } else {
                        self.enter_insert();
                    }
                }
                "f" => self.enter_hint(),
                // Read tabs: enter caret/visual selection (highlight + yank article
                // text with vim motions). On web tabs `v`/`V` are not bound yet.
                "v" if self.active_is_read_native() => self.enter_read_caret(false),
                "V" if self.active_is_read_native() => self.enter_read_caret(true),
                "x" => self.close_active(),
                "r" => self.reload_active(),
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

    /// Translate a raw key event into a vim-pager [`vim::Key`], or `None` if it
    /// doesn't map (so the shell can handle it). Ctrl+D/Ctrl+U are half-page; any
    /// other Ctrl chord (zoom, …) is left for the shell. Shared by the `:error`/
    /// `:res` pagers and the read-mode caret.
    fn map_vim_key(&self, key: &KeyEvent) -> Option<vim::Key> {
        if self.modifiers.control_key() {
            return match key.physical_key {
                KeyCode::KeyD => Some(vim::Key::HalfDown),
                KeyCode::KeyU => Some(vim::Key::HalfUp),
                _ => None,
            };
        }
        match &key.logical_key {
            Key::ArrowLeft => Some(vim::Key::Left),
            Key::ArrowRight => Some(vim::Key::Right),
            Key::ArrowUp => Some(vim::Key::Up),
            Key::ArrowDown => Some(vim::Key::Down),
            Key::Home => Some(vim::Key::Home),
            Key::End => Some(vim::Key::End),
            Key::Escape => Some(vim::Key::Esc),
            Key::Character(s) => {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(vim::Key::Char(c)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Feed a key to the active `:error`/`:errors`/`:res` vim pager. Returns `true`
    /// if the pager consumed it (the shell should then do nothing else with the key).
    fn key_vim(&mut self, key: &KeyEvent) -> bool {
        let Some(vk) = self.map_vim_key(key) else { return false };
        // Viewport in cells (monospace), matching the draw geometry below.
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = ((w as usize).saturating_sub(2 * 8)) / cw;
        let rows = (self.content_view_h() as usize) / line_h;
        let Some(buf) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.vim.as_mut())
        else {
            return false;
        };
        let res = buf.key(vk, rows, cols);
        if let Some(text) = res.yanked {
            let n = text.chars().count();
            clipboard_set(&text);
            self.set_status(format!("yanked {n} chars"));
        }
        if res.consumed {
            self.window.request_redraw();
        }
        res.consumed
    }

    /// Whether the active tab is an engine-free read tab (native render).
    fn active_is_read_native(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.native.is_some())
    }

    /// Whether read-mode caret/visual selection is currently active.
    fn read_caret_active(&self) -> bool {
        self.active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.native.as_ref())
            .is_some_and(|n| n.caret.is_some())
    }

    /// Enter read-mode caret/visual selection (`v` charwise, `V` linewise): build a
    /// caret over the read view's visual lines, place it near the middle of the
    /// viewport, and start a visual selection so motions immediately highlight text.
    fn enter_read_caret(&mut self, linewise: bool) {
        // Geometry first (immutable borrows of painter), before borrowing the tab.
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (((w as usize).saturating_sub(16)) / cw).max(1);
        let view = self.content_view_h();
        let rows = (view as usize / line_h).max(1);
        let Some(nr) = self.active_native_mut() else { return };
        let lines = nr.layout.text_lines();
        if lines.iter().all(|l| l.is_empty()) {
            return;
        }
        let lh = nr.layout.line_h.max(1);
        let mid_line = ((nr.scroll + view / 2) / lh).max(0) as usize;
        let mut tb = vim::TextBuffer::new(lines);
        tb.place_cursor(mid_line, 0, rows, cols);
        tb.key(if linewise { vim::Key::Char('V') } else { vim::Key::Char('v') }, rows, cols);
        nr.scroll = tb.top as i32 * lh;
        nr.caret = Some(tb);
        self.set_status("[VISUAL]  motions select · y yank · Esc exit");
        self.window.request_redraw();
    }

    /// Feed a key to the read-mode caret. Returns `true` if it was consumed. `Esc`
    /// with no selection leaves caret mode; the read view's pixel scroll is kept in
    /// sync with the caret's line so the cursor stays on-screen.
    fn key_read_caret(&mut self, key: &KeyEvent) -> bool {
        let Some(vk) = self.map_vim_key(key) else { return false };
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (((w as usize).saturating_sub(16)) / cw).max(1);
        let rows = (self.content_view_h() as usize / line_h).max(1);

        let mut yanked: Option<String> = None;
        let mut consumed = true;
        let mut exit = false;
        {
            let Some(nr) = self.active_native_mut() else { return false };
            let Some(buf) = nr.caret.as_mut() else { return false };
            if vk == vim::Key::Esc && !buf.has_selection() {
                exit = true;
            } else {
                let res = buf.key(vk, rows, cols);
                yanked = res.yanked;
                consumed = res.consumed;
                nr.scroll = buf.top as i32 * nr.layout.line_h.max(1);
            }
            if exit {
                nr.caret = None;
            }
        }
        if let Some(text) = yanked {
            let n = text.chars().count();
            clipboard_set(&text);
            self.set_status(format!("yanked {n} chars"));
        } else if exit {
            self.clear_status();
        }
        if consumed || exit {
            self.window.request_redraw();
        }
        consumed || exit
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
            if self.mode == ModeKind::Find {
                self.find_update();
            }
            return;
        }
        // Alt+Backspace: delete the word before the caret (a common alias).
        if self.modifiers.alt_key() && key.physical_key == KeyCode::Backspace {
            self.cmd_delete_word();
            self.cursor_on = true;
            if self.mode == ModeKind::Find {
                self.find_update();
            }
            return;
        }
        match &key.logical_key {
            Key::Enter if self.mode == ModeKind::Find => {
                self.find_confirm();
                return;
            }
            Key::Enter => {
                // Quick maths: if the line is an arithmetic expression, evaluate it
                // in place — replace the bar contents with the result so you can copy
                // it or keep calculating (`20*8` → `160` → `160+10`) instead of
                // running it as a command.
                if let Some(result) = self.math_preview() {
                    self.command = result;
                    self.command_cursor = self.command.len();
                    self.command_anchor = None;
                    self.cursor_on = true;
                    self.clear_status();
                    return;
                }
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
        // In Find mode, search live as the query changes.
        if self.mode == ModeKind::Find {
            self.find_update();
        }
    }

    /// Leave the command bar, discarding the line (Esc / Ctrl+C with no selection).
    /// In Find mode this also drops the search and its highlights.
    fn cancel_command(&mut self) {
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        if self.mode == ModeKind::Find {
            self.find_clear();
        }
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

    /// Start of the word before `pos` in the command line (see [`prev_word_boundary`]).
    fn prev_word(&self, pos: usize) -> usize {
        prev_word_boundary(&self.command, pos)
    }

    /// End of the word after `pos` in the command line (see [`next_word_boundary`]).
    fn next_word(&self, pos: usize) -> usize {
        next_word_boundary(&self.command, pos)
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

    // --- find in page ---------------------------------------------------------

    /// Open the `/` find prompt: searches live as you type; Enter keeps the
    /// highlights (then `n`/`N` step through matches), Esc cancels.
    fn enter_find(&mut self) {
        self.find_clear();
        self.mode = ModeKind::Find;
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        self.cursor_on = true;
        self.window.request_redraw();
    }

    /// Live-update the search from the current `/` input.
    fn find_update(&mut self) {
        let q = self.command.clone();
        self.find.query = q.clone();
        self.find_search(&q, true);
        self.window.request_redraw();
    }

    /// Confirm the search: keep the highlights and enable `n`/`N`, or clear it if
    /// the query is empty (or matched nothing on a native tab).
    fn find_confirm(&mut self) {
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        self.mode = ModeKind::Normal;
        let native_empty = self.active_webview().is_none() && self.find.matches.is_empty();
        if self.find.query.is_empty() || native_empty {
            self.find_clear();
        } else {
            self.find.active = true;
            if self.active_webview().is_none() {
                self.set_status(self.find_count_label());
            }
        }
        self.window.request_redraw();
    }

    /// Drop the active search and clear any highlights (page-side and native).
    fn find_clear(&mut self) {
        self.find.active = false;
        self.find.query.clear();
        self.find.matches.clear();
        self.find.current = 0;
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script("window.__findClear&&window.__findClear()");
        }
        self.window.request_redraw();
    }

    /// Reset find state without touching page highlights — used when the active tab
    /// changes (the old page's highlights fade on its own; `n`/`N` shouldn't carry).
    fn find_reset(&mut self) {
        self.find.active = false;
        self.find.query.clear();
        self.find.matches.clear();
        self.find.current = 0;
    }

    /// Run a search for `q` on the active tab (web → injected JS; read/vim → a
    /// native match list). `reveal` scrolls/moves to the first match.
    fn find_search(&mut self, q: &str, reveal: bool) {
        self.find.matches.clear();
        self.find.current = 0;
        // Web tab: hand off to the page's injected search.
        if let Some(wv) = self.active_webview() {
            let js = if q.is_empty() {
                "window.__findClear&&window.__findClear()".to_string()
            } else {
                format!("window.__find&&window.__find({})", js_string(q))
            };
            let _ = wv.evaluate_script(&js);
            return;
        }
        if q.is_empty() {
            return;
        }
        // Native tab: collect this tab's lines and match against them.
        if let Some(lines) = self.find_native_lines() {
            self.find.matches = find_in_lines(&lines, q);
            if reveal && !self.find.matches.is_empty() {
                self.find_reveal_current();
            }
        }
    }

    /// Step to the next (`forward`) or previous match.
    fn find_step(&mut self, forward: bool) {
        if !self.find.active {
            return;
        }
        if let Some(wv) = self.active_webview() {
            let js = if forward {
                "window.__findNext&&window.__findNext()"
            } else {
                "window.__findPrev&&window.__findPrev()"
            };
            let _ = wv.evaluate_script(js);
            return;
        }
        let n = self.find.matches.len();
        if n == 0 {
            return;
        }
        self.find.current =
            if forward { (self.find.current + 1) % n } else { (self.find.current + n - 1) % n };
        self.find_reveal_current();
        self.set_status(self.find_count_label());
        self.window.request_redraw();
    }

    /// The active tab's searchable lines (read = laid-out lines, vim = buffer lines).
    fn find_native_lines(&mut self) -> Option<Vec<String>> {
        let i = self.active?;
        if self.tabs.get(i).is_some_and(|t| t.native.is_some()) {
            self.refresh_read_layout();
            let nr = self.tabs[i].native.as_ref()?;
            return Some(
                nr.layout
                    .lines
                    .iter()
                    .map(|l| l.runs.iter().map(|r| r.text.as_str()).collect::<String>())
                    .collect(),
            );
        }
        let vb = self.tabs.get(i)?.vim.as_ref()?;
        Some(vb.lines.iter().map(|l| l.iter().collect::<String>()).collect())
    }

    /// Scroll a read tab (or move a vim tab's cursor) so the current match shows.
    fn find_reveal_current(&mut self) {
        let Some(&NativeMatch { line, start, .. }) = self.find.matches.get(self.find.current) else {
            return;
        };
        let view = self.content_view_h();
        let Some(i) = self.active else { return };
        if let Some(nr) = self.tabs.get_mut(i).and_then(|t| t.native.as_mut()) {
            let line_h = nr.layout.line_h.max(1);
            let max = (nr.layout.height - view).max(0);
            nr.scroll = (line as i32 * line_h - view / 2).clamp(0, max);
            return;
        }
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (w as usize).saturating_sub(16) / cw;
        let rows = (view as usize) / line_h;
        if let Some(vb) = self.tabs.get_mut(i).and_then(|t| t.vim.as_mut()) {
            vb.place_cursor(line, start, rows, cols);
        }
    }

    /// `"/query  cur/total"` (or `no matches`) for the status bar on native tabs.
    fn find_count_label(&self) -> String {
        if self.find.matches.is_empty() {
            format!("/{}  no matches", self.find.query)
        } else {
            format!("/{}  {}/{}", self.find.query, self.find.current + 1, self.find.matches.len())
        }
    }

    fn enter_command(&mut self, prefill: &str) {
        self.mode = ModeKind::Command;
        self.command = prefill.to_string();
        self.command_cursor = self.command.len();
        self.command_anchor = None;
        self.cursor_on = true;
        self.clear_status();
    }

    fn enter_insert(&mut self) {
        if self.active_webview().is_none() {
            self.set_status("no page — open one first");
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
            self.set_status("no page — open one first");
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
            self.set_status("no page — open one first");
            return;
        };
        // Engine-free read tab: hints are computed and drawn natively.
        if self.tabs[idx].native.is_some() {
            self.hint_input.clear();
            self.mode = ModeKind::Hint;
            self.build_native_hints();
            if self.native_hints.is_empty() {
                self.mode = ModeKind::Normal;
                self.set_status("no links on screen");
            }
            self.window.request_redraw();
            return;
        }
        if self.tabs[idx].nojs {
            self.set_status("hint mode needs JavaScript (this tab is no-js)");
            return;
        }
        self.hint_input.clear();
        self.mode = ModeKind::Hint;
        if let Some(wv) = &self.tabs[idx].webview {
            let _ = wv.evaluate_script(HINT_JS);
        }
    }

    /// Place hint labels over the links currently visible in the native read tab.
    fn build_native_hints(&mut self) {
        self.native_hints.clear();
        let Some(i) = self.active else { return };
        let (top, bottom) = self.content_y_bounds();
        let painter = &self.painter;
        let Some(nr) = self.tabs[i].native.as_ref() else { return };
        let links = read_view::visible_links(&nr.layout, nr.scroll, top, bottom, painter);
        let labels = hint_labels(links.len());
        let mut hints = Vec::with_capacity(links.len());
        for ((id, x, y), label) in links.into_iter().zip(labels) {
            if let Some(url) = nr.doc.link_url(id) {
                // +8 to match the content's left draw margin.
                hints.push(NativeHint { label, url: url.to_string(), x: x + 8, y });
            }
        }
        self.native_hints = hints;
    }

    fn key_hint(&mut self, key: &KeyEvent) {
        let native = !self.native_hints.is_empty();
        match &key.logical_key {
            Key::Escape => self.exit_hint(),
            Key::Backspace => {
                self.hint_input.pop();
                if native {
                    self.window.request_redraw();
                } else {
                    self.hint_send();
                }
            }
            Key::Character(s) => {
                let c = *s;
                if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphabetic()) {
                    self.hint_input.push_str(&c.to_lowercase());
                    if native {
                        self.hint_match_native();
                    } else {
                        self.hint_send();
                    }
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

    /// Native hint input: on an exact label match, follow the link (re-extract it
    /// into the current read tab); reset if the typed prefix matches nothing.
    fn hint_match_native(&mut self) {
        if let Some(h) = self.native_hints.iter().find(|h| h.label == self.hint_input) {
            let url = h.url.clone();
            self.exit_hint();
            self.start_read(&url, true);
            return;
        }
        if !self.native_hints.iter().any(|h| h.label.starts_with(&self.hint_input)) {
            self.hint_input.clear();
        }
        self.window.request_redraw();
    }

    fn exit_hint(&mut self) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script("window.__hintClear&&window.__hintClear()");
        }
        self.native_hints.clear();
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

    /// Set an informational status message (rendered dim). Clears the error flag.
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = false;
    }

    /// "Quick maths": if the command-bar line is an arithmetic expression, return
    /// its formatted result. Gated on the presence of a maths operator so plain
    /// inputs (a lone number, a URL, a command) don't show a spurious result.
    fn math_preview(&self) -> Option<String> {
        let line = self.command.trim();
        if !line.contains(['+', '-', '*', '/', '%', '^']) {
            return None;
        }
        browser_core::math_eval(line).map(format_number)
    }

    /// Clear the status line.
    fn clear_status(&mut self) {
        self.status.clear();
        self.status_is_error = false;
    }

    /// Record a failure: show it in the status bar (red) and append it to the
    /// session error log for `:error`/`:errors`.
    fn set_error(&mut self, msg: impl Into<String>) {
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
    }

    fn run_command(&mut self, line: &str) {
        let line = line.trim();
        // Attribute any failure raised below to this command in the error log.
        self.current_command = if line.is_empty() { None } else { Some(format!(":{line}")) };
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
                None => self.set_status("no page to edit"),
            },
            // Yank (copy) the current URL to the system clipboard.
            "y" | "yank" => match self.current_url() {
                Some(url) => {
                    clipboard_set(&url);
                    self.set_status(format!("yanked {url}"));
                }
                None => self.set_status("no url to yank"),
            },
            "read" => {
                if rest.is_empty() {
                    self.set_status("usage: :read <url>");
                } else {
                    self.start_read(rest, false);
                }
            }
            // Inspect this session's errors in an engine-free, scrollable tab:
            // `:error` shows the most recent one, `:errors` shows them all.
            "error" | "err" => self.open_error_page(false),
            "errors" | "errs" => self.open_error_page(true),
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
                    self.set_status(format!("shell = {}", self.term_command.join(" ")));
                } else {
                    self.term_command = rest.split_whitespace().map(String::from).collect();
                    self.set_status(format!("shell set to: {}", self.term_command.join(" ")));
                }
            }
            // Customize the search engine used when `:open <query>` isn't a URL.
            // `%s` in the template is replaced with the percent-encoded query.
            "search" => {
                if rest.is_empty() {
                    self.set_status(format!("search = {}", self.search_template));
                } else {
                    self.search_template = rest.to_string();
                    self.set_status(format!("search engine set to: {}", self.search_template));
                }
            }
            "nojs" => {
                if rest.is_empty() {
                    self.nojs = !self.nojs;
                    self.set_status(format!(
                        "new tabs: JavaScript {}",
                        if self.nojs { "OFF" } else { "ON" }
                    ));
                } else {
                    self.open_tab(rest, true);
                }
            }
            "close" | "tabclose" | "bd" => self.close_active(),
            "quit" | "q" => self.quit = true,
            "reload" | "r" => self.reload_active(),
            "next" | "tabnext" | "tn" => self.switch_tab(1),
            "prev" | "tabprev" | "tp" => self.switch_tab(-1),
            "back" => self.history(false),
            "forward" => self.history(true),
            "f" | "fullscreen" => self.toggle_fullscreen(),
            "resize" => {
                self.mode = ModeKind::Resize;
                self.clear_status();
            }
            "move" => {
                self.mode = ModeKind::Move;
                self.clear_status();
            }
            "commands" | "help" => self.open_local_page("commands", commands_document()),
            "version" => self.open_version_page(),
            // Total the browser's real footprint across its whole process tree
            // (browser.exe + WebView2 engine procs + pty-hosts), which Task Manager
            // scatters under a separate "WebView2 Manager" group.
            "res" | "resources" => self.open_resource_page(),
            "" => {}
            // A bare bang (`:!yt cats`) opens the whole line as a bang target.
            other if other.starts_with('!') => self.open_tab(line, self.nojs),
            other => self.set_error(format!("unknown command: {other}")),
        }
        self.current_command = None;
    }

    /// Turn a command-bar target into a URL the way `:open` does: a bare query
    /// (spaces, or no scheme/dot like `rustlang`) — or anything that won't parse as
    /// a URL — goes to the configured search engine; a real address opens directly.
    fn resolve_target(&self, target: &str) -> String {
        // DuckDuckGo-style bangs (`!yt cats`, `!osrs dragon`) take priority: they
        // redirect to a specific site's search regardless of the default engine.
        if let Some(url) = browser_core::expand_bang(target) {
            return url;
        }
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
                    webview: Some(webview),
                    url: url.clone(),
                    nojs: disable_js,
                    read: false,
                    research: false,
                    native: None,
                    vim: None,
                    term: None,
                });
                self.active = Some(self.tabs.len() - 1);
                self.refresh_visibility();
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
    /// images and text stay. A lighter browse for "how do I…" lookups.
    fn open_research(&mut self, target: &str) {
        let url = self.resolve_target(target);
        match self.build_content_webview(Source::Url(url.clone()), false, RESEARCH_JS) {
            Ok(webview) => {
                self.tabs.push(Tab {
                    webview: Some(webview),
                    url: url.clone(),
                    nojs: false,
                    read: false,
                    research: true,
                    native: None,
                    vim: None,
                    term: None,
                });
                self.active = Some(self.tabs.len() - 1);
                self.refresh_visibility();
                self.window.set_focus();
                self.set_status("(research — media stripped)");
            }
            Err(e) => self.set_error(format!("failed to open: {e:#}")),
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
            // The shell bridge always loads; `extra_init` (e.g. research-mode DOM
            // pruning) is appended so it runs in the same document-create pass.
            .with_initialization_script(if extra_init.is_empty() {
                format!("{BRIDGE_JS}\n{FIND_JS}")
            } else {
                format!("{BRIDGE_JS}\n{FIND_JS}\n{extra_init}")
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
    fn start_read(&mut self, target: &str, replace: bool) {
        self.set_status(format!("reading {target} …"));
        let proxy = self.proxy.clone();
        let target = target.to_string();
        std::thread::spawn(move || {
            let event = match browser_backend_text::fetch_document_blocking(&target) {
                Ok(doc) => UserEvent::ReadReady { doc: Box::new(doc), replace },
                Err(e) => UserEvent::ReadFailed(format!("{e:#}")),
            };
            let _ = proxy.send_event(event);
        });
        self.window.request_redraw();
    }

    /// Run a local shell command in the background. Result arrives as TermDone.
    /// Strictly shell-initiated — never reachable from page content.
    fn run_term(&mut self, cmd: &str) {
        self.set_status(format!("$ {cmd}"));
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
            self.set_error("could not locate browser-pty-host");
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
                self.set_error(format!("failed to start pty-host: {e}"));
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
                self.set_error(format!("terminal webview: {e:#}"));
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
            webview: Some(webview),
            url: format!("term: {}", shell[0]),
            nojs: false,
            read: false,
            research: false,
            native: None,
            vim: None,
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
        self.set_status("terminal — Shift+Esc returns to the shell");
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
            // Must match the content webviews' args or WebView2 fails to create a
            // second environment on the shared user-data folder (0x8007139F) — this
            // is what broke `:te` opened after a page tab.
            .with_additional_browser_args(BROWSER_ARGS)
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
            if let Some(wv) = &tab.webview {
                let _ = wv.evaluate_script(&format!("window.__feed(\"{data}\")"));
            }
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
        if let Some(wv) = &tab.webview {
            for data in pending {
                let _ = wv.evaluate_script(&format!("window.__feed(\"{data}\")"));
            }
        }
        // Adopt the current global zoom (the page starts at 100%).
        if (self.zoom - 1.0).abs() > f64::EPSILON {
            let term_px = (BASE_TERM_PX * self.zoom).round();
            if let Some(wv) = self
                .tabs
                .iter()
                .find(|t| t.term.as_ref().map(|s| s.id) == Some(id))
                .and_then(|t| t.webview.as_ref())
            {
                let _ = wv.evaluate_script(&format!("window.__setZoom({term_px})"));
            }
        }
    }

    /// Present a finished command vim-style: the result replaces the command-bar
    /// text (collapsed to one line).
    fn show_term_result(&mut self, _cmd: &str, output: &str, code: Option<i32>) {
        let trimmed = output.trim();
        let msg = if trimmed.is_empty() {
            let codestr = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            format!("(exit {codestr})")
        } else {
            trimmed.replace(['\r', '\n'], " ")
        };
        self.set_status(msg);
        self.window.request_redraw();
    }

    /// Render an extracted Document in an engine-free read tab (no WebView2). With
    /// `replace`, swap the active read tab's document in place (link-follow/reload);
    /// otherwise open a new read tab.
    fn show_read_document(&mut self, doc: browser_core::Document, replace: bool) {
        let url = doc.url.clone();
        if replace {
            if let Some(nr) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| {
                t.url = url.clone();
                t.native.as_mut()
            }) {
                nr.doc = doc;
                nr.scroll = 0;
                nr.dirty = true;
                nr.caret = None; // the text changed; drop any caret/selection
                self.clear_status();
                self.window.request_redraw();
                return;
            }
        }
        self.push_native_tab(doc, url, true);
    }

    /// Open a new engine-free native tab rendering `doc` (no WebView2 process). Used
    /// by read mode (`read = true`, tinted green + `f` hint) and the `:error(s)`
    /// pages (`read = false`). Activates and focuses the new tab.
    fn push_native_tab(&mut self, doc: browser_core::Document, url: String, read: bool) {
        self.tabs.push(Tab {
            webview: None,
            url,
            nojs: false,
            read,
            research: false,
            native: Some(NativeRead {
                doc,
                scroll: 0,
                layout: read_view::Layout { lines: Vec::new(), line_h: 1, height: 0 },
                layout_w: -1,
                layout_px: 0.0,
                dirty: true,
                caret: None,
            }),
            vim: None,
            term: None,
        });
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.window.set_focus();
        self.clear_status();
        self.window.request_redraw();
    }

    /// Open the `:error` / `:errors` page: render the session error log in an
    /// engine-free, read-only **vim-style** tab so the full text (which may be long,
    /// like the WebView2 HRESULT messages) is readable, navigable, and — crucially —
    /// selectable/yankable without retyping. `all = false` shows just the most recent
    /// error; `all = true` shows every error this session, newest first.
    fn open_error_page(&mut self, all: bool) {
        if self.errors.is_empty() {
            self.set_status("no errors this session");
            return;
        }
        let lines = error_lines(&self.errors, all);
        self.tabs.push(Tab {
            webview: None,
            url: "browser://error".into(),
            nojs: false,
            read: false,
            research: false,
            native: None,
            vim: Some(vim::TextBuffer::new(lines)),
            term: None,
        });
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.window.set_focus();
        self.clear_status();
        self.window.request_redraw();
    }

    /// `:res` — the browser's whole-tree resource usage (browser.exe + WebView2
    /// engine procs + pty-hosts): memory, CPU%, and disk I/O per process plus a
    /// grand total, in an engine-free vim-style tab. Task Manager won't give this in
    /// one place (it scatters WebView2 under its own group). Auto-refreshes ~1×/sec
    /// and freezes automatically while you're selecting text (so you can copy figures
    /// with vim motions without it shifting).
    fn open_resource_page(&mut self) {
        // No previous sample yet, so the first frame shows memory immediately and
        // CPU/disk fill in on the next refresh.
        self.res_prev.clear();
        self.res_at = Instant::now();
        let lines = self.sample_res_lines();
        if lines.is_empty() {
            self.set_status("resource info unavailable");
            return;
        }
        self.tabs.push(Tab {
            webview: None,
            url: "browser://res".into(),
            nojs: false,
            read: false,
            research: false,
            native: None,
            vim: Some(vim::TextBuffer::new(lines)),
            term: None,
        });
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.window.set_focus();
        self.clear_status();
        self.window.request_redraw();
    }

    /// Whether the active tab is the `:res` resource monitor.
    fn active_is_res(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.url == "browser://res")
    }

    /// If the active tab is the resource monitor, re-sample and update its buffer
    /// **in place** — keeping the cursor, selection, and scroll — so a live refresh
    /// never disturbs navigation/copy. Freezes (skips the update) while a visual
    /// selection is active, so the highlighted text can't shift mid-copy; the
    /// refresh resumes once the selection is cleared (after yank/Esc). Called on the
    /// ~1s tick and on pause/resume.
    fn refresh_res(&mut self) {
        if !self.active_is_res() {
            return;
        }
        let selecting = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.vim.as_ref())
            .is_some_and(|b| b.has_selection());
        if selecting {
            return;
        }
        let lines = self.sample_res_lines();
        if let Some(buf) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.vim.as_mut())
        {
            buf.set_lines(lines);
        }
    }

    /// Take a fresh process-tree sample, fold in CPU%/disk-rate deltas against the
    /// previous sample, format the breakdown, and update `res_prev`/`res_at`.
    fn sample_res_lines(&mut self) -> Vec<String> {
        let sample = procmon::tree_sample();
        if sample.is_empty() {
            return Vec::new();
        }
        let elapsed = self.res_at.elapsed().as_secs_f64();
        let ncores = procmon::cpu_count() as f64;
        let have_prev = !self.res_prev.is_empty() && elapsed > 0.05;

        // Per-process CPU% and disk B/s from the cumulative-counter deltas.
        let rate = |s: &procmon::ProcSample| -> (Option<f64>, Option<f64>) {
            if !have_prev {
                return (None, None);
            }
            match self.res_prev.get(&s.pid) {
                Some(&(pc, pio)) => {
                    let cpu = (s.cpu_100ns.saturating_sub(pc)) as f64
                        / (elapsed * 1e7 * ncores)
                        * 100.0;
                    let disk = (s.io_bytes.saturating_sub(pio)) as f64 / elapsed;
                    (Some(cpu), Some(disk))
                }
                None => (None, None),
            }
        };

        let total_mem: u64 = sample.iter().map(|p| p.working_set).sum();
        let (mut total_cpu, mut total_disk) = (0.0f64, 0.0f64);
        let mut rows = Vec::with_capacity(sample.len());
        for p in &sample {
            let (cpu, disk) = rate(p);
            total_cpu += cpu.unwrap_or(0.0);
            total_disk += disk.unwrap_or(0.0);
            let cpu_s = cpu.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "—".into());
            let disk_s = disk.map(procmon::fmt_rate).unwrap_or_else(|| "—".into());
            rows.push(format!(
                "{:>9}  {:>6}  {:>9}  {:<22} {}",
                procmon::fmt_bytes(p.working_set),
                cpu_s,
                disk_s,
                p.name,
                p.pid
            ));
        }

        let cpu_total = if have_prev { format!("CPU {total_cpu:.1}%") } else { "CPU —".into() };
        let disk_total =
            if have_prev { format!("disk {}", procmon::fmt_rate(total_disk)) } else { "disk —".into() };
        let mut lines = Vec::with_capacity(rows.len() + 5);
        lines.push(format!("browser — {} processes    (live; select to freeze)", sample.len()));
        lines.push(format!("{} · {} · {}", procmon::fmt_bytes(total_mem), cpu_total, disk_total));
        lines.push(String::new());
        lines.push(format!("{:>9}  {:>6}  {:>9}  {:<22} {}", "MEM", "CPU", "DISK", "PROCESS", "PID"));
        lines.extend(rows);

        // Roll the sample forward for the next delta.
        self.res_prev = sample.iter().map(|p| (p.pid, (p.cpu_100ns, p.io_bytes))).collect();
        self.res_at = Instant::now();
        lines
    }

    /// `:version` — build/runtime details in an engine-free vim pager (no WebView2),
    /// so the text is navigable and yankable with the same motions as `:error`/`:res`.
    fn open_version_page(&mut self) {
        self.tabs.push(Tab {
            webview: None,
            url: "browser://version".into(),
            nojs: false,
            read: false,
            research: false,
            native: None,
            vim: Some(vim::TextBuffer::new(version_lines())),
            term: None,
        });
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.window.set_focus();
        self.clear_status();
        self.window.request_redraw();
    }

    /// Open an internal HTML page (e.g. `:commands`) in a new tab.
    fn open_local_page(&mut self, label: &str, html: String) {
        match self.build_content_webview(Source::Html(html), false, "") {
            Ok(webview) => {
                self.tabs.push(Tab {
                    webview: Some(webview),
                    url: format!("browser://{label}"),
                    nojs: false,
                    read: false,
                    research: false,
                    native: None,
                    vim: None,
                    term: None,
                });
                self.active = Some(self.tabs.len() - 1);
                self.refresh_visibility();
                self.window.set_focus();
                self.clear_status();
            }
            Err(e) => self.set_error(format!("failed to open {label}: {e:#}")),
        }
    }

    fn close_active(&mut self) {
        let Some(i) = self.active else {
            self.set_status("no tab to close");
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
        self.find_reset();
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
        self.find_reset();
        self.refresh_visibility();
        self.window.set_focus();
    }

    /// Jump directly to a zero-based tab index (bound to keys 1..9).
    fn jump_to(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = Some(index);
            self.find_reset();
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
            let Some(wv) = &tab.webview else { continue };
            let visible = Some(i) == self.active;
            let _ = wv.set_visible(visible);
            if visible {
                let _ = wv.set_bounds(rect);
                if tab.term.is_some() {
                    // Re-fit xterm to the new size; it reports back to resize the PTY.
                    let _ = wv.evaluate_script("window.__fit&&window.__fit()");
                }
            }
        }
        self.window.request_redraw();
    }

    /// Shut down terminals (kill shells, close PTYs, join readers) and drop every
    /// webview before exiting — so WebView2 processes and ConPTYs close cleanly
    /// rather than leaving a stuck thread that deadlocks process teardown.
    fn teardown(&mut self) {
        // Idempotent: only the first call saves + tears down (see `torn_down`).
        if self.torn_down {
            return;
        }
        self.torn_down = true;
        self.save_session();
        for tab in &mut self.tabs {
            if let Some(session) = tab.term.take() {
                session.shutdown();
            }
        }
        self.tabs.clear();
        self.active = None;
    }

    /// Snapshot the open tabs + UI state to disk so the next launch restores them.
    /// Internal pages (`browser://…`, the `:error(s)` log) are session-specific and
    /// skipped. No-op during headless test runs so they don't clobber a real session.
    fn save_session(&self) {
        if std::env::var("BROWSER_TEST_QUIT_MS").is_ok() {
            return;
        }
        let mut tabs = Vec::new();
        let mut active = 0;
        for (i, tab) in self.tabs.iter().enumerate() {
            if tab.url.starts_with("browser://") || tab.vim.is_some() {
                continue;
            }
            let kind = if tab.term.is_some() {
                "term"
            } else if tab.read || tab.native.is_some() {
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
            search_template: self.search_template.clone(),
            term_command: self.term_command.clone(),
            active,
            window,
            tabs,
        });
    }

    /// Reopen the tabs + UI state saved by a previous run. Read tabs are re-fetched
    /// (so they may arrive slightly out of order, since fetching is async) and
    /// terminals are reopened fresh.
    fn restore_session(&mut self, s: session::Session) {
        self.search_template = s.search_template;
        if !s.term_command.is_empty() {
            self.term_command = s.term_command;
        }
        self.nojs = s.nojs;
        if s.zoom != 1.0 {
            self.set_zoom(s.zoom);
        }
        for tab in &s.tabs {
            match tab.kind.as_str() {
                "term" => self.open_terminal(),
                "read" => self.start_read(&tab.url, false),
                "research" => self.open_research(&tab.url),
                "nojs" => self.open_tab(&tab.url, true),
                _ => self.open_tab(&tab.url, false),
            }
        }
        if !self.tabs.is_empty() {
            self.active = Some(s.active.min(self.tabs.len() - 1));
            self.refresh_visibility();
        }
        self.clear_status();
        self.window.request_redraw();
    }

    fn scroll(&mut self, dy: i32) {
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
    fn scroll_edge(&mut self, bottom: bool) {
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

    fn history(&mut self, forward: bool) {
        if let Some(wv) = self.active_webview() {
            let js = if forward { "history.forward();" } else { "history.back();" };
            let _ = wv.evaluate_script(js);
        }
    }

    /// Reload the active tab: re-extract for an engine-free read tab, else reload
    /// the webview.
    fn reload_active(&mut self) {
        if let Some(url) = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.native.as_ref())
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

    // --- rendering ------------------------------------------------------------

    fn draw(&mut self) -> Result<()> {
        // Keep the engine-free read layout current (cheap no-op unless something
        // that affects layout changed) before we read it for painting.
        self.refresh_read_layout();
        let (w, h) = self.inner();
        // Gather all dynamic text + zoom-scaled metrics up front, while we can
        // still borrow &self.
        let tab_labels = self.tab_labels();
        let welcome = self.active.is_none();
        let native_active = self
            .active
            .and_then(|i| self.tabs.get(i))
            .is_some_and(|t| t.native.is_some());
        let vim_active = self
            .active
            .and_then(|i| self.tabs.get(i))
            .is_some_and(|t| t.vim.is_some());
        let bar_h = self.bar_h() as usize;
        let tab_h = self.tab_bar_h() as usize;
        // Minimized / degenerate size: skip the frame. The tab + command bars are a
        // FIXED height; in a window shorter than they are (e.g. during Show Desktop /
        // minimize, where the client area collapses) drawing them writes past the
        // bottom of the pixel buffer (buf.len() == w*h) and panics. Nothing is
        // visible when minimized anyway, so there's nothing to paint.
        if (h as usize) < tab_h + bar_h + 4 || w < 8 {
            return Ok(());
        }
        // Command bar: draw the `:`-line with horizontal scroll-to-caret so editing
        // a long URL (e.g. after `:edit`, caret parked at the end) keeps the caret —
        // and the tail of the URL — visible. `cmd` is Some((line, scroll_px)) in
        // Command mode and replaces the normal segment list; `caret` is the lit
        // block cursor (x, width) already shifted by the same scroll.
        const MARGIN: i32 = 8;
        let cw = ((self.zoom * 2.0).round() as i32).max(2);
        let (segments, cmd, caret, sel) = if matches!(self.mode, ModeKind::Command | ModeKind::Find)
        {
            // `:` for a command, `/` for a find-in-page search.
            let pre = if self.mode == ModeKind::Find { '/' } else { ':' };
            let line = format!("{pre}{}", self.command);
            let prefix = format!("{pre}{}", &self.command[..self.command_cursor]);
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
                    MARGIN - scroll
                        + self.painter.measure(&format!("{pre}{}", &self.command[..k])) as i32
                };
                (x_of(a).max(MARGIN).max(0) as usize, x_of(b).max(0) as usize)
            });
            (Vec::new(), Some((line, scroll)), caret, sel)
        } else {
            (self.bar_segments(), None, None, None)
        };
        // Quick-maths live result, shown right-aligned in the command bar while the
        // typed line is an arithmetic expression (`:20*8` → `= 160`).
        let math = (self.mode == ModeKind::Command)
            .then(|| self.math_preview())
            .flatten()
            .map(|r| format!("= {r}"));

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
            // Right-aligned quick-maths result, painted last so it sits on top.
            if let Some(text) = &math {
                let tw = p.measure(text) as i32;
                let x = (wz as i32 - MARGIN - tw).max(0) as usize;
                p.text(buf, wz, hz, x, baseline, text, draw::ACCENT);
            }
        };

        if welcome {
            // No engine running: paint the welcome screen, THEN the bar on top so a
            // long welcome list can't bleed into the command bar.
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, draw::BG);
            draw_welcome(p, &mut buf, wz, hz, self.zoom as f32);
            draw_bar(&mut buf);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else if native_active {
            // Engine-free read tab: paint the document ourselves. Content is drawn
            // first; the opaque tab bar and command bar are painted on top, so lines
            // scrolled past either edge are simply covered (no per-line y-clipping).
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, draw::BG);
            let content_top = tab_h as i32;
            if let Some(nr) =
                self.active.and_then(|i| self.tabs.get(i)).and_then(|t| t.native.as_ref())
            {
                let line_h = nr.layout.line_h;
                for (li, line) in nr.layout.lines.iter().enumerate() {
                    let y_top = content_top - nr.scroll + li as i32 * line_h;
                    if y_top + line_h < content_top || y_top > bar_top as i32 {
                        continue;
                    }
                    if line.rule {
                        let ry = y_top + line_h / 2;
                        if ry >= 0 {
                            draw::fill_rect(
                                &mut buf, wz, hz, MARGIN as usize, ry as usize,
                                wz.saturating_sub(MARGIN as usize), ry as usize + 1, draw::DIM,
                            );
                        }
                        continue;
                    }
                    let baseline = y_top + line_h * 3 / 4;
                    if baseline < 0 {
                        continue;
                    }
                    // Find-in-page highlights behind this line's matches.
                    if self.find.active {
                        let chars: Vec<char> =
                            line.runs.iter().flat_map(|r| r.text.chars()).collect();
                        let base = MARGIN + line.indent;
                        for (mi, m) in self.find.matches.iter().enumerate() {
                            if m.line != li {
                                continue;
                            }
                            let s = m.start.min(chars.len());
                            let e = m.end.min(chars.len());
                            let pre: String = chars[..s].iter().collect();
                            let mid: String = chars[s..e].iter().collect();
                            let x0 = base + p.measure(&pre) as i32;
                            let x1 = x0 + p.measure(&mid) as i32;
                            let col =
                                if mi == self.find.current { draw::FIND_CUR } else { draw::FIND };
                            draw::fill_rect(
                                &mut buf, wz, hz, x0.max(0) as usize, y_top.max(0) as usize,
                                x1.max(0) as usize, (y_top + line_h).max(0) as usize, col,
                            );
                        }
                    }
                    // Read-mode caret: visual-selection highlight (behind the text).
                    if let Some(caret) = &nr.caret {
                        if let Some((s0, s1)) = caret.selection_on_row(li) {
                            let chars: Vec<char> =
                                line.runs.iter().flat_map(|r| r.text.chars()).collect();
                            let base = MARGIN + line.indent;
                            let colx = |c: usize| -> i32 {
                                let s: String = chars[..c.min(chars.len())].iter().collect();
                                base + p.measure(&s) as i32
                            };
                            let x0 = colx(s0).max(MARGIN);
                            let x1 = colx(s1).max(MARGIN);
                            if x1 > x0 {
                                draw::fill_rect(
                                    &mut buf, wz, hz, x0 as usize, y_top.max(0) as usize,
                                    x1 as usize, (y_top + line_h).max(0) as usize, draw::SEL,
                                );
                            }
                        }
                    }
                    let mut x = MARGIN + line.indent;
                    for run in &line.runs {
                        x = p.text_clipped(
                            &mut buf, wz, hz, x, baseline as usize, &run.text, run.color, MARGIN,
                        );
                    }
                    // Read-mode caret: block cursor (inverse cell) on the cursor row.
                    if let Some(caret) = &nr.caret {
                        if li == caret.cy {
                            let chars: Vec<char> =
                                line.runs.iter().flat_map(|r| r.text.chars()).collect();
                            let pre: String =
                                chars[..caret.cx.min(chars.len())].iter().collect();
                            let cx0 = MARGIN + line.indent + p.measure(&pre) as i32;
                            let cwid = p.measure("M").max(1) as i32;
                            draw::fill_rect(
                                &mut buf, wz, hz, cx0.max(MARGIN) as usize, y_top.max(0) as usize,
                                (cx0 + cwid) as usize, (y_top + line_h).max(0) as usize, draw::ACCENT,
                            );
                            if let Some(ch) = chars.get(caret.cx) {
                                p.text(
                                    &mut buf, wz, hz, cx0.max(MARGIN) as usize, baseline as usize,
                                    &ch.to_string(), draw::BG,
                                );
                            }
                        }
                    }
                }
            }
            // Hint labels over the visible links (filtered by what's been typed).
            if self.mode == ModeKind::Hint {
                let lh = p.line_height();
                for hint in &self.native_hints {
                    if !hint.label.starts_with(&self.hint_input) {
                        continue;
                    }
                    let lw = p.measure(&hint.label);
                    let bx = hint.x.max(0) as usize;
                    let by = (hint.y - (lh as i32) * 3 / 4).max(0) as usize;
                    draw::fill_rect(&mut buf, wz, hz, bx, by, bx + lw + 4, by + lh, (0xff, 0xd4, 0x00));
                    p.text(&mut buf, wz, hz, bx + 2, hint.y.max(0) as usize, &hint.label, (0x10, 0x10, 0x10));
                }
            }
            // Tab bar + command bar painted on top of the content.
            draw::fill_band(&mut buf, wz, hz, 0, tab_h, draw::BAR_BG);
            draw_tab_bar(p, &mut buf, wz, tab_h, &tab_labels);
            draw_bar(&mut buf);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else if vim_active {
            // Engine-free `:error`/`:errors` vim pager: a monospace text grid with a
            // block cursor and (in visual mode) a selection highlight. Content first,
            // then the opaque bars on top.
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, draw::BG);
            let content_top = tab_h as i32;
            let line_h = p.line_height() as i32;
            let cw = p.measure("M").max(1) as i32;
            if let Some(vb) =
                self.active.and_then(|i| self.tabs.get(i)).and_then(|t| t.vim.as_ref())
            {
                // Pixel x of (absolute) column `col`, measured from the actual glyph
                // advances of the visible slice — so the cursor/selection line up with
                // the text exactly, at any zoom (a fixed cell width drifts on long
                // lines). Columns past end-of-line use the nominal cell width.
                let left = vb.left;
                let col_x = |line: &[char], col: usize| -> i32 {
                    if col <= left {
                        return MARGIN;
                    }
                    let end = col.min(line.len());
                    let slice: String = line[left..end].iter().collect();
                    let mut x = MARGIN + p.measure(&slice) as i32;
                    if col > line.len() {
                        x += (col - line.len()) as i32 * cw;
                    }
                    x
                };
                for r in vb.top..vb.lines.len() {
                    let y_top = content_top + (r - vb.top) as i32 * line_h;
                    if y_top >= bar_top as i32 {
                        break;
                    }
                    let line = &vb.lines[r];
                    let (yt, yb) = (y_top.max(0) as usize, (y_top + line_h).max(0) as usize);
                    // Selection highlight band for this row (visual mode).
                    if let Some((s0, s1)) = vb.selection_on_row(r) {
                        let x0 = col_x(line, s0).max(MARGIN) as usize;
                        let x1 = col_x(line, s1).max(MARGIN) as usize;
                        if x1 > x0 {
                            draw::fill_rect(&mut buf, wz, hz, x0, yt, x1, yb, draw::SEL);
                        }
                    }
                    // Find-in-page highlights for this row.
                    if self.find.active {
                        for (mi, m) in self.find.matches.iter().enumerate() {
                            if m.line != r {
                                continue;
                            }
                            let x0 = col_x(line, m.start).max(MARGIN) as usize;
                            let x1 = col_x(line, m.end).max(MARGIN) as usize;
                            if x1 > x0 {
                                let col =
                                    if mi == self.find.current { draw::FIND_CUR } else { draw::FIND };
                                draw::fill_rect(&mut buf, wz, hz, x0, yt, x1, yb, col);
                            }
                        }
                    }
                    // The visible slice of the line, scrolled left by `vb.left` cols.
                    let baseline = (y_top + line_h * 3 / 4) as usize;
                    if vb.left < line.len() {
                        let text: String = line[vb.left..].iter().collect();
                        p.text_clipped(&mut buf, wz, hz, MARGIN, baseline, &text, draw::FG, MARGIN);
                    }
                    // Block cursor (inverse cell) on the cursor row.
                    if r == vb.cy {
                        let cx0 = col_x(line, vb.cx);
                        let cx1 = col_x(line, vb.cx + 1).max(cx0 + cw);
                        draw::fill_rect(&mut buf, wz, hz, cx0 as usize, yt, cx1 as usize, yb, draw::FG);
                        if let Some(ch) = line.get(vb.cx) {
                            p.text(&mut buf, wz, hz, cx0 as usize, baseline, &ch.to_string(), draw::BG);
                        }
                    }
                }
            }
            draw::fill_band(&mut buf, wz, hz, 0, tab_h, draw::BAR_BG);
            draw_tab_bar(p, &mut buf, wz, tab_h, &tab_labels);
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
                } else if t.vim.is_some() {
                    draw::ERR
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
            // text segment is just the literal command line. (Command/Find are drawn
            // via the dedicated caret path in `draw`, so these arms are unreached.)
            ModeKind::Command => vec![(format!(":{}", self.command), draw::BAR_FG)],
            ModeKind::Find => vec![(format!("/{}", self.command), draw::BAR_FG)],
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
                    // Read-mode caret: show [VISUAL]/[VISUAL LINE] (or [CARET]) + hint.
                    if let Some(caret) = self
                        .active
                        .and_then(|i| self.tabs.get(i))
                        .and_then(|t| t.native.as_ref())
                        .and_then(|n| n.caret.as_ref())
                    {
                        let m = caret.mode_label().unwrap_or("CARET");
                        segs.push((format!("   [{m}]"), draw::ACCENT));
                        segs.push(("  motions select · y yank · Esc exit".into(), draw::DIM));
                    }
                }
                if self.active_is_research() {
                    segs.push(("   [research]".into(), draw::RESEARCH));
                }
                if self.active_is_term() {
                    segs.push(("   [term]".into(), draw::TERM));
                }
                // Vim pager tabs (`:error`/`:errors`, `:res`): show [VISUAL]/[VISUAL
                // LINE] while selecting; otherwise a short motion hint keyed to the
                // tab — the red [error] hint must NOT bleed onto the `:res` monitor.
                if let Some(t) = self.active.and_then(|i| self.tabs.get(i)) {
                    if let Some(vb) = t.vim.as_ref() {
                        match vb.mode_label() {
                            Some(m) => segs.push((format!("   [{m}]"), draw::ACCENT)),
                            None if t.url == "browser://error" => segs.push((
                                "   [error]  v select · y yank · yi( inner ()".into(),
                                draw::ERR,
                            )),
                            None => segs.push(("   v select · y yank".into(), draw::DIM)),
                        }
                    }
                }
                if self.nojs {
                    segs.push(("   [no-js]".into(), draw::ACCENT));
                }
                // Active find-in-page: show the query and (for native tabs) the
                // current/total match counter; `n`/`N` step, Esc clears.
                if self.find.active {
                    let label = if self.active_webview().is_some() {
                        format!("   /{}  · n/N · Esc", self.find.query)
                    } else {
                        format!("   {}  · n/N · Esc", self.find_count_label())
                    };
                    segs.push((label, draw::FIND_CUR));
                }
                if !self.status.is_empty() {
                    let color = if self.status_is_error { draw::ERR } else { draw::DIM };
                    segs.push((format!("   {}", self.status), color));
                }
                segs
            }
        }
    }
}

/// Cap captured command output so a runaway command can't balloon memory.
const TERM_OUTPUT_CAP: usize = 200_000;

/// Maximum number of past errors kept in the session log (oldest dropped first).
const ERROR_LOG_CAP: usize = 200;

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

/// Build the plain-text lines shown by `:error` / `:errors` in the vim pager. Each
/// error becomes a header line (`[HH:MM:SS] :command — error N`) followed by its
/// message (split on newlines), with a blank line between entries. `all = false`
/// renders only the most recent error; `all = true` renders every logged error in
/// chronological order (oldest first, newest last). The text is intentionally flat
/// so vim motions/text-objects work cleanly over it (e.g. `yi(` to grab a
/// `HRESULT(0x…)` token).
fn error_lines(errors: &[ErrorEntry], all: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut emit = |n: usize, e: &ErrorEntry| {
        let cmd = e.command.as_deref().unwrap_or("(no command)");
        out.push(format!("[{}] {} — error {}", e.time, cmd, n + 1));
        for line in e.message.lines() {
            out.push(line.to_string());
        }
        out.push(String::new());
    };
    if all {
        for (n, e) in errors.iter().enumerate() {
            emit(n, e);
        }
    } else if let Some(e) = errors.last() {
        emit(errors.len() - 1, e);
    }
    // Drop the trailing blank so the buffer doesn't end on an empty line.
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// A "word" character for command-bar word motions (`Ctrl+W`, `Ctrl+←/→`):
/// alphanumerics and `_`. Everything else — `/ . : - ? & = # @ ~ + …` — is a
/// separator, so word jumps/deletes stop at URL and path boundaries.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offset of the start of the word before `pos`: skip trailing separators,
/// then the word run. So `Ctrl+W` on `…/foo/bar` erases `bar` (then `/`, then
/// `foo`), not the entire URL.
fn prev_word_boundary(s: &str, pos: usize) -> usize {
    let trimmed = s[..pos].trim_end_matches(|c| !is_word_char(c));
    trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| !is_word_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

/// Byte offset of the end of the word after `pos`: skip leading separators, then
/// the word run.
fn next_word_boundary(s: &str, pos: usize) -> usize {
    let rest = &s[pos..];
    let after_sep = rest.trim_start_matches(|c| !is_word_char(c));
    let sep = rest.len() - after_sep.len();
    let word = after_sep.find(|c| !is_word_char(c)).unwrap_or(after_sep.len());
    pos + sep + word
}

/// Encode `s` as a JavaScript string literal (quoted, with control/quote/`<`
/// escaped) for safe interpolation into an `evaluate_script` call.
fn js_string(s: &str) -> String {
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

/// Case-insensitive (ASCII) find-in-page over `lines`, returning every match as a
/// `(line, char-range)`. Matches don't span lines (each visual/buffer line is
/// searched independently), which is fine for the short queries this is for.
fn find_in_lines(lines: &[String], q: &str) -> Vec<NativeMatch> {
    let needle: Vec<char> = q.chars().map(|c| c.to_ascii_lowercase()).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let hay: Vec<char> = line.chars().map(|c| c.to_ascii_lowercase()).collect();
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()] == needle[..] {
                out.push(NativeMatch { line: li, start: i, end: i + needle.len() });
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Whether `url` points at Google Translate's page-proxy (`*.translate.goog`).
fn is_translate_proxy(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.ends_with(".translate.goog")))
        .unwrap_or(false)
}

/// Reverse a `*.translate.goog` proxy URL back to the original site. Google encodes
/// the host (`.`→`-`, `-`→`--`) and appends `_x_tr_*` query params; we decode the
/// host, keep the path and any genuine query params, and drop the `_x_tr_*` ones.
/// Returns `None` if `url` isn't a translate-proxy URL we can decode.
fn deproxy_translate(url: &str) -> Option<String> {
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

/// Local wall-clock time as `HH:MM:SS`, for stamping logged errors.
#[cfg(windows)]
fn now_hms() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
}

#[cfg(not(windows))]
fn now_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// The `:commands` page: every keybind and command (not customizable yet).
fn commands_document() -> String {
    let normal = help_table(&[
        (":", "open the command bar"),
        ("o", "open a page (prefills “open ”)"),
        ("j / k", "scroll down / up"),
        ("d / u", "scroll half a page down / up"),
        ("g / G", "jump to top / bottom"),
        ("/", "find in page — type to search live; works on web, read & error tabs"),
        ("n / N", "next / previous match (while a search is active); Esc clears"),
        ("i", "insert mode (passthrough on a terminal tab)"),
        ("f", "hint mode — label every link, type the label to follow"),
        ("v / V", "read tabs: caret/visual select — highlight & yank text (y), Esc exits"),
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
    let vimpager = help_table(&[
        ("h j k l · arrows", "move the cursor"),
        ("w / b / e", "next / previous / end of word"),
        ("0 / ^ / $", "start / first non-blank / end of line"),
        ("f / t  (F / T)", "jump to / before a char forward (back); ; , repeat"),
        ("gg / G", "top / bottom; Ctrl+D / Ctrl+U half-page"),
        ("v / V", "charwise / linewise visual select"),
        ("y", "yank: the selection, or with a motion (yy, yw, y$, yf), yt;)"),
        ("yiw · yi( · ya\"", "yank inner/around a text object (word, (), {}, [], <>, quotes)"),
    ]);
    let cmds = help_table(&[
        (":open <url|query> · :o · :t", "open a page (non-URL → search engine)"),
        (":research <url|query> · :rs", "lighter browse: JS on, images kept, media/embeds stripped"),
        (":edit · :e", "edit the current URL (re-opens in the tab's own mode)"),
        (":y · :yank", "copy the current URL to the clipboard"),
        (":read <url>", "engine-free reader: native text render, no WebView2 (j/k scroll, f hint)"),
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
        (":error · :err", "latest error in a read-only vim tab (v/y to select & copy)"),
        (":errors · :errs", "every error this session (newest first), same vim tab"),
        (":res · :resources", "live memory/CPU/disk across the whole browser tree (freezes while you select)"),
        (":commands · :help", "this page"),
        (":version", "version and build information"),
        (":quit · :q", "quit the browser"),
    ]);
    // Bangs: build `!key → description` rows from the core table.
    let bang_rows: Vec<(String, &str)> =
        browser_core::bang_list().into_iter().map(|(k, d)| (format!("!{k} <query>"), d)).collect();
    let bangs = help_table(
        &bang_rows.iter().map(|(k, d)| (k.as_str(), *d)).collect::<Vec<_>>(),
    );
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>commands</title><style>{HELP_CSS}</style></head><body><main>\
         <h1>Commands &amp; keybindings</h1>\
         <p class=\"sub\">Not customizable yet — these are the built-in bindings.</p>\
         <h2>Normal mode</h2>{normal}\
         <h2>Command-line editing</h2>{cmdline}\
         <h2>Other modes</h2>{modes}\
         <h2>Vim pager (:error · :errors · :res · :version · read-mode v/V)</h2>{vimpager}\
         <h2>Commands</h2>{cmds}\
         <h2>Bangs</h2>\
         <p class=\"sub\">A <code>!key</code> token in any open/search target jumps to that \
         site's search (no query → the site's home). Trailing form works too: \
         <code>dragon scimitar !osrs</code>.</p>{bangs}\
         <h2>Quick maths</h2>\
         <p class=\"sub\">Type an arithmetic expression in the command bar \
         (<code>+ - * / %  ^</code>, parentheses) to see the result live, e.g. \
         <code>:20*8</code> → <code>= 160</code>. Press Enter to replace the line with the \
         result so you can copy it or keep calculating (<code>160+10</code>).</p>\
         </main></body></html>"
    )
}

/// The `:version` page: build/runtime details about this browser.
/// Plain-text lines for the `:version` pager (navigable/yankable with vim motions).
fn version_lines() -> Vec<String> {
    let kv = [
        ("Name", env!("CARGO_PKG_NAME")),
        ("Version", env!("CARGO_PKG_VERSION")),
        ("Description", env!("CARGO_PKG_DESCRIPTION")),
        ("Authors", env!("CARGO_PKG_AUTHORS")),
        ("Engine", "WebView2 (Chromium) via wry 0.55 — loaded on demand"),
        ("Windowing", "tao 0.35 + softbuffer/fontdue native chrome"),
        ("Terminal", "xterm.js + a browser-pty-host companion (ConPTY)"),
        ("Platform", std::env::consts::OS),
        ("Architecture", std::env::consts::ARCH),
    ];
    let mut lines = vec![
        format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        String::new(),
    ];
    for (k, v) in kv {
        lines.push(format!("  {:<14}{}", format!("{k}:"), v));
    }
    lines.push(String::new());
    lines.push("A modal, mode-dispatching browser — only what's needed, when needed.".into());
    lines
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

#[cfg(test)]
mod tests {
    use super::vim::{Key, TextBuffer};
    use super::{
        deproxy_translate, error_lines, is_translate_proxy, next_word_boundary, prev_word_boundary,
        ErrorEntry,
    };

    #[test]
    fn ctrl_w_deletes_one_url_segment_at_a_time() {
        let url = "https://example.com/foo/bar";
        // Caret at the end: prev word is `bar`, leaving the trailing slash.
        let p1 = prev_word_boundary(url, url.len());
        assert_eq!(&url[..p1], "https://example.com/foo/");
        // Again from there: skip the `/`, delete `foo`.
        let p2 = prev_word_boundary(url, p1);
        assert_eq!(&url[..p2], "https://example.com/");
        // Not the whole thing in one go.
        assert_ne!(p1, 0);
    }

    #[test]
    fn word_motions_step_over_separators() {
        let s = "ab.cd";
        assert_eq!(next_word_boundary(s, 0), 2); // end of `ab`
        assert_eq!(next_word_boundary(s, 2), 5); // skip `.`, end of `cd`
        assert_eq!(prev_word_boundary(s, 5), 3); // start of `cd`
    }

    #[test]
    fn find_in_lines_is_case_insensitive_and_per_line() {
        let lines = vec!["The Rust language".to_string(), "rust rust".to_string()];
        let m = super::find_in_lines(&lines, "rust");
        // One on line 0 (case-insensitive), two on line 1.
        assert_eq!(m.len(), 3);
        assert_eq!((m[0].line, m[0].start, m[0].end), (0, 4, 8));
        assert_eq!((m[1].line, m[1].start), (1, 0));
        assert_eq!((m[2].line, m[2].start), (1, 5));
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

    fn entry(time: &str, command: Option<&str>, message: &str) -> ErrorEntry {
        ErrorEntry {
            time: time.into(),
            command: command.map(Into::into),
            message: message.into(),
        }
    }

    #[test]
    fn latest_error_has_header_then_message_lines() {
        let errs = vec![
            entry("00:00:01", Some(":open a"), "boom"),
            entry("00:00:02", None, "bad\nthings"),
        ];
        let lines = error_lines(&errs, false);
        assert_eq!(lines[0], "[00:00:02] (no command) — error 2");
        assert_eq!(&lines[1..], &["bad".to_string(), "things".to_string()]);
    }

    #[test]
    fn all_errors_are_oldest_first_with_command_and_time() {
        let errs = vec![entry("00:00:01", Some(":open a"), "e1"), entry("00:00:09", Some(":bad"), "e2")];
        let lines = error_lines(&errs, true);
        assert_eq!(lines[0], "[00:00:01] :open a — error 1");
        assert_eq!(lines[1], "e1");
        assert_eq!(lines[2], ""); // blank separator
        assert_eq!(lines[3], "[00:00:09] :bad — error 2");
        assert_eq!(lines[4], "e2");
    }

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
