//! Native terminal: an in-process `alacritty_terminal` VT engine, rendered by our
//! own softbuffer/fontdue painter — no WebView2. The PTY still lives in the
//! `browser-pty-host` companion (ConPTY isolation); this just parses its byte
//! stream into a cell grid and paints it. Replaces the old xterm.js-in-WebView2
//! terminal, so a `:te` tab now spawns zero WebView2 processes.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
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
        // Keep scrollback so copy-mode (Shift+Esc) can page back through history.
        let config = Config { scrolling_history: 5000, ..Config::default() };
        let vt = Term::new(config, &TermSize::new(cols, rows), listener);
        PtyTerm { vt, parser: Processor::new(), cols, rows, out }
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

    /// Whether the program enabled bracketed-paste mode (DEC 2004). If so, pasted
    /// text must be wrapped in `ESC[200~`…`ESC[201~` so the app treats it as one
    /// literal block (e.g. a shell won't run each pasted line on its own).
    pub fn bracketed_paste(&self) -> bool {
        self.vt.mode().contains(TermMode::BRACKETED_PASTE)
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
        let mut fg = resolve(cell.fg, colors);
        let mut bg = resolve(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        // Copy-mode selection highlight (vi mode).
        if selection.is_some_and(|s| s.contains(item.point)) {
            bg = draw::SEL;
        }
        let cw = if cell.flags.contains(Flags::WIDE_CHAR) { cell_w * 2 } else { cell_w };
        if bg != BG {
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
                (cx + cell_w).max(0) as usize, (cy + cell_h).max(0) as usize, FG,
            );
            if cursor_char != ' ' && cursor_char != '\0'
                && !draw_special(buf, w, h, cx, cy, cell_w, cell_h, cursor_char, BG, FG)
            {
                p.text(buf, w, h, cx.max(0) as usize, (cy + baseline_off) as usize, &cursor_char.to_string(), BG);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vi_copy_mode_selects_and_yanks() {
        let mut pty = PtyTerm::new(20, 5);
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
    fn vi_cursor_scrolls_viewport_into_scrollback() {
        let mut pty = PtyTerm::new(20, 5);
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
}
