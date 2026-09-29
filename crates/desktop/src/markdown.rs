//! Markdown for the `:ai` tab. A reply is turned into wrapped plain-text lines plus
//! per-line colour runs, so the conversation stays a vim buffer (selecting and
//! yanking copy the rendered text, not the markup) while headings, emphasis, code,
//! lists, quotes, rules and tables read as intended. Works in monospace columns,
//! like the rest of the `:ai` tab; colours match the engine-free read view.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::draw::Rgb;
use crate::read_view::{CODE, FG, LINK, MARKER, MUTED, STRONG};

/// Background tint behind code-block lines.
pub(crate) const CODE_BG: Rgb = (0x26, 0x26, 0x26);

/// A coloured column span `[start, end)` of a line, in chars.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Run {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) color: Rgb,
}

/// How to paint one line: colour runs covering it left to right (anything past the
/// last run is drawn in the default colour) and an optional full-width background.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LineStyle {
    pub(crate) runs: Vec<Run>,
    pub(crate) bg: Option<Rgb>,
}

impl LineStyle {
    /// A whole line in one colour.
    pub(crate) fn solid(color: Rgb, len: usize) -> Self {
        LineStyle {
            runs: vec![Run {
                start: 0,
                end: len,
                color,
            }],
            bg: None,
        }
    }
}

