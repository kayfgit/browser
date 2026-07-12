//! Terminal colour-scheme installer: searches two published collections —
//! iTerm2-Color-Schemes (github.com/mbadolato/iTerm2-Color-Schemes, Windows
//! Terminal-format files) and Gogh (github.com/Gogh-Co/Gogh, one JSON with every
//! theme inlined) — and stores the pick locally as editable TOML (see
//! [`SchemeFile`]). The network work runs on a background thread so the UI never
//! blocks; completion posts [`UserEvent::SchemeInstalled`], which applies the
//! scheme and reports back to the status bar or the initiating `:ai` chat.
//! Strictly shell/AI-initiated — never reachable from page content.

use tao::event_loop::EventLoopProxy;

use crate::config::{scheme_key, SchemeFile};
use crate::UserEvent;

const WT_LIST_URL: &str =
    "https://api.github.com/repos/mbadolato/iTerm2-Color-Schemes/contents/windowsterminal";
const WT_RAW_BASE: &str =
    "https://raw.githubusercontent.com/mbadolato/iTerm2-Color-Schemes/master/windowsterminal";
const GOGH_URL: &str = "https://raw.githubusercontent.com/Gogh-Co/Gogh/master/data/themes.json";

/// Kick off a background install of the scheme best matching `query`. The result
/// (success or a human-readable error, possibly a "similar names …" candidate list)
/// arrives as [`UserEvent::SchemeInstalled`] carrying `ai_id` through unchanged.
pub(crate) fn spawn_install(query: String, ai_id: Option<u64>, proxy: EventLoopProxy<UserEvent>) {
    std::thread::spawn(move || {
        let result = install(&query);
        let _ = proxy.send_event(UserEvent::SchemeInstalled { ai_id, result });
    });
}

/// One collection entry: the display name plus how to get its colours. Gogh's index
/// already carries every colour, so its entries convert without a second fetch.
enum Candidate {
    /// A `windowsterminal/<name>.json` file, fetched on demand.
    WindowsTerminal(String),
    /// A fully-inlined Gogh theme object.
    Gogh(serde_json::Value),
}

impl Candidate {
    fn name(&self) -> &str {
        match self {
            Candidate::WindowsTerminal(n) => n,
            Candidate::Gogh(v) => v.get("name").and_then(|x| x.as_str()).unwrap_or(""),
        }
    }
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("browser-desktop") // GitHub rejects requests without a UA
        .build()
        .map_err(|e| e.to_string())
}

/// The whole flow, blocking: gather both collections, pick the scheme `query`
/// means, fetch/convert it, and save. `Ok` carries the installed display name.
/// Either collection may be down — the other still serves; both down is the error.
fn install(query: &str) -> Result<String, String> {
    let c = client()?;
    let (candidates, fetch_err) = gather(&c);
    if candidates.is_empty() {
        return Err(fetch_err.unwrap_or_else(|| "no schemes available".into()));
    }
    let names: Vec<String> = candidates.iter().map(|c| c.name().to_string()).collect();
    let name = pick(query, &names)?;
    let cand = candidates
        .iter()
        .find(|c| c.name() == name)
        .ok_or("internal error: picked a scheme that vanished")?;
    let file = match cand {
        Candidate::WindowsTerminal(n) => {
            let url = format!("{WT_RAW_BASE}/{}.json", n.replace(' ', "%20"));
            let v: serde_json::Value = c
                .get(&url)
                .send()
                .map_err(|e| format!("downloading {n}: {e}"))?
                .error_for_status()
                .map_err(|e| format!("downloading {n}: {e}"))?
                .json()
                .map_err(|e| format!("parsing {n}: {e}"))?;
            convert_wt(n, &v)?
        }
        Candidate::Gogh(v) => convert_gogh(v)?,
    };
    crate::config::save_custom_scheme(&file)
}

/// Fetch both collections, deduped by normalized name (the Windows Terminal set
/// wins a tie — its palettes are curated for terminals). A failed source is
/// reported through the second slot but doesn't sink the other.
fn gather(c: &reqwest::blocking::Client) -> (Vec<Candidate>, Option<String>) {
    let mut out: Vec<Candidate> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    match wt_list(c) {
        Ok(names) => out.extend(names.into_iter().map(Candidate::WindowsTerminal)),
        Err(e) => errs.push(e),
    }
    match gogh_list(c) {
        Ok(themes) => {
            let have: std::collections::HashSet<String> =
                out.iter().map(|c| scheme_key(c.name())).collect();
            out.extend(
                themes
                    .into_iter()
                    .filter(|v| {
                        let n = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        !n.is_empty() && !have.contains(&scheme_key(n))
                    })
                    .map(Candidate::Gogh),
            );
        }
        Err(e) => errs.push(e),
    }
    (out, (!errs.is_empty()).then(|| errs.join("; ")))
}

