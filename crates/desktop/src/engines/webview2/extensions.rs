//! WebView2 profile extension APIs.
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2BrowserExtension, ICoreWebView2BrowserExtensionList, ICoreWebView2Profile7,
    ICoreWebView2_13,
};
use webview2_com::{
    take_pwstr, BrowserExtensionEnableCompletedHandler, BrowserExtensionRemoveCompletedHandler,
    ProfileAddBrowserExtensionCompletedHandler, ProfileGetBrowserExtensionsCompletedHandler,
};
use windows_core::{Interface, BOOL, PWSTR};
use wry::{WebView, WebViewExtWindows};

use browser_engine::{Completion, EngineResult, ExtensionInfo as ExtInfo};

/// The profile-7 handle a webview exposes the extension APIs through. `None` if the raw
/// engine handle or the interface casts aren't available (best-effort, like the other COM
/// glue) — callers then simply do nothing.
fn profile7(webview: &WebView) -> Option<ICoreWebView2Profile7> {
    let core = unsafe { webview.controller().CoreWebView2() }.ok()?;
    let c13 = core.cast::<ICoreWebView2_13>().ok()?;
    let profile = unsafe { c13.Profile() }.ok()?;
    profile.cast::<ICoreWebView2Profile7>().ok()
}

/// Read a `PWSTR`-returning getter into an owned `String` (empty on failure).
fn pwstr_of(f: impl FnOnce(*mut PWSTR) -> windows_core::Result<()>) -> String {
    let mut p = PWSTR::null();
    if f(&mut p).is_ok() {
        take_pwstr(p)
    } else {
        String::new()
    }
}

/// Pull id/name/enabled out of a browser-extension list, synchronously (called inside the
/// async completion handler, on the UI thread).
fn read_list(list: &ICoreWebView2BrowserExtensionList) -> Vec<ExtInfo> {
    let mut out = Vec::new();
    let mut count = 0u32;
    if unsafe { list.Count(&mut count) }.is_err() {
        return out;
    }
    for i in 0..count {
        let Ok(ext) = (unsafe { list.GetValueAtIndex(i) }) else {
            continue;
        };
        let id = pwstr_of(|p| unsafe { ext.Id(p) });
        let name = pwstr_of(|p| unsafe { ext.Name(p) });
        let mut b = BOOL::default();
        let enabled = unsafe { ext.IsEnabled(&mut b) }.is_ok() && b.as_bool();
        out.push(ExtInfo { id, name, enabled });
    }
    out
}

/// Query the installed extensions; report dispatch and completion failures separately.
pub(crate) fn list(webview: &WebView, done: Completion<Vec<ExtInfo>>) -> EngineResult {
    let profile = profile7(webview).ok_or("extension APIs unavailable in this runtime")?;
    let handler = ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |hr, list| {
        done(hr.map_err(|e| e.to_string()).and_then(|()| {
            list.as_ref()
                .map(read_list)
                .ok_or_else(|| "engine returned no extension list".into())
        }));
        Ok(())
    }));
    unsafe { profile.GetBrowserExtensions(&handler) }.map_err(|e| e.to_string())
}

/// Enable/disable one extension by id (fire-and-forget: the shell flips its cached copy).
pub(crate) fn set_enabled(webview: &WebView, id: String, enabled: bool) -> EngineResult {
    apply(webview, move |_ext, eid| eid == id, enabled)
}

