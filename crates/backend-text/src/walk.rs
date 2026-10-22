//! Convert readability-cleaned HTML into the [`Block`]/[`Span`] document model.
//!
//! We walk the DOM once. At each container level we accumulate inline content
//! (text, `<a>`, `<em>`, …) into a run that is flushed as a paragraph whenever a
//! block element (`<p>`, `<h1>`, `<pre>`, …) interrupts it. Links are registered
//! with the [`DocumentBuilder`] so they get stable `[n]` numbers, and element ids
//! as anchors, so a `#fragment` link can jump to its block.

use browser_core::content::{Block, DocumentBuilder, Span};
use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::Html;
use url::Url;

/// Parse `html` and append the resulting blocks to `builder`.
pub fn build_blocks(html: &str, base_url: &str, builder: &mut DocumentBuilder) {
    let doc = Html::parse_document(html);
    let base = Url::parse(base_url).ok();
    walk(doc.tree.root(), &base, builder);
}

/// Register `el`'s id (or an old-style `<a name>`) as an in-page target.
fn note_anchor(el: &scraper::node::Element, builder: &mut DocumentBuilder) {
    if let Some(id) = el.attr("id") {
        builder.anchor(id);
    }
    if el.name() == "a" {
        if let Some(name) = el.attr("name") {
            builder.anchor(name);
        }
    }
}

/// Elements whose contents we ignore entirely.
fn is_skipped(name: &str) -> bool {
    matches!(
        name,
        "head"
            | "title"
            | "script"
            | "style"
            | "noscript"
            | "svg"
            | "iframe"
            | "form"
            | "button"
            | "input"
            | "select"
            | "textarea"
            | "nav"
    )
}

/// Inline elements that contribute spans rather than new blocks.
fn is_inline(name: &str) -> bool {
    matches!(
        name,
        "a" | "em"
            | "i"
            | "strong"
            | "b"
            | "code"
            | "span"
            | "small"
            | "mark"
            | "sub"
            | "sup"
            | "u"
            | "abbr"
            | "time"
            | "cite"
            | "q"
            | "s"
            | "del"
            | "ins"
    )
}

fn walk(node: NodeRef<Node>, base: &Option<Url>, builder: &mut DocumentBuilder) {
    let mut inline: Vec<Span> = Vec::new();

    for child in node.children() {
        match child.value() {
            Node::Text(t) => push_text(&mut inline, &t.text),
            Node::Element(el) => {
                let name = el.name();
                if is_skipped(name) {
                    continue;
                }
                if is_inline(name) {
                    collect_inline(child, base, builder, &mut inline);
                    continue;
                }
                // A block element ends any in-progress inline run.
                flush_inline(&mut inline, builder);
                note_anchor(el, builder);
                match name {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = name[1..].parse().unwrap_or(1);
                        let spans = collect_spans(child, base, builder);
                        if !is_blank(&spans) {
                            builder.push(Block::Heading { level, spans });
                        }
                    }
                    "p" => {
                        let spans = collect_spans(child, base, builder);
                        if !is_blank(&spans) {
                            builder.push(Block::Paragraph { spans });
                        }
                    }
                    "pre" => {
                        let text = raw_text(child);
                        let lines: Vec<String> = text
                            .trim_matches('\n')
                            .split('\n')
                            .map(str::to_string)
                            .collect();
                        if !lines.iter().all(|l| l.trim().is_empty()) {
                            builder.push(Block::Code { lines });
                        }
                    }
                    "blockquote" => {
                        let spans = collect_spans(child, base, builder);
                        if !is_blank(&spans) {
                            builder.push(Block::Quote { spans });
                        }
                    }
                    "ul" | "ol" => emit_list(child, name == "ol", base, builder),
                    "hr" => {
                        builder.push(Block::Rule);
                    }
                    "br" => {}
                    // Generic containers: descend for more blocks.
                    _ => walk(child, base, builder),
                }
            }
            _ => {}
        }
    }
    flush_inline(&mut inline, builder);
}

fn emit_list(
    list: NodeRef<Node>,
    ordered: bool,
    base: &Option<Url>,
    builder: &mut DocumentBuilder,
) {
    let mut index = 0usize;
    for item in list.children() {
        if let Node::Element(el) = item.value() {
            if el.name() == "li" {
                index += 1;
                note_anchor(el, builder);
                let spans = collect_spans(item, base, builder);
                if is_blank(&spans) {
                    continue;
                }
                let marker = if ordered {
                    format!("{index}.")
                } else {
                    "•".to_string()
                };
                builder.push(Block::ListItem {
                    ordered,
                    marker,
                    spans,
                });
            }
        }
    }
}

/// Collect the inline spans of an element's subtree (used for p/h*/li/quote).
fn collect_spans(
    node: NodeRef<Node>,
    base: &Option<Url>,
    builder: &mut DocumentBuilder,
) -> Vec<Span> {
    let mut spans = Vec::new();
    for child in node.children() {
        match child.value() {
            Node::Text(t) => push_text(&mut spans, &t.text),
            Node::Element(_) => collect_inline(child, base, builder, &mut spans),
            _ => {}
        }
    }
    spans
}

