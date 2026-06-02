//! The rendered-page model.
//!
//! Every backend (text, search, …) produces a [`Document`]; the TUI knows how to
//! render a `Document` and nothing else. This is the seam that lets new modes plug
//! in without the renderer learning about them.

/// A followable link, shown to the user as `[id]`.
#[derive(Debug, Clone)]
pub struct Link {
    /// 1-based number displayed in the link table and inline (`[1]`).
    pub id: usize,
    pub url: String,
    pub text: String,
}

/// Inline content within a block.
#[derive(Debug, Clone)]
pub enum Span {
    Text(String),
    Emphasis(String),
    Strong(String),
    Code(String),
    /// References `Document.links[link_id - 1]`.
    Link { text: String, link_id: usize },
}

impl Span {
    /// Plain text of the span, used for in-page search and width math.
    pub fn plain(&self) -> &str {
        match self {
            Span::Text(s) | Span::Emphasis(s) | Span::Strong(s) | Span::Code(s) => s,
            Span::Link { text, .. } => text,
        }
    }
}

/// A block-level element. The TUI lays these out top to bottom.
#[derive(Debug, Clone)]
pub enum Block {
    Heading { level: u8, spans: Vec<Span> },
    Paragraph { spans: Vec<Span> },
    /// Preformatted; the renderer must NOT reflow these lines.
    Code { lines: Vec<String> },
    ListItem { ordered: bool, marker: String, spans: Vec<Span> },
    Quote { spans: Vec<Span> },
    Rule,
    Blank,
}

/// A fully extracted page ready to render.
#[derive(Debug, Clone, Default)]
pub struct Document {
    pub title: String,
    /// Canonical URL this document was loaded from (after redirects).
    pub url: String,
    pub blocks: Vec<Block>,
    /// Link table; `Link.id` is the 1-based position.
    pub links: Vec<Link>,
}

impl Document {
    pub fn new(url: impl Into<String>) -> Self {
        Document { url: url.into(), ..Default::default() }
    }

    /// Resolve a followable link number to its target URL.
    pub fn link_url(&self, id: usize) -> Option<&str> {
        self.links.get(id.checked_sub(1)?).map(|l| l.url.as_str())
    }
}

/// Builder that assigns link ids as links are discovered, so backends don't have
/// to track numbering themselves.
#[derive(Default)]
pub struct DocumentBuilder {
    doc: Document,
}

impl DocumentBuilder {
    pub fn new(url: impl Into<String>) -> Self {
        DocumentBuilder { doc: Document::new(url) }
    }

    pub fn title(&mut self, title: impl Into<String>) -> &mut Self {
        self.doc.title = title.into();
        self
    }

    pub fn push(&mut self, block: Block) -> &mut Self {
        self.doc.blocks.push(block);
        self
    }

    /// Register a link and return its 1-based id for use in a [`Span::Link`].
    pub fn add_link(&mut self, url: impl Into<String>, text: impl Into<String>) -> usize {
        let id = self.doc.links.len() + 1;
        self.doc.links.push(Link { id, url: url.into(), text: text.into() });
        id
    }

    pub fn build(self) -> Document {
        self.doc
    }
}
