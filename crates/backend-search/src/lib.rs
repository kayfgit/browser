//! Search for `:read <query>`: ask DuckDuckGo lite and render the results as a
//! followable list, a [`Document`] whose result titles are registered links.

use anyhow::{anyhow, Context, Result};
use browser_core::content::{Block, DocumentBuilder, Span};
use browser_core::Document;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/124.0 Safari/537.36";

/// DuckDuckGo's HTML-lite endpoint: it needs no instance and no key.
const ENDPOINT: &str = "https://lite.duckduckgo.com/lite/";

/// The most results to list.
const LIMIT: usize = 15;

/// One search hit.
#[derive(Debug, Clone)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Search for `query` and return a results [`Document`] (titles are followable
/// links). Blocking, for callers without an async runtime: it spins up a
/// short-lived current-thread runtime, so run it on a worker thread.
pub fn search_blocking(query: &str) -> Result<Document> {
    let query = query.trim();
    if query.is_empty() {
        return Err(anyhow!("empty search query"));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building runtime")?;
    let html = rt.block_on(fetch_results(query))?;
    let results: Vec<SearchResult> = parse_ddg_lite(&html).into_iter().take(LIMIT).collect();
    Ok(build_document(query, &results))
}

/// The DuckDuckGo lite results page for `query`, as HTML.
async fn fetch_results(query: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(20))
        // DuckDuckGo lite serves its empty front page (not results) when the
        // request negotiates HTTP/2 via ALPN; forcing HTTP/1.1 returns results.
        .http1_only()
        .build()
        .context("building HTTP client")?;
    client
        .get(ENDPOINT)
        .query(&[("q", query)])
        .send()
        .await
        .context("querying DuckDuckGo lite")?
        .error_for_status()
        .context("DuckDuckGo lite returned an error status")?
        .text()
        .await
        .context("reading DuckDuckGo lite body")
}

/// Build the results document. Its `search:` address only labels the page; each
/// result opens at its own URL.
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
            spans: vec![Span::Link {
                text: r.title.clone(),
                link_id: id,
            }],
        });
        if !r.snippet.is_empty() {
            b.push(Block::Paragraph {
                spans: vec![Span::Text(r.snippet.clone())],
            });
        }
        b.push(Block::Paragraph {
            spans: vec![Span::Emphasis(r.url.clone())],
        });
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
        .map(|e| {
            e.text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();

    doc.select(&link_sel)
        .enumerate()
        .filter_map(|(i, el)| {
            let title = el
                .text()
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
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
        assert_eq!(
            unwrap_ddg_redirect("https://example.com/"),
            "https://example.com/"
        );
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

    #[test]
    fn parses_result_links_and_snippets() {
        let html = r#"<table>
            <tr><td><a class="result-link" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.test%2F">
                First  hit</a></td></tr>
            <tr><td class="result-snippet">About   the first.</td></tr>
            <tr><td><a class="result-link" href="https://b.test/">Second</a></td></tr>
            <tr><td><a class="result-link" href="https://c.test/"> </a></td></tr>
        </table>"#;
        let results = parse_ddg_lite(html);
        assert_eq!(results.len(), 2, "the untitled link is dropped");
        assert_eq!(results[0].title, "First hit");
        assert_eq!(results[0].url, "https://a.test/");
        assert_eq!(results[0].snippet, "About the first.");
        assert_eq!(results[1].url, "https://b.test/");
    }
}
