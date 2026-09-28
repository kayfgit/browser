//! The only desktop module allowed to own Wry/WebView2 objects.
//! Page callbacks carry the identity of the view that emitted them. Storage mode
//! is checked against the runtime before loading the requested location.

mod data;
mod extensions;
mod favicon;
mod navigation;
mod suspension;

use crate::tabs::{
    deproxy_translate, download_name, is_risky_download, is_translate_proxy, origin_of,
    ublock_extensions_dir, url_is_ad_host, PageState,
};
use crate::{
    AdblockMode, UserEvent, ADBLOCK_JS, BRIDGE_JS, BROWSER_ARGS, CARET_JS, FEATURES_JS, FIND_JS,
    IPC_PRELUDE,
};
use anyhow::Result;
use browser_engine::{
    BrowsingData, Completion, DataKind, EngineResult, EngineView, ExtensionInfo, Extensions,
    History, RectPx, Source, Suspension,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tao::{event_loop::EventLoopProxy, window::Window};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::{
    NewWindowResponse, PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows,
};

pub(crate) struct BuildOptions<'a> {
    pub source: Source,
    pub disable_js: bool,
    pub extra_init: &'a str,
    pub storage: browser_engine::StorageMode,
    pub bounds: RectPx,
    pub adblock: bool,
    pub adblock_mode: AdblockMode,
    pub mute: bool,
    pub no_css: bool,
    pub no_video: bool,
    pub no_scrollbar: bool,
    pub proxy: EventLoopProxy<UserEvent>,
    pub adblock_on: Arc<AtomicBool>,
    pub blocker: crate::blocklist::SharedBlocker,
    pub allow_risky_downloads: Arc<AtomicBool>,
}

/// The WebView2 user-data folder (cookies, cache, extensions) every view shares.
///
/// `BROWSER_WEBVIEW2_DATA_DIR` overrides it. Debug builds keep WebView2's default, a
/// folder next to the executable, so development never touches the real profile.
/// Release builds use `%LOCALAPPDATA%\browser\data\WebView2`: the default would put
/// it beside `browser.exe`, which an installer may place in a read-only folder.
///
/// Before that location existed, the profile lived next to the executable. The first
/// run moves such a folder over so users stay signed in, or, if it can't be moved
/// (another copy of the browser is using it), keeps using it where it is.
pub(crate) fn data_dir() -> Option<std::path::PathBuf> {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Some(dir) = std::env::var_os("BROWSER_WEBVIEW2_DATA_DIR") {
            return Some(dir.into());
        }
        if cfg!(debug_assertions) {
            return None;
        }
        let target = crate::session::local_data_dir()?.join("WebView2");
        if target.exists() {
            return Some(target);
        }
        for legacy in legacy_data_dirs() {
            if !legacy.is_dir() {
                continue;
            }
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            return Some(match std::fs::rename(&legacy, &target) {
                Ok(()) => target,
                Err(_) => legacy,
            });
        }
        Some(target)
    })
    .clone()
}

/// Where older builds kept the WebView2 profile: WebView2's default beside the running
/// executable, and beside the `browser.exe` the old `install.ps1` put in
/// `%LOCALAPPDATA%\Programs\browser`.
fn legacy_data_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(name) = exe.file_name() {
            let mut folder = name.to_os_string();
            folder.push(".WebView2");
            dirs.push(exe.with_file_name(folder));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(
            std::path::PathBuf::from(local)
                .join("Programs")
                .join("browser")
                .join("browser.exe.WebView2"),
        );
    }
    dirs
}

