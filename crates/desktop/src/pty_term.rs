//! Native terminal: an in-process `alacritty_terminal` VT engine, rendered by our
//! own softbuffer/fontdue painter — no WebView2. The PTY still lives in the
//! `browser-pty-host` companion (ConPTY isolation); this just parses its byte
//! stream into a cell grid and paints it. Replaces the old xterm.js-in-WebView2
//! terminal, so a `:te` tab now spawns zero WebView2 processes.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb as VtRgb};

// Re-exported so the shell can drive vi-mode without importing alacritty directly.
pub use alacritty_terminal::vi_mode::ViMotion;

use crate::draw::{self, Painter, Rgb};

/// Default 16-color ANSI palette — Windows Terminal's "Campbell" scheme, so colors
/// match what the user sees elsewhere on Windows. Used when the program hasn't set
/// its own via OSC. 0–7 normal, 8–15 bright.
const ANSI: [Rgb; 16] = [
    (0x0c, 0x0c, 0x0c), // black
    (0xc5, 0x0f, 0x1f), // red
    (0x13, 0xa1, 0x0e), // green
    (0xc1, 0x9c, 0x00), // yellow
    (0x00, 0x37, 0xda), // blue
    (0x88, 0x17, 0x98), // magenta
    (0x3a, 0x96, 0xdd), // cyan
    (0xcc, 0xcc, 0xcc), // white
    (0x76, 0x76, 0x76), // bright black
    (0xe7, 0x48, 0x56), // bright red
    (0x16, 0xc6, 0x0c), // bright green
    (0xf9, 0xf1, 0xa5), // bright yellow
    (0x3b, 0x78, 0xff), // bright blue
    (0xb4, 0x00, 0x9e), // bright magenta
    (0x61, 0xd6, 0xd6), // bright cyan
    (0xf2, 0xf2, 0xf2), // bright white
];
/// Default foreground / background when the program uses the default colors. The
/// background stays the UI's dark grey (not Campbell's pure black) for cohesion.
const FG: Rgb = (0xcc, 0xcc, 0xcc);
/// Terminal background — also the content-area fill behind the grid.
pub const BG: Rgb = (0x1a, 0x1a, 0x1a);

/// The resolved terminal render style: default fg/bg plus the 16-color ANSI palette.
/// Built from the persisted [`TermConfig`](crate::config::TermConfig) by
/// [`rebuild_term_style`](crate::App::rebuild_term_style) — a named scheme first,
/// then explicit fg/bg overrides on top. Default = Campbell on the UI grey.
#[derive(Clone, Copy)]
pub struct TermStyle {
    pub fg: Rgb,
    pub bg: Rgb,
    pub ansi: [Rgb; 16],
}

impl Default for TermStyle {
    fn default() -> Self {
        TermStyle { fg: FG, bg: BG, ansi: ANSI }
    }
}

/// Scheme names accepted by [`scheme`], in the order shown to the user/AI.
pub const SCHEMES: &[&str] =
    &["campbell", "dracula", "gruvbox", "nord", "solarized", "onedark", "monokai"];

/// Look up a built-in colour scheme by name (case-insensitive). Palettes are the
/// widely-published values for each theme; `campbell` is the built-in default.
pub fn scheme(name: &str) -> Option<TermStyle> {
    // (fg, bg, [16 ANSI colors 0..=7 normal, 8..=15 bright])
    let hex = |n: u32| -> Rgb { (((n >> 16) & 0xff) as u8, ((n >> 8) & 0xff) as u8, (n & 0xff) as u8) };
    let build = |fg: u32, bg: u32, pal: [u32; 16]| -> TermStyle {
        TermStyle { fg: hex(fg), bg: hex(bg), ansi: pal.map(hex) }
    };
    Some(match name.to_ascii_lowercase().as_str() {
        "campbell" | "default" => TermStyle::default(),
        "dracula" => build(0xf8f8f2, 0x282a36, [
            0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
            0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
        ]),
        "gruvbox" => build(0xebdbb2, 0x282828, [
            0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
            0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
        ]),
        "nord" => build(0xd8dee9, 0x2e3440, [
            0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
            0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
        ]),
        "solarized" => build(0x839496, 0x002b36, [
            0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
            0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
        ]),
        "onedark" => build(0xabb2bf, 0x282c34, [
            0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
            0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
        ]),
        "monokai" => build(0xf8f8f2, 0x272822, [
            0x272822, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf8f8f2,
            0x75715e, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf9f8f5,
        ]),
        _ => return None,
    })
}

