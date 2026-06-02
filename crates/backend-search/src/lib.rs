//! Search mode: query a configurable provider and render results as a followable
//! list. Two providers are supported:
//!
//! * **SearXNG** — `GET {base_url}?q=…&format=json`, parsed from JSON.
//! * **DuckDuckGo lite** — `POST {base_url}` with `q=…`, scraped from the HTML.
//!
//! Both produce a [`Document`] whose result titles are registered links, so the
//! TUI can follow `[n]` straight into text/full mode via the normal router.

use anyhow::{anyhow, Context, Result};
use browser_core::content::{Block, DocumentBuilder, Span};
use browser_core::{Backend, Document, SearchConfig, SearchProvider};
use serde::Deserialize;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0 Safari/537.36";

/// One search hit.
#[derive(Debug, Clone)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Search backend, configured with a provider + endpoint.
pub struct SearchBackend {
    client: reqwest::Client,
    config: SearchConfig,
}

impl SearchBackend {
    pub fn new(config: SearchConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(20))
            // DuckDuckGo lite serves its empty front page (not results) when the
            // request negotiates HTTP/2 via ALPN; forcing HTTP/1.1 returns results.
            .http1_only()
            .build()
            .context("building HTTP client")?;
        Ok(SearchBackend { client, config })
    }

    async fn run_query(&self, query: &str) -> Result<Vec<SearchResult>> {
        let results = match self.config.provider {
            SearchProvider::Searxng => self.searxng(query).await?,
            SearchProvider::Ddg => self.ddg_lite(query).await?,
        };
        Ok(results.into_iter().take(self.config.limit).collect())
    }

    async fn searxng(&self, query: &str) -> Result<Vec<SearchResult>> {
        let resp = self
            .client
            .get(&self.config.base_url)
            .query(&[("q", query), ("format", "json")])
            .send()
            .await
            .context("querying SearXNG")?
            .error_for_status()
            .context("SearXNG returned an error status")?;
        let parsed: SearxResponse = resp.json().await.context("parsing SearXNG JSON")?;
        Ok(parsed
            .results
            .into_iter()
            .map(|r| SearchResult {
                title: r.title.unwrap_or_default(),
                url: r.url,
                snippet: r.content.unwrap_or_default(),
            })
            .collect())
    }

    async fn ddg_lite(&self, query: &str) -> Result<Vec<SearchResult>> {
        let html = self
            .client
            .get(&self.config.base_url)
            .query(&[("q", query)])
            .send()
            .await
            .context("querying DuckDuckGo lite")?
            .error_for_status()
            .context("DuckDuckGo lite returned an error status")?
            .text()
            .await
            .context("reading DuckDuckGo lite body")?;
        Ok(parse_ddg_lite(&html))
    }
}

impl Backend for SearchBackend {
    fn name(&self) -> &'static str {
        "search"
    }

    async fn fetch(&self, target: &str) -> Result<Document> {
        let query = target.trim();
        if query.is_empty() {
            return Err(anyhow!("empty search query"));
        }
        let results = self.run_query(query).await?;
        Ok(build_document(query, &results))
    }
}

#[derive(Debug, Deserialize)]
struct SearxResponse {
    #[serde(default)]
    results: Vec<SearxResult>,
}

#[derive(Debug, Deserialize)]
struct SearxResult {
    url: String,
    title: Option<String>,
    content: Option<String>,
}

/// Build the results document. The `search:` scheme is irrelevant; the TUI feeds
/// followed links back through the router, which re-routes them by domain rules.
fn build_document(query: &str, results: &[SearchResult]) -> Document {
    let mut b = DocumentBuilder::new(format!("search:{query}"));
    b.title(format!("Search: {query}"));
    b.push(Block::Heading {
        level: 1,
        spans: vec![Span::Text(format!("Results for \u{201c}{query}\u{201d}"))],
    });
    b.push(Block::Blank);

    if results.is_empty() {
        b.push(Block::Paragraph {
            spans: vec![Span::Text("No results.".into())],
        });
        return b.build();
    }

    for r in results {
        let id = b.add_link(r.url.clone(), r.title.clone());
        b.push(Block::Heading {
            level: 3,
            spans: vec![Span::Link { text: r.title.clone(), link_id: id }],
        });
        if !r.snippet.is_empty() {
            b.push(Block::Paragraph { spans: vec![Span::Text(r.snippet.clone())] });
        }
        b.push(Block::Paragraph { spans: vec![Span::Emphasis(r.url.clone())] });
        b.push(Block::Blank);
    }
    b.build()
}

/// Parse DuckDuckGo lite HTML. Result titles are `a.result-link`; snippets live in
/// `td.result-snippet`. Redirect hrefs (`/l/?uddg=…`) are unwrapped to the target.
fn parse_ddg_lite(html: &str) -> Vec<SearchResult> {
    use scraper::{Html, Selector};

    let doc = Html::parse_document(html);
    let link_sel = Selector::parse("a.result-link").unwrap();
    let snippet_sel = Selector::parse(".result-snippet").unwrap();

    let snippets: Vec<String> = doc
        .select(&snippet_sel)
        .map(|e| e.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();

    doc.select(&link_sel)
        .enumerate()
        .filter_map(|(i, el)| {
            let title = el.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
            let href = el.value().attr("href")?;
            let url = unwrap_ddg_redirect(href);
            if title.is_empty() || url.is_empty() {
                return None;
            }
            Some(SearchResult {
                title,
                url,
                snippet: snippets.get(i).cloned().unwrap_or_default(),
            })
        })
        .collect()
}

/// DDG lite sometimes wraps targets in `//duckduckgo.com/l/?uddg=<encoded>`.
fn unwrap_ddg_redirect(href: &str) -> String {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(u) = url::Url::parse(&absolute) {
        if let Some((_, target)) = u.query_pairs().find(|(k, _)| k == "uddg") {
            return target.into_owned();
        }
    }
    absolute
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_ddg_redirect() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc";
        assert_eq!(unwrap_ddg_redirect(href), "https://example.com/page");
    }

    #[test]
    fn direct_href_passthrough() {
        assert_eq!(unwrap_ddg_redirect("https://example.com/"), "https://example.com/");
    }

    #[test]
    fn builds_document_with_links() {
        let results = vec![SearchResult {
            title: "Example".into(),
            url: "https://example.com".into(),
            snippet: "A snippet".into(),
        }];
        let doc = build_document("test", &results);
        assert_eq!(doc.links.len(), 1);
        assert_eq!(doc.link_url(1), Some("https://example.com"));
    }
}