pub(crate) fn build(
    parent: &Window,
    opts: BuildOptions<'_>,
    identity: browser_engine::ViewIdentity,
) -> Result<(Box<dyn EngineView>, PageState)> {
    let BuildOptions {
        source,
        disable_js,
        extra_init,
        ..
    } = opts;
    let private = identity.storage == browser_engine::StorageMode::Private;
    let proxy = super::PageEventProxy::new(opts.proxy.clone(), identity.id);
    let nav_intent: crate::navguard::NavIntent = Arc::new(Mutex::new(None));
    // Shared with the page-load handler below (and, once built, the favicon
    // watcher) so the tab strip can show this tab's progress and icon. Seeded as
    // loading: the webview is about to fetch its first page, and WebView2's own
    // `Started` event only fires once the response is already coming back.
    let page = PageState::default();
    page.begin_load();
    let load_state = page.clone();
    let ipc_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    let nav_proxy = proxy.clone();
    // Shared with the App so `:ads` toggles the native redirect guard live.
    let adblock_on = opts.adblock_on.clone();
    let popup_adblock = opts.adblock_on.clone();
    let popup_proxy = proxy.clone();
    // The popup guard consults the same trusted-gesture stamp and blocklist engine the
    // navigation guards do, to tell a real "open in new tab" from a scripted popunder.
    let popup_intent = nav_intent.clone();
    let popup_blocker = opts.blocker.clone();
    // This tab's current top origin, so the blocklist's `$third-party` rules resolve
    // against the right source on each navigation. `cur_top` is the full top-frame URL.
    let cur_origin = Arc::new(Mutex::new(String::new()));
    let nav_origin = cur_origin.clone();
    let cur_top = Arc::new(Mutex::new(String::new()));
    let nav_top = cur_top.clone();
    // Page-reported "a TRUSTED gesture landed on a real cross-site link/submit" — the
    // one signal that a cross-site top navigation is genuinely wanted. Stamped here,
    // directly (no event-loop hop), so it lands before the navigation it authorises
    // reaches the native guard. Shared (cloned `Arc`) with that guard.
    let intent_set = nav_intent.clone();
    // The uBlock-style domain blocklist engine — the race-free primary redirect guard.
    let blocker = opts.blocker.clone();
    // Download guard: block executable/installer types unless the user opted in.
    let dl_allow = opts.allow_risky_downloads.clone();
    let dl_proxy = proxy.clone();
    // Wry falls back to a regular controller on runtimes without Environment10.
    // Build blank, verify the real storage mode, and only then load user content.
    let mut isolated_context = data_dir().map(|dir| wry::WebContext::new(Some(dir)));
    let mut builder = if let Some(context) = isolated_context.as_mut() {
        WebViewBuilder::new_with_web_context(context)
    } else {
        WebViewBuilder::new()
    };
    // Load uBlock Origin (any unpacked extension in the dir) into WebView2's own
    // Chromium engine. The extension does network + cosmetic + scriptlet ad-blocking
    // natively — far more capable than a hand-rolled blocker, and it doesn't depend on
    // the WebResourceRequested path. Two distinct knobs here:
    //   * `with_browser_extensions_enabled` is an ENVIRONMENT option, and WebView2
    //     requires every webview sharing the user-data folder to be created with the
    //     same options (like BROWSER_ARGS) — so it's set the same way in every mode.
    //   * `with_extensions_path` makes wry call `AddBrowserExtension` on the shared
    //     profile, and (re-)adding RESETS the extension to ENABLED. Doing that on
    //     every build is what kept uBlock alive in `native`/`off` mode: the
    //     post-build `set_all_enabled(false)` below is async, so the tab's first
    //     page had already loaded with uBlock's content scripts injected — and those
    //     hooks survive the late disable for the page's whole lifetime (the
    //     "YouTube shorts hang with adblock off" bug). Only (re-)add while blocking is
    //     ON, where enabled is the desired state; with it off, leave the profile's
    //     persisted copy alone (swept disabled below). Absent dir → no extensions.
    if let Some(ext_dir) = ublock_extensions_dir() {
        builder = builder.with_browser_extensions_enabled(true);
        if opts.adblock_mode.extension() && !private {
            builder = builder.with_extensions_path(ext_dir);
        }
    }
    builder = builder
        .with_bounds(native_rect(opts.bounds))
        .with_focused(false)
        .with_visible(false)
        // Private (`-n`): WebView2's InPrivate profile — cookies/storage live only
        // as long as the tab. A controller option, so it can differ per webview.
        .with_incognito(private)
        // Disable Chromium's built-in accelerators (Shift+Esc task manager,
        // Ctrl+F/P, F12, …) so our own keybindings own the keyboard. Standard
        // editing keys (Ctrl+C/V/X) are unaffected.
        .with_browser_accelerator_keys(false)
        // Browser process flags — see BROWSER_ARGS. MUST match every other
        // webview (terminal included) or WebView2 creation fails with 0x8007139F.
        .with_additional_browser_args(BROWSER_ARGS)
        // The page-side blocker (cosmetic hiding + popunder/redirect-intent) is
        // injected into EVERY frame (for_main_only = false), not just the top
        // document: scummy sites drive popunders from cross-origin player/ad iframes,
        // which a main-frame-only injection would leave unguarded. (Network blocking
        // of the ad scripts themselves is uBO Lite's.) It starts in the shell's current
        // state (baked as `__adblockDefault`), which every document load then corrects
        // to the LIVE state (see `UserEvent::SyncAdblock`); `:ads` flips it in the top
        // frame via `__setAdblock` (sub-frames adopt it on reload).
        // `window.ipc` is absent in sub-frames, so the blocker's status reports are
        // main-frame-only (guarded), but the neutering itself is universal.
        .with_initialization_script_for_main_only(
            {
                let ab = opts.adblock;
                format!("{IPC_PRELUDE}\nwindow.__adblockDefault={ab};\n{ADBLOCK_JS}")
            },
            false,
        )
        // The shell bridge (keybindings, focus reclaim, hint mode) and the page-
        // feature toggles stay MAIN-FRAME-ONLY — focus/IPC plumbing must not run
        // per iframe. They start in the shell's current state too, so a tab opened
        // while a toggle is active is already in that state. `extra_init` (e.g.
        // research-mode DOM pruning) is appended last.
        .with_initialization_script({
            let (m, c, v, sb) = (opts.mute, opts.no_css, opts.no_video, opts.no_scrollbar);
            let mut init = format!(
                "{IPC_PRELUDE}\n{BRIDGE_JS}\n{FIND_JS}\n{CARET_JS}\n\
                 window.__featureDefaults={{mute:{m},css:{c},video:{v},scrollbar:{sb}}};\n{FEATURES_JS}"
            );
            if !extra_init.is_empty() {
                init.push('\n');
                init.push_str(extra_init);
            }
            // TEMPORARY diagnostic probe (BROWSER_YT_DEBUG=1): logs YouTube SPA
            // navigation events, JS errors and youtubei fetch completions to
            // %TEMP%\ytprobe.log, and auto-clicks the first shorts thumbnail.
            if std::env::var("BROWSER_YT_DEBUG").is_ok() {
                init.push('\n');
                init.push_str(YT_PROBE_JS);
            }
            init
        })
        .with_ipc_handler(move |req| match req.body().as_str() {
            // A trusted click/keypress on a real cross-site link or submit control:
            // authorise the cross-site top navigation it's about to trigger. Stamped
            // directly so it beats that navigation to the native guard.
            "nav-intent" => {
                if let Ok(mut g) = intent_set.lock() {
                    *g = Some(std::time::Instant::now());
                }
            }
            "leave-passthrough" => {
                let _ = ipc_proxy.send_event(UserEvent::ExitToNormal);
            }
            // Insert (light field typing): Esc / focus left the field → back to Normal.
            "insert-escape" | "insert-blur" => {
                let _ = ipc_proxy.send_event(UserEvent::ExitToNormal);
            }
            "page-ready" => {
                let _ = ipc_proxy.send_event(UserEvent::FocusShell);
            }
            // SPA URL changes (pushState/popstate/hashchange) — no document load
            // fires, so this is the only signal the shell gets. 'url-changed'
            // records a back/forward step; 'url-replaced' (replaceState) only
            // syncs the shown URL.
            "url-changed" => {
                let _ = ipc_proxy.send_event(UserEvent::UrlChanged { record: true });
            }
            "url-replaced" => {
                let _ = ipc_proxy.send_event(UserEvent::UrlChanged { record: false });
            }
            "grab-focus" => {
                let _ = ipc_proxy.send_event(UserEvent::GrabFocus);
            }
            "page-hold" => {
                let _ = ipc_proxy.send_event(UserEvent::PageHold);
            }
            "page-edit" => {
                let _ = ipc_proxy.send_event(UserEvent::PageEdit);
            }
            "pane-click" => {
                let _ = ipc_proxy.send_event(UserEvent::PaneClick);
            }
            "hint-exit" => {
                let _ = ipc_proxy.send_event(UserEvent::ExitHint);
            }
            "hint-edit" => {
                let _ = ipc_proxy.send_event(UserEvent::HintEdit);
            }
            "scroll-selected" => {
                let _ = ipc_proxy.send_event(UserEvent::ScrollSelected);
            }
            "scroll-exit" => {
                let _ = ipc_proxy.send_event(UserEvent::ScrollExit);
            }
            "caret-exit" => {
                let _ = ipc_proxy.send_event(UserEvent::CaretExit);
            }
            "fs-enter" => {
                let _ = ipc_proxy.send_event(UserEvent::PageFullscreen(true));
            }
            "fs-exit" => {
                let _ = ipc_proxy.send_event(UserEvent::PageFullscreen(false));
            }
            body => {
                // Web caret-mode yanked a selection: `caret-yank:<text>`.
                if let Some(text) = body.strip_prefix("caret-yank:") {
                    let _ = ipc_proxy.send_event(UserEvent::CaretYank(text.to_string()));
                // A right-click menu item copied something: `clip:<text>` (the
                // selection, a link address, an image address).
                } else if let Some(text) = body.strip_prefix("clip:") {
                    let _ = ipc_proxy.send_event(UserEvent::ClipCopy(text.to_string()));
                // A hint in new-tab mode resolved to a link: `hint-open:<href>`.
                } else if let Some(href) = body.strip_prefix("hint-open:") {
                    let _ = ipc_proxy.send_event(UserEvent::HintOpen(href.to_string()));
                // A hint in copy mode (`yf`) resolved to a link: `hint-copy:<href>`.
                } else if let Some(href) = body.strip_prefix("hint-copy:") {
                    let _ = ipc_proxy.send_event(UserEvent::HintCopy(href.to_string()));
                // The page blocker neutered a scripted pop-up. `popup-blocked:<url>`.
                } else if let Some(url) = body.strip_prefix("popup-blocked:") {
                    let _ = ipc_proxy.send_event(UserEvent::PopupBlocked(url.to_string()));
                // The pointer moved onto/off a link: `link-hover:<href>` (empty = off).
                } else if let Some(href) = body.strip_prefix("link-hover:") {
                    let _ = ipc_proxy.send_event(UserEvent::LinkHover(href.to_string()));
                // TEMPORARY: YT_PROBE_JS diagnostics (BROWSER_YT_DEBUG=1) — append to
                // %TEMP%\ytprobe.log. Remove with the probe when the bug is solved.
                } else if let Some(m) = body.strip_prefix("dbg:") {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(std::env::temp_dir().join("ytprobe.log"))
                    {
                        let _ = writeln!(f, "{m}");
                    }
                }
            }
        })
        // Backup focus reclaim for non-JS tabs (page-ready covers JS tabs), and the
        // tab strip's load progress: `Started` is WebView2's ContentLoading (a
        // navigation committed and content is arriving — this catches link clicks and
        // in-page navigations, which the shell never sees), `Finished` is
        // NavigationCompleted, which fires whether the page loaded or failed.
        .with_on_page_load_handler(move |event, _url| match event {
            PageLoadEvent::Started => {
                load_state.begin_load();
                // The new document seeded its blocker flag from the value baked in at
                // webview-build time; correct it to the shell's live state. See
                // [`UserEvent::SyncAdblock`].
                let _ = load_proxy.send_event(UserEvent::SyncAdblock);
            }
            PageLoadEvent::Finished => {
                load_state.end_load();
                let _ = load_proxy.send_event(UserEvent::FocusShell);
            }
        })
        // Kill auto-translate: a foreign-language site (or a saved/clicked link)
        // can land us on Google's `*.translate.goog` proxy, which mangles the URL
        // and rewrites the page. Cancel any such navigation and load the original
        // (de-proxied) URL instead, so we always show the real page.
        .with_navigation_handler(move |url| {
            if is_translate_proxy(&url) {
                if let Some(original) = deproxy_translate(&url) {
                    let _ = nav_proxy.send_event(UserEvent::Navigate(original));
                    return false;
                }
            }
            // Cancel any top-level navigation to a known ad/redirect/malware DOMAIN,
            // however it was triggered (scripted redirect, `<meta refresh>`, server
            // 3xx). The EasyList-style engine blocks by name like uBlock; `url_is_ad_host`
            // is the tiny always-on fallback for the beat before the engine compiles.
            // Forced redirects to UNLISTED domains (the cross-site hijack vector) are
            // caught separately by the native intent-gate guard (see `navguard`).
            // Honours `:ads` live.
            if adblock_on.load(Ordering::Relaxed) {
                let src = nav_origin.lock().unwrap().clone();
                if crate::blocklist::blocks_navigation(&blocker, &url, &src)
                    || url_is_ad_host(&url)
                {
                    let _ = nav_proxy.send_event(UserEvent::RedirectBlocked(url));
                    return false;
                }
            }
            // Remember the current top origin so the blocklist's `$third-party` rules
            // resolve against the right source on the next navigation, and the full
            // top URL so the sub-resource blocker can tell the main document apart
            // from sub-frames.
            let origin = origin_of(&url);
            if !origin.is_empty() {
                *nav_origin.lock().unwrap() = origin;
                *nav_top.lock().unwrap() = url.clone();
            }
            true
        })
        // Popunder / forced-popup guard, uBlock-Origin-style. Scummy sites spawn scam
        // tabs via `window.open` (often an `about:blank` shell they then navigate) or
        // `target=_blank` on ANY click; this native handler fires for every frame and
        // every variant. Rather than deny EVERY new window (which also killed real
        // "open in new tab" clicks — e.g. Google results), we decide like uBO does:
        // a new window is legitimate only when a TRUSTED user gesture just landed on a
        // link (the same `nav-intent` stamp the redirect guard trusts) AND the
        // destination isn't a known ad/scam domain. Real clicks carry that gesture, so
        // they now re-open as a shell-managed tab; scripted popunders (timers/onload,
        // or synthetic clicks aimed at an ad domain) carry no gesture or hit the
        // blocklist, so they stay blocked. Either way the native OS window is suppressed
        // — new tabs here live in our tab strip, not as OS popups. `:ads` off restores
        // normal popups. (The page-side `window.open` neuter is a further backstop.)
        .with_new_window_req_handler(move |url, _features| {
            if !popup_adblock.load(Ordering::Relaxed) {
                return NewWindowResponse::Allow;
            }
            let wanted = crate::navguard::recent(&popup_intent)
                && !crate::blocklist::blocks_navigation(&popup_blocker, &url, "")
                && !url_is_ad_host(&url);
            let _ = popup_proxy.send_event(if wanted {
                UserEvent::OpenPopupTab(url)
            } else {
                UserEvent::PopupBlocked(url)
            });
            NewWindowResponse::Deny
        })
        // Drive-by install guard. A scam page (or a redirect we missed) can kick off
        // a download of an `.exe`/`.msi`/etc. that a careless click would run. Block
        // executable/installer types by default and warn loudly; everything else
        // (zip, pdf, images, media …) downloads normally. `:downloads` opts in.
        .with_download_started_handler(move |url, path| {
            if dl_allow.load(Ordering::Relaxed) || !is_risky_download(&url, path) {
                return true;
            }
            let name = download_name(&url, path);
            let _ = dl_proxy.send_event(UserEvent::DownloadBlocked(name));
            false
        });
    if disable_js {
        builder = builder.with_javascript_disabled();
    }
    let webview = builder
        .build_as_child(parent)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    verify_storage(&webview, identity.storage)?;
    if private && opts.adblock_mode.extension() {
        if let Some(dir) = ublock_extensions_dir() {
            let _ = extensions::install_dir(&webview, &dir);
        }
    }
    // Once per run, drop stale copies of the bundled extensions left in the profile
    // by an older install location (see `install_dir_replacing`).
    static DEDUPED: AtomicBool = AtomicBool::new(false);
    if !private && opts.adblock_mode.extension() && !DEDUPED.swap(true, Ordering::Relaxed) {
        if let Some(dir) = ublock_extensions_dir() {
            let _ = extensions::install_dir_replacing(&webview, &dir);
        }
    }
    // The native redirect guard: cancels forced (non-user-initiated) cross-site top
    // navigations via WebView2's own `IsUserInitiated` — the structural fix the
    // URL-only wry handler above can't be. Best-effort; the wry guards still stand.
    navigation::install(
        &webview,
        opts.adblock_on.clone(),
        nav_intent.clone(),
        proxy.clone(),
    );
    // NOTE: there is deliberately no `WebResourceRequested` sub-resource blocker here.
    // One used to run the full EasyList engine over every script/iframe/XHR, but
    // registering that filter routes every sub-resource through a handler on the HOST's
    // UI thread, and that alone — even a handler that blocks nothing — stalls the
    // initial parse of streaming pages: a fresh YouTube page hangs at
    // `readyState==loading` and renders only its skeleton until you reload (which masks
    // it by serving the doc from cache). It also duplicated uBlock Origin Lite, which
    // does the same job declaratively inside Chromium's network stack at no cost to us.
    // Network blocking is uBO Lite's; keep it that way.
    //
    // Outside `Ubo` the profile's PERSISTED extension copy can still be enabled — left
    // by an old session, or a crash before a disable landed. Sweep it off so the
    // persisted state converges. This is ASYNC, so a stale-enabled uBO Lite still
    // filters this webview's very FIRST load — which is enough to hang a YouTube watch
    // page (measured; see `AdblockMode`). It settles from the second load on.
    #[cfg(windows)]
    if !opts.adblock_mode.extension() {
        let _ = extensions::set_all_enabled(&webview, false);
    }
    // Watch WebView2's own favicon for this tab, so the strip can show it.
    #[cfg(windows)]
    favicon::install(&webview, page.icon.clone(), proxy.clone());
    match source {
        Source::Url(url) => webview.load_url(&url)?,
        Source::Html(html) => webview.load_html(&html)?,
    }
    Ok((
        Box::new(WebView2View {
            inner: webview,
            identity,
            nav_intent,
            _context: isolated_context,
        }),
        page,
    ))
}

