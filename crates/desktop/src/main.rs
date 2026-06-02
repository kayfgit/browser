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

use std::num::NonZeroU32;
use std::process::Command;
use std::rc::Rc;

use anyhow::{Context as _, Result};
use tao::event::{ElementState, Event, KeyEvent, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::keyboard::{Key, KeyCode, ModifiersState};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows};

mod draw;
use draw::Painter;

/// Height of the bottom command/status bar, in physical pixels.
const BAR_H: u32 = 28;
/// Height of the top tab bar (only shown when at least one tab is open).
const TAB_BAR_H: u32 = 24;

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

/// Events posted from webview IPC back into the event loop.
enum UserEvent {
    /// Leave insert/passthrough: move focus from the page back to the shell.
    ExitToNormal,
    /// Promote insert → passthrough (Ctrl+V while typing); the page keeps focus.
    InsertToPassthrough,
    /// Reclaim keyboard focus for the shell (e.g. after a page finishes loading
    /// and WebView2 has grabbed focus), unless the page should keep focus.
    FocusShell,
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
    /// Accumulated label characters while in Hint mode.
    hint_input: String,
    status: String,
    tabs: Vec<Tab>,
    active: Option<usize>,
    /// Current keyboard modifier state (tracked via ModifiersChanged).
    modifiers: ModifiersState,
    /// When true, new tabs are opened with JavaScript disabled.
    nojs: bool,
    quit: bool,
}

fn main() -> Result<()> {
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

    let painter = Painter::new(17.0).context("loading font")?;

    let mut app = App {
        window: window.clone(),
        _context: context,
        surface,
        painter,
        proxy,
        mode: ModeKind::Normal,
        command: String::new(),
        hint_input: String::new(),
        status: String::new(),
        tabs: Vec::new(),
        active: None,
        modifiers: ModifiersState::default(),
        nojs: false,
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
            Event::UserEvent(UserEvent::FocusShell) => match app.mode {
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
            },
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
    });
}

impl App {
    // --- geometry -------------------------------------------------------------

    fn inner(&self) -> (u32, u32) {
        let s = self.window.inner_size();
        (s.width.max(1), s.height.max(1))
    }

    /// Tab-bar height: present only while at least one tab is open.
    fn tab_bar_h(&self) -> u32 {
        if self.tabs.is_empty() {
            0
        } else {
            TAB_BAR_H
        }
    }

    /// Bounds for a content webview: full width, between the tab bar and command bar.
    fn content_rect(&self) -> Rect {
        let (w, h) = self.inner();
        let top = self.tab_bar_h();
        Rect {
            position: PhysicalPosition::new(0_i32, top as i32).into(),
            size: PhysicalSize::new(w, h.saturating_sub(top + BAR_H)).into(),
        }
    }

    fn on_resize(&mut self, _w: u32, _h: u32) {
        let rect = self.content_rect();
        if let Some(wv) = self.active_webview() {
            let _ = wv.set_bounds(rect);
        }
        self.window.request_redraw();
    }

    // --- tab access -----------------------------------------------------------

