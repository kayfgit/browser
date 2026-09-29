//! browser — a lightweight, keyboard-driven shell that boots a WebView2
//! engine only when you open a page.
//!
//! The window chrome (welcome screen + command bar) is drawn natively with a
//! pixel buffer, so an idle shell holds NO browser engine. Opening a tab builds
//! a child WebView2 on demand; closing it drops the WebView and frees the
//! renderer immediately.
//!
//! Modes (qutebrowser-style):
//!   * Normal      — shell has focus; command bar works; j/k/space scroll the page.
//!   * Command     — typing a `:`-command (entered with `:` or `o`).
//!   * Insert      — `i` / clicking a field types into a page input; Esc, clicking away,
//!     or navigating returns to Normal. Ctrl+V pastes into the field (no promote).
//!   * Passthrough — `Ctrl+V` (or `i` on a terminal / `:ai`) hands every key to the
//!     content; sticky, so it survives clicks/focus/fullscreen/navigation and leaves
//!     only on Ctrl+S or Shift+Esc (a terminal keeps Esc for the shell).

#![windows_subsystem = "windows"]

#[cfg(all(windows, feature = "servo-engine"))]
surfman::declare_surfman!();

use std::rc::Rc;

use anyhow::{Context as _, Result};
use std::time::{Duration, Instant};