/// Event sink for the VT engine. The only events we must act on are `PtyWrite`s —
/// replies the engine generates to device queries (notably the `ESC[6n` cursor-
/// position report the shell sends on startup and STALLS waiting for). We queue
/// those bytes; the app drains them after each feed and writes them to the PTY.
#[derive(Clone)]
pub struct TermListener {
    out: Arc<Mutex<Vec<u8>>>,
    /// The terminal's current OSC title (set by the running program: e.g. vim sets
    /// it to the open file, Claude Code to "Claude Code"). `None` once reset/unset.
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for TermListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                self.out.lock().unwrap().extend_from_slice(text.as_bytes());
            }
            Event::Title(t) => {
                let t = t.trim();
                *self.title.lock().unwrap() = (!t.is_empty()).then(|| t.to_string());
            }
            Event::ResetTitle => *self.title.lock().unwrap() = None,
            _ => {}
        }
    }
}

/// One native terminal: the VT engine + parser + its current grid size. The PTY
/// process handles live alongside this in the shell's `TermSession`.
pub struct PtyTerm {
    pub vt: Term<TermListener>,
    parser: Processor,
    pub cols: usize,
    pub rows: usize,
    out: Arc<Mutex<Vec<u8>>>,
    title: Arc<Mutex<Option<String>>>,
    /// Incremental scanner for the shell-integration cwd escapes (see [`CwdScan`]).
    cwd_scan: CwdScan,
    /// The shell's current working directory, as last reported over OSC 9;9 / OSC 7.
    /// A Windows path (`C:\…`, `\\wsl.localhost\…`) or a Linux one (`/home/…` from
    /// inside WSL). `None` until a shell-integration prompt first reports it.
    cwd: Option<String>,
}

/// Incremental scanner for the two cwd escape sequences shells emit each prompt
/// when shell integration is on: `OSC 9;9;<path> ST/BEL` (the ConEmu / Windows
/// Terminal convention — cmd gets it injected automatically via `PROMPT`, pwsh
/// via a one-line `$PROFILE` hook) and `OSC 7;file://<host><path> ST/BEL` (the
/// Unix convention — bash/zsh inside WSL). Alacritty's VT engine drops OSCs it
/// doesn't know, so these are scanned from the RAW byte stream — statefully,
/// because a sequence can split across PTY read chunks.
#[derive(Default)]
struct CwdScan {
    /// 0 = ground, 1 = saw ESC, 2 = inside OSC, 3 = saw ESC inside OSC (ST?).
    state: u8,
    buf: Vec<u8>,
}

/// Longest OSC payload kept; anything bigger (e.g. a base64 clipboard write) is
/// discarded rather than buffered.
const OSC_CAP: usize = 4096;

impl CwdScan {
    /// Advance by one raw byte; returns a completed OSC payload when one ends.
    fn push(&mut self, b: u8) -> Option<Vec<u8>> {
        match self.state {
            0 => {
                if b == 0x1b {
                    self.state = 1;
                }
            }
            1 => {
                if b == b']' {
                    self.state = 2;
                    self.buf.clear();
                } else {
                    self.state = if b == 0x1b { 1 } else { 0 };
                }
            }
            2 => match b {
                0x07 => {
                    self.state = 0;
                    return Some(std::mem::take(&mut self.buf));
                }
                0x1b => self.state = 3,
                _ => {
                    if self.buf.len() < OSC_CAP {
                        self.buf.push(b);
                    }
                }
            },
            _ => {
                self.state = if b == 0x1b { 1 } else { 0 };
                if b == b'\\' {
                    return Some(std::mem::take(&mut self.buf));
                }
                self.buf.clear();
            }
        }
        None
    }
}

