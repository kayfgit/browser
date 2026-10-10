//! Build steps for the browser:
//!
//! 1. Embed a Windows application manifest declaring PerMonitorV2 DPI awareness.
//!
//!    Without a DPI-awareness manifest, Windows treats a GUI exe as DPI-unaware and
//!    applies a compatibility shim (`__COMPAT_LAYER=HIGHDPIAWARE`). The WebView2
//!    runtime (117+) detects that shim and, to dodge a DPI-related launch bug,
//!    launches its engine processes via `explorer.exe` instead of parenting them to
//!    us. Declaring PerMonitorV2 suppresses the shim, so WebView2 parents its whole
//!    process tree under the browser.
//!
//! 2. Pack `extensions/` (the bundled uBlock Origin Lite) into a gzipped tar in
//!    `OUT_DIR`, which `bundled_extensions.rs` embeds and unpacks on first run. That
//!    keeps the installed browser a self-contained pair of executables. The archive's
//!    hash goes into `BUNDLED_EXTENSIONS_HASH` so a changed bundle gets re-extracted.
//!    `_metadata` folders are skipped: Chromium writes its own indexes there whenever it
//!    loads an unpacked extension, so they're per-machine output, not part of the bundle.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::Path;

fn main() {
    #[cfg(windows)]
    {
        use embed_manifest::manifest::DpiAwareness;
        use embed_manifest::{embed_manifest, new_manifest};

        embed_manifest(
            new_manifest("kayf.browser.Shell").dpi_awareness(DpiAwareness::PerMonitorV2),
        )
        .expect("embed application manifest");
    }
    pack_extensions();
    println!("cargo:rerun-if-changed=build.rs");
}

fn pack_extensions() {
    let src = Path::new("extensions");
    println!("cargo:rerun-if-changed=extensions");
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("extensions.tar.gz");

    let file = std::fs::File::create(&out).expect("create extensions archive");
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::best());
    let mut tar = tar::Builder::new(gz);
    // Deterministic headers (no mtimes/owners) so the hash only changes when the
    // extension's content does.
    tar.mode(tar::HeaderMode::Deterministic);
    if src.is_dir() {
        let mut dirs: Vec<_> = std::fs::read_dir(src)
            .expect("read extensions dir")
            .flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        dirs.sort_by_key(|e| e.file_name());
        for entry in dirs {
            append_tree(&mut tar, &entry.path(), Path::new(&entry.file_name()));
        }
    }
    tar.into_inner()
        .and_then(|gz| gz.finish())
        .expect("finish extensions archive");

    let bytes = std::fs::read(&out).expect("read extensions archive");
    let mut hasher = DefaultHasher::new();
    hasher.write(&bytes);
    println!(
        "cargo:rustc-env=BUNDLED_EXTENSIONS_HASH={:016x}",
        hasher.finish()
    );
}

/// Add `dir` to the archive as `name`, in sorted order (so the archive, and its hash, only
/// change when the files do), leaving out Chromium's `_metadata` output.
fn append_tree<W: std::io::Write>(tar: &mut tar::Builder<W>, dir: &Path, name: &Path) {
    tar.append_dir(name, dir).expect("pack extension folder");
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read extension folder")
        .flatten()
        .filter(|e| e.file_name() != "_metadata")
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let (path, inner) = (entry.path(), name.join(entry.file_name()));
        if path.is_dir() {
            append_tree(tar, &path, &inner);
        } else {
            tar.append_path_with_name(&path, &inner)
                .expect("pack extension file");
        }
    }
}
