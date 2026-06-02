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
use std::rc::Rc;

use anyhow::{Context as _, Result};
use tao::event::{ElementState, Event, KeyEvent, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::keyboard::{Key, KeyCode, ModifiersState};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder};

mod draw;
use draw::Painter;

/// Height of the bottom command/status bar, in physical pixels.
const BAR_H: u32 = 28;
/// Height of the top tab bar (only shown when at least one tab is open).
const TAB_BAR_H: u32 = 24;

/// Injected into every page: report Shift+Escape back to the shell so we can
/// leave passthrough mode even while the page holds keyboard focus. Bare Escape
/// is deliberately NOT intercepted, so terminal apps (vim in ttyd, etc.) keep it.
const ESC_SCRIPT: &str = r#"
(function () {
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && e.shiftKey) {
      e.preventDefault();
      window.ipc.postMessage('leave-passthrough');
    }
  }, true);
})();
"#;

/// Events posted from webview IPC back into the event loop.
enum UserEvent {
    /// Leave passthrough: move focus from the page back to the shell.
    ReturnFocus,
    /// Reclaim keyboard focus for the shell (e.g. after a page finishes loading
    /// and WebView2 has grabbed focus), unless we are intentionally in passthrough.
    FocusShell,
    Quit,
}

#[derive(Clone, Copy, PartialEq)]
enum ModeKind {
    Normal,
    Command,
    /// All keys go to the page (qutebrowser passthrough). Enter: Ctrl+V or `i`.
    /// Leave: Shift+Esc (handled by the injected script while the page is focused).
    Passthrough,
}

struct Tab {
    webview: WebView,
    url: String,
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
            Event::UserEvent(UserEvent::ReturnFocus) => app.return_focus(),
            Event::UserEvent(UserEvent::FocusShell) => {
                // The page just loaded and may have stolen focus; take it back
                // unless the user deliberately switched to passthrough.
                if app.mode != ModeKind::Passthrough {
                    app.window.set_focus();
                }
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
            ModeKind::Passthrough => {
                // The page usually has OS focus, so the injected Shift+Esc hook is
                // what leaves passthrough. This only fires if the page isn't focused.
                if matches!(key.logical_key, Key::Escape) && self.modifiers.shift_key() {
                    self.return_focus();
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
                "i" => self.enter_passthrough(),
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

    fn enter_passthrough(&mut self) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus();
            self.mode = ModeKind::Passthrough;
        } else {
            self.status = "no page — open one first".into();
        }
    }

    fn return_focus(&mut self) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus_parent();
        }
        self.mode = ModeKind::Normal;
        self.window.set_focus();
        self.window.request_redraw();
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
            "" => {}
            other => self.status = format!("unknown command: {other}"),
        }
    }

    fn open_tab(&mut self, target: &str, disable_js: bool) {
        let Some(url) = browser_core::normalize_url(target) else {
            self.status = format!("invalid url: {target}");
            return;
        };
        match self.build_webview(&url, disable_js) {
            Ok(webview) => {
                self.tabs.push(Tab { webview, url: url.clone() });
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

    fn build_webview(&self, url: &str, disable_js: bool) -> Result<WebView> {
        let ipc_proxy = self.proxy.clone();
        let load_proxy = self.proxy.clone();
        let mut builder = WebViewBuilder::new()
            .with_url(url)
            .with_bounds(self.content_rect())
            .with_focused(false)
            .with_initialization_script(ESC_SCRIPT)
            .with_ipc_handler(move |req| {
                if req.body().as_str() == "leave-passthrough" {
                    let _ = ipc_proxy.send_event(UserEvent::ReturnFocus);
                }
            })
            // WebView2 tends to grab focus when navigation completes; reclaim it
            // for the shell so the keyboard UI keeps working in Normal mode.
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

    /// (label, is_active) for each open tab, in order.
    fn tab_labels(&self) -> Vec<(String, bool)> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| (short_label(&t.url), Some(i) == self.active))
            .collect()
    }

    /// Build the bar as a sequence of (text, color) segments drawn left to right.
    fn bar_segments(&self) -> Vec<(String, draw::Rgb)> {
        match self.mode {
            ModeKind::Command => vec![(format!(":{}", self.command), draw::BAR_FG)],
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
fn draw_tab_bar(p: &Painter, buf: &mut [u32], w: usize, labels: &[(String, bool)]) {
    let h = TAB_BAR_H as usize;
    let baseline = h * 2 / 3;
    let mut x = 8;
    for (i, (label, active)) in labels.iter().enumerate() {
        let (text, color) = if *active {
            (format!("[{}:{}]", i + 1, label), draw::ACCENT)
        } else {
            (format!(" {}:{} ", i + 1, label), draw::DIM)
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
        ("j / k / Space / d / u", "scroll the page"),
        ("Ctrl+V  (or i)", "passthrough: all keys go to the page (for ttyd, web apps)"),
        ("Shift+Esc", "leave passthrough (bare Esc passes through to the page)"),
        (":nojs            ", "toggle JavaScript off for new tabs"),
        (":nojs <url>", "open a page with JavaScript disabled"),
        ("n / p", "next / previous tab"),
        ("1 .. 9", "jump straight to tab N"),
        ("< / >", "move the current tab left / right"),
        ("x", "close the current tab (frees its memory)"),
        ("H / L", "history back / forward"),
        (":q", "quit"),
    ] {
        p.text(buf, w, h, x, y, keys, draw::FG);
        p.text(buf, w, h, x + 320, y, desc, draw::DIM);
        y += lh + lh / 3;
    }
}
