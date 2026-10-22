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
    Link {
        text: String,
        link_id: usize,
    },
    /// The marker (`[1]`) where a side note was taken out of the text; the note
    /// itself follows its block as a [`Block::Note`] with the same label.
    NoteRef(String),
}

impl Span {
    /// Plain text of the span, used for in-page search and width math.
    pub fn plain(&self) -> &str {
        match self {
            Span::Text(s)
            | Span::Emphasis(s)
            | Span::Strong(s)
            | Span::Code(s)
            | Span::NoteRef(s) => s,
            Span::Link { text, .. } => text,
        }
    }
}

/// A block-level element. The TUI lays these out top to bottom.
#[derive(Debug, Clone)]
pub enum Block {
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph {
        spans: Vec<Span>,
    },
    /// Preformatted; the renderer must NOT reflow these lines.
    Code {
        lines: Vec<String>,
    },
    ListItem {
        ordered: bool,
        marker: String,
        spans: Vec<Span>,
    },
    Quote {
        spans: Vec<Span>,
    },
    /// A side note (a page's margin note or inline footnote), shown after the block
    /// whose [`Span::NoteRef`] carries the same `label`.
    Note {
        label: String,
        spans: Vec<Span>,
    },
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
    /// In-page targets: an element `id` (or `<a name>`) and the index of the block
    /// it starts in, so a `#fragment` link can jump there.
    pub anchors: Vec<(String, usize)>,
}

impl Document {
    pub fn new(url: impl Into<String>) -> Self {
        Document {
            url: url.into(),
            ..Default::default()
        }
    }

    /// Resolve a followable link number to its target URL.
    pub fn link_url(&self, id: usize) -> Option<&str> {
        self.links.get(id.checked_sub(1)?).map(|l| l.url.as_str())
    }

    /// The block an in-page `#fragment` points at (already percent-decoded).
    pub fn anchor_block(&self, fragment: &str) -> Option<usize> {
        self.anchors
            .iter()
            .find(|(id, _)| id == fragment)
            .map(|&(_, block)| block)
    }
}

/// Builder that assigns link ids as links are discovered, so backends don't have
/// to track numbering themselves.
#[derive(Default)]
pub struct DocumentBuilder {
    doc: Document,
    /// Anchors seen since the last block was pushed; they point at the next one.
    pending_anchors: Vec<String>,
    /// Side notes taken out of the block being built; they follow it.
    pending_notes: Vec<Block>,
    notes: usize,
}

impl DocumentBuilder {
    pub fn new(url: impl Into<String>) -> Self {
        DocumentBuilder {
            doc: Document::new(url),
            pending_anchors: Vec::new(),
            pending_notes: Vec::new(),
            notes: 0,
        }
    }

    pub fn title(&mut self, title: impl Into<String>) -> &mut Self {
        self.doc.title = title.into();
        self
    }

    pub fn push(&mut self, block: Block) -> &mut Self {
        let at = self.doc.blocks.len();
        for id in self.pending_anchors.drain(..) {
            self.doc.anchors.push((id, at));
        }
        self.doc.blocks.push(block);
        // Notes found inside this block follow it.
        let notes = std::mem::take(&mut self.pending_notes);
        self.doc.blocks.extend(notes);
        self
    }

    /// Take a side note out of the text: it's shown after the block being built,
    /// under the returned label (`[1]`), which marks its place in the text.
    pub fn note(&mut self, spans: Vec<Span>) -> String {
        self.notes += 1;
        let label = format!("[{}]", self.notes);
        self.pending_notes.push(Block::Note {
            label: label.clone(),
            spans,
        });
        label
    }

    /// Mark an in-page target (an element `id`): it resolves to the next block pushed.
    pub fn anchor(&mut self, id: impl Into<String>) -> &mut Self {
        let id = id.into();
        if !id.is_empty() {
            self.pending_anchors.push(id);
        }
        self
    }

    /// Register a link and return its 1-based id for use in a [`Span::Link`].
    pub fn add_link(&mut self, url: impl Into<String>, text: impl Into<String>) -> usize {
        let id = self.doc.links.len() + 1;
        self.doc.links.push(Link {
            id,
            url: url.into(),
            text: text.into(),
        });
        id
    }

    pub fn build(mut self) -> Document {
        let notes = std::mem::take(&mut self.pending_notes);
        self.doc.blocks.extend(notes);
        // Targets after the last block (an empty footer) point at the end.
        let last = self.doc.blocks.len().saturating_sub(1);
        for id in self.pending_anchors.drain(..) {
            self.doc.anchors.push((id, last));
        }
        self.doc
    }
}