use tao::event::{ElementState, Event, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::keyboard::ModifiersState;
use tao::window::WindowBuilder;

mod actions;
mod ai;
mod app;
mod blocklist;
mod bookmarks;
mod bundled_extensions;
mod chrome;
mod commands;
mod config;
mod data;
mod draw;
mod engines;
mod extensions;
mod favicon;
mod find;
mod freeze;
mod hints;
mod keys;
mod khook;
mod navguard;
mod pages;
mod panes;
mod proc_cwd;
mod procmon;
mod profiles;
mod pty_term;
mod read_view;
mod schemes;
mod session;
mod tabs;
mod term;
mod vim;
use app::{clipboard_get, clipboard_set, AdblockMode, App, ExtInfo, ModeKind, UserEvent};
use commands::COMMANDS;
use draw::Painter;
use find::FindState;
use hints::HintAct;
use pages::commands_document;
use tabs::{js_string, parse_open_flags, parse_tab_flag, Source, Tab};
use term::program_exists;

/// Height of the bottom command/status bar, in physical pixels (at zoom 1.0).
const BAR_H: u32 = 28;
/// Height of the top tab bar at zoom 1.0 (only shown with ≥1 tab open).
const TAB_BAR_H: u32 = 24;
/// Native chrome font size in px at zoom 1.0.
const BASE_PX: f32 = 17.0;
/// Global zoom bounds and step.
const ZOOM_MIN: f64 = 0.5;
const ZOOM_MAX: f64 = 3.0;
const ZOOM_STEP: f64 = 0.1;

/// Max visited URLs kept for autocomplete (also the cap persisted in the session).
const HISTORY_CAP: usize = 300;

/// How many recently-closed tabs to remember for `U` / Ctrl+Shift+T (reopen).
const CLOSED_CAP: usize = 20;

/// Left/right padding (px) inside a native terminal tab's content area.
const TERM_PAD: i32 = 4;

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
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,\
     Translate,msAutoTranslate --autoplay-policy=no-user-gesture-required";

/// Defines `window.__post`, the ONE safe path for page→shell IPC, prepended to every
/// injected bundle so it exists before anything posts. wry builds an `http::Uri` from
/// the *sender frame's* URL and `unwrap()`s it (`webview2/mod.rs`); a post from a frame
/// whose URL isn't a valid http(s) URI — a `file://` page, or an `about:blank` / `data:`
/// / `blob:` ad iframe — panics inside the FFI callback and ABORTS the whole process.
/// So `__post` only forwards when the frame is http(s); elsewhere it silently drops the
/// message (those pages lose IPC niceties like focus-reclaim, but they no longer crash).
const IPC_PRELUDE: &str = r#"
window.__post = window.__post || function (m) {
  try { if (window.ipc && /^https?:$/.test(location.protocol)) window.ipc.postMessage(m); } catch (e) {}
};
"#;

/// Injected into every page. Reads a synchronous `window.__mode` flag (kept in
/// sync by the shell) and, per mode, intercepts exactly the keys the shell owns.
/// In `insert` it takes Escape (leave) and Ctrl+V (to passthrough) and lets the
/// rest type; in `passthrough` it takes only the leave chord (Ctrl+S) and lets every
/// other key — including Esc — reach the page. In insert it also reports when focus leaves the editable element,
/// so the shell can drop back to normal when you click away.
const BRIDGE_JS: &str = include_str!("../scripts/bridge.js");

/// Injected on demand to drive hint mode. Defines `window.__hintShow/Input/Clear`.
/// The shell collects the typed label and calls `__hintInput`; the page filters
/// badges and, on an exact match, clicks the target and reports back via IPC.
const HINT_JS: &str = include_str!("../scripts/hints.js");

/// Injected into `:research` tabs. Strips the heavy/noisy stuff (video, audio,
/// embeds, ad/social iframes) on document-create and as the page mutates, while
/// leaving images and text intact — a lighter browse for "how do I…" research.
/// Page scripts still run (so SPAs work); this only prunes the DOM after the fact,
/// since wry exposes no sub-resource request blocker to stop the loads outright.
const RESEARCH_JS: &str = r#"
(function () {
  if (window.__researchLite) return;
  window.__researchLite = true;
  var SEL = 'video,audio,iframe,embed,object,track,source';
  // Don't strip CAPTCHA / bot-challenge iframes — removing them leaves a page you
  // can't get past (the missing-checkbox case). Match common providers by src/title.
  var KEEP = /recaptcha|hcaptcha|turnstile|challenges?\.cloudflare|cf[-_]?chl|captcha/i;
  function keep(el) {
    if (el.tagName !== 'IFRAME') return false;
    var s = (el.getAttribute('src') || '') + ' ' + (el.title || '') + ' ' + (el.name || '');
    return KEEP.test(s);
  }
  function strip(root) {
    try {
      var r = root && root.querySelectorAll ? root : document;
      var hits = r.querySelectorAll(SEL);
      for (var i = 0; i < hits.length; i++) { if (!keep(hits[i])) hits[i].remove(); }
    } catch (e) {}
  }
  strip(document);
  document.addEventListener('DOMContentLoaded', function () { strip(document); });
  function observe() {
    if (!document.documentElement) { setTimeout(observe, 0); return; }
    new MutationObserver(function (muts) {
      for (var i = 0; i < muts.length; i++) {
        var added = muts[i].addedNodes;
        for (var j = 0; j < added.length; j++) {
          var n = added[j];
          if (n.nodeType !== 1) continue;
          if (n.matches && n.matches(SEL)) { if (!keep(n)) n.remove(); }
          else strip(n);
        }
      }
    }).observe(document.documentElement, { childList: true, subtree: true });
  }
  observe();
})();
"#;

/// Well-known ad-exchange / analytics / tracker hostnames, lower-cased. Consulted by the
/// native navigation guard ([`url_is_ad_host`](crate::tabs::url_is_ad_host)) as the tiny always-on fallback that stops
/// a forced top-level redirect to one of these hosts during the brief window before the
/// full EasyList [`Engine`](crate::blocklist) finishes compiling off-thread at startup.
/// The page-side cosmetic layer doesn't read this list — the engine is the source of truth
/// there, and sub-resources are uBlock Origin Lite's job. Matched as a host substring, so
/// `adservice.google.` catches `adservice.google.com`.
pub(crate) const AD_HOSTS: &[&str] = &[
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "google-analytics.com",
    "googletagmanager.com",
    "googletagservices.com",
    "adservice.google.",
    "pagead2.googlesyndication",
    "amazon-adsystem.com",
    "adnxs.com",
    "adsrvr.org",
    "rubiconproject.com",
    "pubmatic.com",
    "openx.net",
    "criteo.com",
    "criteo.net",
    "taboola.com",
    "outbrain.com",
    "scorecardresearch.com",
    "quantserve.com",
    "moatads.com",
    "adcolony.com",
    "applovin.com",
    "zedo.com",
    "bidswitch.net",
    "casalemedia.com",
    "sharethrough.com",
    "smartadserver.com",
    "teads.tv",
    "3lift.com",
    "yieldmo.com",
    "contextweb.com",
    "gumgum.com",
    "indexww.com",
    "media.net",
    "mgid.com",
    "revcontent.com",
    "adform.net",
    "adroll.com",
    "bluekai.com",
    "demdex.net",
    "everesttech.net",
    "rlcdn.com",
    "agkn.com",
    "crwdcntrl.net",
    "mathtag.com",
    "adsafeprotected.com",
    "serving-sys.com",
    "flashtalking.com",
    "servedbyadbutler.com",
    "hotjar.com",
    "mixpanel.com",
    "segment.io",
    "amplitude.com",
    "branch.io",
    "onesignal.com",
    "clarity.ms",
    "fullstory.com",
    "heap.io",
    "nr-data.net",
    "bugsnag.com",
    "optimizely.com",
    "chartbeat.com",
    "parsely.com",
    "permutive.com",
    "cxense.com",
    "nitropay.com",
    "nitrocnct.com",
    "analytics.tiktok",
    "ads.linkedin.com",
    "ads.pinterest.com",
    "ads.yahoo.com",
    // NOTE: YouTube's own first-party ad telemetry (`/api/stats/ads`, `/ptracking`,
    // `/get_midroll_`) is DELIBERATELY absent — blocking it trips YouTube's anti-adblock
    // detector, which then serves the "Ad blockers violate ToS" enforcement wall. The ads
    // are killed by pruning the player-response JSON instead (see ADBLOCK_JS).
];

/// uBlock-style content blocker, injected at document-start into every web tab while
/// adblock is on. NETWORK-level blocking — stopping ad/scam scripts, iframes and XHRs from
/// ever loading — belongs to the uBlock Origin Lite extension, which does it declaratively
/// inside Chromium's network stack. This page-side script is the OTHER half, and not a
/// fallback: uBO Lite can't see its `<all_urls>` grant under WebView2, so it demotes itself
/// to network-only and does no cosmetic filtering at all. Everything below is therefore the
/// only thing doing these jobs. Being an initialization script, it also can't lose a race
/// with an extension service worker, and it toggles live without a reload:
///   * cosmetic — imperatively hide generic ad containers (EasyList-ish) plus YouTube's
///     ad slots (inline `display:none`, which survives a strict CSP a `<style>` wouldn't);
///   * YouTube — prune the ad descriptors from the player-response JSON, skip/seek past
///     in-player ads, and remove the "ad blocker" enforcement modal;
///   * redirects/popups — report a trusted cross-site gesture (`nav-intent`, the signal
///     the native redirect guard needs) and neuter scripted `window.open` popunders.
///
/// Every layer honours a live `on` flag: the shell flips it via `window.__setAdblock`
/// on `:ads` (no reload needed) and bakes the initial value as `__adblockDefault` per
/// tab so a tab opened while a toggle is active starts in that state.
const ADBLOCK_JS: &str = r#"
(function () {
  if (window.__adblockInit) return;
  window.__adblockInit = true;
  var on = (typeof window.__adblockDefault === 'undefined') ? true : !!window.__adblockDefault;

  // --- NitroPay neutraliser (the fixed left/right "skin" rails + anchor bar) ---
  // Many wikis (e.g. taskbarhero.wiki) serve ads via NitroPay: the page plants an
  // inline `window.nitroAds` stub that QUEUES createAd() calls until
  // s.nitropay.com/ads-*.js loads and renders them — including the fixed side rails
  // and the bottom anchor (the dismissible-with-an-X popups). Those containers have
  // site-specific ids, so cosmetic selectors are unreliable; instead we kill the
  // source. This init script runs at document-start, BEFORE that inline stub, so we
  // plant our OWN inert nitroAds first and lock it with a non-configurable accessor:
  // the page's `nitroAds = nitroAds || {…}` and the loader can't replace it, createAd
  // is a no-op that never resolves, and the queue swallows pushes — so no ad is ever
  // created even if the loader script itself slips past the host blocklist. The host
  // is also in AD_HOSTS so the network/DOM-sweep layers attack the script too.
  if (on) {
    try {
      var nzStub = {
        createAd: function () { return new Promise(function () {}); },
        addUserToken: function () {},
        abortAd: function () {},
        queue: { push: function () {}, length: 0 },
        loaded: true,
      };
      Object.defineProperty(window, 'nitroAds', {
        configurable: false,
        get: function () { return nzStub; },
        set: function () {},
      });
    } catch (e) {}
  }

  // --- DOM cosmetic observer: hide ad containers as the page builds itself ------
  // Blocking the ad SCRIPTS/iframes/XHRs at the network level (so nothing loads in the
  // first place) is uBlock Origin Lite's job, via declarativeNetRequest.
  // The page-side job here is to HIDE leftover ad containers cosmetically, and — on
  // YouTube — to remove the anti-adblock enforcement modal the INSTANT it's inserted,
  // before it can paint (the observer fires before the next render, so no 0.3s flash).
  function observe() {
    if (!document.documentElement) { setTimeout(observe, 0); return; }
    new MutationObserver(function (muts) {
      if (!on) return;
      for (var i = 0; i < muts.length; i++) {
        var a = muts[i].addedNodes;
        for (var j = 0; j < a.length; j++) {
          var n = a[j];
          if (n.nodeType !== 1) continue;
          hideCosmetic(n);
          if (isYT) killEnforcement(n);
        }
      }
    }).observe(document.documentElement, { childList: true, subtree: true });
  }
  observe();

  // --- YouTube VIDEO ads: prune the ad descriptors from the player response ----
  // uBlock's technique (`json-prune`): YouTube schedules pre-/mid-roll ads from
  // `adPlacements`/`playerAds`/`adSlots` keys in the player API JSON. We trap the
  // ways that JSON reaches the page and delete those keys, so the player simply
  // has no ad to play — no seeking, so no "1s ad then black screen". Only patched
  // on YouTube to avoid overhead/risk elsewhere.
  var isYT = location.hostname.indexOf('youtube.com') !== -1 ||
             location.hostname.indexOf('youtube-nocookie.com') !== -1;
  // The CANONICAL uBO key set — and only these. Earlier we also pruned
  // `adParams`/`adBreakHeartbeatParams`/`importantHeaders`; that over-reach mangled
  // the response shape (importantHeaders isn't even an ad key) and is exactly the kind
  // of tampering YouTube's anti-adblock detector trips on, which is what was raising
  // the "Ad blockers violate ToS" wall. Prune the ads, leave the rest intact.
  var YT_AD_KEYS = ['adPlacements', 'playerAds', 'adSlots'];
  // The enforcement wall ITSELF arrives as data: YouTube delivers it in the player
  // response's `playabilityStatus` (status != OK + an errorScreen carrying the
  // "ad blocker" message). Neutralise it at the source so the popup never renders and
  // playback proceeds — but ONLY for the ad-block case. We gate on the response actually
  // mentioning ad-blocking so genuine unavailability (private/age/region/deleted) still
  // errors correctly instead of being forced to a broken "OK" with no streams.
  function fixPlayability(data) {
    var ps = data.playabilityStatus;
    if (!on || !ps || typeof ps !== 'object' || !ps.status || ps.status === 'OK') return;
    // Only lift the wall when there are real streams to REVEAL. If YouTube withheld
    // `streamingData` (the detected case), forcing "OK" just yields a black screen —
    // worse than its actual message — so leave it. With ad telemetry now flowing
    // (see AD_HOSTS) detection shouldn't fire at all; this is the belt-and-braces path
    // for when the wall rides along with an otherwise-playable response.
    if (!data.streamingData) return;
    try {
      var blob = JSON.stringify(ps).toLowerCase();
      if (blob.indexOf('ad block') !== -1 || blob.indexOf('ad-block') !== -1 ||
          blob.indexOf('adblock') !== -1) {
        ps.status = 'OK';
        delete ps.errorScreen;
        delete ps.reason;
        delete ps.messages;
      }
    } catch (e) {}
  }
  function prunePlayerAds(data) {
    if (!on || !data || typeof data !== 'object') return data;
    try {
      for (var i = 0; i < YT_AD_KEYS.length; i++) {
        if (data[YT_AD_KEYS[i]] != null) delete data[YT_AD_KEYS[i]];
      }
      if (data.playabilityStatus) fixPlayability(data);
      if (data.playerResponse) prunePlayerAds(data.playerResponse);
      if (Array.isArray(data)) for (var j = 0; j < data.length; j++) prunePlayerAds(data[j]);
    } catch (e) {}
    return data;
  }
  // DO NOT PATCH NATIVES HERE. This block used to wrap `JSON.parse` and
  // `Response.prototype.json` so every player response got pruned on the way past.
  // Bisected 2026-08-03: those two wrappers are what broke YouTube. With either one
  // installed the player never gets media (`readyState` stays 0, `duration` NaN), and
  // after ~6s YouTube gives up and auto-advances the playlist — which is the
  // black-screen / no-audio / "page keeps reloading and lands on a different video"
  // report. It is NOT the pruning: with `YT_AD_KEYS` emptied so the walk deletes
  // nothing, merely having the wrappers in place still killed playback. Replacing a
  // native with a function whose `toString()` is no longer `[native code]` is a
  // well-known thing YouTube's integrity checks look for, and it clearly acts on it.
  //
  // Verified against the same watch URL, everything else enabled: wrappers on → 2-3
  // playlist skips per 22s, `ready=0`; wrappers off → 0 skips, video plays.
  //
  // The accessor below is a DIFFERENT mechanism — it defines a property that doesn't
  // exist yet rather than replacing a built-in — and was measured innocent (removing it
  // while the wrappers stayed did not help; keeping it with the wrappers gone plays
  // fine). It covers the case that matters most anyway: a full page load embeds the
  // player response as this inline global, which is where pre-roll ads are scheduled.
  // Ads on subsequent SPA navigations are handled by the `youtube()` tick instead.
  if (isYT) {
    try {
      var _ytIPR;
      Object.defineProperty(window, 'ytInitialPlayerResponse', {
        configurable: true,
        get: function () { return _ytIPR; },
        set: function (v) { _ytIPR = prunePlayerAds(v); }
      });
    } catch (e) {}
  }

  // --- cosmetic: hide ad containers + YouTube ad slots -------------------------
  // We hide IMPERATIVELY (inline `display:none`) rather than via an injected
  // `<style>`: a strict CSP (YouTube's) blocks injected stylesheets, but inline
  // styles set through `el.style` are exempt — so this works where a stylesheet
  // silently wouldn't. Hidden nodes are tagged so `:ads` off can restore them.
  var COSMETIC = [
    'ins.adsbygoogle', '.adsbygoogle',
    'iframe[src*="doubleclick"]', 'iframe[src*="googlesyndication"]',
    'iframe[id^="google_ads_"]', 'iframe[id*="aswift"]',
    'div[id^="google_ads_"]', 'div[id^="div-gpt-ad"]',
    '[data-ad-slot]', '[data-ad-client]', '[aria-label="Advertisement"]',
    '.ad-container', '.ad-banner', '.ad-wrapper', '.ads-container',
    '.advertisement', '.sponsored-content', '.trc_related_container',
    // YouTube: the sponsored/promoted feed cards, the masthead banner, in-player
    // ad slots. `:has()` removes the empty grid cell the ad lived in, not just the
    // ad node, so the feed doesn't keep a blank gap.
    'ytd-display-ad-renderer', 'ytd-promoted-sparmus-renderer',
    'ytd-promoted-video-renderer', 'ytd-ad-slot-renderer',
    'ytd-in-feed-ad-layout-renderer', 'ytd-banner-promo-renderer',
    'ytd-statement-banner-renderer', 'ad-slot-renderer',
    '#masthead-ad', '#player-ads',
    '.ytp-ad-overlay-container', '.ytp-ad-module', '.video-ads',
    '.ytp-ad-overlay-slot', '.ytp-suggested-action'
  ];
  // `:has()` selectors are kept SEPARATE: an engine that rejected one would throw
  // for the whole `querySelectorAll`, so a bad one here can't disable the core list
  // above. These remove the empty feed cell the ad lived in (not just the ad node).
  var COSMETIC_HAS = [
    'ytd-rich-item-renderer:has(ytd-ad-slot-renderer)',
    'ytd-rich-item-renderer:has(ytd-in-feed-ad-layout-renderer)',
    'ytd-rich-section-renderer:has(ytd-statement-banner-renderer)'
  ];
  var COSMETIC_SEL = COSMETIC.join(',');
  var COSMETIC_HAS_SEL = COSMETIC_HAS.join(',');
  function hideEl(el) {
    if (el.getAttribute('data-adblock-hidden')) return;
    el.setAttribute('data-adblock-hidden', '1');
    el.style.setProperty('display', 'none', 'important');
  }
  function hideBySelector(root, selector) {
    try {
      if (root.nodeType === 1 && root.matches && root.matches(selector)) hideEl(root);
      if (root.querySelectorAll) {
        var hits = root.querySelectorAll(selector);
        for (var i = 0; i < hits.length; i++) hideEl(hits[i]);
      }
    } catch (e) {}
  }
  function hideCosmetic(root) {
    if (!on || !root) return;
    hideBySelector(root, COSMETIC_SEL);
    hideBySelector(root, COSMETIC_HAS_SEL);
  }
  function unhideCosmetic() {
    try {
      var hits = document.querySelectorAll('[data-adblock-hidden]');
      for (var i = 0; i < hits.length; i++) {
        hits[i].removeAttribute('data-adblock-hidden');
        hits[i].style.removeProperty('display');
      }
    } catch (e) {}
  }

  // --- YouTube: skip the skippable, seek past the unskippable ------------------
  // The dangerous half of this is the seek. YouTube plays ads through the SAME
  // `<video>` element as the feature, so the `ad-showing` class is the ONLY thing
  // separating "seek the ad to its end" from "end the user's video". If that class is
  // stale or set a beat early/late, seeking to `duration` fires `ended` on the real
  // video — and on a playlist (or with autoplay) YouTube then advances to the NEXT
  // video, which reads as a page that reloads itself forever and lands on something
  // else entirely. So the seek is now cross-checked, and never fires on anything that
  // looks like the feature.
  //
  // `lengthSeconds` from the player response is the feature's length; an ad loaded in
  // the same element has a different duration. If what's loaded matches the feature,
  // the class is lying — refuse. As an independent backstop (the player response can
  // be stale across an SPA navigation) anything longer than AD_MAX_SECS is treated as
  // the feature too: real pre-/mid-rolls are seconds-to-a-couple-minutes long, so the
  // worst case of the bound is one unusually long ad we let play rather than one video
  // we destroy.
  var AD_MAX_SECS = 360;
  function featureLength() {
    try {
      var d = window.ytInitialPlayerResponse && window.ytInitialPlayerResponse.videoDetails;
      var n = d && parseFloat(d.lengthSeconds);
      return (isFinite(n) && n > 0) ? n : 0;
    } catch (e) { return 0; }
  }
  // Mute is forced only for the duration of an ad, and the page's own setting is put
  // back afterwards — the old code muted and never unmuted, so a single misfire left
  // the video silent for the rest of the session.
  var premuted = null;
  function restoreMute(v) {
    if (premuted === null || !v) return;
    try { v.muted = premuted; } catch (e) {}
    premuted = null;
  }
  function youtube() {
    if (!on || location.hostname.indexOf('youtube.com') === -1) return;
    try {
      var skip = document.querySelector(
        '.ytp-ad-skip-button, .ytp-ad-skip-button-modern, .ytp-skip-ad-button');
      if (skip) skip.click();
      var player = document.querySelector('.html5-video-player');
      var v = document.querySelector('video.html5-main-video');
      var adPlaying = !!player && player.classList.contains('ad-showing');
      if (adPlaying && v && v.duration && isFinite(v.duration)) {
        var feat = featureLength();
        var isFeature = (feat && Math.abs(v.duration - feat) < 2) || v.duration > AD_MAX_SECS;
        if (!isFeature) {
          if (premuted === null) premuted = v.muted;
          v.muted = true;
          v.currentTime = v.duration;
        }
      } else {
        restoreMute(v);
      }
      var close = document.querySelector('.ytp-ad-overlay-close-button');
      if (close) close.click();
      killEnforcement(document);
    } catch (e) {}
  }

  // The dismissible "ad blockers are not allowed" MODAL that rides over a playing video.
  // We match it by its dedicated COMPONENT TAG, not by text — that's how uBO does it, and
  // it's why this works in every language (the old English-text gate let the PT/ES/etc.
  // versions through). `ytd-enforcement-message-view-model` is used for nothing else, so
  // removing its enclosing popup + scrim is safe. Returns whether one was found+removed.
  // Called from the MutationObserver the instant the modal is INSERTED (so it never paints
  // — killing the ~0.3s flash) and from the YouTube tick as a backstop. `root` is the node
  // to scan (an inserted subtree, or `document`).
  function killEnforcement(root) {
    if (!on || !isYT) return false;
    try {
      var sel = 'ytd-enforcement-message-view-model, ytd-enforcement-message-view-model-renderer';
      var enf = [];
      if (root.matches && root.matches(sel)) enf.push(root);
      if (root.querySelectorAll) {
        var hits = root.querySelectorAll(sel);
        for (var i = 0; i < hits.length; i++) enf.push(hits[i]);
      }
      if (!enf.length) return false;
      for (var d = 0; d < enf.length; d++) {
        (enf[d].closest('ytd-popup-container') ||
         enf[d].closest('tp-yt-paper-dialog') || enf[d]).remove();
      }
      // Drop the scrim too, else the page stays click-blocked / scroll-locked, and
      // resume playback if YouTube paused it behind the wall.
      var bd = document.querySelector('tp-yt-iron-overlay-backdrop');
      if (bd) bd.remove();
      var vid = document.querySelector('video.html5-main-video');
      if (vid && vid.paused) { try { vid.play(); } catch (e) {} }
      return true;
    } catch (e) {}
    return false;
  }

  // --- forced / auto redirect guard -------------------------------------------
  // Forced cross-site redirects (the "click the video → bounced to a scam" hijack) are
  // cancelled natively by the shell's intent-gate guard, which denies any cross-site TOP
  // navigation that no trusted gesture asked for — see `navguard`. The page side's only
  // job now is to REPORT that trusted intent (below) so genuine link clicks are allowed,
  // and to neuter popunder `window.open`s. `crossOrigin` backs both.
  function crossOrigin(url) {
    try { return new URL(url, document.baseURI).origin !== location.origin; }
    catch (e) { return false; } // unparseable / relative → treat as same-origin (allow)
  }
  // Popup reports go ONLY from the top frame: wry builds an `http::Uri` from the sender
  // frame's URL and unwraps it, so a report from an `about:blank`/`data:` ad iframe would
  // feed it an invalid URI and abort the process. Sub-frames still neuter — they stay quiet.
  var isTop = false;
  try { isTop = (window.top === window); } catch (e) {}
  function reportPopup(url) { if (isTop) window.__post('popup-blocked:' + url); }
  // Cross-site navigation intent. The native guard cancels every cross-site TOP
  // navigation unless the shell just saw legitimate intent — because at the engine
  // level a forced redirect is indistinguishable from a real link click (these scripts
  // hijack your click via a synthetic <a> or a transparent overlay, so WebView2 reports
  // the jump as foreground AND user-initiated). The ONE trustworthy tell is here in the
  // DOM: a navigation is wanted only when a TRUSTED (real, isTrusted) gesture lands on
  // an actual cross-site link or form-submit control. Report exactly that; a synthetic
  // click (isTrusted=false) or a non-link overlay <div> never matches, so its redirect
  // gets no report and is cancelled. Top frame only — the frame the guard governs.
  function navTargetCrossSite(t) {
    try {
      var a = (t && t.closest) ? t.closest('a[href]') : null;
      if (a && a.href && !/^javascript:/i.test(a.href) && crossOrigin(a.href)) return true;
      var sb = (t && t.closest) ? t.closest('button,input[type=submit],input[type=image]') : null;
      if (sb) {
        var f = sb.form || ((sb.closest) ? sb.closest('form') : null);
        if (f && f.action && crossOrigin(f.action)) return true;
      }
    } catch (e) {}
    return false;
  }
  function reportIntent(t) { if (on && isTop && navTargetCrossSite(t)) window.__post('nav-intent'); }
  if (isTop) {
    // pointerdown (not click) so the report is posted BEFORE the navigation fires.
    document.addEventListener('pointerdown', function (e) {
      if (e.isTrusted) reportIntent(e.target);
    }, true);
    document.addEventListener('keydown', function (e) {
      if (e.isTrusted && (e.key === 'Enter' || e.key === ' ')) {
        reportIntent(e.target);
        reportIntent(document.activeElement);
      }
    }, true);
  }
  // window.open popunders are THE dominant "click anywhere → scam tab" vector on
  // scummy streaming sites. Neuter scripted cross-origin opens while blocking is on.
  // The shell's native new-window handler is the primary guard (it also catches frames
  // and about:blank shells); this is the in-page backstop. Runs in every frame, so the
  // ad iframe's own window.open is stopped at the source.
  var _winOpen = window.open ? window.open.bind(window) : null;
  if (_winOpen) {
    window.open = function (u) {
      if (on && (!u || crossOrigin(u))) { reportPopup(u || 'about:blank'); return null; }
      return _winOpen.apply(null, arguments);
    };
  }
  // Periodic backstop. YouTube needs a FAST poll — youtube() skips in-player ads, seeks
  // past unskippables and strips the enforcement modal, all hinging on player state the
  // MutationObserver (childList only) can't see. Everywhere else the observer already
  // hides inserted ad containers as they appear, so a slow pass (catching ad classes
  // toggled onto existing nodes) is plenty — 4x fewer idle wakeups per non-YouTube tab.
  function tick() { if (!on) return; hideCosmetic(document); youtube(); }
  hideCosmetic(document);
  document.addEventListener('DOMContentLoaded', function () { hideCosmetic(document); });
  setInterval(tick, isYT ? 500 : 2000);

  // --- live toggle from the shell (`:ads`), no reload needed -------------------
  window.__setAdblock = function (v) {
    on = !!v;
    if (on) { hideCosmetic(document); youtube(); }
    else { unhideCosmetic(); }
  };
})();
"#;

/// Live page-feature toggles, injected into every web tab. Two independent flags,
/// each seeded from `window.__featureDefaults` (baked per tab from the shell's state)
/// and flipped live by `window.__setToggle(name, on)` — no reload:
///   * `mute`   — keep every `<video>`/`<audio>` muted (observed + a 1s safety tick).
///   * `css`    — disable every stylesheet/`<style>`/`<link rel=stylesheet>`.
///
/// (Pop-up/popunder blocking now lives entirely under `:ads` — the native new-window
/// handler plus the all-frames `window.open` neuter in [`ADBLOCK_JS`].)
///
/// Mirrors the `:ads` pattern so `:mute`/`:css` apply instantly to all tabs.
const FEATURES_JS: &str = r#"
(function () {
  // Document-keyed guard, like BRIDGE_JS: the init script can first run against the
  // initial empty document whose window Chromium reuses for the real page — a
  // window-keyed guard left the observer + applied styles on that dead document.
  // Window-level work (the safety interval) still runs once per window.
  if (window.__featuresInit === document) return;
  var firstRun = !window.__featuresInit;
  window.__featuresInit = document;
  var d = window.__featureDefaults || {};
  var muted = !!d.mute, noCss = !!d.css, noVideo = !!d.video, noSb = !!d.scrollbar;

  // audio: force every media element's muted flag to match the toggle.
  function applyMute(root, val) {
    try {
      var r = (root && root.querySelectorAll) ? root : document;
      var m = r.querySelectorAll('video,audio');
      for (var i = 0; i < m.length; i++) m[i].muted = val;
    } catch (e) {}
  }

  // video: strip players (like :research) — <video> plus the embed elements sites
  // use for video, EXCEPT bot-challenge iframes (removing those traps the page).
  var VID_SEL = 'video,iframe,embed,object,source,track';
  var VID_KEEP = /recaptcha|hcaptcha|turnstile|challenges?\.cloudflare|cf[-_]?chl|captcha/i;
  function vidKeep(el) {
    if (el.tagName !== 'IFRAME') return false;
    var s = (el.getAttribute('src') || '') + ' ' + (el.title || '') + ' ' + (el.name || '');
    return VID_KEEP.test(s);
  }
  function applyVideo(root) {
    if (!noVideo) return;
    try {
      var r = (root && root.querySelectorAll) ? root : document;
      var hits = r.querySelectorAll(VID_SEL);
      for (var i = 0; i < hits.length; i++) { if (!vidKeep(hits[i])) hits[i].remove(); }
    } catch (e) {}
  }

  // scrollbar: hide/show the page's scrollbars via an injected stylesheet
  // (WebView2 is Chromium, so ::-webkit-scrollbar covers native scrollbars; the
  // scrollbar-width rule catches pages that opted into the standard property).
  function applyScrollbar() {
    try {
      var st = document.getElementById('__no-scrollbar');
      if (!noSb) { if (st) st.remove(); return; }
      if (st) return;
      var root = document.head || document.documentElement;
      if (!root) { document.addEventListener('DOMContentLoaded', applyScrollbar); return; }
      st = document.createElement('style');
      st.id = '__no-scrollbar';
      st.textContent = '::-webkit-scrollbar{display:none!important;width:0!important;height:0!important}' +
        'html,body{scrollbar-width:none!important}';
      root.appendChild(st);
    } catch (e) {}
  }

  // css: toggle every stylesheet (both the <style>/<link> nodes and the live sheets).
  function applyCss() {
    try {
      var nodes = document.querySelectorAll('style,link[rel="stylesheet"]');
      for (var i = 0; i < nodes.length; i++) nodes[i].disabled = noCss;
      var sheets = document.styleSheets;
      for (var j = 0; j < sheets.length; j++) { try { sheets[j].disabled = noCss; } catch (e) {} }
    } catch (e) {}
  }

  function observe() {
    if (!document.documentElement) { setTimeout(observe, 0); return; }
    new MutationObserver(function (muts) {
      for (var i = 0; i < muts.length; i++) {
        var a = muts[i].addedNodes;
        for (var j = 0; j < a.length; j++) {
          var n = a[j];
          if (n.nodeType !== 1) continue;
          if (muted) {
            if (n.tagName === 'VIDEO' || n.tagName === 'AUDIO') n.muted = true;
            else applyMute(n, true);
          }
          if (noCss && (n.tagName === 'STYLE' || n.tagName === 'LINK')) {
            try { n.disabled = true; } catch (e) {}
          }
          if (noVideo) {
            if (n.matches && n.matches(VID_SEL)) { if (!vidKeep(n)) n.remove(); }
            else applyVideo(n);
          }
        }
      }
    }).observe(document.documentElement, { childList: true, subtree: true });
  }
  observe();
  applyCss();
  applyVideo(document);
  applyScrollbar();
  document.addEventListener('DOMContentLoaded', function () {
    applyCss();
    if (muted) applyMute(document, true);
    applyVideo(document);
    applyScrollbar();
  });
  if (firstRun) setInterval(function () { if (muted) applyMute(document, true); }, 1000);

  window.__setToggle = function (name, val) {
    val = !!val;
    if (name === 'mute') { muted = val; applyMute(document, val); }
    else if (name === 'css') { noCss = val; applyCss(); }
    else if (name === 'video') { noVideo = val; applyVideo(document); }
    else if (name === 'scrollbar') { noSb = val; applyScrollbar(); }
  };
})();
"#;

// Page-side selection mode (vim selection on live web pages). The shell
// forwards motions here; we drive a real DOM Selection via `Selection.modify`
// (Chromium supports character/word/line/lineboundary/documentboundary), draw a
// fixed-width block cursor over the logical cursor, and post the yanked
// text back over IPC. `__caretEnter` places the caret at the viewport center
// WITHOUT selecting — press `v`/`V` again for charwise/linewise visual; `__caretKey`
// moves or extends and scrolls the window when the cursor nears a viewport edge
// (vim scrolloff); `__caretYank` copies; `__caretEsc` collapses then exits (posting
// `caret-exit` so the shell leaves selection mode).
const CARET_JS: &str = include_str!("../scripts/selection.js");

// Page-side find-in-page: `__find(q)` highlights every match (CSS Custom Highlight
// API — no DOM mutation, so it can't break the page), scrolls to the first, and
// `__findNext`/`__findPrev` move the "current" highlight. `__findClear` removes it.
const FIND_JS: &str = r#"
(function () {
  if (window.__find) return;
  var all = [], cur = -1;
  var hAll = null, hCur = null;
  function ready() {
    if (hAll || typeof Highlight === 'undefined' || !CSS || !CSS.highlights) return;
    hAll = new Highlight(); hCur = new Highlight();
    CSS.highlights.set('bfind', hAll);
    CSS.highlights.set('bfindcur', hCur);
    var st = document.createElement('style');
    st.textContent = '::highlight(bfind){background:#5a5214;color:#fff}' +
                     '::highlight(bfindcur){background:#c8641e;color:#000}';
    (document.head || document.documentElement).appendChild(st);
  }
  function clear() {
    all = []; cur = -1;
    if (hAll) hAll.clear();
    if (hCur) hCur.clear();
  }
  function show() {
    if (!hCur) return;
    hCur.clear();
    if (cur < 0 || cur >= all.length) return;
    hCur.add(all[cur]);
    var r = all[cur].getBoundingClientRect();
    window.scrollBy(0, r.top - window.innerHeight / 2);
  }
  window.__find = function (q) {
    ready();
    clear();
    if (!q || !hAll) return 0;
    var ql = q.toLowerCase();
    var walk = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT, {
      acceptNode: function (n) {
        if (!n.nodeValue || !n.nodeValue.trim()) return NodeFilter.FILTER_REJECT;
        var p = n.parentElement; if (!p) return NodeFilter.FILTER_REJECT;
        var t = p.tagName; if (t === 'SCRIPT' || t === 'STYLE' || t === 'NOSCRIPT') return NodeFilter.FILTER_REJECT;
        var s = getComputedStyle(p);
        if (s.display === 'none' || s.visibility === 'hidden') return NodeFilter.FILTER_REJECT;
        return NodeFilter.FILTER_ACCEPT;
      }
    });
    var n;
    while ((n = walk.nextNode())) {
      var low = n.nodeValue.toLowerCase(), i = 0;
      while ((i = low.indexOf(ql, i)) !== -1) {
        var r = document.createRange();
        r.setStart(n, i); r.setEnd(n, i + ql.length);
        all.push(r); hAll.add(r);
        i += ql.length;
      }
    }
    cur = all.length ? 0 : -1;
    show();
    return all.length;
  };
  window.__findNext = function () { if (all.length) { cur = (cur + 1) % all.length; show(); } };
  window.__findPrev = function () { if (all.length) { cur = (cur - 1 + all.length) % all.length; show(); } };
  window.__findClear = clear;
})();
"#;