fn verify_storage(view: &WebView, expected: browser_engine::StorageMode) -> Result<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_13;
    use windows_core::{Interface, BOOL};
    use wry::WebViewExtWindows;
    // Old runtimes without the profile API can serve regular pages, but cannot
    // provide the evidence needed to honor a private request.
    let profile = unsafe { view.controller().CoreWebView2() }
        .and_then(|core| core.cast::<ICoreWebView2_13>())
        .and_then(|core| unsafe { core.Profile() });
    match profile {
        Ok(profile) => {
            let mut private = BOOL::default();
            unsafe { profile.IsInPrivateModeEnabled(&mut private) }?;
            anyhow::ensure!(
                private.as_bool() == (expected == browser_engine::StorageMode::Private),
                "WebView2 did not honor the requested storage mode; page was not loaded"
            );
        }
        Err(_) if expected == browser_engine::StorageMode::Private => {
            anyhow::bail!("this WebView2 runtime cannot verify private mode; page was not loaded");
        }
        Err(_) => {}
    }
    Ok(())
}

/// TEMPORARY diagnostic probe for the YouTube half-load bug — injected only when
/// `BROWSER_YT_DEBUG=1` (see the init-script assembly below). Remove when solved.
const YT_PROBE_JS: &str = r#"
(function () {
  if (location.hostname.indexOf('youtube.com') === -1) return;
  var isTop = false; try { isTop = (window.top === window); } catch (e) {}
  if (!isTop) return;
  function log(m) { try { window.__post('dbg:' + Date.now() + ' ' + m); } catch (e) {} }
  // Did ADBLOCK_JS's window-keyed guard already fire on THIS window? If the flag is
  // set before any of its per-document work could have run here, the guard swallowed
  // the whole script for this document.
  // Decisive test of whether ADBLOCK_JS's per-document work ran on THIS document: its
  // `isYT` block only installs the `ytInitialPlayerResponse` accessor when
  // `location.hostname` is youtube.com — never on the initial about:blank. So a plain
  // data property here means the window-keyed `__adblockInit` guard swallowed the
  // script for the real page.
  function abState() {
    try {
      var d = Object.getOwnPropertyDescriptor(window, 'ytInitialPlayerResponse');
      return d ? (d.get ? 'accessor' : 'data') : 'none';
    } catch (e) { return 'err'; }
  }
  log('INIT ' + location.href + ' adblockInit=' + !!window.__adblockInit +
      ' iprProp=' + abState() +
      ' navtype=' + (function () {
        try { return performance.getEntriesByType('navigation')[0].type; } catch (e) { return '?'; }
      })());
  window.addEventListener('error', function (e) {
    log('JSERR ' + e.message + ' @ ' + (e.filename || '?') + ':' + (e.lineno || 0));
  }, true);
  window.addEventListener('unhandledrejection', function (e) {
    var r = ''; try { r = e.reason && (e.reason.stack || e.reason.message || String(e.reason)); } catch (_) {}
    log('REJECT ' + String(r).slice(0, 300));
  });
  // Who is tearing the document down / driving us somewhere else?
  window.addEventListener('beforeunload', function () {
    log('BEFOREUNLOAD from=' + location.href + '\n  stack=' + new Error().stack);
  }, true);
  ['yt-navigate-start', 'yt-navigate-finish', 'yt-navigate-error', 'yt-player-error']
    .forEach(function (ev) {
      document.addEventListener(ev, function () {
        log('EVT ' + ev + ' url=' + location.href + ' rs=' + document.readyState);
      });
    });
  try {
    var _reload = location.reload.bind(location);
    location.reload = function () { log('RELOAD() ' + new Error().stack); return _reload.apply(null, arguments); };
    var _assign = location.assign.bind(location);
    location.assign = function (u) { log('ASSIGN ' + u + '\n  ' + new Error().stack); return _assign.apply(null, arguments); };
  } catch (e) {}
  var _ps = history.pushState.bind(history);
  history.pushState = function () { log('PUSHSTATE ' + (arguments[2] || '')); return _ps.apply(null, arguments); };
  // The ad-skip hazard: ADBLOCK_JS seeks `video.html5-main-video` to its duration
  // whenever the player carries `ad-showing`. If that class is ever seen while the
  // MAIN video is loaded, the seek ends the real video — which on a playlist advances
  // to the next one. Log every ad-showing transition and every ended/seek so a loop
  // leaves a trace.
  var wasAd = null, seen = null;
  setInterval(function () {
    try {
      var p = document.querySelector('.html5-video-player');
      var v = document.querySelector('video.html5-main-video');
      if (!p) return;
      var ad = p.classList.contains('ad-showing');
      if (ad !== wasAd) {
        wasAd = ad;
        log('ADCLASS ' + (ad ? 'ON' : 'off') +
            ' interrupting=' + p.classList.contains('ad-interrupting') +
            ' dur=' + (v ? v.duration : '?') + ' t=' + (v ? v.currentTime.toFixed(1) : '?') +
            ' muted=' + (v ? v.muted : '?') +
            ' cls=[' + p.className + ']');
      }
      if (v && v !== seen) {
        seen = v;
        v.addEventListener('ended', function () {
          log('ENDED t=' + v.currentTime.toFixed(1) + '/' + v.duration +
              ' adclass=' + p.classList.contains('ad-showing') + ' muted=' + v.muted);
        });
      }
    } catch (e) {}
  }, 250);

  // The player's own view of the world, sampled while the loop runs.
  setInterval(function () {
    try {
      var v = document.querySelector('video.html5-main-video');
      var p = document.querySelector('.html5-video-player');
      var ipr = null;
      try { ipr = window.ytInitialPlayerResponse; } catch (e) {}
      log('STATE url=' + location.href.slice(0, 90) +
          ' rs=' + document.readyState +
          ' vid=' + (v ? ('t=' + v.currentTime.toFixed(1) + '/' + v.duration +
                          ' ready=' + v.readyState + ' paused=' + v.paused +
                          ' err=' + (v.error ? v.error.code : '-') +
                          ' src=' + (v.src || v.currentSrc || '').slice(0, 40))
                       : 'none') +
          ' cls=' + (p ? p.className.slice(0, 120) : 'noplayer') +
          ' play=' + (ipr && ipr.playabilityStatus ? ipr.playabilityStatus.status : '?') +
          ' streams=' + !!(ipr && ipr.streamingData) +
          ' enf=' + document.querySelectorAll('ytd-enforcement-message-view-model').length);
    } catch (e) { log('STATEX ' + e); }
  }, 2000);
})();
"#;

