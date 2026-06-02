//! Command parsing and intent routing.
//!
//! The command bar produces a line of text; [`parse_command`] turns it into a
//! [`Command`]. For an `Open` with no explicit mode, [`route`] picks the [`Mode`]
//! from the user's config (per-domain rules, else the default mode).

use serde::Deserialize;

use crate::config::Config;

/// The set of rendering backends a target can be dispatched to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Lightweight reader-mode extraction in the terminal.
    Text,
    /// Search-results list.
    Search,
    /// yt-dlp + mpv (roadmap; parses now, errors at open time).
    Video,
    /// On-demand system webview (roadmap; parses now, errors at open time).
    Full,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Text => "text",
            Mode::Search => "search",
            Mode::Video => "video",
            Mode::Full => "full",
        }
    }
}

/// A parsed command-bar instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Open a target; `mode == None` means "let the router decide".
    Open { mode: Option<Mode>, target: String },
    Reload,
    Back,
    Forward,
    Quit,
    /// Unrecognized input; carries a message to show the user.
    Unknown(String),
}

/// Parse a command-bar line (with or without a leading `:`).
pub fn parse_command(input: &str) -> Command {
    let line = input.trim().trim_start_matches(':').trim();
    if line.is_empty() {
        return Command::Unknown("empty command".into());
    }

    let (head, rest) = match line.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (line, ""),
    };

    match head {
        "q" | "quit" => Command::Quit,
        "r" | "reload" => Command::Reload,
        "back" | "b" => Command::Back,
        "forward" | "f" => Command::Forward,
        "text" | "t" => open_with(Some(Mode::Text), rest),
        "search" | "s" => open_with(Some(Mode::Search), rest),
        "video" | "v" => open_with(Some(Mode::Video), rest),
        "open" | "o" => open_with(Some(Mode::Full), rest),
        // No recognized verb: treat the whole line as a target for the router.
        _ => Command::Open { mode: None, target: line.to_string() },
    }
}

fn open_with(mode: Option<Mode>, target: &str) -> Command {
    if target.is_empty() {
        Command::Unknown("missing target".into())
    } else {
        Command::Open { mode, target: target.to_string() }
    }
}

/// Decide which mode an un-prefixed target should open in.
///
/// Rules, in order:
/// 1. If the target looks like a query (has spaces, or no dot and no scheme) → `Search`.
/// 2. Otherwise match the host against the configured per-domain rules.
/// 3. Otherwise the configured `default_mode`.
pub fn route(target: &str, config: &Config) -> Mode {
    if looks_like_query(target) {
        return Mode::Search;
    }
    match host_of(target) {
        Some(host) => config.mode_for_host(&host).unwrap_or(config.default_mode),
        None => config.default_mode,
    }
}

fn looks_like_query(target: &str) -> bool {
    if target.contains(char::is_whitespace) {
        return true;
    }
    let has_scheme = target.contains("://");
    let has_dot = target.contains('.');
    !has_scheme && !has_dot
}

/// Extract the lowercased host from a target, adding a scheme if missing.
pub fn host_of(target: &str) -> Option<String> {
    let with_scheme = if target.contains("://") {
        target.to_string()
    } else {
        format!("https://{target}")
    };
    url::Url::parse(&with_scheme).ok()?.host_str().map(|h| h.to_lowercase())
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
    fn parses_verbs_and_aliases() {
        assert_eq!(parse_command(":q"), Command::Quit);
        assert_eq!(parse_command("quit"), Command::Quit);
        assert_eq!(
            parse_command(":s rust crate"),
            Command::Open { mode: Some(Mode::Search), target: "rust crate".into() }
        );
        assert_eq!(
            parse_command(":text docs.rs"),
            Command::Open { mode: Some(Mode::Text), target: "docs.rs".into() }
        );
    }

    #[test]
    fn bare_target_defers_to_router() {
        assert_eq!(
            parse_command("example.com"),
            Command::Open { mode: None, target: "example.com".into() }
        );
    }

    #[test]
    fn query_detection() {
        assert!(looks_like_query("how to fix rust"));
        assert!(looks_like_query("rustlang")); // no dot, no scheme
        assert!(!looks_like_query("example.com"));
        assert!(!looks_like_query("https://example.com"));
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("https://www.example.com/x"), Some("www.example.com".into()));
        assert_eq!(host_of("example.com/x"), Some("example.com".into()));
    }
}