/// Render `text` as markdown wrapped to `cols` columns, appending to `lines`/`styles`.
pub(crate) fn render(
    text: &str,
    cols: usize,
    lines: &mut Vec<String>,
    styles: &mut Vec<LineStyle>,
) {
    let mut r = Renderer {
        cols: cols.max(8),
        out: Vec::new(),
        ..Renderer::default()
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(unwrap_markdown_fence(text), options) {
        r.event(event);
    }
    r.flush();
    // No trailing blank: the conversation adds its own spacing between turns.
    while r.out.last().is_some_and(|l| l.cells.is_empty()) {
        r.out.pop();
    }
    if r.out.is_empty() {
        r.out.push(OutLine::default());
    }
    for line in r.out {
        let (text, style) = finish(line);
        lines.push(text);
        styles.push(style);
    }
}

/// Models sometimes wrap a whole reply in a ```` ```markdown ```` fence when asked for
/// markdown, which would render as one big code block. Render the inside instead.
fn unwrap_markdown_fence(text: &str) -> &str {
    let t = text.trim();
    let Some((first, rest)) = t.split_once('\n') else {
        return text;
    };
    let lang = first
        .trim()
        .trim_start_matches('`')
        .trim()
        .to_ascii_lowercase();
    if !first.trim_start().starts_with("```") || !(lang == "markdown" || lang == "md") {
        return text;
    }
    match rest.trim_end().strip_suffix("```") {
        Some(inner) if inner.ends_with('\n') || inner.is_empty() => inner,
        _ => text,
    }
}

/// A character and its colour: the unit lines are built from.
type Cell = (char, Rgb);

/// A rendered line: its cells, and a background for the whole row (code blocks).
#[derive(Default)]
struct OutLine {
    cells: Vec<Cell>,
    bg: Option<Rgb>,
}

/// Inline content waiting to be wrapped: coloured text, or a forced line break.
enum Piece {
    Text(String, Rgb),
    Break,
}

/// One list item being rendered: its marker, and whether its first line (the one
/// that shows the marker) has been emitted yet.
struct Item {
    marker: String,
    used: bool,
}

#[derive(Default)]
struct Renderer {
    cols: usize,
    out: Vec<OutLine>,
    /// Inline content of the block being built (paragraph, heading, list item text).
    pieces: Vec<Piece>,
    /// Put a blank line before the next block.
    gap: bool,
    heading: Option<HeadingLevel>,
    strong: usize,
    strike: usize,
    link: Option<String>,
    link_text: String,
    quote: usize,
    /// Open lists: `Some(next number)` for ordered ones.
    lists: Vec<Option<u64>>,
    items: Vec<Item>,
    code: Option<(String, String)>,
    table: Option<Table>,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Cell>>>,
    header_rows: usize,
    in_header: bool,
    cell: Vec<Cell>,
}

impl Renderer {
    fn color(&self) -> Rgb {
        if self.link.is_some() {
            LINK
        } else if self.strike > 0 || self.quote > 0 {
            MUTED
        } else if self.strong > 0 {
            STRONG
        } else if let Some(level) = self.heading {
            heading_color(level)
        } else {
            FG
        }
    }

    fn push_text(&mut self, text: &str, color: Rgb) {
        if let Some(table) = self.table.as_mut() {
            table.cell.extend(text.chars().map(|ch| (ch, color)));
            return;
        }
        if self.link.is_some() {
            self.link_text.push_str(text);
        }
        self.pieces.push(Piece::Text(text.to_string(), color));
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some((_, code)) = self.code.as_mut() {
                    code.push_str(&t);
                } else {
                    let c = self.color();
                    self.push_text(&t, c);
                }
            }
            Event::Code(t) => self.push_text(&t, CODE),
            Event::Html(t) | Event::InlineHtml(t) => self.push_text(&t, MUTED),
            Event::SoftBreak => self.push_text(" ", FG),
            Event::HardBreak => {
                if let Some(table) = self.table.as_mut() {
                    table.cell.push((' ', FG));
                } else {
                    self.pieces.push(Piece::Break);
                }
            }
            Event::Rule => {
                self.flush();
                self.open_block();
                let width = self.cols.saturating_sub(self.prefix_width()).min(60);
                let line = vec![('─', MUTED); width];
                self.emit(line);
                self.gap = true;
            }
            Event::TaskListMarker(done) => {
                if let Some(item) = self.items.last_mut() {
                    item.marker = if done { "[x] " } else { "[ ] " }.to_string();
                }
            }
            Event::FootnoteReference(t) => self.push_text(&format!("[{t}]"), MUTED),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush();
                self.heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(first) => {
                self.flush();
                if self.lists.is_empty() {
                    self.open_block();
                }
                self.lists.push(first);
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.items.push(Item {
                    marker,
                    used: false,
                });
            }
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.link = Some(dest_url.to_string());
                self.link_text.clear();
            }
            Tag::Image { dest_url, .. } => self.push_text(&format!("[image: {dest_url}] "), MUTED),
            Tag::Table(_) => {
                self.flush();
                self.table = Some(Table::default());
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.in_header = true;
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.cell.clear();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.gap = true;
            }
            TagEnd::Heading(level) => {
                self.flush();
                if level == HeadingLevel::H1 {
                    let width = self.out.last().map_or(0, |l| l.cells.len());
                    self.emit(vec![('─', MUTED); width]);
                }
                self.heading = None;
                self.gap = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.gap = true;
            }
            TagEnd::CodeBlock => self.code_block(),
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.gap = true;
                }
            }
            TagEnd::Item => {
                self.flush();
                self.items.pop();
            }
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                // Keep the address visible (and yankable) when the text isn't it.
                if let Some(url) = self.link.take() {
                    let shown = self.link_text.trim();
                    if !url.is_empty() && shown != url && !url.starts_with('#') {
                        self.push_text(&format!(" ({url})"), MUTED);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    let cell = std::mem::take(&mut t.cell);
                    if let Some(row) = t.rows.last_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.in_header = false;
                    t.header_rows = t.rows.len();
                }
            }
            TagEnd::Table => self.table_block(),
            _ => {}
        }
    }

    /// Before a block's first line: a blank line if the previous block asked for one.
    fn open_block(&mut self) {
        if self.gap && !self.out.is_empty() {
            self.out.push(OutLine::default());
        }
        self.gap = false;
    }

    /// Width of the quote bars and list indentation in front of body lines.
    fn prefix_width(&self) -> usize {
        self.quote * 2
            + self
                .items
                .iter()
                .map(|i| i.marker.chars().count())
                .sum::<usize>()
    }

    /// The prefix for the next line: quote bars, then list indentation with the
    /// innermost item's marker on its first line.
    fn prefix(&mut self) -> Vec<Cell> {
        let mut p: Vec<Cell> = Vec::new();
        for _ in 0..self.quote {
            p.push(('│', MUTED));
            p.push((' ', MUTED));
        }
        let last = self.items.len().saturating_sub(1);
        for (i, item) in self.items.iter_mut().enumerate() {
            if i == last && !item.used {
                p.extend(item.marker.chars().map(|ch| (ch, MARKER)));
                item.used = true;
            } else {
                p.extend(item.marker.chars().map(|_| (' ', FG)));
            }
        }
        p
    }

    fn emit(&mut self, content: Vec<Cell>) {
        let mut cells = self.prefix();
        cells.extend(content);
        self.out.push(OutLine { cells, bg: None });
    }

    /// Wrap and emit the pending inline content as one block.
    fn flush(&mut self) {
        if self.pieces.is_empty() {
            return;
        }
        let pieces = std::mem::take(&mut self.pieces);
        let width = self.cols.saturating_sub(self.prefix_width()).max(8);
        let wrapped = wrap(&pieces, width);
        if wrapped.iter().all(|l| l.is_empty()) {
            return;
        }
        self.open_block();
        for line in wrapped {
            self.emit(line);
        }
    }

    fn code_block(&mut self) {
        let Some((lang, code)) = self.code.take() else {
            return;
        };
        self.open_block();
        if !lang.is_empty() {
            self.emit(lang.chars().map(|ch| (ch, MUTED)).collect());
        }
        let body = code.strip_suffix('\n').unwrap_or(&code);
        for src in body.split('\n') {
            // Code keeps its indentation and is never reflowed; long lines scroll.
            let mut line: Vec<Cell> = vec![(' ', CODE)];
            line.extend(src.replace('\t', "    ").chars().map(|ch| (ch, CODE)));
            self.emit(line);
            if let Some(last) = self.out.last_mut() {
                last.bg = Some(CODE_BG);
            }
        }
        self.gap = true;
    }

    fn table_block(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![0usize; columns];
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(trimmed(cell).len());
            }
        }
        self.open_block();
        for (r, row) in table.rows.iter().enumerate() {
            let header = r < table.header_rows;
            let mut line: Vec<Cell> = Vec::new();
            for (i, width) in widths.iter().enumerate() {
                if i > 0 {
                    line.extend(" │ ".chars().map(|ch| (ch, MUTED)));
                }
                let cell = row.get(i).map(|c| trimmed(c)).unwrap_or_default();
                let len = cell.len();
                line.extend(
                    cell.into_iter()
                        .map(|(ch, c)| (ch, if header { STRONG } else { c })),
                );
                // Pad every column but the last, so no line ends in spaces.
                if i + 1 < widths.len() {
                    line.extend(std::iter::repeat_n((' ', FG), width - len));
                }
            }
            self.emit(line);
            if header && r + 1 == table.header_rows {
                let mut rule: Vec<Cell> = Vec::new();
                for (i, width) in widths.iter().enumerate() {
                    if i > 0 {
                        rule.extend("─┼─".chars().map(|ch| (ch, MUTED)));
                    }
                    rule.extend(std::iter::repeat_n(('─', MUTED), *width));
                }
                self.emit(rule);
            }
        }
        self.gap = true;
    }
}

