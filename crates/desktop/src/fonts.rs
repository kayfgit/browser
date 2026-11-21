//! Where the system's font files are, per platform. The painter ([`crate::draw`])
//! only rasterizes; this module decides which files it reads.
//!
//! Windows keeps every font in `C:\Windows\Fonts` (plus the per-user folder), under
//! stable file names. Linux spreads them over nested folders whose layout differs per
//! distro, so there fontconfig (`fc-match`, installed wherever GTK is) names the file,
//! and a recursive scan of the usual folders backs it up.

use std::path::{Path, PathBuf};

/// A fallback face: a file path on Windows, a fontconfig family on Linux.
pub(crate) struct FallbackSpec {
    pub(crate) spec: &'static str,
    /// Only consulted for CJK, kana and Hangul characters.
    pub(crate) cjk_only: bool,
}

const fn spec(spec: &'static str, cjk_only: bool) -> FallbackSpec {
    FallbackSpec { spec, cjk_only }
}

/// Tried in order, so terminals and pages show real glyphs rather than `.notdef` tofu
/// boxes: symbols (Braille, dingbats) and other Latin first, then CJK — Japanese first
/// so the Han ideographs shared across CJK render in Japanese forms, then Korean, then
/// Chinese. `.ttc` collections load their first face.
#[cfg(windows)]
pub(crate) const FALLBACKS: &[FallbackSpec] = &[
    spec(r"C:\Windows\Fonts\seguisym.ttf", false), // Segoe UI Symbol
    spec(r"C:\Windows\Fonts\arial.ttf", false),
    spec(r"C:\Windows\Fonts\YuGothR.ttc", true), // Yu Gothic — Japanese
    spec(r"C:\Windows\Fonts\msgothic.ttc", true), // MS Gothic — Japanese
    spec(r"C:\Windows\Fonts\malgun.ttf", true),  // Malgun Gothic — Korean
    spec(r"C:\Windows\Fonts\msyh.ttc", true),    // Microsoft YaHei — Chinese
    spec(r"C:\Windows\Fonts\simsun.ttc", true),  // SimSun — Chinese
];

/// See the Windows list. Noto Sans CJK's `.ttc` opens on its Japanese face.
#[cfg(not(windows))]
pub(crate) const FALLBACKS: &[FallbackSpec] = &[
    spec("DejaVu Sans", false),
    spec("Noto Sans Symbols2", false),
    spec("Noto Sans", false),
    spec("Noto Sans CJK JP", true),
    spec("Noto Sans CJK KR", true),
    spec("Noto Sans CJK SC", true),
    spec("WenQuanYi Zen Hei", true),
];

/// The file behind a [`FallbackSpec`], if that font is installed.
pub(crate) fn fallback_path(spec: &FallbackSpec) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        Some(PathBuf::from(spec.spec))
    }
    #[cfg(not(windows))]
    {
        family_file(spec.spec)
    }
}

/// The primary face's file: a monospace font, since the chrome and terminals lay text
/// out in cells. The first installed candidate wins.
pub(crate) fn system_font_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        [
            r"C:\Windows\Fonts\consola.ttf",
            r"C:\Windows\Fonts\segoeui.ttf",
            r"C:\Windows\Fonts\arial.ttf",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    }
    #[cfg(not(windows))]
    {
        const FAMILIES: &[&str] = &["DejaVu Sans Mono", "Liberation Mono", "Noto Sans Mono"];
        FAMILIES
            .iter()
            .find_map(|f| family_file(f))
            // Whatever the system calls monospace (fontconfig's generic alias).
            .or_else(|| fc_match("monospace").map(|(path, _)| path))
    }
}

/// What to name in the "no font" error.
pub(crate) const SYSTEM_FONT_NAMES: &str = if cfg!(windows) {
    "Consolas/Segoe UI/Arial"
} else {
    "DejaVu Sans Mono/Liberation Mono/Noto Sans Mono (install fonts-dejavu-core)"
};

