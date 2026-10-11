//! The only desktop module allowed to own Wry/WebView2 objects.
//! Page callbacks carry the identity of the view that emitted them. Storage mode
//! is checked against the runtime before loading the requested location.

mod crashes;
mod data;
pub(crate) mod downloads;
mod extensions;
mod favicon;
mod input;
mod keys;
mod navigation;
pub(crate) mod permissions;
mod suspension;

use crate::tabs::{
    deproxy_translate, is_translate_proxy, origin_of, ublock_extensions_dir, url_is_ad_host,
    PageState,
};
use crate::{UserEvent, BRIDGE_JS, CARET_JS, FEATURES_JS, FIND_JS, IPC_PRELUDE, NAVGUARD_JS};
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

/// WebView2 browser-process arguments, applied to EVERY webview we build.
///
/// This MUST be identical across all webviews: WebView2 requires every
/// environment sharing a user-data folder to be created with the same options,
/// or the second creation fails with `ERROR_INVALID_STATE` (HRESULT 0x8007139F).
/// (That's why a `:te` terminal opened after a content tab used to error — the
/// terminal webview had no args while content tabs did.) Overrides wry's default
/// arg string, so we re-include its defaults (mini-menu / PDF UI / SmartScreen off,
/// plus gesture-free autoplay) and add `Translate,msAutoTranslate` to kill Edge's
/// "translate this page?" bar.
pub(crate) const BROWSER_ARGS: &str =
    "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,\
     Translate,msAutoTranslate --autoplay-policy=no-user-gesture-required";