/// Tag this process with an explicit AppUserModelID so the taskbar treats it and
/// the processes it spawns as one application. (Task Manager still files WebView2
/// under its own "WebView2 Manager" group: the runtime claims a separate identity.
/// Correct parenting comes from the DPI manifest in `build.rs`.) Best-effort: any
/// failure is ignored.
#[cfg(windows)]
fn set_app_user_model_id() {
    use windows::core::w;
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(w!("kayf.browser.Shell"));
    }
}

fn main() -> Result<()> {
    // Give this process a single explicit AppUserModelID *before* anything is
    // spawned. Child processes inherit it at creation time, so every descendant —
    // the on-demand WebView2 engine and its renderer/GPU/utility processes, the
    // browser-pty-host companion, its conhost + shell — shares one identity and
    // Task Manager groups the whole tree under a single "browser" entry instead of
    // scattering the WebView2 manager and friends as separate top-level apps.
    #[cfg(windows)]
    set_app_user_model_id();

    // Release builds load uBlock Origin Lite from a copy unpacked out of the
    // executable; get that done before the first web tab needs it.
    if !cfg!(debug_assertions) {
        bundled_extensions::prepare_in_background();
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Decide up front whether we're restoring a session: only with no CLI target
    // and outside headless test runs. Load it here so the saved window geometry can
    // be applied at build time (no visible jump from the default size). Always the
    // LIVE session — profiles are snapshots you load on purpose, never a thing the
    // browser silently boots into — except in `:scratch`, whose own file is live for
    // as long as the detour lasts (quitting inside it and relaunching isn't the same
    // as ENTERING it, which always starts clean). The config knows which, so it has
    // to be read before the window exists.
    // `--scratch` boots this RUN into a throwaway slate: the config pointer is set in
    // memory only (so the next ordinary launch returns to the real profile), and every
    // write is redirected to `scratch-cli.toml` — not the live session, not a profile,
    // and not the `:scratch` slate either. The safe way to poke at a dev build while
    // real data is set up. Note you can't get here by typing `:scratch` after launch:
    // that parks whatever is on screen INTO `session.toml` first (`profiles::switch_to`
    // step 1), which is exactly what a throwaway run must not do.
    let mut cli_arg = None;
    let mut cli_scratch = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--scratch" => cli_scratch = true,
            _ if cli_arg.is_none() => cli_arg = Some(a),
            _ => {}
        }
    }
    let is_test = std::env::var("BROWSER_TEST_QUIT_MS").is_ok();
    let mut cfg = config::load();
    if cli_scratch {
        // In-memory only: never `config::save`d here, so the next ordinary launch
        // comes back to the profile the user actually left off in.
        cfg.scratch = true;
        cfg.scratch_return = cfg.profile.take();
    }
    let restore = if cli_arg.is_none() && !is_test {
        let path = if cli_scratch {
            session::cli_scratch_path()
        } else if cfg.scratch {
            session::scratch_path()
        } else {
            session::session_path()
        };
        path.as_deref().and_then(session::load_from)
    } else {
        None
    };

    let mut builder = WindowBuilder::new()
        .with_title("browser")
        .with_decorations(false); // no OS title bar; window control is command-driven
    builder = match restore.as_ref().and_then(|s| s.window.as_ref()) {
        Some(g) => builder
            .with_inner_size(tao::dpi::PhysicalSize::new(g.w, g.h))
            .with_position(tao::dpi::PhysicalPosition::new(g.x, g.y)),
        None => builder.with_inner_size(tao::dpi::LogicalSize::new(1100.0, 740.0)),
    };
    let window = builder.build(&event_loop).context("creating window")?;
    let window = Rc::new(window);

    let context = softbuffer::Context::new(window.clone())
        .map_err(|e| anyhow::anyhow!("softbuffer context: {e}"))?;
    let surface = softbuffer::Surface::new(&context, window.clone())
        .map_err(|e| anyhow::anyhow!("softbuffer surface: {e}"))?;

    let painter = Painter::new(BASE_PX).context("loading font")?;

    let mut app = App {
        window: window.clone(),
        _context: context,
        surface,
        painter,
        proxy,
        mode: ModeKind::Normal,
        command: String::new(),
        command_cursor: 0,
        command_anchor: None,
        hint_input: String::new(),
        hint_act: HintAct::Follow,
        native_hints: Vec::new(),
        status: String::new(),
        status_is_error: false,
        status_color: None,
        status_clear_at: None,
        ai_prev_active: None,
        errors: Vec::new(),
        current_command: None,
        find: FindState::default(),
        tabs: Vec::new(),
        active: None,
        modifiers: ModifiersState::default(),
        nojs: false,
        // Blocking on by default: uBO Lite (network) plus the native layers, which cover
        // different halves of the job (see `AdblockMode`). Session restore may override.
        adblock_mode: AdblockMode::Ubo,
        adblock_prev: AdblockMode::Ubo,
        term_drag: None,
        term_clicks: None,
        extension_request: 0,
        adblock: true,
        adblock_on: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        allow_risky_downloads: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        blocker: blocklist::new_shared(),
        mute: false,
        no_css: false,
        no_video: false,
        no_scrollbar: false,
        term_command: term::default_shell(),
        search_template: browser_core::DEFAULT_SEARCH_URL.to_string(),
        next_term_id: 0,
        groq_key: ai::load_key(),
        next_ai_id: 0,
        ai_model: ai::load_model().unwrap_or_else(|| ai::DEFAULT_MODEL.to_string()),
        ai_chats: ai::load_chats(),
        zoom: 1.0,
        content_zoom: 1.0,
        cursor_on: true,
        term_resize_want: Vec::new(),
        term_resize_want_at: None,
        quit: false,
        torn_down: false,
        res_prev: std::collections::HashMap::new(),
        res_at: Instant::now(),
        cursor_pos: (0.0, 0.0),
        bar_hover: false,
        hover_link: None,
        bar_dragging: false,
        bar_cmd_scroll: 0,
        page_focus_yielded: false,
        page_gesture_at: None,
        acting_ai: None,
        config: cfg,
        theme: draw::Theme::default(),
        last_focus_gain: Instant::now(),
        history: Vec::new(),
        history_at: Vec::new(),
        saved: bookmarks::load(),
        closed_tabs: Vec::new(),
        fs_from_page: false,
        windows: Vec::new(),
        pane_focus: panes::PaneFocus::default(),
        pending_window_key: false,
        pending_window_at: Instant::now(),
        pending_yank_key: false,
        pending_yank_at: Instant::now(),
        cli_scratch,
        pane_resize_at: Instant::now(),
        pane_move_orig: None,
        active_pane_is_webview: false,
        background_webview_visible: false,
        term_scrollback: pty_term::DEFAULT_SCROLLBACK,
        term_style: pty_term::TermStyle::default(),
        term_painter: None,
        nav_replaying: false,
        term_find_pending: None,
        engine_keepalive: None,
        config_edit_term: None,
        installing_scheme: None,
        term_last_find: None,
        frozen: false,
    };
    // Resolve the persisted appearance overrides into the live chrome theme and
    // terminal style (colours + optional custom terminal font).
    app.rebuild_theme();
    app.rebuild_term_style();

    // Compile the ad/redirect blocklist engine off-thread; it goes live a beat after
    // launch (BlocklistReady), and navigations use the timing heuristic until then.
    blocklist::spawn_build(app.blocker.clone(), app.proxy.clone());

    // Optional: open a page immediately, e.g. `browser youtube.com`,
    // or run a command, e.g. `browser ":nojs youtube.com"`. An explicit
    // CLI target takes precedence over (and skips) session restore. With no
    // argument, restore the previous session's tabs + UI state (window geometry was
    // already applied at build time above).
    engines::with_window_target(&event_loop, || match cli_arg {
        Some(target) => {
            let t = target.trim_start();
            if let Some(cmd) = t.strip_prefix(':') {
                app.run_command(cmd);
            } else {
                app.open_tab(&target, false, true);
            }
        }
        None => {
            if let Some(s) = restore {
                app.restore_session(s);
            }
        }
    });

    #[cfg(all(windows, feature = "servo-engine"))]
    engines::with_window_target(&event_loop, || engines::servo::smoke::start(&mut app))?;

    // Test hook: auto-quit after N ms so cleanup can be verified headlessly.
    if let Ok(ms) = std::env::var("BROWSER_TEST_QUIT_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            let proxy = app.proxy.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                let _ = proxy.send_event(UserEvent::Quit);
            });
        }
    }

    // Install the low-level keyboard hook so the shell can always reclaim control
    // (leave passthrough/insert, or snap back from a click that yielded focus to the
    // page) regardless of which HWND/iframe holds keyboard focus. Must run on the
    // event-loop thread (here) so the hook proc fires from its message pump.
    #[cfg(windows)]
    {
        use tao::platform::windows::WindowExtWindows;
        khook::install(app.window.hwnd() as isize, app.proxy.clone());
    }

    window.request_redraw();

    event_loop.run(move |event, target, control_flow| engines::with_window_target(target, || {
        #[cfg(all(windows, feature = "servo-engine"))]
        let event = engines::servo::intercept(&mut app, event).unwrap_or(Event::UserEvent(UserEvent::Servo(engines::servo::Event::Wake)));

        let event = match event {
            Event::UserEvent(UserEvent::Engine { view, event }) => {
                Event::UserEvent(app.route_engine_event(view, *event).unwrap_or(UserEvent::Redraw))
            }
            event => event,
        };
        *control_flow = ControlFlow::Wait;
        match event {
            // Command-bar cursor blink: the WaitUntil deadline (set below while in
            // Command mode) wakes us here to flip the cursor and repaint.
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                // A transient status flash (e.g. a finished background `:ai`) times out.
                app.expire_status_flash();
                // Advance the tab strip's loading sweep. Mode-independent: a page can
                // load while you're typing a command or in passthrough.
                if app.any_tab_loading() {
                    app.window.request_redraw();
                }
                // A held-back terminal resize (zoom/drag burst settling): repaint —
                // the draw's sync_active_term_size applies it once the target settles.
                if app.term_resize_want_at.is_some() {
                    app.window.request_redraw();
                }
                // Repeatable pane-resize auto-leaves after a spell of no resize key, so a
                // later j/k (meant to scroll) doesn't silently resize.
                if app.mode == ModeKind::PaneResize
                    && app.pane_resize_at.elapsed() >= crate::app::PANE_RESIZE_TIMEOUT
                {
                    app.mode = ModeKind::Normal;
                    app.clear_status();
                    app.window.request_redraw();
                }
                if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
                    app.cursor_on = !app.cursor_on;
                    app.window.request_redraw();
                } else if matches!(app.mode, ModeKind::Normal | ModeKind::Scroll | ModeKind::ScrollCaret)
                    || (app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll)
                {
                    // Read-mode caret: blink its block cursor.
                    if app.read_caret_active() {
                        app.cursor_on = !app.cursor_on;
                        app.window.request_redraw();
                    }
                    // Focus backstop: reclaim keyboard focus if a click handed it to
                    // the webview (see reclaim_focus_tick).
                    app.reclaim_focus_tick();
                    // Live `:res` monitor: re-sample on the tick.
                    if app.active_is_res() {
                        app.refresh_res();
                        app.window.request_redraw();
                    }
                }
            }
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    app.teardown();
                    *control_flow = ControlFlow::Exit;
                }
                WindowEvent::Resized(size) => {
                    app.on_resize(size.width, size.height);
                }
                WindowEvent::ModifiersChanged(state) => {
                    app.modifiers = state;
                    app.on_modifiers_changed();
                }
                WindowEvent::Focused(focused) => {
                    if focused {
                        app.last_focus_gain = Instant::now();
                        // Let the keyboard hook swallow the Alt+Tab straggler `Tab` on a
                        // focused web page too (the terminal is guarded in `key_term`).
                        khook::note_focus_gain();
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    app.cursor_pos = (position.x, position.y);
                    // Left-drag inside the command line extends the selection.
                    app.bar_drag(position.x);
                    // Left-drag inside a terminal pane extends ITS selection.
                    app.term_select_drag(position.x, position.y);
                    // Hovering the command bar reveals the full (vs. shortened) URL.
                    let (_, h) = app.inner();
                    let over_bar =
                        app.bar_h() > 0 && position.y >= (h as f64 - app.bar_h() as f64);
                    if over_bar != app.bar_hover {
                        app.bar_hover = over_bar;
                        if app.mode == ModeKind::Normal {
                            app.window.request_redraw();
                        }
                    }
                }
                // The cursor left the parent client area — including moving up onto the
                // child webview, which swallows CursorMoved. Drop the bar hover so the
                // URL collapses back to its short form instead of getting stuck full.
                WindowEvent::CursorLeft { .. } => {
                    // A release over the child webview can be missed by the parent, so
                    // also end any in-progress drag-select here.
                    app.bar_dragging = false;
                    // Same for a terminal drag — but drop it WITHOUT copying: leaving
                    // the client area (often just crossing onto a sibling web pane's
                    // HWND) isn't a release, and shouldn't clobber the clipboard. What
                    // was selected stays highlighted and yankable.
                    app.term_drag = None;
                    if app.bar_hover {
                        app.bar_hover = false;
                        if app.mode == ModeKind::Normal {
                            app.window.request_redraw();
                        }
                    }
                }
                // Clicks on the top tab bar: hit a tab label to switch to it, or
                // drag the borderless window by the empty strip (QoL — like a title
                // bar). The webview owns clicks below the bar.
                WindowEvent::MouseInput {
                    state: ElementState::Released,
                    button: MouseButton::Left,
                    ..
                } => {
                    app.bar_dragging = false;
                    // Ends a terminal drag-select and copies what it covered.
                    app.term_select_end();
                }
                // Right-press on a natively-drawn surface (terminal, :read, a vim
                // pager, the command line): copy that surface's selection. Web panes
                // handle their own right-click in the page's context menu.
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Right,
                    ..
                } => app.right_click_copy(app.cursor_pos.0, app.cursor_pos.1),
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    ..
                } => {
                    let (_, h) = app.inner();
                    let bar_top = h as f64 - app.bar_h() as f64;
                    if app.bar_h() > 0 && app.cursor_pos.1 >= bar_top {
                        // Click the command/status bar: edit the URL or place the caret.
                        app.bar_click(app.cursor_pos.0);
                    } else if !app.tabs.is_empty() && app.cursor_pos.1 < app.tab_bar_h() as f64 {
                        if let Some(i) = app.tab_at_pixel(app.cursor_pos.0) {
                            app.jump_to(i);
                            app.window.request_redraw();
                        } else {
                            let _ = app.window.drag_window();
                        }
                    } else if let Some((tab, rect)) =
                        app.pane_at_pixel(app.cursor_pos.0, app.cursor_pos.1)
                    {
                        // A press on a (native) pane below the tab bar. Web panes
                        // consume the click in their own HWND, so this only fires for
                        // terminal/read/vim/blank panes; the web half of the same
                        // gesture arrives as `PaneClick`. Both go through
                        // `focus_pane_click`, which carries passthrough across the move
                        // and hands the keyboard to whichever pane now owns it.
                        if app.is_split() {
                            app.focus_pane_click(tab);
                        }
                        // Then start a drag-selection if that pane is a terminal (also
                        // when unsplit — one terminal filling the band still selects).
                        app.term_select_start(tab, rect, app.cursor_pos.0, app.cursor_pos.1);
                    }
                }
                // Mouse wheel: scroll the native-drawn content under the cursor (the
                // terminal's scrollback, or a `:read` document). Web tabs receive the
                // wheel directly via their child window, so they never reach here.
                WindowEvent::MouseWheel { delta, .. } => {
                    let dy = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y as f64,
                        MouseScrollDelta::PixelDelta(pos) => pos.y / 40.0,
                        _ => 0.0,
                    };
                    if dy != 0.0 {
                        app.on_wheel(dy);
                    }
                }
                WindowEvent::KeyboardInput { event: key, .. }
                    if key.state == ElementState::Pressed =>
                {
                    app.handle_key(&key);
                    if app.quit {
                        app.teardown();
                        *control_flow = ControlFlow::Exit;
                    }
                }
                _ => {}
            },
            Event::UserEvent(UserEvent::ExitToNormal) => app.exit_to_normal(),
            Event::UserEvent(UserEvent::SyncAdblock) => app.broadcast_adblock(),
            Event::UserEvent(UserEvent::FocusShell) => {
                match app.mode {
                    ModeKind::Hint if app.hint_act == HintAct::Scroll => {
                        app.exit_hint();
                        app.reclaim_shell_focus();
                    }
                    // Passthrough persists across navigation: re-assert it on the new
                    // page and keep the page focused.
                    ModeKind::Passthrough => {
                        app.set_page_mode("passthrough");
                        if let Some(wv) = app.active_webview() {
                            let _ = wv.focus();
                        }
                    }
                    // Insert, Caret and Scroll are tied to the old page's DOM; navigation ends
                    // them (the field/caret is gone on the new document).
                    ModeKind::Insert | ModeKind::Caret | ModeKind::Scroll | ModeKind::ScrollCaret => {
                        app.set_page_mode("normal");
                        app.mode = ModeKind::Normal;
                        app.reclaim_shell_focus();
                        app.window.request_redraw();
                    }
                    // Reclaim KEYBOARD focus from the webview child, but never steal the
                    // FOREGROUND: this fires on every page-load/`page-ready`, and a busy
                    // site (ads, video, redirects) that finishes loading after you've
                    // alt-tabbed away must not pop the window back to the front.
                    // `SetFocus` no-ops when we aren't the foreground app, so leaving is
                    // respected; the periodic `reclaim_focus_tick` restores keyboard
                    // focus when you actually return.
                    _ => app.reclaim_shell_focus(),
                }
                // A navigation (e.g. clicking a link that yielded focus) lands on a
                // fresh page with the shell back in control — end any page-focus yield.
                app.page_focus_yielded = false;
                // The old page's hovered-link readout is stale on a new document.
                app.hover_link = None;
                // A fresh navigation can reset the page's zoom factor — re-apply.
                app.apply_active_zoom();
                // Track the post-navigation URL in the status bar.
                app.refresh_active_url();
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::GrabFocus) => {
                // Only in Normal mode: the shell owns the keyboard there. In
                // Insert/Passthrough the page legitimately holds focus. A bare-area
                // click ends any prior page-focus yield.
                if app.mode == ModeKind::Normal {
                    app.page_focus_yielded = false;
                    // The gesture resolved to "the shell keeps the keyboard": end the
                    // grace so the poll guards this page again immediately.
                    app.page_gesture_at = None;
                    app.reclaim_shell_focus();
                }
            }
            // A click on a page control left focus in the page so its menu stays open.
            Event::UserEvent(UserEvent::PageHold) => {
                if app.mode == ModeKind::Normal && app.active_webview().is_some() {
                    app.page_focus_yielded = true;
                    app.window.request_redraw();
                }
            }
            // A click landed in a text field: enter Insert so typing goes to the page
            // (and clicking away later drops back to Normal on its own).
            Event::UserEvent(UserEvent::PageEdit) => {
                if app.mode == ModeKind::Normal {
                    app.page_focus_yielded = false;
                    app.enter_insert();
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::LinkHover(href)) => {
                // Only the focused tab's hover readout matters; ignore stray reports
                // from a background pane. Empty = the pointer left the link.
                let next = (!href.is_empty()).then_some(href);
                if app.hover_link != next {
                    app.hover_link = next;
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::ReclaimNormal) => app.reclaim_from_page(),
            Event::UserEvent(UserEvent::ReplayToShell(key)) => {
                // Only while still in Normal: a key queued behind one that changed
                // mode (`:` opening the command bar) is already on its way to the
                // shell, which holds focus by then.
                if app.mode == ModeKind::Normal {
                    app.reclaim_from_page();
                    // Update the hook now, so keys typed right behind this one aren't
                    // taken from the page a second time.
                    khook::set_mode(app.hook_mode_code());
                }
                khook::replay(key);
            }
            Event::UserEvent(UserEvent::PaneClick) => {
                // A gesture is under way in the page: hold the focus-reclaim poll off
                // until it has finished and the bridge has said who should keep the
                // keyboard (see `reclaim_focus_tick`).
                app.page_gesture_at = Some(Instant::now());
                // Clicking inside a web pane focuses it (web panes consume the click in
                // their HWND, so this is the only way they reach us). Use the OS cursor
                // position, since CursorMoved isn't delivered over a child webview.
                // `focus_pane_click` only marks the pane active — it must NOT reclaim the
                // keyboard, or it'd yank focus straight back off the page you just
                // clicked, leaving the pane merely "selected" until a second click.
                if app.is_split() {
                    if let Some((x, y)) = app.cursor_client_pos() {
                        if let Some((tab, _)) = app.pane_at_pixel(x as f64, y as f64) {
                            app.focus_pane_click(tab);
                        }
                    }
                }
            }
            Event::UserEvent(UserEvent::ExitHint) => {
                app.hint_input.clear();
                app.hint_act = HintAct::Follow;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::ScrollSelected) => {
                if app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll {
                    app.hint_input.clear();
                    app.hint_act = HintAct::Follow;
                    app.mode = ModeKind::Scroll;
                    app.set_page_mode("scroll");
                    app.reclaim_shell_focus();
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::ScrollExit) => {
                if matches!(app.mode, ModeKind::Scroll | ModeKind::ScrollCaret) {
                    app.exit_to_normal();
                    app.set_status("scroll target is no longer available");
                } else if app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll {
                    app.exit_hint();
                    app.set_status("no scrollable boxes on screen");
                }
            }
            Event::UserEvent(UserEvent::HintEdit) => {
                // The hint selected a text field: enter Insert (type into the page),
                // then focus the field itself within the page.
                app.hint_input.clear();
                app.enter_insert();
                if let Some(wv) = app.active_webview() {
                    let _ = wv.evaluate_script(
                        "window.__hintTarget&&(window.__hintTarget.focus(),window.__hintTarget=null)",
                    );
                }
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::HintOpen(url)) => {
                // The page already cleared its badges; just reset shell hint state
                // and open the link in a fresh tab.
                app.hint_input.clear();
                app.hint_act = HintAct::Follow;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.open_tab(&url, app.nojs, true);
            }
            Event::UserEvent(UserEvent::HintCopy(url)) => {
                // Copy mode (`yf`): the page already cleared its badges — reset the
                // shell's hint state, take the keyboard back, and yank the address.
                app.hint_input.clear();
                app.hint_act = HintAct::Follow;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.copy_text(&url);
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::ReadReady { doc, replace, record }) => {
                app.show_read_document(*doc, replace, record);
            }
            Event::UserEvent(UserEvent::ReadFailed(e)) => {
                app.set_error(format!("read failed: {e}"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::Navigate(url)) => {
                if let Some(i) = app.active {
                    // Shell-driven `load_url` reads as not-user-initiated; stamp intent so
                    // the native guard lets this de-proxy redirect through.
                    if let Some(wv) = app.tabs.get(i).and_then(|t| t.webview()) {
                        let _ = wv.load_url(&url);
                    }
                    // Reflect the de-proxied address in the status bar right away
                    // (the live URL refresh on page-load will confirm it).
                    if let Some(t) = app.tabs.get_mut(i) {
                        t.url = url;
                    }
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::RedirectBlocked(url)) => {
                // Show the destination we refused, truncated so a long tracking URL
                // doesn't blow out the status bar.
                let short: String = url.chars().take(80).collect();
                app.set_status(format!("blocked redirect → {short}"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::PopupBlocked(url)) => {
                let short: String = url.chars().take(80).collect();
                app.set_status(format!("blocked pop-up → {short}  (:ads off to allow)"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::OpenPopupTab(url)) => {
                // A real new-tab click the popup guard cleared: re-open it as a managed
                // tab (new tabs here live in our tab strip, not as OS popups).
                app.open_tab(&url, app.nojs, true);
            }
            Event::UserEvent(UserEvent::BlocklistReady) => {
                // Quiet by default (don't clobber a useful status); the engine simply
                // starts catching navigations from here on.
            }
            Event::UserEvent(UserEvent::ExtensionsListed { request, view, result }) => {
                if request == app.extension_request && app.view_by_id(view).is_some() {
                    match result {
                        Ok(items) => app.show_extensions_page(view, items),
                        Err(error) => app.set_error(error),
                    }
                }
            }
            Event::UserEvent(UserEvent::DownloadBlocked(name)) => {
                let short: String = name.chars().take(60).collect();
                app.set_error(format!(
                    "blocked download of {short} — executable/installer. :downloads to allow"
                ));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::DataCleared { label, ai_id, result }) => {
                // Announce failures, including those from the bonus engine-history
                // clear. An empty label only suppresses successful bonus reports.
                if let Err(error) = result {
                    let target = if label.is_empty() { "engine history" } else { &label };
                    let message = format!("could not clear {target}: {error}");
                    let shown_in_chat = ai_id.is_some_and(|id| app.ai_note(id, &message));
                    if !shown_in_chat {
                        app.set_error(message);
                        app.window.request_redraw();
                    }
                } else if !label.is_empty() {
                    // If an :ai tab asked for it, confirm in that chat; fall back to the
                    // status bar only when that tab isn't the one on screen.
                    let shown_in_chat = ai_id.is_some_and(|id| app.ai_action_done(id, &label));
                    if !shown_in_chat {
                        app.set_status(format!("cleared {label}"));
                        app.window.request_redraw();
                    }
                }
            }
            Event::UserEvent(UserEvent::SchemeInstalled { ai_id, result }) => {
                app.finish_scheme_install(ai_id, result);
            }
            Event::UserEvent(UserEvent::RestoreDefaults) => {
                // The Ctrl+Alt+Shift+R panic chord (caught by the keyboard hook below
                // the keybind layer). Route through the same action so it's one path.
                app.run_action("restore", serde_json::json!({}));
            }
            Event::UserEvent(UserEvent::TermDone { cmd, output, code }) => {
                app.show_term_result(&cmd, &output, code);
            }
            Event::UserEvent(UserEvent::TermOutput { id, data }) => app.feed_terminal(id, &data),
            Event::UserEvent(UserEvent::CaretYank(text)) => {
                let n = text.chars().count();
                clipboard_set(&text);
                if app.mode == ModeKind::Caret {
                    app.mode = ModeKind::Normal;
                } else if app.mode == ModeKind::ScrollCaret {
                    app.mode = ModeKind::Scroll;
                }
                app.set_status(format!("yanked {n} chars"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::ClipCopy(text)) => {
                app.copy_text(&text);
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::CaretExit) => {
                if matches!(app.mode, ModeKind::Caret | ModeKind::ScrollCaret) {
                    app.mode = if app.mode == ModeKind::ScrollCaret {
                        ModeKind::Scroll
                    } else {
                        ModeKind::Normal
                    };
                    app.clear_status();
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::AiReply { id, convo, round, result }) => {
                app.ai_reply(id, convo, round, result)
            }
            Event::UserEvent(UserEvent::PageFullscreen(on)) => app.set_page_fullscreen(on),
            // An SPA navigation changed the page URL without a document load: sync the
            // stored URL (and, for real steps, the H/L back stack) + repaint the bar.
            Event::UserEvent(UserEvent::UrlChanged { record }) => {
                app.refresh_active_url_record(record);
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::Redraw) => app.window.request_redraw(),
            Event::UserEvent(UserEvent::TermClosed { id }) => app.close_term_tab(id),
            Event::UserEvent(UserEvent::Quit) => {
                app.teardown();
                *control_flow = ControlFlow::Exit;
            }
            Event::LoopDestroyed => app.teardown(),
            Event::RedrawRequested(_) => {
                if let Err(e) = app.draw() {
                    eprintln!("draw error: {e}");
                }
            }
            _ => {}
        }
        // Keep the keyboard hook's view of the mode current, so it intercepts the
        // right chords (leave passthrough/insert, Esc out of a page-focus yield).
        #[cfg(all(windows, feature = "servo-engine"))]
        let servo_smoke = engines::servo::smoke::tick(&mut app);
        khook::set_mode(app.hook_mode_code());
        if app.quit { app.teardown(); *control_flow = ControlFlow::Exit; }

        // While typing a command, keep waking to blink the cursor (unless we're
        // already exiting). Outside Command mode we stay on plain Wait.
        if !matches!(*control_flow, ControlFlow::Exit | ControlFlow::ExitWithCode(_)) {
            if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
                // Blink the command-bar cursor.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
            } else if app.mode == ModeKind::Normal && app.read_caret_active() {
                // Blink the read-mode caret's block cursor.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
            } else if app.active_pane_is_webview
                && (matches!(app.mode, ModeKind::Normal | ModeKind::Scroll | ModeKind::ScrollCaret)
                    || (app.mode == ModeKind::Hint && app.hint_act == HintAct::Scroll))
            {
                // Poll to keep keyboard focus on the shell while the FOCUSED pane is a
                // web tab (the click-focus backstop).
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(300));
            } else if app.mode == ModeKind::Normal && app.background_webview_visible {
                // A web pane is merely visible beside a focused terminal/read pane: it
                // can still trap the keyboard on a stray click, but that's rare — tick
                // slowly so working in the native pane stays near-idle. Fully idle
                // otherwise — zero wakeups on welcome/read/term-only layouts.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1000));
            } else if app.mode == ModeKind::Normal && app.active_is_res() {
                // Auto-refresh the live resource monitor about once a second.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1000));
            } else if app.mode == ModeKind::PaneResize {
                // Wake at the resize-mode idle deadline so it can auto-exit.
                *control_flow =
                    ControlFlow::WaitUntil(app.pane_resize_at + crate::app::PANE_RESIZE_TIMEOUT);
            }
            // A page is loading: wake fast enough for the tab strip's progress sweep to
            // read as motion. Merged (not chained onto the mode branches above) because
            // a load can be in flight in any mode.
            if app.any_tab_loading() {
                let deadline = Instant::now() + Duration::from_millis(60);
                let next = match *control_flow {
                    ControlFlow::WaitUntil(t) => t.min(deadline),
                    _ => deadline,
                };
                *control_flow = ControlFlow::WaitUntil(next);
            }
            // A pending status-flash auto-clear: wake at its deadline (or sooner, if
            // another timer above already wins).
            if let Some(clear_at) = app.status_clear_at {
                let next = match *control_flow {
                    ControlFlow::WaitUntil(t) => t.min(clear_at),
                    _ => clear_at,
                };
                *control_flow = ControlFlow::WaitUntil(next);
            }
            // A held-back terminal resize: wake when its settle window closes.
            if let Some(at) = app.term_resize_want_at {
                let deadline = at + crate::app::TERM_RESIZE_DEBOUNCE;
                let next = match *control_flow {
                    ControlFlow::WaitUntil(t) => t.min(deadline),
                    _ => deadline,
                };
                *control_flow = ControlFlow::WaitUntil(next);
            }
            #[cfg(all(windows, feature = "servo-engine"))]
            if servo_smoke { *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(50)); }
            #[cfg(all(windows, feature = "servo-engine"))]
            if let Some(deadline) = engines::servo::tick(&app) {
                let next = match *control_flow { ControlFlow::WaitUntil(t) => t.min(deadline), _ => deadline };
                *control_flow = ControlFlow::WaitUntil(next);
            }
        }
    }));
}
#[cfg(test)]
mod tests {
    use super::vim::{Key, TextBuffer};

    fn type_keys(b: &mut TextBuffer, chars: &str) -> Option<String> {
        let mut last = None;
        for c in chars.chars() {
            last = b.key(Key::Char(c), 20, 80).yanked;
        }
        last
    }

    #[test]
    fn yank_inside_parens_grabs_the_hresult_token() {
        let mut b = TextBuffer::new(vec!["WindowsError(HRESULT(0x8007139f))".into()]);
        b.cx = 25; // somewhere inside the inner parens
        assert_eq!(type_keys(&mut b, "yi(").as_deref(), Some("0x8007139f"));
    }

    #[test]
    fn yank_inner_word_grabs_the_whole_token() {
        let mut b = TextBuffer::new(vec!["code 0x8007139f here".into()]);
        b.cx = 8; // inside the hex token
        assert_eq!(type_keys(&mut b, "yiw").as_deref(), Some("0x8007139f"));
    }

    #[test]
    fn charwise_visual_selection_yanks_inclusively() {
        let mut b = TextBuffer::new(vec!["HRESULT(0x..)".into()]);
        assert!(b.key(Key::Char('v'), 20, 80).consumed);
        assert_eq!(b.mode_label(), Some("VISUAL"));
        for _ in 0..6 {
            b.key(Key::Char('l'), 20, 80); // cursor 0 -> 6, inclusive of char 6
        }
        let yanked = b.key(Key::Char('y'), 20, 80).yanked;
        assert_eq!(yanked.as_deref(), Some("HRESULT"));
        assert_eq!(b.mode_label(), None); // visual cleared after yank
    }

    #[test]
    fn yy_yanks_the_whole_line() {
        let mut b = TextBuffer::new(vec!["first".into(), "second".into()]);
        b.key(Key::Char('j'), 20, 80); // -> line 1
        assert_eq!(type_keys(&mut b, "yy").as_deref(), Some("second"));
    }

    #[test]
    fn np_swallowed_colon_falls_through() {
        let mut b = TextBuffer::new(vec!["x".into()]);
        // `n`/`p` are swallowed (no tab switch from the pager); `:` falls through.
        assert!(b.key(Key::Char('n'), 20, 80).consumed);
        assert!(b.key(Key::Char('p'), 20, 80).consumed);
        assert!(!b.key(Key::Char(':'), 20, 80).consumed);
    }

    #[test]
    fn find_char_moves_cursor_onto_target() {
        let mut b = TextBuffer::new(vec!["abc(def)ghi".into()]);
        type_keys(&mut b, "f("); // jump to the '('
        assert_eq!(b.cx, 3);
        type_keys(&mut b, "f)"); // jump forward to the ')'
        assert_eq!(b.cx, 7);
        type_keys(&mut b, "F("); // jump back to the '('
        assert_eq!(b.cx, 3);
    }

    #[test]
    fn till_char_stops_before_target() {
        let mut b = TextBuffer::new(vec!["abc(def)".into()]);
        type_keys(&mut b, "t("); // land just before '('
        assert_eq!(b.cx, 2);
    }

    #[test]
    fn repeat_find_with_semicolon_and_comma() {
        let mut b = TextBuffer::new(vec!["a.b.c.d".into()]);
        type_keys(&mut b, "f."); // first '.'
        assert_eq!(b.cx, 1);
        type_keys(&mut b, ";"); // next '.'
        assert_eq!(b.cx, 3);
        type_keys(&mut b, ","); // back to previous '.'
        assert_eq!(b.cx, 1);
    }

    #[test]
    fn yank_find_includes_target_till_excludes_it() {
        let mut b = TextBuffer::new(vec!["key=value;".into()]);
        assert_eq!(type_keys(&mut b, "yf;").as_deref(), Some("key=value;"));
        let mut b2 = TextBuffer::new(vec!["key=value;".into()]);
        assert_eq!(type_keys(&mut b2, "yt;").as_deref(), Some("key=value"));
    }
}