struct WebView2View {
    inner: WebView,
    _context: Option<wry::WebContext>,
    identity: browser_engine::ViewIdentity,
    nav_intent: crate::navguard::NavIntent,
}

fn native_rect(rect: RectPx) -> Rect {
    Rect {
        position: PhysicalPosition::new(rect.x, rect.y).into(),
        size: PhysicalSize::new(rect.w, rect.h).into(),
    }
}

pub(crate) fn keep_alive(parent: &Window) -> Result<Box<dyn EngineView>> {
    let mut isolated_context = data_dir().map(|dir| wry::WebContext::new(Some(dir)));
    let mut builder = if let Some(context) = isolated_context.as_mut() {
        WebViewBuilder::new_with_web_context(context)
    } else {
        WebViewBuilder::new()
    }
    .with_html("");
    // Match the content views' environment options, without reinstalling extensions.
    if ublock_extensions_dir().is_some() {
        builder = builder.with_browser_extensions_enabled(true);
    }
    let inner = builder
        .with_additional_browser_args(BROWSER_ARGS)
        .with_visible(false)
        .with_focused(false)
        .with_bounds(native_rect(RectPx {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        }))
        .build_as_child(parent)?;
    let _ = suspension::suspend(&inner);
    Ok(Box::new(WebView2View {
        inner,
        _context: isolated_context,
        identity: browser_engine::ViewIdentity {
            id: browser_engine::ViewId::allocate(),
            provider: "webview2".into(),
            storage: browser_engine::StorageMode::Persistent,
        },
        nav_intent: Arc::new(Mutex::new(None)),
    }))
}