fn heading_color(level: HeadingLevel) -> Rgb {
    match level {
        HeadingLevel::H1 => STRONG,
        HeadingLevel::H2 => LINK,
        HeadingLevel::H3 => crate::draw::READ,
        _ => FG,
    }
}

/// A table cell without its surrounding whitespace.
fn trimmed(cell: &[Cell]) -> Vec<Cell> {
    let start = cell
        .iter()
        .position(|(ch, _)| !ch.is_whitespace())
        .unwrap_or(cell.len());
    let end = cell
        .iter()
        .rposition(|(ch, _)| !ch.is_whitespace())
        .map_or(start, |i| i + 1);
    cell[start..end].to_vec()
}

/// Greedy word wrap of coloured inline content to `width` columns. Runs of
/// whitespace collapse to one space; a word longer than a line is hard-broken.
fn wrap(pieces: &[Piece], width: usize) -> Vec<Vec<Cell>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut line: Vec<Cell> = Vec::new();
    let mut word: Vec<Cell> = Vec::new();
    let mut space = false;

    let place =
        |line: &mut Vec<Cell>, lines: &mut Vec<Vec<Cell>>, word: &mut Vec<Cell>, space: bool| {
            if word.is_empty() {
                return;
            }
            let gap = usize::from(space && !line.is_empty());
            if !line.is_empty() && line.len() + gap + word.len() > width {
                lines.push(std::mem::take(line));
            } else if gap == 1 {
                line.push((' ', FG));
            }
            for cell in word.drain(..) {
                if line.len() == width {
                    lines.push(std::mem::take(line));
                }
                line.push(cell);
            }
        };

    for piece in pieces {
        match piece {
            Piece::Text(text, color) => {
                for ch in text.chars() {
                    if ch.is_whitespace() {
                        if !word.is_empty() {
                            place(&mut line, &mut lines, &mut word, space);
                        }
                        space = true;
                    } else {
                        word.push((ch, *color));
                    }
                }
            }
            Piece::Break => {
                place(&mut line, &mut lines, &mut word, space);
                lines.push(std::mem::take(&mut line));
                space = false;
            }
        }
    }
    place(&mut line, &mut lines, &mut word, space);
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Turn a rendered line into its plain text and colour runs.
fn finish(line: OutLine) -> (String, LineStyle) {
    let mut runs: Vec<Run> = Vec::new();
    for (i, (_, color)) in line.cells.iter().enumerate() {
        match runs.last_mut() {
            Some(run) if run.color == *color && run.end == i => run.end = i + 1,
            _ => runs.push(Run {
                start: i,
                end: i + 1,
                color: *color,
            }),
        }
    }
    let text = line.cells.iter().map(|(ch, _)| *ch).collect();
    (text, LineStyle { runs, bg: line.bg })
}

