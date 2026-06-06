//! Native terminal: an in-process `alacritty_terminal` VT engine, rendered by our
//! own softbuffer/fontdue painter — no WebView2. The PTY still lives in the
//! `browser-pty-host` companion (ConPTY isolation); this just parses its byte
//! stream into a cell grid and paints it. Replaces the old xterm.js-in-WebView2
//! terminal, so a `:te` tab now spawns zero WebView2 processes.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb as VtRgb};

use crate::draw::{self, Painter, Rgb};

/// Default 16-color ANSI palette (Tango-ish dark), used when the program hasn't set
/// its own via OSC. 0–7 normal, 8–15 bright.
const ANSI: [Rgb; 16] = [
    (0x1a, 0x1a, 0x1a),
    (0xcc, 0x33, 0x33),
    (0x4e, 0x9a, 0x06),
    (0xc4, 0xa0, 0x00),
    (0x34, 0x65, 0xa4),
    (0x75, 0x50, 0x7b),
    (0x06, 0x98, 0x9a),
    (0xd3, 0xd7, 0xcf),
    (0x55, 0x57, 0x53),
    (0xef, 0x29, 0x29),
    (0x8a, 0xe2, 0x34),
    (0xfc, 0xe9, 0x4f),
    (0x72, 0x9f, 0xcf),
    (0xad, 0x7f, 0xa8),
    (0x34, 0xe2, 0xe2),
    (0xee, 0xee, 0xec),
];
/// Default foreground / background when the program uses the default colors.
const FG: Rgb = (0xd0, 0xd0, 0xd0);
/// Terminal background — also the content-area fill behind the grid.
pub const BG: Rgb = (0x1a, 0x1a, 0x1a);

/// Event sink for the VT engine. The only events we must act on are `PtyWrite`s —
/// replies the engine generates to device queries (notably the `ESC[6n` cursor-
/// position report the shell sends on startup and STALLS waiting for). We queue
/// those bytes; the app drains them after each feed and writes them to the PTY.
#[derive(Clone)]
pub struct TermListener {
    out: Arc<Mutex<Vec<u8>>>,
}

impl EventListener for TermListener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.out.lock().unwrap().extend_from_slice(text.as_bytes());
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
}

impl PtyTerm {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let out = Arc::new(Mutex::new(Vec::new()));
        let listener = TermListener { out: out.clone() };
        let vt = Term::new(Config::default(), &TermSize::new(cols, rows), listener);
        PtyTerm { vt, parser: Processor::new(), cols, rows, out }
    }

    /// Feed raw PTY output bytes through the VT parser into the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
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
}

fn to_rgb(c: VtRgb) -> Rgb {
    (c.r, c.g, c.b)
}

fn named_default(n: NamedColor) -> Rgb {
    use NamedColor::*;
    match n {
        Black | DimBlack => ANSI[0],
        Red | DimRed => ANSI[1],
        Green | DimGreen => ANSI[2],
        Yellow | DimYellow => ANSI[3],
        Blue | DimBlue => ANSI[4],
        Magenta | DimMagenta => ANSI[5],
        Cyan | DimCyan => ANSI[6],
        White | DimWhite => ANSI[7],
        BrightBlack => ANSI[8],
        BrightRed => ANSI[9],
        BrightGreen => ANSI[10],
        BrightYellow => ANSI[11],
        BrightBlue => ANSI[12],
        BrightMagenta => ANSI[13],
        BrightCyan => ANSI[14],
        BrightWhite => ANSI[15],
        Foreground | BrightForeground | DimForeground | Cursor => FG,
        Background => BG,
    }
}

/// xterm 256-color default for an index the program hasn't overridden.
fn indexed_default(i: u8) -> Rgb {
    match i {
        0..=15 => ANSI[i as usize],
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

/// Resolve a cell color to RGB: a program-set palette entry wins, else our default.
fn resolve(c: Color, colors: &Colors) -> Rgb {
    match c {
        Color::Spec(rgb) => to_rgb(rgb),
        Color::Named(n) => colors[n].map(to_rgb).unwrap_or_else(|| named_default(n)),
        Color::Indexed(i) => colors[i as usize].map(to_rgb).unwrap_or_else(|| indexed_default(i)),
    }
}

/// Paint the terminal grid into `buf`. The grid's top-left cell is at (`x0`, `y0`);
/// `cell_w`/`cell_h` are the monospace cell size. Cells past the bottom of the
/// content band (`>= clip_bottom`) are skipped; the caller paints the bars on top.
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
) {
    let content = pty.vt.renderable_content();
    let colors = content.colors;
    let cursor = content.cursor;
    let baseline_off = cell_h * 3 / 4;
    let mut cursor_char = ' ';

    for item in content.display_iter {
        let cell = item.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let row = item.point.line.0;
        let col = item.point.column.0 as i32;
        if item.point == cursor.point {
            cursor_char = cell.c;
        }
        let cx = x0 + col * cell_w;
        let cy = y0 + row * cell_h;
        if cy < y0 || cy + cell_h > clip_bottom {
            continue;
        }
        let mut fg = resolve(cell.fg, colors);
        let mut bg = resolve(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let cw = if cell.flags.contains(Flags::WIDE_CHAR) { cell_w * 2 } else { cell_w };
        if bg != BG {
            draw::fill_rect(
                buf, w, h, cx.max(0) as usize, cy.max(0) as usize,
                (cx + cw).max(0) as usize, (cy + cell_h).max(0) as usize, bg,
            );
        }
        if cell.c != ' ' && cell.c != '\0' {
            p.text(buf, w, h, cx.max(0) as usize, (cy + baseline_off) as usize, &cell.c.to_string(), fg);
        }
    }

    // Block cursor (inverse cell), unless hidden.
    if !matches!(cursor.shape, CursorShape::Hidden) {
        let cx = x0 + cursor.point.column.0 as i32 * cell_w;
        let cy = y0 + cursor.point.line.0 * cell_h;
        if cy >= y0 && cy + cell_h <= clip_bottom {
            draw::fill_rect(
                buf, w, h, cx.max(0) as usize, cy.max(0) as usize,
                (cx + cell_w).max(0) as usize, (cy + cell_h).max(0) as usize, FG,
            );
            if cursor_char != ' ' && cursor_char != '\0' {
                p.text(buf, w, h, cx.max(0) as usize, (cy + baseline_off) as usize, &cursor_char.to_string(), BG);
            }
        }
    }
}
