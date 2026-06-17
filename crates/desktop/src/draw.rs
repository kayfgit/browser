//! Minimal native text rendering for the shell chrome (welcome screen + command
//! bar). Uses `fontdue` to rasterize glyphs into a `softbuffer` pixel buffer.
//! This is what keeps the shell engine-free at idle: no WebView is involved in
//! drawing the UI.

use anyhow::{anyhow, Result};
use fontdue::{Font, FontSettings};

/// Pixel buffer format is 0x00RRGGBB (softbuffer ignores the top byte).
pub type Rgb = (u8, u8, u8);

pub const BG: Rgb = (0x1e, 0x1e, 0x1e);
pub const FG: Rgb = (0xd0, 0xd0, 0xd0);
pub const DIM: Rgb = (0x80, 0x80, 0x80);
pub const BAR_BG: Rgb = (0x2d, 0x2d, 0x2d);
pub const BAR_FG: Rgb = (0xf0, 0xf0, 0xf0);
pub const ACCENT: Rgb = (0x6c, 0xb6, 0xff);
pub const READ: Rgb = (0x7c, 0xd9, 0x92);
pub const TERM: Rgb = (0xe6, 0xa5, 0x5e);
pub const RESEARCH: Rgb = (0x5e, 0xc8, 0xd9);
/// The `:ai` tab accent (lilac).
pub const AI: Rgb = (0xc6, 0x9c, 0xf6);
/// Error/failure messages in the status bar.
pub const ERR: Rgb = (0xe0, 0x6c, 0x6c);
/// Command-bar text-selection highlight.
pub const SEL: Rgb = (0x2d, 0x4f, 0x7a);
/// Find-in-page: all matches (dim) and the current match (bright).
pub const FIND: Rgb = (0x5a, 0x52, 0x14);
pub const FIND_CUR: Rgb = (0xc8, 0x64, 0x1e);

pub struct Painter {
    /// Primary monospace font + fallbacks, tried in order for each glyph. The
    /// primary (Consolas) covers normal text; fallbacks (e.g. Segoe UI Symbol)
    /// cover glyphs it lacks (Braille, symbols, dingbats) so terminals/pages don't
    /// show `.notdef` tofu boxes for them.
    fonts: Vec<Font>,
    px: f32,
}