/// Decode `%XX` percent-escapes (an OSC 7 `file://` URI is percent-encoded).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| u8::from_str_radix(&s[i + 1..i + 3], 16).ok())
            .flatten();
        match decoded {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The cwd carried by a completed OSC payload, if it is one of the two cwd
/// sequences. `9;9;<path>` → the path verbatim (surrounding quotes stripped —
/// some prompts emit `"%cd%"` quoted); `7;file://<host><path>` → `C:\…` for a
/// Windows `file:///C:/…` URI, else the percent-decoded Linux path.
fn osc_cwd(payload: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(payload);
    if let Some(path) = s.strip_prefix("9;9;") {
        let path = path.trim().trim_matches('"');
        return (!path.is_empty()).then(|| path.to_string());
    }
    let uri = s.strip_prefix("7;")?;
    let rest = uri.strip_prefix("file://")?;
    let path = percent_decode(&rest[rest.find('/').unwrap_or(rest.len())..]);
    if path.is_empty() || path == "/" {
        return None;
    }
    // `/C:/Users/x` → `C:\Users\x`; anything else is a Linux path, kept as-is.
    let b = path.as_bytes();
    if b.len() >= 3 && b[2] == b':' && b[1].is_ascii_alphabetic() {
        return Some(path[1..].replace('/', "\\"));
    }
    Some(path)
}

/// Default scrollback kept per terminal, in lines. Each kept line costs grid
/// memory for the terminal's lifetime, so heavy multi-terminal users can lower it.
pub const DEFAULT_SCROLLBACK: usize = 5000;

impl PtyTerm {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let out = Arc::new(Mutex::new(Vec::new()));
        let title = Arc::new(Mutex::new(None));
        let listener = TermListener { out: out.clone(), title: title.clone() };
        // Keep scrollback so copy-mode (Shift+Esc) can page back through history.
        let config = Config { scrolling_history: scrollback, ..Config::default() };
        let vt = Term::new(config, &TermSize::new(cols, rows), listener);
        PtyTerm {
            vt,
            parser: Processor::new(),
            cols,
            rows,
            out,
            title,
            cwd_scan: CwdScan::default(),
            cwd: None,
        }
    }

    /// The shell's current working directory, as last reported by shell
    /// integration (OSC 9;9 / OSC 7). `None` when no prompt has reported one.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// The terminal's current OSC title, if the running program set one.
    pub fn title(&self) -> Option<String> {
        self.title.lock().unwrap().clone()
    }

    // --- copy / vi mode (Alacritty's own) -------------------------------------
    // Shift+Esc toggles the engine into vi mode: a vi cursor moves over the LIVE
    // colored grid (no text snapshot), `v`/`V` start a grid selection that motions
    // extend, and `selection_to_string` yanks the real text. This is how Alacritty/
    // WezTerm do copy-mode.

    pub fn is_vi(&self) -> bool {
        self.vt.mode().contains(TermMode::VI)
    }
    pub fn toggle_vi(&mut self) {
        self.vt.toggle_vi_mode();
    }

    /// Extend the active selection to the vi cursor. Alacritty's own recompute only
    /// extends a NON-empty selection, so a just-started `v` (zero-width) would never
    /// grow — force it so the very first motion already extends.
    fn force_extend(&mut self) {
        let p = self.vt.vi_mode_cursor.point;
        if let Some(sel) = self.vt.selection.as_mut() {
            sel.update(p, Side::Left);
            sel.include_all();
        }
    }

    pub fn vi_motion(&mut self, m: ViMotion) {
        self.vt.vi_motion(m);
        // Keep the viewport on the cursor (alacritty's vi_motion moves the cursor
        // into scrollback but does NOT scroll the display itself).
        let p = self.vt.vi_mode_cursor.point;
        self.vt.scroll_to_point(p);
        self.force_extend();
    }

    /// Move the vi cursor (and viewport) to a line, clamped to the buffer.
    fn vi_goto_line(&mut self, line: i32, column: Column) {
        let lo = self.vt.topmost_line().0;
        let hi = self.vt.bottommost_line().0;
        let p = Point::new(Line(line.clamp(lo, hi)), column);
        self.vt.vi_goto_point(p);
        self.force_extend();
    }
    pub fn vi_top(&mut self) {
        let line = self.vt.topmost_line().0;
        self.vi_goto_line(line, Column(0));
    }
    pub fn vi_bottom(&mut self) {
        let line = self.vt.bottommost_line().0;
        self.vi_goto_line(line, Column(0));
    }
    /// Scroll the vi cursor by `lines` (negative = up), keeping it in view.
    pub fn vi_scroll(&mut self, lines: i32) {
        let cur = self.vt.vi_mode_cursor.point;
        self.vi_goto_line(cur.line.0 + lines, cur.column);
    }

    /// Vi find-char on the current line (`f`/`F`/`t`/`T`): move the vi cursor to the
    /// next/previous occurrence of `target`. `forward` = search rightward (`f`/`t`);
    /// `till` = stop one cell short of the match (`t`/`T`). A no-op if the char isn't
    /// found on the line, so an unmatched find leaves the cursor put (like vim).
    pub fn vi_find_char(&mut self, target: char, forward: bool, till: bool) {
        let point = self.vt.vi_mode_cursor.point;
        let line = point.line;
        let start = point.column.0;
        let cols = self.cols;
        let grid = self.vt.grid();
        let hit = |c: usize| grid[Point::new(line, Column(c))].c == target;
        let found = if forward {
            (start + 1..cols).find(|&c| hit(c))
        } else {
            (0..start).rev().find(|&c| hit(c))
        };
        if let Some(mut col) = found {
            // `t`/`T` land just before the target (toward the cursor's side).
            if till {
                col = if forward { col.saturating_sub(1) } else { col + 1 };
            }
            self.vt.vi_goto_point(Point::new(line, Column(col)));
            self.force_extend();
        }
    }

    /// Scroll the viewport through scrollback by `lines` (positive = up, toward
    /// older output) without entering vi mode — for mouse-wheel scrolling a live
    /// shell. New PTY output snaps the view back to the bottom, as terminals do.
    pub fn scroll_display(&mut self, lines: i32) {
        self.vt.grid_mut().scroll_display(Scroll::Delta(lines));
    }

    /// Snap the viewport back to the live (bottom) line. Copy/vi-mode motions park
    /// the display in scrollback and NOTHING else resets it on exit — Alacritty's
    /// `toggle_vi_mode` only moves the cursor, and quiet shells produce no output
    /// to snap the view — so resuming the shell calls this (the "camera stuck
    /// after Esc" bug).
    pub fn scroll_to_bottom(&mut self) {
        self.vt.grid_mut().scroll_display(Scroll::Bottom);
    }

    /// Start a selection at the vi cursor (`lines` = linewise `V` vs charwise `v`).
    pub fn start_selection(&mut self, lines: bool) {
        let ty = if lines { SelectionType::Lines } else { SelectionType::Simple };
        let p = self.vt.vi_mode_cursor.point;
        self.vt.selection = Some(Selection::new(ty, p, Side::Left));
    }
    /// Clear any active selection; returns whether there was one.
    pub fn clear_selection(&mut self) -> bool {
        self.vt.selection.take().is_some()
    }
    /// Yank the current selection's text (and clear it).
    pub fn yank(&mut self) -> Option<String> {
        let text = self.vt.selection_to_string();
        self.vt.selection = None;
        text
    }

    /// Feed raw PTY output bytes through the VT parser into the grid — and through
    /// the cwd scanner, since the VT engine drops the OSC 9;9 / OSC 7 sequences.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if let Some(payload) = self.cwd_scan.push(b) {
                if let Some(dir) = osc_cwd(&payload) {
                    self.cwd = Some(dir);
                }
            }
        }
        self.parser.advance(&mut self.vt, bytes);
    }

    /// Drain bytes the engine wants written back to the PTY (e.g. the cursor-position
    /// report answering `ESC[6n`). Call after `feed`; without this the shell hangs.
    pub fn take_reply(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.out.lock().unwrap())
    }

    /// Resize the grid (after the window/zoom changes the cell count). Returns
    /// whether the size actually changed (so the caller can resize the PTY too).
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        self.vt.resize(TermSize::new(cols, rows));
        true
    }

    /// Whether the program requested application cursor keys (DECCKM) — changes the
    /// arrow-key escape sequences we send (`ESC O A` vs `ESC [ A`).
    pub fn app_cursor(&self) -> bool {
        self.vt.mode().contains(TermMode::APP_CURSOR)
    }

    /// Whether the program enabled bracketed-paste mode (DEC 2004). If so, pasted
    /// text must be wrapped in `ESC[200~`…`ESC[201~` so the app treats it as one
    /// literal block (e.g. a shell won't run each pasted line on its own).
    /// Whether the running program enabled any mouse reporting (e.g. vim `mouse=a`,
    /// less, tmux). When set, wheel/clicks should be forwarded to it as mouse events
    /// instead of driving our own scrollback.
    pub fn mouse_mode(&self) -> bool {
        self.vt.mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Whether mouse reports should use the SGR (1006) encoding rather than the
    /// legacy X10 byte form.
    pub fn sgr_mouse(&self) -> bool {
        self.vt.mode().contains(TermMode::SGR_MOUSE)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.vt.mode().contains(TermMode::BRACKETED_PASTE)
    }
}