/// The iTerm2-Color-Schemes `windowsterminal/*.json` file names.
fn wt_list(c: &reqwest::blocking::Client) -> Result<Vec<String>, String> {
    let v: serde_json::Value = c
        .get(WT_LIST_URL)
        .send()
        .map_err(|e| format!("reaching iTerm2-Color-Schemes: {e}"))?
        .error_for_status()
        .map_err(|e| format!("reaching iTerm2-Color-Schemes: {e}"))?
        .json()
        .map_err(|e| format!("parsing the iTerm2-Color-Schemes list: {e}"))?;
    let arr = v.as_array().ok_or("unexpected response from iTerm2-Color-Schemes")?;
    Ok(arr
        .iter()
        .filter_map(|f| Some(f.get("name")?.as_str()?.strip_suffix(".json")?.to_string()))
        .collect())
}

/// Gogh's whole index — every theme fully inlined.
fn gogh_list(c: &reqwest::blocking::Client) -> Result<Vec<serde_json::Value>, String> {
    let v: serde_json::Value = c
        .get(GOGH_URL)
        .send()
        .map_err(|e| format!("reaching Gogh: {e}"))?
        .error_for_status()
        .map_err(|e| format!("reaching Gogh: {e}"))?
        .json()
        .map_err(|e| format!("parsing the Gogh theme list: {e}"))?;
    v.as_array().cloned().ok_or_else(|| "unexpected response from Gogh".into())
}

/// Resolve `query` to one collection entry. An exact (normalized) name wins; then
/// names containing the whole query ("gruvbox" → GruvboxDark, …); then names
/// covering all the query's words. One clear hit is returned; several become an
/// `Err` listing them; names sharing only SOME words are offered as "similar"
/// suggestions, never auto-picked — so "ibm 3270" won't silently install an
/// unrelated IBM scheme.
fn pick(query: &str, names: &[String]) -> Result<String, String> {
    let qk = scheme_key(query);
    if qk.is_empty() {
        return Err("give a scheme name to install".into());
    }
    if let Some(n) = names.iter().find(|n| scheme_key(n) == qk) {
        return Ok(n.clone());
    }
    let listed = |hits: &[&String]| {
        let mut shown: Vec<&str> = hits.iter().take(8).map(|s| s.as_str()).collect();
        shown.sort();
        format!("{}{}", shown.join(", "), if hits.len() > 8 { ", …" } else { "" })
    };
    // Whole-query containment, then all-words coverage — both count as real matches.
    let mut hits: Vec<&String> = names.iter().filter(|n| scheme_key(n).contains(&qk)).collect();
    let tokens: Vec<String> =
        query.split_whitespace().map(scheme_key).filter(|t| t.len() >= 2).collect();
    let matched =
        |n: &str| -> usize { tokens.iter().filter(|t| scheme_key(n).contains(*t)).count() };
    if hits.is_empty() && !tokens.is_empty() {
        hits = names.iter().filter(|n| matched(n) == tokens.len()).collect();
    }
    match hits.len() {
        1 => return Ok(hits[0].clone()),
        2.. => {
            return Err(format!(
                "'{query}' matches several schemes — {}. Install one by its exact name.",
                listed(&hits)
            ))
        }
        0 => {}
    }
    // Nothing really matches; offer names sharing at least one word as suggestions.
    let similar: Vec<&String> = names.iter().filter(|n| matched(n) > 0).collect();
    if similar.is_empty() {
        Err(format!("no scheme in the collections matches '{query}'"))
    } else {
        Err(format!(
            "no scheme matches '{query}' — similar names: {}. If one of these is what's \
             wanted, install it by that exact name.",
            listed(&similar)
        ))
    }
}

