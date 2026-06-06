//! Engine-free read mode: lay a [`Document`] out into wrapped, styled pixel lines
//! the shell paints itself with the softbuffer [`Painter`] — no WebView2 process.
//!
//! This mirrors the TUI's `render` word-wrapper, but in pixels (proportional/mono
//! glyph advances from `Painter::measure`) instead of a character grid. The result
//! is a flat `Vec<VLine>` the app scrolls with a vertical pixel offset; runs carry
//! an optional `link_id` so hint mode can label and follow links natively.

use browser_core::content::{Block, Document, Span};

use crate::draw::{self, Painter, Rgb};

/// Body text.
const FG: Rgb = draw::FG;
/// Brighter foreground for headings / strong text.
const STRONG: Rgb = draw::BAR_FG;
/// Inline / block code.
const CODE: Rgb = (0x9c, 0xd9, 0x7c);
/// Links (followed via hint mode).
const LINK: Rgb = draw::ACCENT;
/// Quote text and the quote bar / rule color.
const MUTED: Rgb = draw::DIM;
/// List-item markers.
const MARKER: Rgb = (0xe6, 0xc7, 0x5e);

/// A styled fragment within a line. Adjacent runs are drawn left to right.
pub struct Run {
    pub text: String,
    pub color: Rgb,
    /// 1-based `Document.links` id when this run is (part of) a link.
    pub link_id: Option<usize>,
}

/// One visual line: a left indent (px), its runs, and whether it's a horizontal
/// rule (drawn as a thin line spanning the content width instead of text).
pub struct VLine {
    pub indent: i32,
    pub runs: Vec<Run>,
    pub rule: bool,
}

/// A laid-out document: the lines, the per-line height, and total content height.
pub struct Layout {
    pub lines: Vec<VLine>,
    pub line_h: i32,
    pub height: i32,
}

impl Layout {
    /// The plain text of each visual line (run texts concatenated, indent excluded)
    /// — the grid the read-mode caret/visual selection operates on. Indices line up
    /// 1:1 with `lines`, so a caret row maps straight to a `VLine`.
    pub fn text_lines(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|vl| vl.runs.iter().map(|r| r.text.as_str()).collect::<String>())
            .collect()
    }
}

/// A whitespace-delimited, non-breakable unit carrying one styled fragment.
struct Word {
    text: String,
    color: Rgb,
    link_id: Option<usize>,
    width: i32,
}

/// Lay `doc` out for a content area `width` px wide.
pub fn layout(doc: &Document, width: i32, p: &Painter) -> Layout {
    let width = width.max(40);
    let line_h = p.line_height() as i32;
    let space_w = p.measure(" ") as i32;
    let indent = p.measure("    ") as i32;
    let mut lines: Vec<VLine> = Vec::new();

    // Title as a top heading, then a blank line.
    if !doc.title.is_empty() {
        let words = vec![word(&doc.title, STRONG, None, p)];
        wrap_into(&mut lines, words, width, 0, space_w);
        lines.push(blank());
    }

    for block in &doc.blocks {
        match block {
            Block::Heading { level, spans } => {
                let color = heading_color(*level);
                lines.push(blank());
                wrap_into(&mut lines, spans_to_words(spans, color, p), width, 0, space_w);
            }
            Block::Paragraph { spans } => {
                wrap_into(&mut lines, spans_to_words(spans, FG, p), width, 0, space_w);
                lines.push(blank());
            }
            Block::Quote { spans } => {
                let words = spans_to_words(spans, MUTED, p);
                let start = lines.len();
                wrap_into(&mut lines, words, width - indent, indent, space_w);
                // Prefix each wrapped quote line with a bar.
                for line in &mut lines[start..] {
                    line.runs.insert(
                        0,
                        Run { text: "\u{2502} ".into(), color: MUTED, link_id: None },
                    );
                    line.indent = indent / 2;
                }
                lines.push(blank());
            }
            Block::ListItem { marker, spans, .. } => {
                let mut words = vec![word(marker, MARKER, None, p)];
                words.extend(spans_to_words(spans, FG, p));
                wrap_into(&mut lines, words, width - indent, indent, space_w);
            }
            Block::Code { lines: code } => {
                for src in code {
                    lines.push(VLine {
                        indent,
                        runs: vec![Run { text: src.clone(), color: CODE, link_id: None }],
                        rule: false,
                    });
                }
                lines.push(blank());
            }
            Block::Rule => lines.push(VLine { indent: 0, runs: Vec::new(), rule: true }),
            Block::Blank => lines.push(blank()),
        }
    }

    let height = lines.len() as i32 * line_h;
    Layout { lines, line_h, height }
}

/// First on-screen occurrence of each link, as `(link_id, x_px, baseline_y_px)`,
/// for placing hint labels. `top`/`bottom` bound the visible content band.
pub fn visible_links(
    layout: &Layout,
    scroll: i32,
    top: i32,
    bottom: i32,
    p: &Painter,
) -> Vec<(usize, i32, i32)> {
    let mut out: Vec<(usize, i32, i32)> = Vec::new();
    let mut seen: Vec<usize> = Vec::new();
    for (i, line) in layout.lines.iter().enumerate() {
        let baseline = top - scroll + i as i32 * layout.line_h + (layout.line_h * 3 / 4);
        if baseline < top || baseline > bottom + layout.line_h {
            continue;
        }
        let mut x = line.indent;
        for run in &line.runs {
            if let Some(id) = run.link_id {
                if !seen.contains(&id) {
                    seen.push(id);
                    out.push((id, x, baseline));
                }
            }
            x += p.measure(&run.text) as i32;
        }
    }
    out
}

fn heading_color(level: u8) -> Rgb {
    match level {
        1 => STRONG,
        2 => LINK,
        3 => draw::READ,
        _ => FG,
    }
}

fn blank() -> VLine {
    VLine { indent: 0, runs: Vec::new(), rule: false }
}

fn word(text: &str, color: Rgb, link_id: Option<usize>, p: &Painter) -> Word {
    Word { text: text.to_string(), color, link_id, width: p.measure(text) as i32 }
}

/// Flatten spans into wrappable words, coloring per span kind (links keep their id).
fn spans_to_words(spans: &[Span], base: Rgb, p: &Painter) -> Vec<Word> {
    let mut words = Vec::new();
    for span in spans {
        let (color, id) = match span {
            Span::Text(_) | Span::Emphasis(_) => (base, None),
            Span::Strong(_) => (STRONG, None),
            Span::Code(_) => (CODE, None),
            Span::Link { link_id, .. } => (LINK, Some(*link_id)),
        };
        for piece in span.plain().split_whitespace() {
            words.push(word(piece, color, id, p));
        }
    }
    words
}

/// Greedily pack words into lines no wider than `width`, with a left `indent`.
fn wrap_into(out: &mut Vec<VLine>, words: Vec<Word>, width: i32, indent: i32, space_w: i32) {
    let avail = (width - indent).max(space_w * 4);
    let mut runs: Vec<Run> = Vec::new();
    let mut cur_w = 0i32;
    for w in words {
        let extra = if cur_w == 0 { 0 } else { space_w };
        if cur_w > 0 && cur_w + extra + w.width > avail {
            out.push(VLine { indent, runs: std::mem::take(&mut runs), rule: false });
            cur_w = 0;
        }
        if cur_w > 0 {
            runs.push(Run { text: " ".into(), color: FG, link_id: None });
            cur_w += space_w;
        }
        runs.push(Run { text: w.text, color: w.color, link_id: w.link_id });
        cur_w += w.width;
    }
    if !runs.is_empty() {
        out.push(VLine { indent, runs, rule: false });
    }
}