/// [`Extensions::sync_bundled`](browser_engine::Extensions::sync_bundled): make each
/// unpacked extension under `dir` installed (when wanted) and set to `enabled`, touching
/// nothing else in the profile — extensions the user installed keep whatever state they
/// gave them.
///
/// It LOOKS before it acts, because acting is not free. Re-adding an installed unpacked
/// extension reloads it, and loading an extension with network rules makes Chromium reset
/// every page's network loaders, aborting requests in flight. That was the YouTube "half
/// loaded" page: its document (streamed through YouTube's service worker) was cut off
/// mid-download when the bundled uBO Lite was re-added at startup, or — while wry did the
/// adding — whenever any tab was built, leaving the parser stuck in `loading` with the
/// skeleton showing (measured 2026-10: 4 of 5 fresh loads; reload "fixed" it because nothing
/// re-added the extension then). So: an installed copy is only switched if its state
/// differs, and only (re-)added when it's missing or `refresh` says the files on disk
/// changed since the profile loaded them.
///
/// The state is read from `GetBrowserExtensions`, never from the object `Add` hands back —
/// that one reports `IsEnabled == true` even while the profile holds the extension disabled
/// (measured on runtime 154), which once left uBO Lite silently off. Failures go to `done`.
pub(crate) fn sync_bundled(
    webview: &WebView,
    dir: &Path,
    enabled: bool,
    refresh: bool,
    done: Completion,
) -> EngineResult {
    let profile = profile7(webview).ok_or("extension APIs unavailable in this runtime")?;
    let folders: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    let tally = Rc::new(Tally {
        left: Cell::new(folders.len().max(1)),
        error: RefCell::new(None),
        done: RefCell::new(Some(done)),
    });
    if folders.is_empty() {
        tally.finish(Ok(()));
        return Ok(());
    }
    let lookup = profile.clone();
    let listed_tally = tally.clone();
    let handler = ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |hr, list| {
        let tally = listed_tally;
        let installed = match (hr, list) {
            (Ok(()), Some(list)) => installed(&list),
            (Err(e), _) => {
                for _ in &folders {
                    tally.finish(Err(format!("couldn't list the installed extensions: {e}")));
                }
                return Ok(());
            }
            (Ok(()), None) => Vec::new(),
        };
        for folder in folders {
            let id = unpacked_id(&folder);
            match installed.iter().find(|e| e.id == id) {
                Some(ext) if !refresh => {
                    remove_stale_copies(&installed, &ext.id, &ext.name);
                    if ext.enabled == enabled {
                        tally.finish(Ok(()));
                    } else {
                        set_enabled_reporting(&ext.handle, &ext.name, enabled, tally.clone());
                    }
                }
                // Nothing to switch off, and no reason to install it just to disable it.
                None if !enabled => tally.finish(Ok(())),
                _ => add_and_set(&lookup, &folder, &installed, enabled, tally.clone()),
            }
        }
        Ok(())
    }));
    unsafe { profile.GetBrowserExtensions(&handler) }.map_err(|e| e.to_string())
}

/// An installed extension as `GetBrowserExtensions` reports it.
struct Installed {
    id: String,
    name: String,
    enabled: bool,
    handle: ICoreWebView2BrowserExtension,
}

fn installed(list: &ICoreWebView2BrowserExtensionList) -> Vec<Installed> {
    let mut out = Vec::new();
    let mut count = 0u32;
    let _ = unsafe { list.Count(&mut count) };
    for i in 0..count {
        let Ok(handle) = (unsafe { list.GetValueAtIndex(i) }) else {
            continue;
        };
        let mut on = BOOL::default();
        out.push(Installed {
            id: pwstr_of(|p| unsafe { handle.Id(p) }),
            name: pwstr_of(|p| unsafe { handle.Name(p) }),
            enabled: unsafe { handle.IsEnabled(&mut on) }.is_ok() && on.as_bool(),
            handle,
        });
    }
    out
}

/// Chromium's ID for an unpacked extension loaded from `folder` (`GenerateIdForPath`):
/// the first 16 bytes of the SHA-256 of the path's UTF-16 bytes, drive letter uppercased,
/// written as hex digits mapped onto `a`–`p`.
fn unpacked_id(folder: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = folder.as_os_str().encode_wide().collect();
    if wide.get(1) == Some(&(b':' as u16)) {
        if let Some(drive) = char::from_u32(u32::from(wide[0])) {
            wide[0] = drive.to_ascii_uppercase() as u16;
        }
    }
    let bytes: Vec<u8> = wide.iter().flat_map(|w| w.to_le_bytes()).collect();
    Sha256::digest(&bytes)[..16]
        .iter()
        .flat_map(|b| [b >> 4, b & 0xf])
        .map(|n| char::from(b'a' + n))
        .collect()
}

/// `Enable(enabled)` on an installed extension, reporting the outcome to `tally`.
fn set_enabled_reporting(
    ext: &ICoreWebView2BrowserExtension,
    name: &str,
    enabled: bool,
    tally: Rc<Tally>,
) {
    let verb = if enabled { "enable" } else { "disable" };
    let what = format!("{name}: couldn't {verb} it");
    let failed = what.clone();
    let report = tally.clone();
    let after = BrowserExtensionEnableCompletedHandler::create(Box::new(move |hr| {
        report.finish(hr.map_err(|e| format!("{what}: {e}")));
        Ok(())
    }));
    if let Err(e) = unsafe { ext.Enable(enabled, &after) } {
        tally.finish(Err(format!("{failed}: {e}")));
    }
}