fn to_rgb(c: VtRgb) -> Rgb {
    (c.r, c.g, c.b)
}

fn named_default(n: NamedColor, st: &TermStyle) -> Rgb {
    use NamedColor::*;
    match n {
        Black | DimBlack => st.ansi[0],
        Red | DimRed => st.ansi[1],
        Green | DimGreen => st.ansi[2],
        Yellow | DimYellow => st.ansi[3],
        Blue | DimBlue => st.ansi[4],
        Magenta | DimMagenta => st.ansi[5],
        Cyan | DimCyan => st.ansi[6],
        White | DimWhite => st.ansi[7],
        BrightBlack => st.ansi[8],
        BrightRed => st.ansi[9],
        BrightGreen => st.ansi[10],
        BrightYellow => st.ansi[11],
        BrightBlue => st.ansi[12],
        BrightMagenta => st.ansi[13],
        BrightCyan => st.ansi[14],
        BrightWhite => st.ansi[15],
        Foreground | BrightForeground | DimForeground | Cursor => st.fg,
        Background => st.bg,
    }
}

/// xterm 256-color default for an index the program hasn't overridden.
fn indexed_default(i: u8, st: &TermStyle) -> Rgb {
    match i {
        0..=15 => st.ansi[i as usize],
        16..=231 => {
            let i = i - 16;
            let step = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
            (step(i / 36), step((i % 36) / 6), step(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// Blend two colors: `t` (0..=255) of `b` over `a`.
fn lerp(a: Rgb, b: Rgb, t: u32) -> Rgb {
    let f = |x: u8, y: u8| ((x as u32 * (255 - t) + y as u32 * t) / 255) as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// Draw box-drawing (U+2500..) and block (U+2580..) glyphs PROCEDURALLY so they tile
/// seamlessly (table borders connect, blocks fill the cell) instead of relying on the
/// font's glyph metrics, which leave gaps/misalignment. Returns `true` if `ch` was a
/// special it handled (caller then skips the normal glyph).
#[allow(clippy::too_many_arguments)]
fn draw_special(
    buf: &mut [u32], w: usize, h: usize,
    cx: i32, cy: i32, cw: i32, ch_h: i32, c: char, fg: Rgb, bg: Rgb,
) -> bool {
    let fill = |buf: &mut [u32], x0: i32, y0: i32, x1: i32, y1: i32, col: Rgb| {
        draw::fill_rect(buf, w, h, x0.max(0) as usize, y0.max(0) as usize, x1.max(0) as usize, y1.max(0) as usize, col);
    };
    // Block elements (U+2580..U+259F) — drawn as exact cell fills so block/pixel art
    // (e.g. the Claude Code logo) tiles cleanly.
    let mid_x = cx + cw / 2;
    let mid_y = cy + ch_h / 2;
    // Fill the bottom `n/8` of the cell (for lower-eighth blocks ▁▂▃…).
    let lower = |buf: &mut [u32], n: i32| fill(buf, cx, cy + ch_h * (8 - n) / 8, cx + cw, cy + ch_h, fg);
    // Fill the left `n/8` of the cell (for left-eighth blocks ▏▎▍…).
    let left = |buf: &mut [u32], n: i32| fill(buf, cx, cy, cx + cw * n / 8, cy + ch_h, fg);
    // Quadrant fills (UL/UR/LL/LR), for ▖▗▘▙▚▛▜▝▞▟ and the half blocks.
    let ul = |buf: &mut [u32]| fill(buf, cx, cy, mid_x, mid_y, fg);
    let ur = |buf: &mut [u32]| fill(buf, mid_x, cy, cx + cw, mid_y, fg);
    let ll = |buf: &mut [u32]| fill(buf, cx, mid_y, mid_x, cy + ch_h, fg);
    let lr = |buf: &mut [u32]| fill(buf, mid_x, mid_y, cx + cw, cy + ch_h, fg);
    match c {
        '\u{2580}' => { fill(buf, cx, cy, cx + cw, mid_y, fg); return true; } // ▀ upper half
        '\u{2581}'..='\u{2587}' => { lower(buf, c as i32 - 0x2580); return true; } // ▁..▇ lower n/8
        '\u{2588}' => { fill(buf, cx, cy, cx + cw, cy + ch_h, fg); return true; } // █ full
        '\u{2589}'..='\u{258f}' => { left(buf, 8 - (c as i32 - 0x2588)); return true; } // ▉..▏ left n/8
        '\u{2590}' => { fill(buf, mid_x, cy, cx + cw, cy + ch_h, fg); return true; } // ▐ right half
        '\u{2591}' => { fill(buf, cx, cy, cx + cw, cy + ch_h, lerp(bg, fg, 64)); return true; } // ░
        '\u{2592}' => { fill(buf, cx, cy, cx + cw, cy + ch_h, lerp(bg, fg, 128)); return true; } // ▒
        '\u{2593}' => { fill(buf, cx, cy, cx + cw, cy + ch_h, lerp(bg, fg, 192)); return true; } // ▓
        '\u{2594}' => { fill(buf, cx, cy, cx + cw, cy + ch_h / 8, fg); return true; } // ▔ upper 1/8
        '\u{2595}' => { fill(buf, cx + cw * 7 / 8, cy, cx + cw, cy + ch_h, fg); return true; } // ▕ right 1/8
        '\u{2596}' => { ll(buf); return true; }
        '\u{2597}' => { lr(buf); return true; }
        '\u{2598}' => { ul(buf); return true; }
        '\u{2599}' => { ul(buf); ll(buf); lr(buf); return true; }
        '\u{259a}' => { ul(buf); lr(buf); return true; }
        '\u{259b}' => { ul(buf); ur(buf); ll(buf); return true; }
        '\u{259c}' => { ul(buf); ur(buf); lr(buf); return true; }
        '\u{259d}' => { ur(buf); return true; }
        '\u{259e}' => { ur(buf); ll(buf); return true; }
        '\u{259f}' => { ur(buf); ll(buf); lr(buf); return true; }
        _ => {}
    }
    // Line-drawing: which arms reach the cell centre (up/down/left/right). Heavy and
    // double variants are drawn as their light single-line equivalent.
    let (u, d, l, r) = match c {
        '\u{2500}' | '\u{2501}' | '\u{2550}' => (false, false, true, true),   // ─ ━ ═
        '\u{2502}' | '\u{2503}' | '\u{2551}' => (true, true, false, false),   // │ ┃ ║
        '\u{250c}' | '\u{250f}' | '\u{2554}' | '\u{256d}' => (false, true, false, true), // ┌ ┏ ╔ ╭
        '\u{2510}' | '\u{2513}' | '\u{2557}' | '\u{256e}' => (false, true, true, false), // ┐ ┓ ╗ ╮
        '\u{2514}' | '\u{2517}' | '\u{255a}' | '\u{2570}' => (true, false, false, true), // └ ┗ ╚ ╰
        '\u{2518}' | '\u{251b}' | '\u{255d}' | '\u{256f}' => (true, false, true, false), // ┘ ┛ ╝ ╯
        '\u{251c}' | '\u{2523}' | '\u{2560}' => (true, true, false, true),    // ├ ┣ ╠
        '\u{2524}' | '\u{252b}' | '\u{2563}' => (true, true, true, false),    // ┤ ┫ ╣
        '\u{252c}' | '\u{2533}' | '\u{2566}' => (false, true, true, true),    // ┬ ┳ ╦
        '\u{2534}' | '\u{253b}' | '\u{2569}' => (true, false, true, true),    // ┴ ┻ ╩
        '\u{253c}' | '\u{254b}' | '\u{256c}' => (true, true, true, true),     // ┼ ╋ ╬
        _ => return false,
    };
    let th = (ch_h / 9).max(1);
    let midx = cx + cw / 2;
    let midy = cy + ch_h / 2;
    let (vx0, vx1) = (midx - th / 2, midx - th / 2 + th);
    let (hy0, hy1) = (midy - th / 2, midy - th / 2 + th);
    if u {
        fill(buf, vx0, cy, vx1, hy1, fg);
    }
    if d {
        fill(buf, vx0, hy0, vx1, cy + ch_h, fg);
    }
    if l {
        fill(buf, cx, hy0, vx1, hy1, fg);
    }
    if r {
        fill(buf, vx0, hy0, cx + cw, hy1, fg);
    }
    true
}

/// Resolve a cell color to RGB: a program-set palette entry wins, else our default.
fn resolve(c: Color, colors: &Colors, st: &TermStyle) -> Rgb {
    match c {
        Color::Spec(rgb) => to_rgb(rgb),
        Color::Named(n) => colors[n].map(to_rgb).unwrap_or_else(|| named_default(n, st)),
        Color::Indexed(i) => {
            colors[i as usize].map(to_rgb).unwrap_or_else(|| indexed_default(i, st))
        }
    }
}

/// Paint the terminal grid into `buf`. The grid's top-left cell is at (`x0`, `y0`);
/// `cell_w`/`cell_h` are the monospace cell size. Cells past the bottom of the
/// content band (`>= clip_bottom`) are skipped; the caller paints the bars on top.
/// `st` supplies the default fg/bg + ANSI palette (the `:theme term_*` overrides).
#[allow(clippy::too_many_arguments)]
pub fn render(
    pty: &PtyTerm,
    p: &Painter,
    buf: &mut [u32],
    w: usize,
    h: usize,
    x0: i32,
    y0: i32,
    cell_w: i32,
    cell_h: i32,
    clip_bottom: i32,
    st: &TermStyle,
) {
    let content = pty.vt.renderable_content();
    let colors = content.colors;
    let cursor = content.cursor;
    let selection = content.selection;
    let baseline_off = cell_h * 3 / 4;
    let mut cursor_char = ' ';

    // Both `display_iter` cells AND `cursor.point` use ABSOLUTE buffer coordinates
    // (lines are negative for scrollback): `display_iter` starts at
    // `Line(-display_offset - 1)`. To paint them in the visible band we convert each
    // to a viewport row the way Alacritty's own frontend does (`point_to_viewport`):
    // `row = absolute_line + display_offset`. Without this, scrolling copy-mode into
    // scrollback clips the scrolled-in lines (negative rows) and pins the rest at
    // their absolute rows — the "camera doesn't follow the cursor" bug, where the
    // bottom goes blank as you scroll up. At display_offset 0 this is a no-op.
    let display_offset = pty.vt.grid().display_offset() as i32;
    let cursor_row = cursor.point.line.0 + display_offset;
    let cursor_col = cursor.point.column.0 as i32;

    for item in content.display_iter {
        let cell = item.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let row = item.point.line.0 + display_offset;
        let col = item.point.column.0 as i32;
        if row == cursor_row && col == cursor_col {
            cursor_char = cell.c;
        }
        let cx = x0 + col * cell_w;
        let cy = y0 + row * cell_h;
        if cy < y0 || cy + cell_h > clip_bottom {
            continue;
        }
        let mut fg = resolve(cell.fg, colors, st);
        let mut bg = resolve(cell.bg, colors, st);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        // Copy-mode selection highlight (vi mode).
        if selection.is_some_and(|s| s.contains(item.point)) {
            bg = draw::SEL;
        }
        let cw = if cell.flags.contains(Flags::WIDE_CHAR) { cell_w * 2 } else { cell_w };
        if bg != st.bg {
            draw::fill_rect(
                buf, w, h, cx.max(0) as usize, cy.max(0) as usize,
                (cx + cw).max(0) as usize, (cy + cell_h).max(0) as usize, bg,
            );
        }
        if cell.c != ' ' && cell.c != '\0'
            && !draw_special(buf, w, h, cx, cy, cell_w, cell_h, cell.c, fg, bg)
        {
            p.text(buf, w, h, cx.max(0) as usize, (cy + baseline_off) as usize, &cell.c.to_string(), fg);
        }
    }

    // Block cursor (inverse cell), unless hidden. Uses the viewport-converted row
    // so it tracks the display when copy-mode has scrolled into scrollback.
    if !matches!(cursor.shape, CursorShape::Hidden) {
        let cx = x0 + cursor_col * cell_w;
        let cy = y0 + cursor_row * cell_h;
        if cy >= y0 && cy + cell_h <= clip_bottom {
            draw::fill_rect(
                buf, w, h, cx.max(0) as usize, cy.max(0) as usize,
                (cx + cell_w).max(0) as usize, (cy + cell_h).max(0) as usize, st.fg,
            );
            if cursor_char != ' ' && cursor_char != '\0'
                && !draw_special(buf, w, h, cx, cy, cell_w, cell_h, cursor_char, st.bg, st.fg)
            {
                p.text(buf, w, h, cx.max(0) as usize, (cy + baseline_off) as usize, &cursor_char.to_string(), st.bg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vi_copy_mode_selects_and_yanks() {
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        pty.feed(b"hello world\r\n"); // row 0 = "hello world", cursor drops to row 1
        assert!(!pty.is_vi());
        pty.toggle_vi();
        assert!(pty.is_vi());
        // Move the vi cursor to the start of "hello world".
        pty.vi_motion(ViMotion::Up);
        pty.vi_motion(ViMotion::First);
        // Start a charwise selection and extend across the word — the force-extend in
        // `vi_motion` must grow the just-started (zero-width) selection.
        pty.start_selection(false);
        for _ in 0..10 {
            pty.vi_motion(ViMotion::Right);
        }
        let yanked = pty.yank().expect("selection should yank text");
        assert!(yanked.contains("hello"), "yank was {yanked:?}");
        // Yank clears the selection; leaving vi mode works.
        pty.toggle_vi();
        assert!(!pty.is_vi());
    }

    #[test]
    fn vi_find_char_moves_along_the_line() {
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        pty.feed(b"abcdef\r\n");
        pty.toggle_vi();
        // Park the cursor at the start of "abcdef".
        pty.vi_motion(ViMotion::Up);
        pty.vi_motion(ViMotion::First);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 0);
        // `f e` lands ON the 'e' (column 4).
        pty.vi_find_char('e', true, false);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 4);
        // `F b` searches backward to 'b' (column 1).
        pty.vi_find_char('b', false, false);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 1);
        // `t f` (till) from column 1 stops one short of 'f' → column 4.
        pty.vi_find_char('f', true, true);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 4);
        // A char that isn't on the line leaves the cursor put.
        pty.vi_find_char('z', true, false);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 4);
    }

    #[test]
    fn vi_cursor_scrolls_viewport_into_scrollback() {
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        // Print more lines than fit, so there's scrollback.
        for i in 0..20 {
            pty.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(pty.vt.grid().display_offset(), 0); // viewport at the bottom
        pty.toggle_vi();
        // Moving the vi cursor up past the top must scroll the display (camera
        // follows) — the regression we're guarding.
        for _ in 0..15 {
            pty.vi_motion(ViMotion::Up);
        }
        assert!(pty.vt.grid().display_offset() > 0, "viewport should have scrolled up");
        // The cursor's VIEWPORT row (absolute line + display_offset) must stay within
        // the visible band — this is what `render` uses to place the block, so a value
        // outside [0, rows) is the "cursor flew off-screen, camera didn't follow" bug.
        let vp_row = pty.vt.vi_mode_cursor.point.line.0 + pty.vt.grid().display_offset() as i32;
        assert!(
            (0..pty.rows as i32).contains(&vp_row),
            "cursor viewport row {vp_row} should be on-screen (rows={})",
            pty.rows
        );
        // gg jumps to the very top.
        pty.vi_top();
        assert!(pty.vt.grid().display_offset() > 0);
        let vp_row = pty.vt.vi_mode_cursor.point.line.0 + pty.vt.grid().display_offset() as i32;
        assert!((0..pty.rows as i32).contains(&vp_row), "cursor row {vp_row} on-screen after gg");
    }

    #[test]
    fn osc_9_9_reports_cwd_even_split_across_feed_chunks() {
        let mut pty = PtyTerm::new(80, 24, DEFAULT_SCROLLBACK);
        assert_eq!(pty.cwd(), None);
        // BEL-terminated, split mid-sequence across two PTY reads (the real
        // failure mode a per-chunk regex would miss).
        pty.feed(b"prompt> \x1b]9;9;C:\\proj");
        pty.feed(b"ects\\browser\x07more output");
        assert_eq!(pty.cwd(), Some("C:\\projects\\browser"));
        // ST-terminated (ESC \) and quoted (some prompts emit "%cd%" quoted);
        // a later report replaces the earlier one.
        pty.feed(b"\x1b]9;9;\"D:\\stuff\"\x1b\\");
        assert_eq!(pty.cwd(), Some("D:\\stuff"));
        // A WSL path via wslpath -w survives verbatim.
        pty.feed(b"\x1b]9;9;\\\\wsl.localhost\\Ubuntu\\home\\kayf\x07");
        assert_eq!(pty.cwd(), Some("\\\\wsl.localhost\\Ubuntu\\home\\kayf"));
        // Unrelated OSCs (title etc.) don't disturb the last-known cwd.
        pty.feed(b"\x1b]0;some title\x07");
        assert_eq!(pty.cwd(), Some("\\\\wsl.localhost\\Ubuntu\\home\\kayf"));
    }

    #[test]
    fn osc_7_file_uri_reports_linux_and_windows_paths() {
        let mut pty = PtyTerm::new(80, 24, DEFAULT_SCROLLBACK);
        // bash-in-WSL convention: file://<hostname><path>, percent-encoded.
        pty.feed(b"\x1b]7;file://mybox/home/kayf/my%20dir\x07");
        assert_eq!(pty.cwd(), Some("/home/kayf/my dir"));
        // A Windows-style file URI decodes to a drive path.
        pty.feed(b"\x1b]7;file:///C:/Users/kayf\x07");
        assert_eq!(pty.cwd(), Some("C:\\Users\\kayf"));
        // An empty/rootless URI is ignored, keeping the last good report.
        pty.feed(b"\x1b]7;file://\x07");
        assert_eq!(pty.cwd(), Some("C:\\Users\\kayf"));
    }

    #[test]
    fn resuming_the_shell_snaps_the_camera_back_to_the_prompt() {
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        for i in 0..20 {
            pty.feed(format!("line{i}\r\n").as_bytes());
        }
        // Ctrl+S → copy mode, scroll up into scrollback.
        pty.toggle_vi();
        for _ in 0..15 {
            pty.vi_motion(ViMotion::Up);
        }
        assert!(pty.vt.grid().display_offset() > 0, "should be parked in scrollback");
        // Esc/i resumes: leaving vi mode alone does NOT reset the viewport (the
        // "camera stuck after Esc" bug) — the resume path must snap it down.
        pty.toggle_vi();
        pty.scroll_to_bottom();
        assert_eq!(pty.vt.grid().display_offset(), 0, "camera should be back at the live line");
    }
}
