//! Keeps ARCHITECTURE.md from going stale: every file it links must exist, and every
//! module in `crates/desktop/src` must appear in its code map.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn architecture_doc() -> String {
    fs::read_to_string(repo_root().join("ARCHITECTURE.md")).expect("reading ARCHITECTURE.md")
}

/// The targets of the doc's relative Markdown links (`[text](target)`), without any
/// `#fragment`. Web links are skipped.
fn link_targets(doc: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = doc;
    while let Some(i) = rest.find("](") {
        rest = &rest[i + 2..];
        let Some(end) = rest.find(')') else { break };
        let target = &rest[..end];
        if !target.contains("://") && !target.starts_with('#') {
            let path = target.split('#').next().unwrap_or(target);
            targets.push(path.to_string());
        }
        rest = &rest[end..];
    }
    targets
}

#[test]
fn every_linked_file_exists() {
    let doc = architecture_doc();
    let targets = link_targets(&doc);
    assert!(targets.len() > 20, "found only {} links", targets.len());
    let missing: Vec<&String> = targets
        .iter()
        .filter(|t| !repo_root().join(t).exists())
        .collect();
    assert!(
        missing.is_empty(),
        "ARCHITECTURE.md links to files that don't exist: {missing:?}"
    );
}

#[test]
fn every_desktop_module_is_in_the_code_map() {
    let doc = architecture_doc();
    let src = repo_root().join("crates/desktop/src");
    let mut unlisted = Vec::new();
    for entry in fs::read_dir(&src).expect("reading crates/desktop/src") {
        let name = entry.expect("reading a directory entry").file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".rs") || src.join(&*name).is_dir()) {
            continue;
        }
        if !doc.contains(&format!("](crates/desktop/src/{name}")) {
            unlisted.push(name.into_owned());
        }
    }
    assert!(
        unlisted.is_empty(),
        "add these to the code map in ARCHITECTURE.md: {unlisted:?}"
    );
}

#[test]
fn link_targets_skip_web_links_and_fragments() {
    let doc = "[a](x.rs) [b](https://e.test/) [c](#top) [d](dir/y.md#part)";
    assert_eq!(link_targets(doc), ["x.rs", "dir/y.md"]);
}
