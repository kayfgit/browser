//! Minimal native text rendering for the shell chrome (welcome screen + command
//! bar). Uses `fontdue` to rasterize glyphs into a `softbuffer` pixel buffer.
//! This is what keeps the shell engine-free at idle: no WebView is involved in
//! drawing the UI.

use std::sync::OnceLock;

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
/// Read-mode caret: the row the cursor is on (vim's `cursorline` — a touch
/// lighter than [`BG`] so text stays readable through it).
pub const CURSORLINE: Rgb = (0x2c, 0x2c, 0x2c);
/// Find-in-page: all matches (dim) and the current match (bright).
pub const FIND: Rgb = (0x5a, 0x52, 0x14);
pub const FIND_CUR: Rgb = (0xc8, 0x64, 0x1e);
/// The yellow highlight around a pane being moved (`Ctrl+W` move-pane mode).
pub const GRAB: Rgb = (0xf2, 0xc6, 0x4f);

/// The live, resolved chrome theme used at paint time — the customizable subset of the
/// colours above plus a bar-height multiplier. Built from the persisted
/// [`ThemeConfig`](crate::config::ThemeConfig) by
/// [`rebuild_theme`](crate::App::rebuild_theme); any unset override keeps its default
/// (the constants above). Only chrome is themed — page/content text keeps [`FG`]/[`BG`].
#[derive(Clone, Copy)]
pub struct Theme {
    /// Multiplier on the command/status bar height (1.0 = default).
    pub bar_scale: f32,
    pub bar_bg: Rgb,
    pub bar_fg: Rgb,
    pub accent: Rgb,
    pub bg: Rgb,
}

impl Default for Theme {
    fn default() -> Self {
        Theme { bar_scale: 1.0, bar_bg: BAR_BG, bar_fg: BAR_FG, accent: ACCENT, bg: BG }
    }
}

/// Parse a colour written as a `#rrggbb` / `rrggbb` hex string or a common colour
/// name (case- and space-insensitive). Returns `None` for anything unrecognised, so a
/// caller can report a typo instead of silently choosing the wrong colour.
pub fn parse_color(s: &str) -> Option<Rgb> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        let n = u32::from_str_radix(hex, 16).ok()?;
        return Some((((n >> 16) & 0xff) as u8, ((n >> 8) & 0xff) as u8, (n & 0xff) as u8));
    }
    let name: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    Some(match name.as_str() {
        "black" => (0x10, 0x10, 0x10),
        "white" => (0xf0, 0xf0, 0xf0),
        "gray" | "grey" => (0x80, 0x80, 0x80),
        "darkgray" | "darkgrey" => (0x3a, 0x3a, 0x3a),
        "red" => (0xe0, 0x6c, 0x6c),
        "green" => (0x3f, 0xb9, 0x50),
        "darkgreen" => (0x0a, 0x2a, 0x0a),
        "blue" => (0x6c, 0xb6, 0xff),
        "navy" | "darkblue" => (0x10, 0x1f, 0x40),
        "yellow" => (0xe6, 0xc5, 0x4e),
        "orange" => (0xe6, 0xa5, 0x5e),
        "purple" | "violet" => (0xc6, 0x9c, 0xf6),
        "pink" | "magenta" => (0xf6, 0x9c, 0xd0),
        "cyan" | "teal" => (0x5e, 0xc8, 0xd9),
        _ => return None,
    })
}

pub struct Painter {
    /// Primary monospace font + fallbacks, tried in order for each glyph. The
    /// primary (Consolas) covers normal text; fallbacks (e.g. Segoe UI Symbol)
    /// cover glyphs it lacks (Braille, symbols, dingbats) so terminals/pages don't
    /// show `.notdef` tofu boxes for them.
    fonts: Vec<Font>,
    /// Broad-script (CJK …) fallbacks, loaded LAZILY the first time a glyph misses
    /// every `fonts` entry — so a session that only ever shows Latin text never pays
    /// the cost of holding several large CJK faces in memory.
    cjk: OnceLock<Vec<Font>>,
    px: f32,
}

impl Painter {
    /// Load the monospace primary (Consolas → Segoe UI → Arial) plus best-effort
    /// symbol fallbacks for broad glyph coverage. CJK faces are deferred (see `cjk`).
    pub fn new(px: f32) -> Result<Self> {
        Self::with_primary(None, px)
    }

