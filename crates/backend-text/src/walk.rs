//! Convert readability-cleaned HTML into the [`Block`]/[`Span`] document model.
//!
//! We walk the DOM once. At each container level we accumulate inline content
//! (text, `<a>`, `<em>`, …) into a run that is flushed as a paragraph whenever a
//! block element (`<p>`, `<h1>`, `<pre>`, …) interrupts it. Links are registered
//! with the [`DocumentBuilder`] so they get stable `[n]` numbers.

use browser_core::content::{Block, DocumentBuilder, Span};
use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::Html;
use url::Url;

/// Parse `html` and append the resulting blocks to `builder`.
pub fn build_blocks(html: &str, base_url: &str, builder: &mut DocumentBuilder) {
    let doc = Html::parse_document(html);
    let base = Url::parse(base_url).ok();
    let mut out: Vec<Block> = Vec::new();
    walk(doc.tree.root(), &base, builder, &mut out);
    for block in out {
        builder.push(block);
    }
}

/// Elements whose contents we ignore entirely.
fn is_skipped(name: &str) -> bool {
    matches!(
        name,
        "head" | "title" | "script" | "style" | "noscript" | "svg" | "iframe"
            | "form" | "button" | "input" | "select" | "textarea" | "nav"
    )
}

/// Inline elements that contribute spans rather than new blocks.
fn is_inline(name: &str) -> bool {
    matches!(
        name,
        "a" | "em" | "i" | "strong" | "b" | "code" | "span" | "small" | "mark"
            | "sub" | "sup" | "u" | "abbr" | "time" | "cite" | "q" | "s" | "del" | "ins"
    )
}

fn walk(
    node: NodeRef<Node>,
    base: &Option<Url>,
    builder: &mut DocumentBuilder,
    out: &mut Vec<Block>,
) {
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
                flush_inline(&mut inline, out);
                match name {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = name[1..].parse().unwrap_or(1);
                        let spans = collect_spans(child, base, builder);
                        if !is_blank(&spans) {
                            out.push(Block::Heading { level, spans });
                        }
                    }
                    "p" => {
                        let spans = collect_spans(child, base, builder);
                        if !is_blank(&spans) {
                            out.push(Block::Paragraph { spans });
                        }
                    }
                    "pre" => {
                        let text = raw_text(child);
                        let lines: Vec<String> =
                            text.trim_matches('\n').split('\n').map(str::to_string).collect();
                        if !lines.iter().all(|l| l.trim().is_empty()) {
                            out.push(Block::Code { lines });
                        }
                    }
                    "blockquote" => {
                        let spans = collect_spans(child, base, builder);
                        if !is_blank(&spans) {
                            out.push(Block::Quote { spans });
                        }
                    }
                    "ul" | "ol" => emit_list(child, name == "ol", base, builder, out),
                    "hr" => out.push(Block::Rule),
                    "br" => {}
                    // Generic containers: descend for more blocks.
                    _ => walk(child, base, builder, out),
                }
            }
            _ => {}
        }
    }
    flush_inline(&mut inline, out);
}

fn emit_list(
    list: NodeRef<Node>,
    ordered: bool,
    base: &Option<Url>,
    builder: &mut DocumentBuilder,
    out: &mut Vec<Block>,
) {
    let mut index = 0usize;
    for item in list.children() {
        if let Node::Element(el) = item.value() {
            if el.name() == "li" {
                index += 1;
                let spans = collect_spans(item, base, builder);
                if is_blank(&spans) {
                    continue;
                }
                let marker = if ordered { format!("{index}.") } else { "•".to_string() };
                out.push(Block::ListItem { ordered, marker, spans });
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
    let Node::Element(el) = node.value() else { return };
    let name = el.name();
    if is_skipped(name) {
        return;
    }
    match name {
        "a" => {
            let text = collapse(&inner_text(node));
            if text.is_empty() {
                return;
            }
            match el.attr("href").map(|h| resolve(base, h)) {
                Some(url) if !url.starts_with("javascript:") => {
                    let id = builder.add_link(url, text.clone());
                    spans.push(Span::Link { text, link_id: id });
                }
                _ => spans.push(Span::Text(text)),
            }
        }
        "strong" | "b" => push_styled(spans, &inner_text(node), Span::Strong),
        "em" | "i" => push_styled(spans, &inner_text(node), Span::Emphasis),
        "code" => push_styled(spans, &inner_text(node), Span::Code),
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

fn push_styled(spans: &mut Vec<Span>, text: &str, make: fn(String) -> Span) {
    let t = collapse(text);
    if !t.is_empty() {
        spans.push(make(t));
    }
}

/// Push collapsed text as a span, merging with a previous text span's spacing.
fn push_text(spans: &mut Vec<Span>, raw: &str) {
    let collapsed = collapse(raw);
    if collapsed.is_empty() {
        return;
    }
    spans.push(Span::Text(collapsed));
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
        Some(b) => b.join(href).map(|u| u.to_string()).unwrap_or_else(|_| href.to_string()),
        None => href.to_string(),
    }
}

/// Emit any accumulated inline content as a paragraph and reset the run.
fn flush_inline(inline: &mut Vec<Span>, out: &mut Vec<Block>) {
    if inline.is_empty() {
        return;
    }
    let spans = std::mem::take(inline);
    if !is_blank(&spans) {
        out.push(Block::Paragraph { spans });
    }
}

/// True if the spans carry no visible text.
fn is_blank(spans: &[Span]) -> bool {
    spans.iter().all(|s| s.plain().trim().is_empty())
}
