//! Colours for the engine-free `browser://` pages (`:res`, `:history`, `:errors`, …).
//!
//! The pages build plain text lines; this colours them as they're drawn, by page and
//! by what each line holds — so a page that refreshes itself (`:res`) stays coloured
//! and no page builder has to carry styling. A page can also set its own colours
//! (`TextBuffer::tints`, as `:downloads` does); those win.
use crate::draw::{self, Rgb};

/// Where a line changes colour: from each `(column, colour)` to the next. Text before
/// the first switch is the normal text colour.
pub(crate) type Switches = Vec<(usize, Rgb)>;

/// The colours for row `row` of the page at `url`, whose text is `line`.
pub(crate) fn line_colors(url: &str, row: usize, line: &[char]) -> Switches {
    let s: String = line.iter().collect();
    if s.trim().is_empty() {
        return Vec::new();
    }
    let page = url.strip_prefix("browser://").unwrap_or(url);
    match page {
        "error" => error_line(&s),
        "res" => res_line(row, &s),
        "history" if row > 0 => url_line(&s, 0),
        "saved" if row > 0 => saved_line(&s),
        "aihist" if row > 0 => vec![(0, draw::DIM), (28.min(len(&s)), draw::FG)],
        "aliases" if row > 0 => alias_line(&s),
        "version" => version_line(row, &s),
        "profiles" if row > 0 => profile_line(&s),
        "extensions" if row > 0 => extension_line(&s),
        "bangs" if row > 0 => bang_line(&s),
        "engines" => engine_line(row, &s),
        _ if row == 0 => header(&s),
        _ => Vec::new(),
    }
}

fn len(s: &str) -> usize {
    s.chars().count()
}

/// The char column of `pat` in `s`.
fn col(s: &str, pat: &str) -> Option<usize> {
    s.find(pat).map(|b| s[..b].chars().count())
}

/// A page title: the name in the accent colour, the hints after it dimmed.
fn header(s: &str) -> Switches {
    // The first of these that the title has, in this order: hints come in parentheses
    // or after a wide gap, while a single " — " is part of the title ("history — 3").
    let split = ["    (", "   (", "   —   ", "    "]
        .iter()
        .find_map(|p| col(s, p));
    match split {
        Some(at) => vec![(0, draw::ACCENT), (at, draw::DIM)],
        None => vec![(0, draw::ACCENT)],
    }
}

/// `:error(s)`: `[time] command — error N` headers, then the message in red.
fn error_line(s: &str) -> Switches {
    if s.starts_with('[') && s.len() > 10 && s.as_bytes().get(9) == Some(&b']') {
        let mut out = vec![(0, draw::DIM), (11, draw::ACCENT)];
        if let Some(at) = col(s, " — error ") {
            out.push((at, draw::DIM));
        }
        return out;
    }
    vec![(0, draw::ERR)]
}

/// `:res`: the title, totals, then `MEM CPU DISK PROCESS PID` rows with the CPU
/// coloured by load.
fn res_line(row: usize, s: &str) -> Switches {
    match row {
        0 => header(s),
        1 => vec![(0, draw::FG)],
        3 => vec![(0, draw::DIM)],
        _ if row > 3
            && s.len() > 30
            && (s.starts_with(' ') || s.as_bytes()[0].is_ascii_digit()) =>
        {
            let cpu: f64 = s
                .get(11..17)
                .and_then(|c| c.trim().trim_end_matches('%').parse().ok())
                .unwrap_or(0.0);
            let load = if cpu >= 25.0 {
                draw::ERR
            } else if cpu >= 5.0 {
                draw::GRAB
            } else {
                draw::READ
            };
            let pid_at = s
                .rfind(' ')
                .map(|b| s[..b].chars().count() + 1)
                .unwrap_or(len(s));
            vec![
                (0, draw::FG),
                (11, load),
                (17, draw::DIM),
                (30, draw::ACCENT),
                (pid_at, draw::DIM),
            ]
        }
        _ => vec![(0, draw::DIM)],
    }
}

/// A URL: the scheme dim, the host in the accent colour, the path plain.
fn url_line(s: &str, start: usize) -> Switches {
    let rest: String = s.chars().skip(start).collect();
    let Some(host_at) = col(&rest, "://").map(|c| c + 3) else {
        return vec![(start, draw::FG)];
    };
    let after = &rest[rest
        .char_indices()
        .nth(host_at)
        .map(|(b, _)| b)
        .unwrap_or(rest.len())..];
    let host_len = after
        .find('/')
        .map(|b| after[..b].chars().count())
        .unwrap_or(len(after));
    vec![
        (start, draw::DIM),
        (start + host_at, draw::ACCENT),
        (start + host_at + host_len, draw::FG),
    ]
}

