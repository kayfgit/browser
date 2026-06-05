//! Terminal text mode: fetch a page, run it through readability extraction, and
//! convert the cleaned HTML into a [`Document`] for the TUI.

use anyhow::{anyhow, Context, Result};
use browser_core::content::{Block, DocumentBuilder, Span};
use browser_core::{intent, Backend, Document};
use dom_smoothie::{Config as ReadabilityConfig, Readability};

mod walk;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0 Safari/537.36";

/// Reader-mode backend.
pub struct TextBackend {
    client: reqwest::Client,
}

impl TextBackend {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("building HTTP client")?;
        Ok(TextBackend { client })
    }

    async fn get_html(&self, url: &str) -> Result<(String, String)> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        let final_url = resp.url().to_string();
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("HTTP {status} for {url}"));
        }
        let html = resp.text().await.context("reading response body")?;
        Ok((final_url, html))
    }
}

impl Backend for TextBackend {
    fn name(&self) -> &'static str {
        "text"
    }

    async fn fetch(&self, target: &str) -> Result<Document> {
        let url = intent::normalize_url(target)
            .ok_or_else(|| anyhow!("not a valid URL: {target}"))?;
        let (final_url, html) = self.get_html(&url).await?;
        extract(&final_url, &html)
    }
}

/// Run readability on `html` and walk the cleaned content into a `Document`.
/// Exposed (and tested) independently of the network fetch.
pub fn extract(url: &str, html: &str) -> Result<Document> {
    let mut readability = Readability::new(html, Some(url), Some(ReadabilityConfig::default()))
        .map_err(|e| anyhow!("readability init failed: {e}"))?;
    let article = readability
        .parse()
        .map_err(|e| anyhow!("readability parse failed: {e}"))?;

    let cleaned_html = article.content.to_string();
    let mut builder = DocumentBuilder::new(url);
    builder.title(if article.title.is_empty() {
        url.to_string()
    } else {
        article.title.to_string()
    });

    walk::build_blocks(&cleaned_html, url, &mut builder);

    let mut doc = builder.build();
    if doc.blocks.is_empty() {
        doc.blocks.push(Block::Paragraph {
            spans: vec![Span::Text("(no readable content extracted)".into())],
        });
    }
    Ok(doc)
}

/// Extracted article ready to render as a web page: the final URL (after
/// redirects, for resolving relative links), the title, and cleaned article HTML.
pub struct Readable {
    pub url: String,
    pub title: String,
    pub html: String,
}

/// Run readability and return the cleaned article HTML (not our Document model).
/// Used by the GUI read mode, which renders the HTML in a JS-disabled webview.
pub fn extract_article(url: &str, html: &str) -> Result<(String, String)> {
    let mut readability = Readability::new(html, Some(url), Some(ReadabilityConfig::default()))
        .map_err(|e| anyhow!("readability init failed: {e}"))?;
    let article = readability
        .parse()
        .map_err(|e| anyhow!("readability parse failed: {e}"))?;
    let title = if article.title.is_empty() {
        url.to_string()
    } else {
        article.title.to_string()
    };
    Ok((title, article.content.to_string()))
}

/// Blocking fetch + readability extraction into the [`Document`] model, for the
/// GUI shell's engine-free read mode (it renders the Document natively, with no
/// WebView2). Spins up a short-lived current-thread runtime.
pub fn fetch_document_blocking(target: &str) -> Result<Document> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building runtime")?;
    rt.block_on(async move {
        let url = intent::normalize_url(target)
            .ok_or_else(|| anyhow!("not a valid URL: {target}"))?;
        let backend = TextBackend::new()?;
        let (final_url, html) = backend.get_html(&url).await?;
        extract(&final_url, &html)
    })
}

/// Blocking fetch + readability extraction, for callers without an async runtime
/// (e.g. the GUI shell). Spins up a short-lived current-thread runtime.
pub fn fetch_readable_blocking(target: &str) -> Result<Readable> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building runtime")?;
    rt.block_on(async move {
        let url = intent::normalize_url(target)
            .ok_or_else(|| anyhow!("not a valid URL: {target}"))?;
        let backend = TextBackend::new()?;
        let (final_url, html) = backend.get_html(&url).await?;
        let (title, article_html) = extract_article(&final_url, &html)?;
        Ok(Readable { url: final_url, title, html: article_html })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_core::content::Block;

    #[test]
    fn extracts_headings_and_links() {
        let html = r#"<html><head><title>Doc</title></head><body>
            <article>
              <h1>Hello</h1>
              <p>Some intro text with a <a href="https://example.com/more">link</a>.</p>
              <pre><code>fn main() {}</code></pre>
              <ul><li>one</li><li>two</li></ul>
            </article>
        </body></html>"#;
        let doc = extract("https://example.com/", html).unwrap();
        assert!(!doc.blocks.is_empty());
        assert!(doc.links.iter().any(|l| l.url.contains("example.com/more")));
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Code { .. })));
    }
}