impl EngineView for WebView2View {
    fn identity(&self) -> &browser_engine::ViewIdentity {
        &self.identity
    }
    fn authorize_navigation(&self) {
        crate::navguard::mark(&self.nav_intent);
    }
    fn load_url(&self, url: &str) -> EngineResult {
        self.authorize_navigation();
        self.inner.load_url(url).map_err(|e| e.to_string())
    }
    fn reload(&self) -> EngineResult {
        self.inner.reload().map_err(|e| e.to_string())
    }
    fn url(&self) -> EngineResult<String> {
        self.inner.url().map_err(|e| e.to_string())
    }
    fn set_bounds(&self, rect: RectPx) -> EngineResult {
        self.inner
            .set_bounds(native_rect(rect))
            .map_err(|e| e.to_string())
    }
    fn set_visible(&self, visible: bool) -> EngineResult {
        self.inner.set_visible(visible).map_err(|e| e.to_string())
    }
    fn zoom(&self, factor: f64) -> EngineResult {
        self.inner.zoom(factor).map_err(|e| e.to_string())
    }
    fn focus(&self) -> EngineResult {
        self.inner.focus().map_err(|e| e.to_string())
    }
    fn focus_parent(&self) -> EngineResult {
        self.inner.focus_parent().map_err(|e| e.to_string())
    }
    fn evaluate_script(&self, script: &str) -> EngineResult {
        self.inner
            .evaluate_script(script)
            .map_err(|e| e.to_string())
    }
    fn history(&self) -> Option<&dyn History> {
        Some(self)
    }
    fn extensions(&self) -> Option<&dyn Extensions> {
        Some(self)
    }
    fn browsing_data(&self) -> Option<&dyn BrowsingData> {
        Some(self)
    }
    fn suspension(&self) -> Option<&dyn Suspension> {
        Some(self)
    }
}