/// `:saved`: the name, then its URL.
fn saved_line(s: &str) -> Switches {
    let mut out = vec![(0, draw::FG)];
    if let Some(at) = col(s, "http") {
        out.extend(
            url_line(s, at)
                .into_iter()
                .map(|(c, k)| (c, if k == draw::FG { draw::DIM } else { k })),
        );
    }
    out
}

/// `:alias`: `:name → expansion`, with a red note when a built-in shadows it.
fn alias_line(s: &str) -> Switches {
    let mut out = vec![(0, draw::ACCENT)];
    if let Some(at) = col(s, " → ") {
        out.push((at, draw::DIM));
        out.push((at + 3, draw::FG));
    }
    if let Some(at) = col(s, "    (ignored") {
        out.push((at, draw::ERR));
    }
    out
}

/// `:version`: the title, then `Key: value` rows.
fn version_line(row: usize, s: &str) -> Switches {
    if row == 0 {
        return vec![(0, draw::ACCENT)];
    }
    match col(s, ":") {
        Some(at) if s.starts_with("  ") => vec![(0, draw::DIM), (at + 1, draw::FG)],
        _ => vec![(0, draw::DIM)],
    }
}

/// `:profiles`: the active one (marked `▸`) in green, notes dimmed.
fn profile_line(s: &str) -> Switches {
    let name = if s.starts_with('▸') {
        draw::READ
    } else {
        draw::FG
    };
    let mut out = vec![(0, name)];
    if let Some(at) = col(s, "   (") {
        out.push((at, draw::DIM));
    }
    out
}

/// `:extensions`: `[on ]` green or `[off]` dim, the name, then the id dimmed.
fn extension_line(s: &str) -> Switches {
    let state = if s.starts_with("[on") {
        draw::READ
    } else {
        draw::DIM
    };
    let mut out = vec![(0, state), (5, draw::FG)];
    if let Some(b) = s.rfind(' ') {
        out.push((s[..b].chars().count() + 1, draw::DIM));
    }
    out
}

/// `:bangs`: the keys, the site name, then the source and search URL dimmed.
fn bang_line(s: &str) -> Switches {
    if s.starts_with("switched off:") {
        return vec![(0, draw::ERR)];
    }
    if !s.starts_with('!') {
        return vec![(0, draw::DIM)];
    }
    let mut out = vec![(0, draw::ACCENT)];
    if let Some(at) = col(s, "  ") {
        out.push((at, draw::FG));
    }
    if let Some(at) = col(s, "  [") {
        out.push((at, draw::DIM));
    }
    out
}

/// `:engines`: the title, `id — family — name (default)` rows, dim detail lines.
fn engine_line(row: usize, s: &str) -> Switches {
    if row == 0 {
        return vec![(0, draw::ACCENT)];
    }
    if s.starts_with("  ") || !s.contains(" — ") {
        return vec![(0, draw::DIM)];
    }
    let mut out = vec![(0, draw::ACCENT)];
    if let Some(at) = col(s, " — ") {
        out.push((at, draw::FG));
    }
    if let Some(at) = col(s, " (default)") {
        out.push((at, draw::READ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors(url: &str, row: usize, s: &str) -> Switches {
        line_colors(url, row, &s.chars().collect::<Vec<_>>())
    }

    #[test]
    fn titles_split_the_name_from_the_hints() {
        let c = colors(
            "browser://history",
            0,
            "history — 3 entries    (Enter: open)",
        );
        assert_eq!(c, vec![(0, draw::ACCENT), (19, draw::DIM)]);
    }

    #[test]
    fn urls_dim_the_scheme_and_highlight_the_host() {
        let c = colors("browser://history", 2, "https://github.com/kayfgit");
        assert_eq!(c, vec![(0, draw::DIM), (8, draw::ACCENT), (18, draw::FG)]);
    }

    #[test]
    fn res_rows_colour_cpu_by_load() {
        let row = format!(
            "{:>9}  {:>6}  {:>9}  {:<22} {}",
            "52.0 MB", "31.0%", "0 B/s", "browser.exe", 1234
        );
        let c = colors("browser://res", 4, &row);
        assert!(c.contains(&(11, draw::ERR)), "{c:?}");
        let idle = format!(
            "{:>9}  {:>6}  {:>9}  {:<22} {}",
            "52.0 MB", "0.1%", "0 B/s", "browser.exe", 1234
        );
        assert!(colors("browser://res", 4, &idle).contains(&(11, draw::READ)));
    }

    #[test]
    fn errors_show_the_message_in_red() {
        assert_eq!(
            colors("browser://error", 1, "WebView2 error"),
            vec![(0, draw::ERR)]
        );
        let head = colors("browser://error", 0, "[12:34:56] :open x — error 1");
        assert_eq!(head[0], (0, draw::DIM));
        assert_eq!(head[1], (11, draw::ACCENT));
    }

    #[test]
    fn blank_lines_and_unknown_rows_stay_plain() {
        assert!(colors("browser://history", 1, "").is_empty());
        assert!(colors("browser://something", 5, "text").is_empty());
    }
}
