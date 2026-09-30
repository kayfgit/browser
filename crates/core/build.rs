//! Compiles the bundled Kagi bang list (`data/kagi-bangs.json`) into lookup tables: one
//! string blob plus integer offsets into it, sorted by trigger. The browser then looks
//! bangs up with a binary search over static data, without parsing JSON or allocating.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::{env, fs};

// Kagi's format flags (see src/bangs.rs). With no `fmt`, all of them apply.
const OPEN_BASE_PATH: u8 = 1;
const OPEN_SNAP_DOMAIN: u8 = 2;
const URL_ENCODE_PLACEHOLDER: u8 = 4;
const URL_ENCODE_SPACE_TO_PLUS: u8 = 8;
const ALL_FLAGS: u8 = 15;

fn main() {
    let json_path = "data/kagi-bangs.json";
    let version_path = "data/kagi-bangs.version";
    println!("cargo:rerun-if-changed={json_path}");
    println!("cargo:rerun-if-changed={version_path}");
    let json = fs::read_to_string(json_path).expect("reading data/kagi-bangs.json");
    let bangs: Vec<serde_json::Value> =
        serde_json::from_str(&json).expect("parsing data/kagi-bangs.json");
    let version = fs::read_to_string(version_path).unwrap_or_default();

    let mut blob = String::new();
    let mut table = String::new();
    let mut triggers: BTreeMap<String, usize> = BTreeMap::new();
    let mut count = 0usize;
    for bang in &bangs {
        let text = |key: &str| bang.get(key).and_then(|v| v.as_str()).unwrap_or("");
        let url = text("u");
        // Skip what can't work outside Kagi: regex bangs, relative links and kagi.com
        // pages (which need a Kagi account), and templates without a query slot.
        if bang.get("x").is_some() || !url.contains("{{{s}}}") || !is_http(url) || is_kagi(url) {
            continue;
        }
        let flags = match bang.get("fmt").and_then(|f| f.as_array()) {
            None => ALL_FLAGS,
            Some(list) => list.iter().filter_map(|f| f.as_str()).fold(0, |acc, f| {
                acc | match f {
                    "open_base_path" => OPEN_BASE_PATH,
                    "open_snap_domain" => OPEN_SNAP_DOMAIN,
                    "url_encode_placeholder" => URL_ENCODE_PLACEHOLDER,
                    "url_encode_space_to_plus" => URL_ENCODE_SPACE_TO_PLUS,
                    _ => 0,
                }
            }),
        };
        let mut keys: Vec<String> = std::iter::once(text("t"))
            .chain(
                bang["ts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str()),
            )
            .map(str::to_lowercase)
            .filter(|t| !t.is_empty() && !triggers.contains_key(t))
            .collect();
        keys.dedup();
        if keys.is_empty() {
            continue;
        }
        let (name_at, name_len) = push(&mut blob, text("s"));
        let (url_at, url_len) = push(&mut blob, url);
        let (snap_at, snap_len) = push(&mut blob, text("ad"));
        writeln!(
            table,
            "({name_at},{name_len},{url_at},{url_len},{snap_at},{snap_len},{flags}),"
        )
        .unwrap();
        for key in keys {
            triggers.insert(key, count);
        }
        count += 1;
    }
    let mut trigger_table = String::new();
    for (key, index) in &triggers {
        let (at, len) = push(&mut blob, key);
        writeln!(trigger_table, "({at},{len},{index}),").unwrap();
    }

    let out = env::var("OUT_DIR").unwrap();
    fs::write(Path::new(&out).join("kagi_bangs.txt"), &blob).unwrap();
    let code = format!(
        "/// The Kagi release the bang list comes from.\n\
         pub const KAGI_VERSION: &str = {version:?};\n\
         static BLOB: &str = include_str!(concat!(env!(\"OUT_DIR\"), \"/kagi_bangs.txt\"));\n\
         /// Per bang: name, URL template and snap domain as (offset, length) into BLOB,\n\
         /// then its format flags.\n\
         static BANGS: [(u32, u32, u32, u32, u32, u32, u8); {count}] = [\n{table}];\n\
         /// Every trigger as (offset, length) into BLOB and its index into BANGS, sorted\n\
         /// by trigger.\n\
         static TRIGGERS: [(u32, u32, u32); {n}] = [\n{trigger_table}];\n",
        version = version.trim(),
        n = triggers.len(),
    );
    fs::write(Path::new(&out).join("kagi_bangs.rs"), code).unwrap();
}

fn push(blob: &mut String, s: &str) -> (usize, usize) {
    let at = blob.len();
    blob.push_str(s);
    (at, s.len())
}

fn is_http(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn is_kagi(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or("");
    let host = host.split(['/', '?', '#', ':']).next().unwrap_or("");
    host == "kagi.com" || host.ends_with(".kagi.com")
}