impl Painter {
    /// Load the monospace primary (Consolas → Segoe UI → Arial) plus best-effort
    /// symbol fallbacks for broad glyph coverage.
    pub fn new(px: f32) -> Result<Self> {
        let mut fonts = vec![Font::from_bytes(load_system_font()?, FontSettings::default())
            .map_err(|e| anyhow!("parsing font: {e}"))?];
        // Optional fallbacks — skipped silently if a face isn't installed.
        for path in [r"C:\Windows\Fonts\seguisym.ttf", r"C:\Windows\Fonts\arial.ttf"] {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(f) = Font::from_bytes(bytes, FontSettings::default()) {
                    fonts.push(f);
                }
            }
        }
        Ok(Painter { fonts, px })
    }

    /// The first loaded font that has a glyph for `ch` (else the primary, which
    /// renders its `.notdef`).
    fn font_for(&self, ch: char) -> &Font {
        self.fonts
            .iter()
            .find(|f| f.lookup_glyph_index(ch) != 0)
            .unwrap_or(&self.fonts[0])
    }

    pub fn line_height(&self) -> usize {
        (self.px * 1.45).ceil() as usize
    }

    /// Current font size in px (used to key the read-mode layout cache to zoom).
    pub fn px(&self) -> f32 {
        self.px
    }

    /// Rescale the font (for global zoom). Glyphs are rasterized per-draw, so a
    /// new size takes effect on the next paint with no cache to invalidate.
    pub fn set_px(&mut self, px: f32) {
        self.px = px;
    }

    /// Pixel width of `s` at the current size — matches what [`Painter::text`]
    /// advances, so it can position a caret at a byte offset within a string.
    pub fn measure(&self, s: &str) -> usize {
        let mut pen = 0f32;
        for ch in s.chars() {
            pen += self.font_for(ch).metrics(ch, self.px).advance_width;
        }
        pen as usize
    }

    /// Horizontal advance of a single glyph (f32), so callers can accumulate caret
    /// positions exactly as [`Painter::text_clipped`] does (which keeps the pen in
    /// f32 within a run). Using `measure()` of a whole multi-run prefix instead
    /// drifts, because each run boundary floors the pen — the drift grows with the
    /// column and the font size.
    pub fn advance(&self, ch: char) -> f32 {
        self.font_for(ch).metrics(ch, self.px).advance_width
    }

    /// Draw a string with its baseline at `baseline`, left edge at `x`.
    /// Returns the pen x position after the string.
    #[allow(clippy::too_many_arguments)]
    pub fn text(
        &self,
        buf: &mut [u32],
        w: usize,
        h: usize,
        x: usize,
        baseline: usize,
        s: &str,
        color: Rgb,
    ) -> usize {
        self.text_clipped(buf, w, h, x as i32, baseline, s, color, 0).max(0) as usize
    }

    /// Like [`Painter::text`] but the start `x` may be negative (for horizontal
    /// scrolling) and pixels left of `clip_x0` are dropped, so text can scroll
    /// under a left margin with a clean vertical edge. Returns the pen x after the
    /// string (may exceed `w`; right/top/bottom are clipped to the buffer).
    #[allow(clippy::too_many_arguments)]
    pub fn text_clipped(
        &self,
        buf: &mut [u32],
        w: usize,
        h: usize,
        x: i32,
        baseline: usize,
        s: &str,
        color: Rgb,
        clip_x0: i32,
    ) -> i32 {
        self.text_rect(buf, w, h, x, baseline, s, color, clip_x0, w as i32, 0, h as i32)
    }

    /// Like [`Painter::text_clipped`] but confined to a sub-rect: pixels outside
    /// `[clip_x0, clip_x1)` × `[clip_y0, clip_y1)` are dropped, so text painted into a
    /// tmux pane stays inside it with clean edges instead of bleeding into a neighbour.
    #[allow(clippy::too_many_arguments)]
    pub fn text_rect(
        &self,
        buf: &mut [u32],
        w: usize,
        h: usize,
        x: i32,
        baseline: usize,
        s: &str,
        color: Rgb,
        clip_x0: i32,
        clip_x1: i32,
        clip_y0: i32,
        clip_y1: i32,
    ) -> i32 {
        let right = clip_x1.min(w as i32);
        let bottom = clip_y1.min(h as i32);
        let top = clip_y0.max(0);
        let mut pen = x as f32;
        for ch in s.chars() {
            let (m, bitmap) = self.font_for(ch).rasterize(ch, self.px);
            for row in 0..m.height {
                for col in 0..m.width {
                    let cov = bitmap[row * m.width + col];
                    if cov == 0 {
                        continue;
                    }
                    let gx = pen as i32 + m.xmin + col as i32;
                    let gy = baseline as i32 - m.ymin - m.height as i32 + row as i32;
                    if gx < clip_x0 || gx >= right || gy < top || gy >= bottom {
                        continue;
                    }
                    // Defensive: callers pass chrome metrics (tab/command-bar height)
                    // that can momentarily exceed the buffer during minimize/resize.
                    // The `gy >= h` check above normally covers this, but guard the
                    // raw index too so a geometry mismatch can never panic the render.
                    let idx = gy as usize * w + gx as usize;
                    if idx >= buf.len() {
                        continue;
                    }
                    buf[idx] = blend(buf[idx], color, cov);
                }
            }
            pen += m.advance_width;
        }
        pen as i32
    }
}

/// Fill a horizontal band [y0, y1) with a solid color.
pub fn fill_band(buf: &mut [u32], w: usize, h: usize, y0: usize, y1: usize, color: Rgb) {
    let v = pack(color);
    let y1 = y1.min(h);
    for y in y0..y1 {
        let row = &mut buf[y * w..y * w + w];
        row.iter_mut().for_each(|p| *p = v);
    }
}

/// Fill a rectangle [x0, x1) × [y0, y1) with a solid color (used for the caret).
#[allow(clippy::too_many_arguments)]
pub fn fill_rect(
    buf: &mut [u32],
    w: usize,
    h: usize,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    color: Rgb,
) {
    let v = pack(color);
    let x1 = x1.min(w);
    let y1 = y1.min(h);
    if x0 >= x1 {
        return;
    }
    for y in y0..y1 {
        buf[y * w + x0..y * w + x1].iter_mut().for_each(|p| *p = v);
    }
}

fn pack((r, g, b): Rgb) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

fn blend(bg: u32, fg: Rgb, cov: u8) -> u32 {
    let a = cov as u32;
    let inv = 255 - a;
    let br = (bg >> 16) & 0xff;
    let bgc = (bg >> 8) & 0xff;
    let bb = bg & 0xff;
    let r = (fg.0 as u32 * a + br * inv) / 255;
    let g = (fg.1 as u32 * a + bgc * inv) / 255;
    let b = (fg.2 as u32 * a + bb * inv) / 255;
    (r << 16) | (g << 8) | b
}

fn load_system_font() -> Result<Vec<u8>> {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\consola.ttf",
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\arial.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            return Ok(bytes);
        }
    }
    Err(anyhow!("no system font found (looked for Consolas/Segoe UI/Arial)"))
}