/// Install (or refresh) the unpacked extension in `folder`, drop stale copies of it, and
/// set it to `enabled`. The state is set unconditionally: the object `Add` hands back can't
/// be trusted to say whether the profile already holds it enabled.
fn add_and_set(
    profile: &ICoreWebView2Profile7,
    folder: &Path,
    installed: &[Installed],
    enabled: bool,
    tally: Rc<Tally>,
) {
    let label = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let others: Vec<(String, String, ICoreWebView2BrowserExtension)> = installed
        .iter()
        .map(|e| (e.id.clone(), e.name.clone(), e.handle.clone()))
        .collect();
    let added_tally = tally.clone();
    let handler = ProfileAddBrowserExtensionCompletedHandler::create(Box::new(move |hr, added| {
        let tally = added_tally;
        let added = match (hr, added) {
            (Ok(()), Some(added)) => added,
            (Err(e), _) => {
                tally.finish(Err(format!("{label}: couldn't load it: {e}")));
                return Ok(());
            }
            (Ok(()), None) => {
                tally.finish(Err(format!("{label}: WebView2 didn't report it loaded")));
                return Ok(());
            }
        };
        let id = pwstr_of(|p| unsafe { added.Id(p) });
        let name = pwstr_of(|p| unsafe { added.Name(p) });
        for (other_id, other_name, handle) in &others {
            if *other_id != id && *other_name == name {
                remove(handle);
            }
        }
        set_enabled_reporting(&added, &name, enabled, tally);
        Ok(())
    }));
    let path = windows_core::HSTRING::from(folder.as_os_str());
    if let Err(e) = unsafe { profile.AddBrowserExtension(&path, &handler) } {
        tally.finish(Err(format!("{}: couldn't load it: {e}", folder.display())));
    }
}

/// Counts down the extensions [`sync_bundled`] is waiting on and reports once, with the
/// first failure if there was one. Completion handlers run on the UI thread, so `Rc`.
struct Tally {
    left: Cell<usize>,
    error: RefCell<Option<String>>,
    done: RefCell<Option<Completion>>,
}

impl Tally {
    fn finish(&self, result: EngineResult) {
        if let Err(e) = result {
            self.error.borrow_mut().get_or_insert(e);
        }
        let left = self.left.get().saturating_sub(1);
        self.left.set(left);
        if left == 0 {
            if let Some(done) = self.done.borrow_mut().take() {
                done(self.error.take().map_or(Ok(()), Err));
            }
        }
    }
}

/// Remove every installed extension named `name` other than `keep`. An unpacked extension
/// without a manifest `key` gets its ID from its folder path, so when the bundle moves
/// (a new unpack location), the profile keeps the old copy too and both would run.
fn remove_stale_copies(installed: &[Installed], keep: &str, name: &str) {
    if keep.is_empty() || name.is_empty() {
        return;
    }
    for ext in installed {
        if ext.id != keep && ext.name == name {
            remove(&ext.handle);
        }
    }
}

fn remove(ext: &ICoreWebView2BrowserExtension) {
    let done = BrowserExtensionRemoveCompletedHandler::create(Box::new(|_hr| Ok(())));
    let _ = unsafe { ext.Remove(&done) };
}

/// Shared body of the enable/disable calls: fetch the list, then `Enable(enabled)` every
/// extension the `want` predicate accepts. All best-effort.
fn apply(
    webview: &WebView,
    want: impl Fn(&ICoreWebView2BrowserExtension, &str) -> bool + 'static,
    enabled: bool,
) -> EngineResult {
    let profile = profile7(webview).ok_or("extension APIs unavailable in this runtime")?;
    let handler =
        ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |_hr, list| {
            if let Some(list) = list.as_ref() {
                let mut count = 0u32;
                let _ = unsafe { list.Count(&mut count) };
                for i in 0..count {
                    let Ok(ext) = (unsafe { list.GetValueAtIndex(i) }) else {
                        continue;
                    };
                    let id = pwstr_of(|p| unsafe { ext.Id(p) });
                    if want(&ext, &id) {
                        let done =
                            BrowserExtensionEnableCompletedHandler::create(Box::new(|_hr| Ok(())));
                        let _ = unsafe { ext.Enable(enabled, &done) };
                    }
                }
            }
            Ok(())
        }));
    unsafe { profile.GetBrowserExtensions(&handler) }.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IDs read from real profiles: Chromium derives them from these folder paths.
    #[test]
    fn unpacked_ids_match_chromium() {
        for (path, id) in [
            (
                r"D:\projects\browser\target\debug\bundled\extensions\uBOLite.chromium",
                "kbfenggpbkpiffncmnjjndieopcbmnhk",
            ),
            (
                r"d:\projects\browser\crates\desktop\extensions\uBOLite.chromium",
                "llohiebfplbpnlfdkhaccilbofngmmjm",
            ),
        ] {
            assert_eq!(unpacked_id(Path::new(path)), id, "{path}");
        }
    }
}
