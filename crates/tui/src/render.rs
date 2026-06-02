//! Turn a [`Document`] into wrapped, styled terminal lines.
//!
//! We pre-wrap to the viewport width ourselves (rather than leaning on ratatui's
//! `Wrap`) so that code blocks can opt out of wrapping and so inline link markers
//! `[n]` stay attached to their link text. The result is a flat `Vec<Line>` the
//! app scrolls through with a simple vertical offset.

use browser_core::content::{Block, Document, Span};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span as TSpan};

/// A styled, non-breakable unit (a "word") made of one or more styled fragments.
struct Word {
    frags: Vec<(String, Style)>,
}

impl Word {
    fn width(&self) -> usize {
        self.frags.iter().map(|(s, _)| s.chars().count()).sum()
    }
}

fn link_style() -> Style {
    Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)
}
fn marker_style() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn code_style() -> Style {
    Style::default().fg(Color::Green)
}

/// Render the whole document to lines for a given content `width`.
pub fn render_document(doc: &Document, width: usize) -> Vec<Line<'static>> {
    let width = width.max(10);
    let mut out: Vec<Line<'static>> = Vec::new();

    for block in &doc.blocks {
        match block {
            Block::Heading { level, spans } => {
                let base = heading_style(*level);
                out.extend(wrap_words(spans_to_words(spans, base), width, 0));
            }
            Block::Paragraph { spans } => {
                out.extend(wrap_words(spans_to_words(spans, Style::default()), width, 0));
            }
            Block::Quote { spans } => {
                let base = Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC);
                for mut line in wrap_words(spans_to_words(spans, base), width, 2) {
                    line.spans.insert(0, TSpan::styled("│ ", marker_style()));
                    out.push(line);
                }
            }
            Block::ListItem { marker, spans, .. } => {
                let mut words = vec![Word {
                    frags: vec![(marker.clone(), Style::default().fg(Color::Yellow))],
                }];
                words.extend(spans_to_words(spans, Style::default()));
                out.extend(wrap_words(words, width, 2));
            }
            Block::Code { lines } => {
                for src in lines {
                    out.push(Line::from(vec![
                        TSpan::raw("  "),
                        TSpan::styled(src.clone(), code_style()),
                    ]));
                }
            }
            Block::Rule => {
                out.push(Line::styled("─".repeat(width), marker_style()));
            }
            Block::Blank => out.push(Line::raw("")),
        }
    }

    if !doc.links.is_empty() {
        out.push(Line::raw(""));
        out.push(Line::styled("─".repeat(width), marker_style()));
        out.push(Line::styled("Links", heading_style(2)));
        for link in &doc.links {
            out.push(Line::from(vec![
                TSpan::styled(format!("[{}] ", link.id), marker_style()),
                TSpan::styled(link.url.clone(), Style::default().fg(Color::Blue)),
            ]));
        }
    }

    out
}

fn heading_style(level: u8) -> Style {
    let color = match level {
        1 => Color::Magenta,
        2 => Color::Cyan,
        3 => Color::Blue,
        _ => Color::White,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// Flatten spans into wrappable words, merging each span's style over `base`.
fn spans_to_words(spans: &[Span], base: Style) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    for span in spans {
        match span {
            Span::Text(t) => push_words(&mut words, t, base),
            Span::Emphasis(t) => push_words(&mut words, t, base.add_modifier(Modifier::ITALIC)),
            Span::Strong(t) => push_words(&mut words, t, base.add_modifier(Modifier::BOLD)),
            Span::Code(t) => push_words(&mut words, t, base.patch(code_style())),
            Span::Link { text, link_id } => {
                push_words(&mut words, text, base.patch(link_style()));
                let marker = format!("[{link_id}]");
                match words.last_mut() {
                    Some(last) => last.frags.push((marker, marker_style())),
                    None => words.push(Word { frags: vec![(marker, marker_style())] }),
                }
            }
        }
    }
    words
}

/// Split `text` on whitespace into single-fragment words.
fn push_words(words: &mut Vec<Word>, text: &str, style: Style) {
    for piece in text.split_whitespace() {
        words.push(Word { frags: vec![(piece.to_string(), style)] });
    }
}

/// Greedily pack words into lines no wider than `width`, with a left `indent`.
fn wrap_words(words: Vec<Word>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let avail = width.saturating_sub(indent).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<(String, Style)> = Vec::new();
    let mut cur_w = 0usize;

    for word in words {
        let ww = word.width();
        let extra = if cur_w == 0 { 0 } else { 1 };
        if cur_w > 0 && cur_w + extra + ww > avail {
            lines.push(make_line(indent, std::mem::take(&mut cur)));
            cur_w = 0;
        }
        if cur_w > 0 {
            cur.push((" ".to_string(), Style::default()));
            cur_w += 1;
        }
        cur.extend(word.frags);
        cur_w += ww;
    }
    if !cur.is_empty() {
        lines.push(make_line(indent, cur));
    }
    if lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

fn make_line(indent: usize, frags: Vec<(String, Style)>) -> Line<'static> {
    let mut spans: Vec<TSpan<'static>> = Vec::with_capacity(frags.len() + 1);
    if indent > 0 {
        spans.push(TSpan::raw(" ".repeat(indent)));
    }
    for (text, style) in frags {
        spans.push(TSpan::styled(text, style));
    }
    Line::from(spans)
}

/// Plain text of a rendered line, for in-page search.
pub fn line_text(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}