/// Convert a Windows Terminal scheme JSON into our on-disk form. WT calls the
/// magenta slots "purple"; order follows our ANSI layout (0–7 normal, 8–15 bright).
fn convert_wt(name: &str, v: &serde_json::Value) -> Result<SchemeFile, String> {
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("scheme file is missing '{k}'"))
    };
    const ANSI_KEYS: [&str; 16] = [
        "black", "red", "green", "yellow", "blue", "purple", "cyan", "white",
        "brightBlack", "brightRed", "brightGreen", "brightYellow", "brightBlue",
        "brightPurple", "brightCyan", "brightWhite",
    ];
    let mut ansi = Vec::with_capacity(16);
    for k in ANSI_KEYS {
        ansi.push(get(k)?);
    }
    Ok(SchemeFile {
        name: v.get("name").and_then(|x| x.as_str()).unwrap_or(name).to_string(),
        fg: get("foreground")?,
        bg: get("background")?,
        ansi,
    })
}

/// Convert an inlined Gogh theme: `color_01`…`color_16` are the ANSI palette in
/// our order (01–08 normal, 09–16 bright), plus `foreground`/`background`.
fn convert_gogh(v: &serde_json::Value) -> Result<SchemeFile, String> {
    let get = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("scheme entry is missing '{k}'"))
    };
    let mut ansi = Vec::with_capacity(16);
    for i in 1..=16 {
        ansi.push(get(&format!("color_{i:02}"))?);
    }
    Ok(SchemeFile { name: get("name")?, fg: get("foreground")?, bg: get("background")?, ansi })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        [
            "3270-Dark", "3270-Light", "GruvboxDark", "GruvboxDarkHard", "Dracula",
            "Tokyo Night", "Ibm3270", "IBM 5153 CGA", "IBM 5153 CGA (Black)",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn pick_exact_and_full_word_matches_win() {
        // Exact (normalized) match wins outright — "ibm 3270" IS Ibm3270.
        assert_eq!(pick("ibm 3270", &names()).unwrap(), "Ibm3270");
        assert_eq!(pick("dracula", &names()).unwrap(), "Dracula");
        assert_eq!(pick("tokyo night", &names()).unwrap(), "Tokyo Night");
        // A lone all-words match is picked ("5153 black" covers both words).
        assert_eq!(pick("5153 black", &names()).unwrap(), "IBM 5153 CGA (Black)");
    }

    #[test]
    fn pick_lists_candidates_and_similar_names() {
        // Whole-query containment with several hits lists them to choose from.
        let err = pick("gruvbox", &names()).unwrap_err();
        assert!(err.contains("GruvboxDark") && err.contains("GruvboxDarkHard"), "{err}");
        // Names sharing only SOME words are suggestions, never auto-installed.
        let err = pick("ibm quantum", &names()).unwrap_err();
        assert!(err.contains("no scheme matches") && err.contains("IBM 5153 CGA"), "{err}");
        // Unknown names say so.
        assert!(pick("zzzz", &names()).unwrap_err().contains("no scheme"));
    }

    #[test]
    fn converts_windows_terminal_and_gogh_formats() {
        let wt: serde_json::Value = serde_json::from_str(
            r##"{
                "name": "Example", "background": "#000000", "foreground": "#f9fafa",
                "black": "#222222", "red": "#f24d42", "green": "#57ce39",
                "yellow": "#fcbb39", "blue": "#8ba3e8", "purple": "#c956db",
                "cyan": "#5cccc8", "white": "#f2f5f4", "brightBlack": "#686a66",
                "brightRed": "#f96f60", "brightGreen": "#75e155", "brightYellow": "#fcd669",
                "brightBlue": "#a5bbf2", "brightPurple": "#dc84e8", "brightCyan": "#7de0da",
                "brightWhite": "#ffffff"
            }"##,
        )
        .unwrap();
        let f = convert_wt("Example", &wt).unwrap();
        assert_eq!((f.ansi.len(), f.ansi[5].as_str()), (16, "#c956db")); // WT "purple" → magenta slot

        let gogh: serde_json::Value = serde_json::from_str(
            r##"{
                "name": "Ibm3270", "background": "#000000", "foreground": "#FDFDFD",
                "color_01": "#222222", "color_02": "#F01818", "color_03": "#24D830",
                "color_04": "#F0D824", "color_05": "#7890F0", "color_06": "#F078D8",
                "color_07": "#54E4E4", "color_08": "#A5A5A5", "color_09": "#888888",
                "color_10": "#EF8383", "color_11": "#7ED684", "color_12": "#EFE28B",
                "color_13": "#B3BFEF", "color_14": "#EFB3E3", "color_15": "#9CE2E2",
                "color_16": "#FFFFFF"
            }"##,
        )
        .unwrap();
        let f = convert_gogh(&gogh).unwrap();
        assert_eq!(f.name, "Ibm3270");
        assert_eq!(f.ansi[8], "#888888"); // color_09 → bright black
    }
}
