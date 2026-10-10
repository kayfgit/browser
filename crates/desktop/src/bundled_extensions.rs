//! The browser extensions bundled into the executable (uBlock Origin Lite).
//!
//! `build.rs` packs `crates/desktop/extensions/` into a gzipped tar that is embedded
//! here, so an installed browser is just its two executables. On first use it is
//! unpacked to `%LOCALAPPDATA%\browser\data\extensions`, and re-unpacked whenever the
//! bundle changes (a hash stamp sits next to that folder).
//!
//! Debug builds unpack beside the executable instead (`target/debug/bundled/extensions`).
//! They used to load the source tree directly, but Chromium writes its own indexes into
//! an unpacked extension's `_metadata` folder, which dirtied the checkout on every run. A
//! separate folder also keeps a debug run from swapping out the copy an installed
//! browser has open.
//!
//! The folder path must never change. The bundled extensions have no `key` in their
//! manifests, so WebView2 derives each one's ID from its folder path; a new path would
//! register a second copy in the profile instead of updating the first.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/extensions.tar.gz"));
const HASH: &str = env!("BUNDLED_EXTENSIONS_HASH");

static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The folder holding the unpacked bundled extensions, unpacking them first if needed.
/// `None` if there is no data folder or nothing could be unpacked. The work happens
/// once per process; later calls (and calls racing [`prepare_in_background`]) wait
/// for that first result.
pub(crate) fn dir() -> Option<PathBuf> {
    DIR.get_or_init(|| {
        let root = root()?;
        match unpack(&root) {
            Ok(()) => Some(root),
            Err(e) => {
                eprintln!("bundled extensions: {e}");
                // An older unpacked copy still beats no ad blocking at all.
                root.is_dir().then_some(root)
            }
        }
    })
    .clone()
}

/// Where the bundle is unpacked: the local data folder, or beside a debug executable.
fn root() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        let exe = std::env::current_exe().ok()?;
        return Some(exe.parent()?.join("bundled").join("extensions"));
    }
    Some(crate::session::local_data_dir()?.join("extensions"))
}

/// Start unpacking on a background thread, so the first web tab doesn't wait for it.
pub(crate) fn prepare_in_background() {
    std::thread::spawn(|| {
        let _ = dir();
    });
}

/// Make `root` hold exactly the embedded bundle. A no-op when the stamp already
/// matches. Otherwise unpack into a staging folder beside it and swap it in, so a
/// half-written unpack is never used.
fn unpack(root: &Path) -> io::Result<()> {
    let parent = root
        .parent()
        .ok_or_else(|| io::Error::other("extension folder has no parent"))?;
    // The stamp lives beside the folder, not in it: WebView2 treats every entry in
    // the folder as an extension to load.
    let stamp = parent.join("extensions.hash");
    if root.is_dir() && fs::read_to_string(&stamp).is_ok_and(|s| s == HASH) {
        return Ok(());
    }
    fs::create_dir_all(parent)?;
    let pid = std::process::id();
    let staging = parent.join(format!("extensions.new-{pid}"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    if let Err(e) = tar::Archive::new(flate2::read::GzDecoder::new(ARCHIVE)).unpack(&staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    if root.exists() {
        // Fails if another running browser has the old files open; keep using them.
        let old = parent.join(format!("extensions.old-{pid}"));
        if let Err(e) = fs::rename(root, &old) {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
        let _ = fs::remove_dir_all(&old);
    }
    fs::rename(&staging, root)?;
    fs::write(&stamp, HASH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpacks_once_and_repairs_a_stale_copy() {
        let base = std::env::temp_dir().join(format!("browser-ext-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("extensions");

        unpack(&root).unwrap();
        assert!(root
            .join("uBOLite.chromium")
            .join("manifest.json")
            .is_file());
        assert_eq!(
            fs::read_to_string(base.join("extensions.hash")).unwrap(),
            HASH
        );

        // A matching stamp leaves the folder alone.
        fs::write(root.join("marker"), "x").unwrap();
        unpack(&root).unwrap();
        assert!(root.join("marker").exists());

        // A stale stamp replaces the folder wholesale.
        fs::write(base.join("extensions.hash"), "stale").unwrap();
        unpack(&root).unwrap();
        assert!(!root.join("marker").exists());
        assert!(root
            .join("uBOLite.chromium")
            .join("manifest.json")
            .is_file());

        fs::remove_dir_all(&base).unwrap();
    }
}
