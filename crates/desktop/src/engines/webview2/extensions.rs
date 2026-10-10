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

/// [`Extensions::sync_bundled`](browser_engine::Extensions::sync_bundled): add each
/// unpacked extension under `dir`, drop stale copies of it, and set it to `enabled`.
///
/// Adding is idempotent (WebView2 keeps one copy per folder) and hands back the installed
/// extension, which is how we learn its ID without touching anything else in the profile —
/// extensions the user installed keep whatever state they gave them.
///
/// The enable/disable step is explicit because adding no longer does it: on runtime 154 a
/// profile that once had the extension disabled keeps it disabled through every re-add.
/// Until 2026-10 that left uBO Lite silently OFF, because nothing else turned it back on
/// and every failure here was dropped. `done` now reports them.
pub(crate) fn sync_bundled(
    webview: &WebView,
    dir: &Path,
    enabled: bool,
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
        left: Cell::new(folders.len()),
        error: RefCell::new(None),
        done: RefCell::new(Some(done)),
    });
    if folders.is_empty() {
        tally.left.set(1);
        tally.finish(Ok(()));
    }
    let verb = if enabled { "enable" } else { "disable" };
    for folder in folders {
        let label = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (added_tally, lookup) = (tally.clone(), profile.clone());
        let handler =
            ProfileAddBrowserExtensionCompletedHandler::create(Box::new(move |hr, added| {
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
                remove_stale_copies(&lookup, id, name.clone());
                // Always set the state, never check it first: the object `Add` hands back
                // reports `IsEnabled == true` even while the profile holds the extension
                // disabled (measured on runtime 154), which is how a skip-if-already-set
                // check left it off.
                let enable_tally = tally.clone();
                let what = format!("{name}: couldn't {verb} it");
                let failed = what.clone();
                let after = BrowserExtensionEnableCompletedHandler::create(Box::new(move |hr| {
                    enable_tally.finish(hr.map_err(|e| format!("{what}: {e}")));
                    Ok(())
                }));
                if let Err(e) = unsafe { added.Enable(enabled, &after) } {
                    tally.finish(Err(format!("{failed}: {e}")));
                }
                Ok(())
            }));
        let path = windows_core::HSTRING::from(folder.as_os_str());
        if let Err(e) = unsafe { profile.AddBrowserExtension(&path, &handler) } {
            tally.finish(Err(format!("{}: couldn't load it: {e}", folder.display())));
        }
    }
    Ok(())
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
fn remove_stale_copies(profile: &ICoreWebView2Profile7, keep: String, name: String) {
    if keep.is_empty() || name.is_empty() {
        return;
    }
    let sweep = ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |_hr, list| {
        let Some(list) = list.as_ref() else {
            return Ok(());
        };
        let mut count = 0u32;
        let _ = unsafe { list.Count(&mut count) };
        for i in 0..count {
            let Ok(ext) = (unsafe { list.GetValueAtIndex(i) }) else {
                continue;
            };
            let id = pwstr_of(|p| unsafe { ext.Id(p) });
            let other = pwstr_of(|p| unsafe { ext.Name(p) });
            if id != keep && other == name {
                let done = BrowserExtensionRemoveCompletedHandler::create(Box::new(|_hr| Ok(())));
                let _ = unsafe { ext.Remove(&done) };
            }
        }
        Ok(())
    }));
    let _ = unsafe { profile.GetBrowserExtensions(&sweep) };
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