impl History for WebView2View {
    fn can_go(&self, forward: bool) -> bool {
        navigation::can_go(&self.inner, forward)
    }
    fn go(&self, forward: bool) -> EngineResult {
        self.authorize_navigation();
        navigation::go(&self.inner, forward)
    }
}
impl Extensions for WebView2View {
    fn list(&self, done: Completion<Vec<ExtensionInfo>>) -> EngineResult {
        extensions::list(&self.inner, done)
    }
    fn set_enabled(&self, id: String, enabled: bool) -> EngineResult {
        extensions::set_enabled(&self.inner, id, enabled)
    }
    fn set_all_enabled(&self, enabled: bool) -> EngineResult {
        extensions::set_all_enabled(&self.inner, enabled)
    }
    fn install_dir(&self, dir: &std::path::Path) -> EngineResult {
        extensions::install_dir(&self.inner, dir)
    }
}
impl BrowsingData for WebView2View {
    fn clear(&self, kind: DataKind, range: Option<(f64, f64)>, done: Completion) -> EngineResult {
        data::clear(&self.inner, kind, range, done)
    }
}
impl Suspension for WebView2View {
    fn suspend(&self) -> EngineResult {
        suspension::suspend(&self.inner)
    }
    fn resume(&self) -> EngineResult {
        suspension::resume(&self.inner)
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    #[test]
    #[ignore = "requires an installed WebView2 runtime; creates hidden isolated views"]
    fn runtime_storage_mode_is_verified_before_user_content() {
        use browser_engine::StorageMode;
        use tao::platform::windows::EventLoopBuilderExtWindows;
        let mut builder = tao::event_loop::EventLoopBuilder::<()>::new();
        builder.with_any_thread(true);
        let event_loop = builder.build();
        let window = tao::window::WindowBuilder::new()
            .with_visible(false)
            .build(&event_loop)
            .unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/engine-tests")
            .join(format!("{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut context = wry::WebContext::new(Some(dir));
        let regular = WebViewBuilder::new_with_web_context(&mut context)
            .with_focused(false)
            .with_visible(false)
            .build_as_child(&window)
            .unwrap();
        assert!(verify_storage(&regular, StorageMode::Persistent).is_ok());
        assert!(verify_storage(&regular, StorageMode::Private).is_err());
        let private = WebViewBuilder::new_with_web_context(&mut context)
            .with_incognito(true)
            .with_focused(false)
            .with_visible(false)
            .build_as_child(&window)
            .unwrap();
        assert!(verify_storage(&private, StorageMode::Private).is_ok());
        assert!(verify_storage(&private, StorageMode::Persistent).is_err());
        // Profiles are isolated under target/engine-tests; teardown is asynchronous.
        // Keep them available for inspecting failures instead of racing runtime cleanup.
    }
}