/// The folders a font name is looked up in: the system folder and the per-user one.
pub(crate) fn font_dirs() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut dirs = vec![PathBuf::from(r"C:\Windows\Fonts")];
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            // Per-user installed fonts (the default for "Install for me" on Windows 10+).
            dirs.push(PathBuf::from(local).join(r"Microsoft\Windows\Fonts"));
        }
        dirs
    }
    #[cfg(not(windows))]
    {
        let mut dirs = vec![
            PathBuf::from("/usr/share/fonts"),
            PathBuf::from("/usr/local/share/fonts"),
        ];
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
        if let Some(data_home) = data_home {
            dirs.push(data_home.join("fonts"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".fonts"));
        }
        dirs
    }
}

/// Every font file (`.ttf`, `.otf`, `.ttc`) in `dir`, descending into subfolders on
/// platforms that nest them (Linux sorts fonts into per-package folders).
pub(crate) fn font_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_font_files(dir, if cfg!(windows) { 0 } else { 6 }, &mut found);
    found
}

fn collect_font_files(dir: &Path, depth: u32, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                collect_font_files(&path, depth - 1, found);
            }
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(ext.as_deref(), Some("ttf" | "otf" | "ttc")) {
            found.push(path);
        }
    }
}

/// The installed file of font `family`, or `None` when fontconfig only has a
/// substitute for it (`fc-match` always answers with *something*).
#[cfg(not(windows))]
fn family_file(family: &str) -> Option<PathBuf> {
    match fc_match(family) {
        Some((path, matched)) if family_matches(family, &matched) => Some(path),
        Some(_) => None,
        // No fontconfig: guess from the file names in the font folders.
        None => scan_for_family(family),
    }
}

/// Ask fontconfig which file it would use for `pattern`, and that file's families.
#[cfg(not(windows))]
fn fc_match(pattern: &str) -> Option<(PathBuf, String)> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\n%{family}", pattern])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (file, families) = text.split_once('\n').unwrap_or((&text, ""));
    let path = PathBuf::from(file.trim());
    path.is_file().then(|| (path, families.trim().to_string()))
}

/// Whether fontconfig's `matched` family list (comma separated: a font can carry
/// several names) includes the `wanted` family.
#[cfg_attr(windows, allow(dead_code))]
fn family_matches(wanted: &str, matched: &str) -> bool {
    let norm = normalize;
    let wanted = norm(wanted);
    matched.split(',').any(|f| norm(f) == wanted)
}

#[cfg(not(windows))]
fn scan_for_family(family: &str) -> Option<PathBuf> {
    let wanted = normalize(family);
    font_dirs()
        .iter()
        .flat_map(|d| font_files(d))
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| normalize(s).starts_with(&wanted))
        })
        // The shortest name is the plain face, not "…Bold" or "…Oblique".
        .min_by_key(|p| p.as_os_str().len())
}

/// A name with everything but ASCII letters and digits dropped, lowercased: the form
/// font names and file names are compared in ("Cascadia Code" = "CascadiaCode").
pub(crate) fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_match_ignores_case_spaces_and_extra_names() {
        assert!(family_matches("DejaVu Sans Mono", "DejaVu Sans Mono"));
        assert!(family_matches(
            "noto sans cjk jp",
            "Noto Sans CJK JP,Noto Sans CJK JP Regular"
        ));
        // A substitute fontconfig picked is not the font asked for.
        assert!(!family_matches("Noto Sans CJK JP", "DejaVu Sans"));
    }

    #[test]
    fn nested_font_folders_are_searched() {
        let root = std::env::temp_dir().join(format!("browser-fonts-{}", std::process::id()));
        let nested = root.join("truetype").join("dejavu");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("DejaVuSansMono.ttf"), b"").unwrap();
        std::fs::write(root.join("readme.txt"), b"").unwrap();
        let found = font_files(&root);
        let _ = std::fs::remove_dir_all(&root);
        if cfg!(windows) {
            // Windows keeps fonts in one flat folder.
            assert!(found.is_empty());
        } else {
            assert_eq!(found.len(), 1);
            assert!(found[0].ends_with("DejaVuSansMono.ttf"));
        }
    }
}