pub(crate) struct BuildOptions<'a> {
    pub source: Source,
    pub disable_js: bool,
    pub extra_init: &'a str,
    pub storage: browser_engine::StorageMode,
    pub bounds: RectPx,
    pub adblock: bool,
    pub mute: bool,
    pub no_css: bool,
    pub no_video: bool,
    pub no_scrollbar: bool,
    pub proxy: EventLoopProxy<UserEvent>,
    pub adblock_on: Arc<AtomicBool>,
    pub blocker: crate::blocklist::SharedBlocker,
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
    // Wry falls back to a regular controller on runtimes without Environment10.
    // Build blank, verify the real storage mode, and only then load user content.
    let mut isolated_context = data_dir().map(|dir| wry::WebContext::new(Some(dir)));
    let mut builder = if let Some(context) = isolated_context.as_mut() {
        WebViewBuilder::new_with_web_context(context)
    } else {
        WebViewBuilder::new()
    };
    // Extension support is an ENVIRONMENT option, and WebView2 requires every webview
    // sharing the user-data folder to be created with the same options (like
    // BROWSER_ARGS) — so it's on for every webview, whatever the ad-blocking state. The
    // bundled ad blocker itself is installed and switched on/off after the build (see
    // `sync_bundled_blocker`), never through wry's builder-time loading.
    builder = builder.with_browser_extensions_enabled(true);
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
        // DevTools for `:inspect` (F12). With the accelerator keys off, the page can't
        // open them on its own; only the shell does.
        .with_devtools(true)
        // Browser process flags — see BROWSER_ARGS. MUST match every other
        // webview (terminal included) or WebView2 creation fails with 0x8007139F.
        .with_additional_browser_args(BROWSER_ARGS)
        // The redirect/popup guard (popunder neutering + redirect intent) is injected
        // into EVERY frame (for_main_only = false), not just the top document: scummy
        // sites drive popunders from cross-origin player/ad iframes, which a
        // main-frame-only injection would leave unguarded. (Blocking the ads
        // themselves is uBO Lite's.) It starts in the shell's current
        // state (baked as `__adblockDefault`), which every document load then corrects
        // to the LIVE state (see `UserEvent::SyncAdblock`); `:ads` flips it in the top
        // frame via `__setAdblock` (sub-frames adopt it on reload).
        // `window.ipc` is absent in sub-frames, so the blocker's status reports are
        // main-frame-only (guarded), but the neutering itself is universal.
        .with_initialization_script_for_main_only(
            {
                let ab = opts.adblock;
                format!("{IPC_PRELUDE}\nwindow.__adblockDefault={ab};\n{NAVGUARD_JS}")
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
            // TEMPORARY: YT_PROBE_JS diagnostics (BROWSER_YT_DEBUG=1) — append to
            // %TEMP%\ytprobe.log. Remove with the probe when the bug is solved.
            body if body.starts_with("dbg:") => {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(std::env::temp_dir().join("ytprobe.log"))
                {
                    let _ = writeln!(f, "{}", &body[4..]);
                }
            }
            // Everything else is the engine-neutral bridge protocol.
            body => {
                if let Some(event) = super::events::decode_page_message(body) {
                    let _ = ipc_proxy.send_event(event);
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
        });
    if disable_js {
        builder = builder.with_javascript_disabled();
    }
    let webview = builder
        .build_as_child(parent)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    verify_storage(&webview, identity.storage)?;
    // Bring the bundled ad blocker to the shell's state in this webview's profile. The
    // regular profile is shared by every tab, so once per run is enough (`:ads` re-syncs
    // all of them); each InPrivate webview has a fresh profile of its own.
    static SYNCED: AtomicBool = AtomicBool::new(false);
    if private || !SYNCED.swap(true, Ordering::Relaxed) {
        sync_bundled_blocker(&webview, opts.adblock, &opts.proxy);
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
    // Downloads ask first and report progress to the shell (see `downloads`).
    downloads::install(&webview, opts.proxy.clone());
    // Permission requests are answered from the bar, not Edge's unreachable bubble.
    permissions::install(&webview, opts.proxy.clone());
    // A dead page or engine process gets the crash page, not a black pane.
    crashes::install(&webview, proxy.clone());
    // The shell's leave/reclaim keys and the reset chord, inside iframes too.
    keys::install(&webview, proxy.clone());
    // NOTE: there is deliberately no `WebResourceRequested` sub-resource blocker here.
    // One used to run the full EasyList engine over every script/iframe/XHR, but
    // registering that filter routes every sub-resource through a handler on the HOST's
    // UI thread, and that alone — even a handler that blocks nothing — stalls the
    // initial parse of streaming pages: a fresh YouTube page hangs at
    // `readyState==loading` and renders only its skeleton until you reload (which masks
    // it by serving the doc from cache). It also duplicated uBlock Origin Lite, which
    // does the same job declaratively inside Chromium's network stack at no cost to us.
    // Network blocking is uBO Lite's; keep it that way.
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
    use windows_core061::{Interface, BOOL};
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
/// (`ADBLOCK_JS` no longer runs in WebView2 tabs, so its `adblockInit`/`iprProp`
/// fields now always read false/`none` here; uBO Lite handles YouTube ads.)
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

/// Install the bundled ad blocker (uBO Lite) into `webview`'s profile and switch it to
/// `on`, sending any failure to the shell's error log.
fn sync_bundled_blocker(webview: &WebView, on: bool, proxy: &EventLoopProxy<UserEvent>) {
    let Some(dir) = ublock_extensions_dir() else {
        let _ = proxy.send_event(UserEvent::AdblockFailed(
            "the bundled extensions couldn't be unpacked".into(),
        ));
        return;
    };
    let report = proxy.clone();
    let done: Completion = Box::new(move |result| {
        if let Err(e) = result {
            let _ = report.send_event(UserEvent::AdblockFailed(e));
        }
    });
    // Reinstall only when this run unpacked new files: reloading the extension aborts
    // page loads in flight (see `extensions::sync_bundled`).
    let refresh = crate::bundled_extensions::changed_this_run();
    if let Err(e) = extensions::sync_bundled(webview, &dir, on, refresh, done) {
        let _ = proxy.send_event(UserEvent::AdblockFailed(e));
    }
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
    // Match the content views' environment options, without touching extensions.
    builder = builder.with_browser_extensions_enabled(true);
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
    fn open_devtools(&self) -> EngineResult {
        self.inner.open_devtools();
        Ok(())
    }
    fn trusted_click(&self, x: f64, y: f64) -> EngineResult {
        // A click may follow a link or submit a form cross-site; mark it as wanted so
        // the navigation guard doesn't read it as a forced redirect.
        self.authorize_navigation();
        input::click(&self.inner, x, y)
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
    fn sync_bundled(
        &self,
        dir: &std::path::Path,
        enabled: bool,
        refresh: bool,
        done: Completion,
    ) -> EngineResult {
        extensions::sync_bundled(&self.inner, dir, enabled, refresh, done)
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
