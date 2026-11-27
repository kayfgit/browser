//! Microsoft's ConPTY (`conpty.dll` + `OpenConsole.exe`), bundled into the executable.
//!
//! Terminals run their shell under a pseudoconsole. Windows' own (conhost) drops colour
//! queries, turns an underline colour into blinking text and is several times slower
//! on heavy output. Microsoft's current ConPTY package, the one Windows Terminal and VS
//! Code ship, passes them through. `build.rs` packs `crates/desktop/conpty/` (refreshed
//! by `update-conpty.ps1`) into a gzipped tar embedded here; the first terminal unpacks
//! it to `%LOCALAPPDATA%\browser\data\conpty\<hash>` and hands that folder to
//! `browser-pty-host` (`--conpty`), which loads `conpty.dll` from it. `conpty.dll`
//! starts the `OpenConsole.exe` beside it.
//!
//! The folder is named after the bundle's hash rather than replaced in place: a running
//! terminal keeps its `OpenConsole.exe` open, so an updated browser unpacks beside it
//! and older copies are removed once nothing uses them. Debug builds unpack beside the
//! executable (`target/debug/bundled/conpty`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/conpty.tar.gz"));
const HASH: &str = env!("BUNDLED_CONPTY_HASH");

/// The two files the folder must hold for `conpty.dll` to be used.
pub(crate) const FILES: [&str; 2] = ["conpty.dll", "OpenConsole.exe"];

static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The folder holding the unpacked ConPTY, unpacking it the first time. `None` when
/// there's no data folder, the bundle is empty, or unpacking failed: terminals then
/// use Windows' own console host.
pub(crate) fn dir() -> Option<PathBuf> {
    DIR.get_or_init(|| {
        let parent = root()?;
        match unpack(&parent, HASH, ARCHIVE) {
            Ok(dir) => Some(dir),
            Err(e) => {
                crate::journal::log("terminal", format!("preparing the bundled ConPTY: {e}"));
                None
            }
        }
    })
    .clone()
}

/// Where versions of the bundle are unpacked: the local data folder, or beside a
/// debug executable.
fn root() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        let exe = std::env::current_exe().ok()?;
        return Some(exe.parent()?.join("bundled").join("conpty"));
    }
    Some(crate::session::local_data_dir()?.join("conpty"))
}

/// Whether `dir` holds both files.
pub(crate) fn complete(dir: &Path) -> bool {
    FILES.iter().all(|f| dir.join(f).is_file())
}

/// Unpack `archive` into `<parent>/<hash>` unless it's already there, then remove
/// other versions beside it that nothing has open. Returns the folder.
fn unpack(parent: &Path, hash: &str, archive: &[u8]) -> io::Result<PathBuf> {
    let dir = parent.join(hash);
    if !complete(&dir) {
        fs::create_dir_all(parent)?;
        let staging = parent.join(format!("{hash}.new-{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;
        let unpacked = tar::Archive::new(flate2::read::GzDecoder::new(archive)).unpack(&staging);
        if let Err(e) = unpacked.and_then(|()| {
            if complete(&staging) {
                Ok(())
            } else {
                Err(io::Error::other(
                    "the bundle has no conpty.dll/OpenConsole.exe",
                ))
            }
        }) {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
        let _ = fs::remove_dir_all(&dir);
        if let Err(e) = fs::rename(&staging, &dir) {
            let _ = fs::remove_dir_all(&staging);
            // Another browser may have unpacked the same version a moment ago.
            if !complete(&dir) {
                return Err(e);
            }
        }
    }
    // Older versions: removing fails while a terminal still runs their OpenConsole.exe,
    // which is fine, a later start tries again.
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            if entry.file_name() != hash && entry.path().is_dir() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, name, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn unpacks_into_a_folder_per_version_and_clears_old_ones() {
        let base = std::env::temp_dir().join(format!("browser-conpty-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let both = archive(&[("conpty.dll", b"dll"), ("OpenConsole.exe", b"exe")]);

        let v1 = unpack(&base, "v1", &both).unwrap();
        assert_eq!(v1, base.join("v1"));
        assert!(complete(&v1));
        // Already there: left alone.
        fs::write(v1.join("marker"), "x").unwrap();
        unpack(&base, "v1", &both).unwrap();
        assert!(v1.join("marker").exists());

        // A new version unpacks beside it, and the old one goes.
        let v2 = unpack(&base, "v2", &both).unwrap();
        assert!(complete(&v2) && !v1.exists());

        // A bundle missing a file is refused, and leaves nothing half-written.
        let half = archive(&[("conpty.dll", b"dll")]);
        assert!(unpack(&base, "v3", &half).is_err());
        assert!(!base.join("v3").exists());

        fs::remove_dir_all(&base).unwrap();
    }

    /// The real bundle: both files, and they're the real thing.
    #[test]
    fn the_embedded_bundle_has_conpty_and_openconsole() {
        let base = std::env::temp_dir().join(format!("browser-conpty-real-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let dir = unpack(&base, HASH, ARCHIVE).unwrap();
        for f in FILES {
            let bytes = fs::read(dir.join(f)).unwrap();
            assert!(bytes.starts_with(b"MZ") && bytes.len() > 50_000, "{f}");
        }
        fs::remove_dir_all(&base).unwrap();
    }
}
