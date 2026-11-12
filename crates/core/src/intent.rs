//! Reading what the user typed as a target: telling a search query from an address,
//! building search URLs, and normalizing addresses.

/// Whether a target should be treated as a search query rather than an address:
/// it has whitespace, or it has neither a scheme nor a dot (a bare word like
/// `rustlang`, which would otherwise parse as the host `rustlang`).
pub fn looks_like_query(target: &str) -> bool {
    if target.contains(char::is_whitespace) {
        return true;
    }
    let has_scheme = target.contains("://");
    let has_dot = target.contains('.');
    !has_scheme && !has_dot
}

/// Default search-engine URL template (Google). `%s` is replaced with the
/// percent-encoded query.
pub const DEFAULT_SEARCH_URL: &str = "https://www.google.com/search?q=%s";

/// Build a search URL from a template and a query. `%s` in the template is
/// replaced with the percent-encoded query; if the template has no `%s`, the
/// encoded query is appended. Lets callers route a non-URL `:open` to a search.
pub fn search_url(template: &str, query: &str) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    if template.contains("%s") {
        template.replace("%s", &encoded)
    } else {
        format!("{template}{encoded}")
    }
}

/// Extract the lowercased host from a target, adding a scheme if missing.
pub fn host_of(target: &str) -> Option<String> {
    let with_scheme = if target.contains("://") {
        target.to_string()
    } else {
        format!("https://{target}")
    };
    url::Url::parse(&with_scheme)
        .ok()?
        .host_str()
        .map(|h| h.to_lowercase())
}

/// Normalize a target into a fetchable absolute URL (adds https:// if missing).
pub fn normalize_url(target: &str) -> Option<String> {
    let with_scheme = if target.contains("://") {
        target.to_string()
    } else {
        format!("https://{target}")
    };
    url::Url::parse(&with_scheme).ok().map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_detection() {
        assert!(looks_like_query("how to fix rust"));
        assert!(looks_like_query("rustlang")); // no dot, no scheme
        assert!(!looks_like_query("example.com"));
        assert!(!looks_like_query("https://example.com"));
    }

    #[test]
    fn search_url_encodes_query() {
        assert_eq!(
            search_url(DEFAULT_SEARCH_URL, "rust ownership"),
            "https://www.google.com/search?q=rust+ownership"
        );
        // No `%s` placeholder → query is appended (percent-encoded).
        assert_eq!(
            search_url("https://example.com/q=", "a&b"),
            "https://example.com/q=a%26b"
        );
    }

    #[test]
    fn host_extraction() {
        assert_eq!(
            host_of("https://www.example.com/x"),
            Some("www.example.com".into())
        );
        assert_eq!(host_of("example.com/x"), Some("example.com".into()));
    }
}