    /// Like [`new`](Self::new), but with an explicit primary font FILE in front (the
    /// terminal's custom font). The system monospace + symbol fallbacks still load
    /// behind it, so glyphs the custom face lacks don't render as tofu.
    pub fn with_primary(primary: Option<&std::path::Path>, px: f32) -> Result<Self> {
        let mut fonts = Vec::new();
        if let Some(path) = primary {
            let bytes = std::fs::read(path)
                .map_err(|e| anyhow!("reading font {}: {e}", path.display()))?;
            fonts.push(
                Font::from_bytes(bytes, FontSettings::default())
                    .map_err(|e| anyhow!("parsing font {}: {e}", path.display()))?,
            );
        }
        fonts.push(
            Font::from_bytes(load_system_font()?, FontSettings::default())
                .map_err(|e| anyhow!("parsing font: {e}"))?,
        );
        // Optional fallbacks — skipped silently if a face isn't installed.
        for path in [r"C:\Windows\Fonts\seguisym.ttf", r"C:\Windows\Fonts\arial.ttf"] {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(f) = Font::from_bytes(bytes, FontSettings::default()) {
                    fonts.push(f);
                }
            }
        }
        Ok(Painter { fonts, cjk: OnceLock::new(), px })
    }

    /// The first loaded font that has a glyph for `ch` (else the primary, which
    /// renders its `.notdef`). The Latin/symbol `fonts` are checked first — the hot
    /// path for ordinary text — and only on a miss are the CJK fallbacks consulted
    /// (loading them once, on demand), so Japanese/Chinese/Korean text renders
    /// instead of showing tofu boxes on `:read` and `:term`.
    fn font_for(&self, ch: char) -> &Font {
        if let Some(f) = self.fonts.iter().find(|f| f.lookup_glyph_index(ch) != 0) {
            return f;
        }
        for f in self.cjk.get_or_init(load_cjk_fonts) {
            if f.lookup_glyph_index(ch) != 0 {
                return f;
            }
        }
        &self.fonts[0]
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

/// Best-effort broad-script fallback faces for glyphs the Latin/symbol fonts lack —
/// chiefly CJK. Loaded once, on first miss (see [`Painter::font_for`]). Each path is
/// skipped silently if the face isn't installed. Japanese is listed first so the Han
/// ideographs shared across CJK render in Japanese forms; then Korean, then Chinese.
/// `.ttc` collections load their first face (fontdue's default `collection_index`).
fn load_cjk_fonts() -> Vec<Font> {
    const PATHS: &[&str] = &[
        r"C:\Windows\Fonts\YuGothR.ttc",   // Yu Gothic — Japanese (kana + kanji)
        r"C:\Windows\Fonts\msgothic.ttc",  // MS Gothic — Japanese fallback
        r"C:\Windows\Fonts\malgun.ttf",    // Malgun Gothic — Korean (Hangul)
        r"C:\Windows\Fonts\msyh.ttc",      // Microsoft YaHei — Simplified Chinese
        r"C:\Windows\Fonts\simsun.ttc",    // SimSun — Chinese fallback
    ];
    let mut fonts = Vec::new();
    for path in PATHS {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(f) = Font::from_bytes(bytes, FontSettings::default()) {
                fonts.push(f);
            }
        }
    }
    fonts
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

/// Resolve a font NAME (e.g. "Cascadia Code", "jetbrains mono") or explicit file
/// path to a font file. Names are matched case/space-insensitively against the
/// filenames in the system (`C:\Windows\Fonts`) and per-user font directories;
/// among matches the best score wins (exact > prefix > contains), ties broken by
/// the shortest name — so "cascadia code" picks CascadiaCode.ttf over
/// CascadiaCodeItalic.ttf. `None` when nothing matches (the caller reports it).
pub fn find_font(query: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    // An explicit path (or a bare filename with an extension) is used as-is.
    if query.contains(['/', '\\']) || query.contains('.') {
        let p = PathBuf::from(query);
        return p.is_file().then_some(p);
    }
    let norm = |s: &str| -> String {
        s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
    };
    let q = norm(query);
    if q.is_empty() {
        return None;
    }
    let mut dirs = vec![PathBuf::from(r"C:\Windows\Fonts")];
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        // Per-user installed fonts (the default for "Install for me" on Windows 10+).
        dirs.push(PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
    }
    let mut best: Option<(u32, usize, PathBuf)> = None;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str()).map(str::to_ascii_lowercase);
            if !matches!(ext.as_deref(), Some("ttf" | "otf" | "ttc")) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            let ns = norm(stem);
            let score = if ns == q {
                0
            } else if ns.starts_with(&q) {
                1
            } else if ns.contains(&q) || q.contains(&ns) {
                2
            } else {
                continue;
            };
            if best.as_ref().is_none_or(|(s, l, _)| (score, ns.len()) < (*s, *l)) {
                best = Some((score, ns.len(), path));
            }
        }
    }
    best.map(|(_, _, p)| p)
}

#[cfg(test)]
mod tests {
    use super::parse_color;

    #[test]
    fn parse_color_accepts_hex_and_names() {
        assert_eq!(parse_color("#0a2a0a"), Some((0x0a, 0x2a, 0x0a)));
        assert_eq!(parse_color("0a2a0a"), Some((0x0a, 0x2a, 0x0a)));
        assert_eq!(parse_color("#FFFFFF"), Some((0xff, 0xff, 0xff)));
        // Names are case- and space-insensitive.
        assert_eq!(parse_color("green"), parse_color("GREEN"));
        assert_eq!(parse_color("dark green"), parse_color("darkgreen"));
        assert!(parse_color("green").is_some());
        // Unrecognised input is reported, not silently coerced.
        assert_eq!(parse_color("chartreuse"), None);
        assert_eq!(parse_color("#12"), None);
        assert_eq!(parse_color("#xyzxyz"), None);
    }
}
