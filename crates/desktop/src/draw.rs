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

pub struct Painter {
    font: Font,
    px: f32,
}

impl Painter {
    /// Load a monospace system font (Consolas), falling back to Segoe UI.
    pub fn new(px: f32) -> Result<Self> {
        let bytes = load_system_font()?;
        let font = Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow!("parsing font: {e}"))?;
        Ok(Painter { font, px })
    }

    pub fn line_height(&self) -> usize {
        (self.px * 1.45).ceil() as usize
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
            pen += self.font.metrics(ch, self.px).advance_width;
        }
        pen as usize
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
        let mut pen = x as f32;
        for ch in s.chars() {
            let (m, bitmap) = self.font.rasterize(ch, self.px);
            for row in 0..m.height {
                for col in 0..m.width {
                    let cov = bitmap[row * m.width + col];
                    if cov == 0 {
                        continue;
                    }
                    let gx = pen as i32 + m.xmin + col as i32;
                    let gy = baseline as i32 - m.ymin - m.height as i32 + row as i32;
                    if gx < 0 || gy < 0 || gx >= w as i32 || gy >= h as i32 {
                        continue;
                    }
                    let idx = gy as usize * w + gx as usize;
                    buf[idx] = blend(buf[idx], color, cov);
                }
            }
            pen += m.advance_width;
        }
        pen as usize
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