/// Append the spans for one inline element (or descend through it).
fn collect_inline(
    node: NodeRef<Node>,
    base: &Option<Url>,
    builder: &mut DocumentBuilder,
    spans: &mut Vec<Span>,
) {
    let Node::Element(el) = node.value() else {
        return;
    };
    let name = el.name();
    if is_skipped(name) {
        return;
    }
    note_anchor(el, builder);
    if is_side_note(el) {
        let note = collect_spans(note_body(node), base, builder);
        if !is_blank(&note) {
            let label = builder.note(note);
            spans.push(Span::NoteRef(label));
        }
        return;
    }
    match name {
        "a" => match el.attr("href").map(|h| resolve(base, h)) {
            Some(url) if !url.starts_with("javascript:") => {
                push_spaced(spans, &inner_text(node), |text| {
                    let link_id = builder.add_link(url, text.clone());
                    Span::Link { text, link_id }
                });
            }
            _ => push_spaced(spans, &inner_text(node), Span::Text),
        },
        "strong" | "b" => push_spaced(spans, &inner_text(node), Span::Strong),
        "em" | "i" => push_spaced(spans, &inner_text(node), Span::Emphasis),
        "code" => push_spaced(spans, &inner_text(node), Span::Code),
        // Other inline wrappers: recurse so nested links/styles are preserved.
        _ => {
            for child in node.children() {
                match child.value() {
                    Node::Text(t) => push_text(spans, &t.text),
                    Node::Element(_) => collect_inline(child, base, builder, spans),
                    _ => {}
                }
            }
        }
    }
}

/// Whether an inline element is a side note: text a page's CSS floats into the
/// margin or folds away behind a number (`<span class="sidenote">`, an inline
/// `<aside>`). Without the CSS it would read as part of the sentence, so it's
/// taken out and shown after the paragraph. The numbered references pointing at
/// notes (`footnote-ref`, `sidenote-number`, back links) stay in the text.
fn is_side_note(el: &scraper::node::Element) -> bool {
    const NOTE: &[&str] = &[
        "note",
        "notes",
        "sidenote",
        "marginnote",
        "footnote",
        "aside",
    ];
    const NOT: &[&str] = &[
        "ref",
        "reference",
        "number",
        "num",
        "marker",
        "toggle",
        "label",
        "link",
        "anchor",
        "call",
        "back",
        "backref",
        "return",
    ];
    if el.name() == "aside" || el.attr("role") == Some("note") {
        return true;
    }
    el.classes().any(|class| {
        let class = class.to_ascii_lowercase();
        let parts: Vec<&str> = class.split(['-', '_']).collect();
        parts.iter().any(|p| NOTE.contains(p)) && !parts.iter().any(|p| NOT.contains(p))
    })
}

/// The content of a side note, past wrappers that are notes too (browser.engineering
/// puts `<span class="note">` inside `<span class="note-container">`), so it isn't
/// turned into a note that only points at another one.
fn note_body(mut node: NodeRef<Node>) -> NodeRef<Node> {
    loop {
        let mut children = node.children().filter(|c| match c.value() {
            Node::Text(t) => !t.trim().is_empty(),
            Node::Element(_) => true,
            _ => false,
        });
        match (children.next(), children.next()) {
            (Some(only), None) if only.value().as_element().is_some_and(is_side_note) => {
                node = only;
            }
            _ => return node,
        }
    }
}

/// Push `raw` collapsed into a span built by `make`, keeping a single space on
/// either side where the HTML had whitespace there. The renderers glue a span
/// onto the previous one unless a space separates them, so trimming the edges
/// would turn `MDN's <a>Learn</a> series` into "MDN'sLearnseries".
fn push_spaced(spans: &mut Vec<Span>, raw: &str, make: impl FnOnce(String) -> Span) {
    if raw.starts_with(char::is_whitespace) {
        push_space(spans);
    }
    let t = collapse(raw);
    if !t.is_empty() {
        spans.push(make(t));
    }
    if raw.ends_with(char::is_whitespace) {
        push_space(spans);
    }
}

/// Push collapsed text as a span, keeping its edge spacing (see [`push_spaced`]).
fn push_text(spans: &mut Vec<Span>, raw: &str) {
    push_spaced(spans, raw, Span::Text);
}

/// End the run with a word boundary: extend a trailing text span with one space,
/// or add a lone space span after a styled one. Never doubles a space, and a run
/// never starts with one.
fn push_space(spans: &mut Vec<Span>) {
    match spans.last_mut() {
        None => {}
        Some(Span::Text(s)) if s.ends_with(' ') => {}
        Some(Span::Text(s)) => s.push(' '),
        _ => spans.push(Span::Text(" ".into())),
    }
}

/// All descendant text concatenated, with original whitespace (caller collapses).
fn inner_text(node: NodeRef<Node>) -> String {
    let mut buf = String::new();
    collect_text(node, &mut buf);
    buf
}

fn collect_text(node: NodeRef<Node>, buf: &mut String) {
    for child in node.children() {
        match child.value() {
            Node::Text(t) => buf.push_str(&t.text),
            Node::Element(el) if !is_skipped(el.name()) => collect_text(child, buf),
            _ => {}
        }
    }
}

/// Verbatim text content (for `<pre>`), preserving newlines and spacing.
fn raw_text(node: NodeRef<Node>) -> String {
    let mut buf = String::new();
    collect_text(node, &mut buf);
    buf
}

/// Collapse runs of ASCII whitespace into single spaces and trim the ends.
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn resolve(base: &Option<Url>, href: &str) -> String {
    match base {
        Some(b) => b
            .join(href)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| href.to_string()),
        None => href.to_string(),
    }
}

/// Emit any accumulated inline content as a paragraph and reset the run.
fn flush_inline(inline: &mut Vec<Span>, builder: &mut DocumentBuilder) {
    if inline.is_empty() {
        return;
    }
    let spans = std::mem::take(inline);
    if !is_blank(&spans) {
        builder.push(Block::Paragraph { spans });
    }
}

/// True if the spans carry no visible text.
fn is_blank(spans: &[Span]) -> bool {
    spans.iter().all(|s| s.plain().trim().is_empty())
}