    fn active_webview(&self) -> Option<&WebView> {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| &t.webview)
    }

    fn active_url(&self) -> Option<&str> {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.url.as_str())
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
            if key.physical_key == KeyCode::KeyV {
                self.enter_passthrough();
            }
            return;
        }
        let (_, h) = self.inner();
        let page = (h as i32 - BAR_H as i32).max(40);
        match &key.logical_key {
            Key::Character(s) => match *s {
                ":" => self.enter_command(""),
                "o" => self.enter_command("open "),
                "j" => self.scroll(80),
                "k" => self.scroll(-80),
                "d" => self.scroll(page / 2),
                "u" => self.scroll(-page / 2),
                "i" => self.enter_insert(),
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
            Key::Space => self.scroll(page * 9 / 10),
            Key::ArrowDown => self.scroll(80),
            Key::ArrowUp => self.scroll(-80),
            _ => {}
        }
    }

    fn key_command(&mut self, key: &KeyEvent) {
        match &key.logical_key {
            Key::Enter => {
                let line = std::mem::take(&mut self.command);
                self.mode = ModeKind::Normal;
                self.run_command(&line);
            }
            Key::Escape => {
                self.command.clear();
                self.mode = ModeKind::Normal;
            }
            Key::Backspace => {
                self.command.pop();
            }
            Key::Space => self.command.push(' '),
            Key::Character(s) => self.command.push_str(s),
            _ => {}
        }
    }

    fn enter_command(&mut self, prefill: &str) {
        self.mode = ModeKind::Command;
        self.command = prefill.to_string();
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
            "read" => {
                if rest.is_empty() {
                    self.status = "usage: :read <url>".into();
                } else {
                    self.start_read(rest);
                }
            }
            "te" | "term" => {
                if rest.is_empty() {
                    self.status = "usage: :te <command>".into();
                } else {
                    self.run_term(rest);
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
            "" => {}
            other => self.status = format!("unknown command: {other}"),
        }
    }

    fn open_tab(&mut self, target: &str, disable_js: bool) {
        let Some(url) = browser_core::normalize_url(target) else {
            self.status = format!("invalid url: {target}");
            return;
        };
        match self.build_content_webview(Source::Url(url.clone()), disable_js) {
            Ok(webview) => {
                self.tabs.push(Tab { webview, url: url.clone(), nojs: disable_js, read: false });
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

    /// Build a child webview from either a URL or an inline HTML document, with
    /// the full shell bridge (keybindings, focus reclaim, hint mode).
    fn build_content_webview(&self, source: Source, disable_js: bool) -> Result<WebView> {
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
            .with_initialization_script(BRIDGE_JS)
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
        // JS stays enabled: the article has no page scripts (readability stripped
        // them), but our bridge/scroll/hint need JS. Leanness comes from the
        // stripped DOM, not from disabling the engine.
        match self.build_content_webview(Source::Html(doc), false) {
            Ok(webview) => {
                self.tabs.push(Tab { webview, url: url.to_string(), nojs: false, read: true });
                self.active = Some(self.tabs.len() - 1);
                self.refresh_visibility();
                self.window.set_focus();
                self.status.clear();
            }
            Err(e) => self.status = format!("read failed: {e:#}"),
        }
    }

    fn close_active(&mut self) {
        let Some(i) = self.active else {
            self.status = "no tab to close".into();
            return;
        };
        // Dropping the WebView destroys the WebView2 control and frees its renderer.
        let _ = self.tabs.remove(i);
        self.active = if self.tabs.is_empty() {
            None
        } else {
            Some(i.min(self.tabs.len() - 1))
        };
        self.refresh_visibility();
        self.window.set_focus();
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
            }
        }
        self.window.request_redraw();
    }

    /// Drop every webview before exiting so the WebView2 processes are closed
    /// gracefully rather than orphaned.
    fn teardown(&mut self) {
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
        // Gather all dynamic text up front, while we can still borrow &self.
        let segments = self.bar_segments();
        let tab_labels = self.tab_labels();
        let welcome = self.active.is_none();

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
        let bar_top = hz.saturating_sub(BAR_H as usize);

        draw::fill_band(&mut buf, wz, hz, bar_top, hz, draw::BAR_BG);
        let baseline = bar_top + (BAR_H as usize * 2 / 3);
        let mut x = 8;
        for (text, color) in &segments {
            x = p.text(&mut buf, wz, hz, x, baseline, text, *color) + 6;
        }

        if welcome {
            // No engine running: paint the welcome screen behind the bar.
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, draw::BG);
            draw_welcome(p, &mut buf, wz, hz);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else {
            // A webview covers the middle; redraw only the top tab bar and the
            // bottom command bar so we never paint over the live page.
            draw::fill_band(&mut buf, wz, hz, 0, TAB_BAR_H as usize, draw::BAR_BG);
            draw_tab_bar(p, &mut buf, wz, &tab_labels);
            let top = softbuffer::Rect {
                x: 0,
                y: 0,
                width: NonZeroU32::new(w).unwrap(),
                height: NonZeroU32::new(TAB_BAR_H).unwrap(),
            };
            let bottom = softbuffer::Rect {
                x: 0,
                y: bar_top as u32,
                width: NonZeroU32::new(w).unwrap(),
                height: NonZeroU32::new(BAR_H).unwrap(),
            };
            buf.present_with_damage(&[top, bottom])
                .map_err(|e| anyhow::anyhow!("present: {e}"))?;
        }
        Ok(())
    }

    /// (label, is_active, is_read) for each open tab, in order.
    fn tab_labels(&self) -> Vec<(String, bool, bool)> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (short_label(&t.url), Some(i) == self.active, t.read))
            .collect()
    }

    /// Whether the active tab is a read-mode tab.
    fn active_is_read(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.read).unwrap_or(false)
    }

    /// Build the bar as a sequence of (text, color) segments drawn left to right.
    fn bar_segments(&self) -> Vec<(String, draw::Rgb)> {
        match self.mode {
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

/// A clean dark reading stylesheet for read mode.
const READ_CSS: &str = "html{background:#1e1e1e;color:#d0d0d0}body{margin:0}\
main{max-width:760px;margin:48px auto;padding:0 22px;\
font:17px/1.65 -apple-system,Segoe UI,Roboto,sans-serif}\
h1,h2,h3,h4{line-height:1.25;color:#fff}h1{font-size:1.9em}\
a{color:#6cb6ff}img,video{max-width:100%;height:auto}\
pre,code{font-family:Consolas,monospace;font-size:.92em}\
pre{background:#2a2a2a;padding:12px;overflow:auto;border-radius:6px}\
code{background:#2a2a2a;padding:1px 4px;border-radius:3px}\
pre code{background:none;padding:0}\
blockquote{border-left:3px solid #444;margin:0 0 1em;padding-left:16px;color:#a8a8a8}\
hr{border:none;border-top:1px solid #333}";

/// Wrap extracted article HTML in a full document with a `<base>` (so relative
/// links/images resolve against the source) and the reading stylesheet.
fn read_document(url: &str, title: &str, article_html: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <base href=\"{url}\"><title>{title}</title><style>{READ_CSS}</style></head>\
         <body><main>{article_html}</main></body></html>"
    )
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
fn draw_tab_bar(p: &Painter, buf: &mut [u32], w: usize, labels: &[(String, bool, bool)]) {
    let h = TAB_BAR_H as usize;
    let baseline = h * 2 / 3;
    let mut x = 8;
    for (i, (label, active, read)) in labels.iter().enumerate() {
        let color = if *read {
            draw::READ
        } else if *active {
            draw::ACCENT
        } else {
            draw::DIM
        };
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

fn draw_welcome(p: &Painter, buf: &mut [u32], w: usize, h: usize) {
    let lh = p.line_height();
    let mut y = lh * 2;
    let x = 40;
    let after = p.text(buf, w, h, x, y, "browser", draw::ACCENT);
    p.text(buf, w, h, after + 16, y, "— lightweight modal shell", draw::DIM);
    y += lh * 2;
    for (keys, desc) in [
        (":open <url>   or   o <url>", "open a page (boots the engine on demand)"),
        (":read <url>", "reader mode: extract the article, no JS/ads (green tab)"),
        (":te <command>", "run a local command; result shows in the command bar"),
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
        (":f", "toggle fullscreen"),
        (":resize", "resize mode — then hjkl to size, Esc to finish"),
        (":move", "move mode — then hjkl to reposition, Esc to finish"),
        (":q", "quit"),
    ] {
        p.text(buf, w, h, x, y, keys, draw::FG);
        p.text(buf, w, h, x + 320, y, desc, draw::DIM);
        y += lh + lh / 3;
    }
}