/// Convert markdown to a read-view [`Document`](browser_core::Document), for pages the
/// browser ships as markdown (`:news`). Links become followable link spans. Nested
/// list items are flattened (the read view has one list indent).
pub(crate) fn to_document(md: &str, url: &str) -> browser_core::Document {
    use browser_core::content::{Block, Link, Span};

    struct ItemState {
        ordered: bool,
        marker: String,
        /// Already emitted, because a nested list started after its text.
        emitted: bool,
    }

    let mut doc = browser_core::Document::new(url);
    let mut spans: Vec<Span> = Vec::new();
    let (mut strong, mut emphasis) = (0usize, 0usize);
    let mut link: Option<(String, String)> = None;
    let mut heading: Option<u8> = None;
    let mut quote = 0usize;
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut items: Vec<ItemState> = Vec::new();
    let mut code: Option<String> = None;

    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(md, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => heading = Some(level as u8),
            Event::End(TagEnd::Heading(_)) => {
                let spans = std::mem::take(&mut spans);
                doc.blocks.push(Block::Heading {
                    level: heading.take().unwrap_or(1),
                    spans,
                });
            }
            Event::Start(Tag::BlockQuote(_)) => quote += 1,
            Event::End(TagEnd::BlockQuote(_)) => quote = quote.saturating_sub(1),
            Event::End(TagEnd::Paragraph) => {
                if items.is_empty() {
                    let spans = std::mem::take(&mut spans);
                    doc.blocks.push(if quote > 0 {
                        Block::Quote { spans }
                    } else {
                        Block::Paragraph { spans }
                    });
                } else {
                    // A loose list item's paragraphs run together in the item.
                    spans.push(Span::Text(" ".into()));
                }
            }
            Event::Start(Tag::List(first)) => {
                // A nested list after an item's own text: emit that text first.
                if let Some(item) = items.last_mut() {
                    if !spans.is_empty() {
                        doc.blocks.push(Block::ListItem {
                            ordered: item.ordered,
                            marker: item.marker.clone(),
                            spans: std::mem::take(&mut spans),
                        });
                        item.emitted = true;
                    }
                }
                lists.push(first);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                if lists.is_empty() {
                    doc.blocks.push(Block::Blank);
                }
            }
            Event::Start(Tag::Item) => {
                let depth = lists.len();
                let (ordered, marker) = match lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}.");
                        *n += 1;
                        (true, m)
                    }
                    _ => (false, if depth > 1 { "◦" } else { "•" }.to_string()),
                };
                items.push(ItemState {
                    ordered,
                    marker,
                    emitted: false,
                });
            }
            Event::End(TagEnd::Item) => {
                if let Some(item) = items.pop() {
                    if !item.emitted || !spans.is_empty() {
                        doc.blocks.push(Block::ListItem {
                            ordered: item.ordered,
                            marker: item.marker,
                            spans: std::mem::take(&mut spans),
                        });
                    }
                }
            }
            Event::TaskListMarker(done) => {
                if let Some(item) = items.last_mut() {
                    item.marker = if done { "[x]" } else { "[ ]" }.to_string();
                }
            }
            Event::Start(Tag::CodeBlock(_)) => code = Some(String::new()),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(src) = code.take() {
                    let src = src.strip_suffix('\n').unwrap_or(&src);
                    doc.blocks.push(Block::Code {
                        lines: src.split('\n').map(str::to_string).collect(),
                    });
                }
            }
            Event::Rule => doc.blocks.push(Block::Rule),
            Event::Start(Tag::Strong) => strong += 1,
            Event::End(TagEnd::Strong) => strong = strong.saturating_sub(1),
            Event::Start(Tag::Emphasis) => emphasis += 1,
            Event::End(TagEnd::Emphasis) => emphasis = emphasis.saturating_sub(1),
            Event::Start(Tag::Link { dest_url, .. }) => {
                link = Some((dest_url.to_string(), String::new()));
            }
            Event::End(TagEnd::Link) => {
                if let Some((url, text)) = link.take() {
                    let id = doc.links.len() + 1;
                    doc.links.push(Link {
                        id,
                        url,
                        text: text.clone(),
                    });
                    spans.push(Span::Link { text, link_id: id });
                }
            }
            Event::Text(t) => {
                if let Some(src) = code.as_mut() {
                    src.push_str(&t);
                } else if let Some((_, text)) = link.as_mut() {
                    text.push_str(&t);
                } else if strong > 0 {
                    spans.push(Span::Strong(t.to_string()));
                } else if emphasis > 0 {
                    spans.push(Span::Emphasis(t.to_string()));
                } else {
                    spans.push(Span::Text(t.to_string()));
                }
            }
            Event::Code(t) => match link.as_mut() {
                Some((_, text)) => text.push_str(&t),
                None => spans.push(Span::Code(t.to_string())),
            },
            Event::Html(t) | Event::InlineHtml(t) => spans.push(Span::Text(t.to_string())),
            Event::SoftBreak | Event::HardBreak => spans.push(Span::Text(" ".into())),
            Event::End(TagEnd::TableCell) => spans.push(Span::Text("   ".into())),
            Event::End(TagEnd::TableHead | TagEnd::TableRow) => {
                let spans = std::mem::take(&mut spans);
                doc.blocks.push(Block::Paragraph { spans });
            }
            _ => {}
        }
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_keep_headings_lists_and_followable_links() {
        use browser_core::content::{Block, Span};
        let md = "## 0.1.1 (2026-09-29)\n\n### Features\n\n* add `:news` ([abc1234](https://example.com/c/abc))\n* nested\n  * inner\n";
        let doc = to_document(md, "browser://news");
        assert!(matches!(&doc.blocks[0], Block::Heading { level: 2, .. }));
        assert!(matches!(&doc.blocks[1], Block::Heading { level: 3, .. }));
        let Block::ListItem { marker, spans, .. } = &doc.blocks[2] else {
            panic!("expected a list item, got {:?}", doc.blocks[2]);
        };
        assert_eq!(marker, "•");
        assert!(spans
            .iter()
            .any(|s| matches!(s, Span::Code(c) if c == ":news")));
        assert!(spans
            .iter()
            .any(|s| matches!(s, Span::Link { link_id: 1, .. })));
        assert_eq!(doc.links[0].url, "https://example.com/c/abc");
        assert_eq!(doc.links[0].text, "abc1234");
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::ListItem { marker, .. } if marker == "◦")));
    }

    fn text(md: &str, cols: usize) -> Vec<String> {
        let (mut lines, mut styles) = (Vec::new(), Vec::new());
        render(md, cols, &mut lines, &mut styles);
        lines
    }

    fn styled(md: &str, cols: usize) -> (Vec<String>, Vec<LineStyle>) {
        let (mut lines, mut styles) = (Vec::new(), Vec::new());
        render(md, cols, &mut lines, &mut styles);
        (lines, styles)
    }

    #[test]
    fn markup_is_removed_and_blocks_are_spaced() {
        let lines = text("# Title\n\nSome **bold** and `code`.\n\n---\n\nEnd", 40);
        assert_eq!(
            lines,
            vec![
                "Title",
                "─────",
                "",
                "Some bold and code.",
                "",
                &"─".repeat(40),
                "",
                "End"
            ]
        );
    }

    #[test]
    fn inline_styles_become_colour_runs() {
        let (lines, styles) = styled("plain **bold** `code`", 40);
        assert_eq!(lines, vec!["plain bold code"]);
        let color_at = |col: usize| {
            styles[0]
                .runs
                .iter()
                .find(|r| r.start <= col && col < r.end)
                .map(|r| r.color)
        };
        assert_eq!(color_at(0), Some(FG));
        assert_eq!(color_at(6), Some(STRONG));
        assert_eq!(color_at(11), Some(CODE));
    }

    #[test]
    fn nested_lists_indent_and_number() {
        let lines = text("- one\n  - inner\n- two\n\n1. first\n2. second", 40);
        assert_eq!(
            lines,
            vec!["• one", "  • inner", "• two", "", "1. first", "2. second"]
        );
    }

    #[test]
    fn list_items_wrap_under_their_text() {
        let lines = text("- alpha beta gamma delta", 12);
        assert_eq!(lines, vec!["• alpha beta", "  gamma", "  delta"]);
    }

    #[test]
    fn code_blocks_keep_indentation_and_get_a_background() {
        let (lines, styles) = styled("```python\ndef f():\n    return 1\n```", 40);
        assert_eq!(lines, vec!["python", " def f():", "     return 1"]);
        assert_eq!(styles[0].bg, None);
        assert_eq!(styles[1].bg, Some(CODE_BG));
        assert_eq!(styles[2].bg, Some(CODE_BG));
    }

    #[test]
    fn a_reply_wrapped_in_a_markdown_fence_is_rendered() {
        let md = "```markdown\n# Sample\n\n- item\n\n```python\nprint(1)\n```\n```";
        let lines = text(md, 40);
        assert_eq!(lines[0], "Sample");
        assert!(lines.contains(&"• item".to_string()));
        assert!(lines.contains(&" print(1)".to_string()));
    }

    #[test]
    fn tables_align_columns() {
        let lines = text("| a | bb |\n|---|---|\n| ccc | d |", 40);
        assert_eq!(lines, vec!["a   │ bb", "────┼───", "ccc │ d"]);
    }

    #[test]
    fn links_show_their_address_and_quotes_get_a_bar() {
        assert_eq!(
            text("[docs](https://docs.rs)", 60),
            vec!["docs (https://docs.rs)"]
        );
        assert_eq!(text("<https://docs.rs>", 60), vec!["https://docs.rs"]);
        assert_eq!(text("> quoted", 60), vec!["│ quoted"]);
    }

    #[test]
    fn task_lists_and_empty_input() {
        assert_eq!(
            text("- [x] done\n- [ ] todo", 40),
            vec!["[x] done", "[ ] todo"]
        );
        assert_eq!(text("", 40), vec![""]);
    }
}
