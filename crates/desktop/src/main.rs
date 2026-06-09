//! browser-desktop — a lightweight, keyboard-driven shell that boots a WebView2
//! engine only when you open a page.
//!
//! The window chrome (welcome screen + command bar) is drawn natively with a
//! pixel buffer, so an idle shell holds NO browser engine. Opening a tab builds
//! a child WebView2 on demand; closing it drops the WebView and frees the
//! renderer immediately.
//!
//! Modes (qutebrowser-style):
//!   * Normal  — shell has focus; command bar works; j/k/space scroll the page.
//!   * Command — typing a `:`-command (entered with `:` or `o`).
//!   * Insert  — `i` hands focus to the page (e.g. to click YouTube); Esc returns.

#![windows_subsystem = "windows"]

use std::io::{Read, Write};
use std::num::NonZeroU32;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::thread::JoinHandle;

use anyhow::{Context as _, Result};
use std::time::{Duration, Instant};

use tao::event::{
    ElementState, Event, KeyEvent, MouseButton, MouseScrollDelta, StartCause, WindowEvent,
};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::keyboard::{Key, KeyCode, ModifiersState};
use tao::window::{Window, WindowBuilder};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows};

mod draw;
mod procmon;
mod pty_term;
mod read_view;
mod session;
mod vim;
use draw::Painter;

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

/// Command verbs offered by command-bar autocomplete (`:ver`→`:version`). Longest-
/// useful canonical spellings; ordered so the first prefix match is the best one.
const COMMANDS: &[&str] = &[
    "open", "tabopen", "edit", "yank", "read", "research", "reload", "resize", "res", "resources", "reopen",
    "error", "errors", "te", "term", "shell", "search", "js", "nojs", "ads", "adblock",
    "popups", "pops", "mute", "audio", "css", "next", "tabnext", "tabprev",
    "prev", "back", "forward", "fullscreen", "move", "commands", "help", "version", "close",
    "write", "wq", "quit",
];

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

/// Injected into every page. Reads a synchronous `window.__mode` flag (kept in
/// sync by the shell) and, per mode, intercepts exactly the keys the shell owns.
/// In `insert` it takes Escape (leave) and Ctrl+V (to passthrough) and lets the
/// rest type; in `passthrough` it takes only Shift+Escape and lets every other key
/// reach the page. In insert it also reports when focus leaves the editable element,
/// so the shell can drop back to normal when you click away.
const BRIDGE_JS: &str = r#"
(function () {
  if (window.__shellBridge) return;
  window.__shellBridge = true;
  if (typeof window.__mode === 'undefined') window.__mode = 'normal';
  function post(m) { if (window.ipc) window.ipc.postMessage(m); }
  function editable(el) {
    if (!el) return false;
    var tag = el.tagName;
    if (tag === 'TEXTAREA' || tag === 'SELECT') return true;
    if (tag === 'INPUT') {
      var t = (el.getAttribute('type') || 'text').toLowerCase();
      return ['button','submit','reset','checkbox','radio','file','image','range','color','hidden']
        .indexOf(t) === -1;
    }
    return !!el.isContentEditable;
  }
  window.__shellEditable = editable;
  document.addEventListener('keydown', function (e) {
    var m = window.__mode;
    if (m === 'insert') {
      if (e.key === 'Escape' && !e.shiftKey) { e.preventDefault(); e.stopPropagation(); post('insert-escape'); }
      else if (e.ctrlKey && (e.key === 'v' || e.key === 'V')) { e.preventDefault(); e.stopPropagation(); post('to-passthrough'); }
    } else if (m === 'passthrough') {
      if (e.key === 'Escape' && e.shiftKey) { e.preventDefault(); e.stopPropagation(); post('leave-passthrough'); }
    }
  }, true);
  document.addEventListener('focusout', function () {
    if (window.__mode !== 'insert') return;
    setTimeout(function () {
      var a = document.activeElement;
      if (!a || !editable(a)) post('insert-blur');
    }, 0);
  }, true);
  // In Normal mode the shell owns the keyboard. A click — or a script calling
  // .focus() (common on SPAs like YouTube Shorts) — can move OS keyboard focus
  // into the page and lock the user out of shell keys (':' / Esc). Bounce it back
  // to the shell. Throttled so a page that keeps re-grabbing focus can't spin.
  var __lastGrab = 0;
  function grabBack() {
    if (window.__mode && window.__mode !== 'normal') return;
    var now = Date.now();
    if (now - __lastGrab < 200) return;
    __lastGrab = now;
    // Defer past the current gesture so the webview has actually taken focus by
    // the time the shell calls SetFocus to take it back (avoids a focus race).
    setTimeout(function () { post('grab-focus'); }, 0);
  }
  // Reclaim on `click`, which is the END of the gesture (mousedown→mouseup→click),
  // and only via setTimeout so the page's own click handlers run first. Reclaiming
  // on `mousedown` (mid-gesture) used to eat the click — a single click did nothing
  // and you had to double-click thumbnails / the hover mute & caption buttons. A
  // bare body click bubbles a `click` to the document too, so this still covers the
  // "clicked empty page, lost the keyboard" case. A script `.focus()` with no click
  // is caught instead by the shell's periodic focus-reclaim tick.
  document.addEventListener('click', grabBack, true);
  // Tell the shell once the page is up so it can reclaim keyboard focus — works
  // for both URL and with_html content, independent of native load events.
  window.addEventListener('load', function () { post('page-ready'); });
  // Mirror HTML fullscreen (e.g. clicking YouTube's fullscreen button) to the
  // shell: it fullscreens the window so the page fills the screen and the bars
  // hide. wry exposes no native fullscreen-element event on Windows, so we detect
  // it here. (`webkit`-prefixed for older players that fire only that.)
  function fsPost() { post(document.fullscreenElement ? 'fs-enter' : 'fs-exit'); }
  document.addEventListener('fullscreenchange', fsPost);
  document.addEventListener('webkitfullscreenchange', fsPost);
})();
"#;

/// Injected on demand to drive hint mode. Defines `window.__hintShow/Input/Clear`.
/// The shell collects the typed label and calls `__hintInput`; the page filters
/// badges and, on an exact match, clicks the target and reports back via IPC.
const HINT_JS: &str = r#"
(function () {
  if (window.__hintClear) window.__hintClear();
  // Wake auto-hiding media controls (YouTube's player hides the skip-ad/settings
  // buttons until the mouse moves over it) so they're present to label & click.
  try {
    var pl = document.querySelector('.html5-video-player');
    if (pl) {
      var pr = pl.getBoundingClientRect();
      pl.dispatchEvent(new MouseEvent('mousemove',
        { bubbles: true, clientX: pr.left + pr.width / 2, clientY: pr.top + pr.height / 2 }));
    }
  } catch (e) {}
  var chars = "asdfghjkl";
  var sel = "a[href], button, input:not([type=hidden]):not([disabled]), textarea, " +
            "select, [onclick], [role='button'], [role='link'], [tabindex]:not([tabindex='-1'])";
  var els = Array.prototype.slice.call(document.querySelectorAll(sel)).filter(function (el) {
    var r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return false;
    if (r.bottom < 0 || r.right < 0 || r.top > innerHeight || r.left > innerWidth) return false;
    var st = getComputedStyle(el);
    return st.visibility !== 'hidden' && st.display !== 'none';
  });
  function gen(n) {
    if (n === 0) return [];
    var width = 1, cap = chars.length;
    while (cap < n) { width++; cap *= chars.length; }
    var out = [];
    for (var i = 0; i < n; i++) {
      var s = '', x = i;
      for (var w = 0; w < width; w++) { s = chars[x % chars.length] + s; x = Math.floor(x / chars.length); }
      out.push(s);
    }
    return out;
  }
  var labels = gen(els.length);
  var box = document.createElement('div');
  box.id = '__hint_box';
  var map = {};
  for (var i = 0; i < els.length; i++) {
    var r = els[i].getBoundingClientRect();
    var b = document.createElement('span');
    // New-tab mode (`F`) shows labels uppercase as a cue; matching stays lowercase.
    b.textContent = window.__hintUpper ? labels[i].toUpperCase() : labels[i];
    b.style.cssText = 'position:fixed;left:' + Math.max(0, r.left) + 'px;top:' + Math.max(0, r.top) +
      'px;z-index:2147483647;background:#ffd400;color:#000;font:bold 11px monospace;padding:0 3px;' +
      'border:1px solid #000;border-radius:3px;line-height:14px;pointer-events:none;';
    box.appendChild(b);
    map[labels[i]] = { el: els[i], badge: b };
  }
  document.documentElement.appendChild(box);
  window.__hintMap = map;
  window.__hintClear = function () {
    var x = document.getElementById('__hint_box');
    if (x) x.remove();
    window.__hintMap = null;
  };
  function editable(el) {
    if (!el) return false;
    var tag = el.tagName;
    if (tag === 'TEXTAREA' || tag === 'SELECT') return true;
    if (tag === 'INPUT') {
      var t = (el.getAttribute('type') || 'text').toLowerCase();
      return ['button','submit','reset','checkbox','radio','file','image','range','color','hidden']
        .indexOf(t) === -1;
    }
    return !!el.isContentEditable;
  }
  // Dispatch a full pointer+mouse press/release/click at the element's center. A bare
  // `.click()` is ignored by some custom elements (YouTube's Polymer skip-ad/settings
  // buttons listen for pointer/mouse events), so we synthesize the whole sequence.
  function fireClick(el) {
    var r = el.getBoundingClientRect();
    var o = { bubbles: true, cancelable: true, view: window,
              clientX: r.left + r.width / 2, clientY: r.top + r.height / 2, button: 0 };
    ['pointerdown','mousedown','pointerup','mouseup','click'].forEach(function (t) {
      var C = t.indexOf('pointer') === 0 ? (window.PointerEvent || MouseEvent) : MouseEvent;
      try { el.dispatchEvent(new C(t, o)); } catch (e) {}
    });
  }
  // Activate a hinted target. Real links are followed by NAVIGATING to their href —
  // reliable across SPA routers (YouTube thumbnails intercept clicks and a synthetic
  // one often does nothing). Everything else gets the full click sequence.
  function activate(el) {
    var a = el.closest ? el.closest('a[href]') : (el.tagName === 'A' ? el : null);
    if (a && a.href && !/^javascript:/i.test(a.href)) {
      var here = location.href.split('#')[0];
      // A same-page hash link: let the click scroll instead of reloading.
      if (a.href.indexOf('#') !== -1 && a.href.split('#')[0] === here) { fireClick(el); return; }
      location.href = a.href;
      return;
    }
    try { el.focus(); } catch (e) {}
    fireClick(el);
  }
  window.__hintInput = function (s, nt) {
    var m = window.__hintMap; if (!m) return;
    window.__hintUpper = !!nt;
    s = (s || '').toLowerCase();
    var exact = null;
    for (var k in m) {
      // Keep the badge text in sync with the mode (UPPERCASE once new-tab is set),
      // so a plain-`f` hint that flips to new-tab mid-typing repaints its labels.
      m[k].badge.textContent = nt ? k.toUpperCase() : k;
      if (k.indexOf(s) === 0) { m[k].badge.style.display = ''; if (k === s) exact = m[k]; }
      else { m[k].badge.style.display = 'none'; }
    }
    if (exact) {
      var el = exact.el;
      var edit = editable(el);
      // For new-tab mode, resolve the link href (if any) before clearing badges.
      var a = !edit && el.closest ? el.closest('a[href]') : (el.tagName === 'A' ? el : null);
      var href = (a && a.href && !/^javascript:/i.test(a.href)) ? a.href : null;
      window.__hintClear();
      if (edit) {
        // Defer focusing until the shell has handed the webview OS focus, so the
        // field (not the document body) ends up focused; then enter passthrough.
        window.__hintTarget = el;
        if (window.ipc) window.ipc.postMessage('hint-edit');
      } else if (nt && href) {
        // New-tab mode on a real link: let the shell open it as a new tab.
        if (window.ipc) window.ipc.postMessage('hint-open:' + href);
      } else {
        activate(el);
        if (window.ipc) window.ipc.postMessage('hint-exit');
      }
    }
  };
})();
"#;

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

/// uBlock-style content blocker, injected at document-start into every web tab
/// while adblock is on. wry exposes no sub-resource request blocker (see
/// [`RESEARCH_JS`]), so we do it page-side in three layers:
///   * network — wrap `fetch`/`XHR`/`sendBeacon` so requests to known ad/tracker
///     hosts resolve empty instead of hitting the network;
///   * DOM — a MutationObserver removes script/iframe/img/ins nodes that load from
///     those hosts (and `ins.adsbygoogle`) as the page builds itself;
///   * cosmetic — a `<style>` hides generic ad containers (EasyList-ish), plus
///     YouTube's ad slots, and a 400ms tick skips/seeks past YouTube video ads.
///
/// All three honour a live `on` flag: the shell flips it via `window.__setAdblock`
/// on `:ads` (no reload needed), and bakes the initial value as `__adblockDefault`
/// per tab so new tabs start in the current state.
const ADBLOCK_JS: &str = r#"
(function () {
  if (window.__adblockInit) return;
  window.__adblockInit = true;
  var on = (typeof window.__adblockDefault === 'undefined') ? true : !!window.__adblockDefault;

  // Hostnames / URL fragments to drop (substring match, lower-cased). Kept to
  // well-known ad-exchange, analytics and tracker endpoints to avoid false hits.
  var HOSTS = [
    'doubleclick.net','googlesyndication.com','googleadservices.com',
    'google-analytics.com','googletagmanager.com','googletagservices.com',
    'adservice.google.','pagead2.googlesyndication','amazon-adsystem.com',
    'adnxs.com','adsrvr.org','rubiconproject.com','pubmatic.com','openx.net',
    'criteo.com','criteo.net','taboola.com','outbrain.com','scorecardresearch.com',
    'quantserve.com','moatads.com','adcolony.com','applovin.com','zedo.com',
    'bidswitch.net','casalemedia.com','sharethrough.com','smartadserver.com',
    'teads.tv','3lift.com','yieldmo.com','contextweb.com','gumgum.com',
    'indexww.com','media.net','mgid.com','revcontent.com','adform.net',
    'adroll.com','bluekai.com','demdex.net','everesttech.net','rlcdn.com',
    'agkn.com','crwdcntrl.net','mathtag.com','adsafeprotected.com',
    'serving-sys.com','flashtalking.com','servedbyadbutler.com',
    'hotjar.com','mixpanel.com','segment.io','amplitude.com','branch.io',
    'onesignal.com','clarity.ms','fullstory.com','heap.io','nr-data.net',
    'bugsnag.com','optimizely.com','chartbeat.com','parsely.com',
    'permutive.com','cxense.com','facebook.net/en_us/fbevents','analytics.tiktok',
    'ads.linkedin.com','ads.pinterest.com','ads.yahoo.com',
    '/pagead/','/adsbygoogle','/gampad/','/securepubads',
    'youtube.com/api/stats/ads','youtube.com/ptracking','/get_midroll_'
  ];
  function blocked(url) {
    if (!on || !url) return false;
    try {
      var u = String(url).toLowerCase();
      for (var i = 0; i < HOSTS.length; i++) if (u.indexOf(HOSTS[i]) !== -1) return true;
    } catch (e) {}
    return false;
  }

  // --- network: make ad/tracker requests resolve empty instead of loading ------
  var _fetch = window.fetch;
  if (_fetch) {
    window.fetch = function (input) {
      var url = (input && input.url) ? input.url : input;
      // 204 must carry a NULL body (an empty string throws in the Response ctor).
      if (blocked(url)) return Promise.resolve(new Response(null, { status: 204, statusText: 'No Content' }));
      return _fetch.apply(this, arguments);
    };
  }
  var _open = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function (method, url) {
    this.__adBlocked = blocked(url);
    return _open.apply(this, arguments);
  };
  var _send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function () {
    if (this.__adBlocked) { try { this.abort(); } catch (e) {} return; }
    return _send.apply(this, arguments);
  };
  var _beacon = navigator.sendBeacon && navigator.sendBeacon.bind(navigator);
  if (_beacon) {
    navigator.sendBeacon = function (url) { return blocked(url) ? true : _beacon.apply(null, arguments); };
  }

  // --- DOM: strip nodes that pull in ad URLs (and adsbygoogle <ins>) -----------
  function adNode(n) {
    if (!on || n.nodeType !== 1) return false;
    var tag = n.tagName;
    if (tag === 'SCRIPT' || tag === 'IFRAME' || tag === 'IMG' || tag === 'EMBED' ||
        tag === 'OBJECT' || tag === 'SOURCE') {
      var s = n.getAttribute('src') || n.getAttribute('data-src') ||
              n.getAttribute('data') || '';
      if (blocked(s)) return true;
    }
    if (tag === 'INS' && /adsbygoogle/i.test(n.className)) return true;
    return false;
  }
  function sweep(root) {
    if (!on) return;
    try {
      var r = (root && root.querySelectorAll) ? root : document;
      var hits = r.querySelectorAll('script,iframe,img,embed,object,ins,source');
      for (var i = 0; i < hits.length; i++) if (adNode(hits[i])) hits[i].remove();
    } catch (e) {}
  }
  function observe() {
    if (!document.documentElement) { setTimeout(observe, 0); return; }
    new MutationObserver(function (muts) {
      if (!on) return;
      for (var i = 0; i < muts.length; i++) {
        var a = muts[i].addedNodes;
        for (var j = 0; j < a.length; j++) {
          var n = a[j];
          if (n.nodeType !== 1) continue;
          if (adNode(n)) { n.remove(); continue; }
          sweep(n);
          hideCosmetic(n);
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
  var YT_AD_KEYS = ['adPlacements', 'playerAds', 'adSlots', 'adBreakHeartbeatParams',
                    'adParams', 'importantHeaders'];
  function prunePlayerAds(data) {
    if (!on || !data || typeof data !== 'object') return data;
    try {
      for (var i = 0; i < YT_AD_KEYS.length; i++) {
        if (data[YT_AD_KEYS[i]] != null) delete data[YT_AD_KEYS[i]];
      }
      if (data.playerResponse) prunePlayerAds(data.playerResponse);
      if (Array.isArray(data)) for (var j = 0; j < data.length; j++) prunePlayerAds(data[j]);
    } catch (e) {}
    return data;
  }
  if (isYT) {
    var _parse = JSON.parse;
    JSON.parse = function () { return prunePlayerAds(_parse.apply(this, arguments)); };
    var _rjson = Response.prototype.json;
    Response.prototype.json = function () {
      var pr = _rjson.apply(this, arguments);
      return on ? pr.then(prunePlayerAds) : pr;
    };
    // The first full page load embeds the response as an inline `ytInitialPlayerResponse`
    // global (not via JSON.parse). Our init script runs first, so define a setter that
    // prunes it the moment YouTube assigns it. Kept configurable so YT can redefine it.
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
  function youtube() {
    if (!on || location.hostname.indexOf('youtube.com') === -1) return;
    try {
      var skip = document.querySelector(
        '.ytp-ad-skip-button, .ytp-ad-skip-button-modern, .ytp-skip-ad-button');
      if (skip) skip.click();
      var player = document.querySelector('.html5-video-player');
      if (player && player.classList.contains('ad-showing')) {
        var v = document.querySelector('video.html5-main-video');
        if (v && v.duration && isFinite(v.duration)) { v.muted = true; v.currentTime = v.duration; }
      }
      var close = document.querySelector('.ytp-ad-overlay-close-button');
      if (close) close.click();
    } catch (e) {}
  }

  // One periodic pass covers SPA navigations and lazily-inserted ads that slip the
  // observer; cheap (a couple of querySelectorAll calls).
  function tick() { if (!on) return; hideCosmetic(document); youtube(); }
  hideCosmetic(document);
  document.addEventListener('DOMContentLoaded', function () { sweep(document); hideCosmetic(document); });
  setInterval(tick, 500);

  // --- live toggle from the shell (`:ads`), no reload needed -------------------
  window.__setAdblock = function (v) {
    on = !!v;
    if (on) { sweep(document); hideCosmetic(document); youtube(); }
    else { unhideCosmetic(); }
  };
})();
"#;

/// Live page-feature toggles, injected into every web tab. Three independent flags,
/// each seeded from `window.__featureDefaults` (baked per tab from the shell's state)
/// and flipped live by `window.__setToggle(name, on)` — no reload:
///   * `popups` — neuter `window.open` so scripted pop-ups can't spawn windows.
///   * `mute`   — keep every `<video>`/`<audio>` muted (observed + a 1s safety tick).
///   * `css`    — disable every stylesheet/`<style>`/`<link rel=stylesheet>`.
///
/// Mirrors the `:ads` pattern so `:pops`/`:mute`/`:css` apply instantly to all tabs.
const FEATURES_JS: &str = r#"
(function () {
  if (window.__featuresInit) return;
  window.__featuresInit = true;
  var d = window.__featureDefaults || {};
  var blockPopups = !!d.popups, muted = !!d.mute, noCss = !!d.css;

  // popups: scripted window.open returns null while blocking.
  var _open = window.open ? window.open.bind(window) : null;
  if (_open) {
    window.open = function () { return blockPopups ? null : _open.apply(null, arguments); };
  }

  // audio: force every media element's muted flag to match the toggle.
  function applyMute(root, val) {
    try {
      var r = (root && root.querySelectorAll) ? root : document;
      var m = r.querySelectorAll('video,audio');
      for (var i = 0; i < m.length; i++) m[i].muted = val;
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
        }
      }
    }).observe(document.documentElement, { childList: true, subtree: true });
  }
  observe();
  applyCss();
  document.addEventListener('DOMContentLoaded', function () { applyCss(); if (muted) applyMute(document, true); });
  setInterval(function () { if (muted) applyMute(document, true); }, 1000);

  window.__setToggle = function (name, val) {
    val = !!val;
    if (name === 'popups') blockPopups = val;
    else if (name === 'mute') { muted = val; applyMute(document, val); }
    else if (name === 'css') { noCss = val; applyCss(); }
  };
})();
"#;

// Page-side caret/visual browsing (TODO: vim selection on live web pages). The
// shell forwards motions here; we drive a real DOM Selection via `Selection.modify`
// (Chromium supports character/word/line/lineboundary/documentboundary), draw a thin
// caret bar at the focus end, and post the yanked text back over IPC. `__caretEnter`
// places the caret at the viewport center and starts visual; `__caretKey` moves or
// extends (visual toggles which); `__caretYank` copies; `__caretEsc` collapses then
// exits (posting `caret-exit` so the shell leaves caret mode).
const CARET_JS: &str = r#"
(function () {
  if (window.__caretInit) return;
  window.__caretInit = true;
  var on = false, visual = false, pend = '', bar = null;
  function sel() { return window.getSelection(); }
  function ensureBar() {
    if (bar && bar.isConnected) return bar;
    bar = document.createElement('div');
    bar.style.cssText = 'position:fixed;z-index:2147483647;width:2px;background:#6cb6ff;' +
      'box-shadow:0 0 3px #6cb6ff;pointer-events:none;display:none';
    (document.body || document.documentElement).appendChild(bar);
    return bar;
  }
  function focusRect() {
    var s = sel();
    if (s.rangeCount === 0 || !s.focusNode) return null;
    try {
      var r = document.createRange();
      r.setStart(s.focusNode, s.focusOffset);
      r.collapse(true);
      var rc = r.getClientRects()[0] || r.getBoundingClientRect();
      if (rc && (rc.height || rc.width)) return rc;
      // Fall back to the parent element's box (e.g. focus on an element boundary).
      var el = s.focusNode.nodeType === 1 ? s.focusNode : s.focusNode.parentElement;
      return el ? el.getBoundingClientRect() : null;
    } catch (e) { return null; }
  }
  function updateBar() {
    var b = ensureBar();
    if (!on) { b.style.display = 'none'; return; }
    var rc = focusRect();
    if (!rc) { b.style.display = 'none'; return; }
    b.style.display = 'block';
    b.style.left = Math.max(0, rc.left) + 'px';
    b.style.top = Math.max(0, rc.top) + 'px';
    b.style.height = (rc.height || 16) + 'px';
  }
  function rangeAt(x, y) {
    if (document.caretRangeFromPoint) return document.caretRangeFromPoint(x, y);
    if (document.caretPositionFromPoint) {
      var p = document.caretPositionFromPoint(x, y);
      if (p) { var r = document.createRange(); r.setStart(p.offsetNode, p.offset); return r; }
    }
    return null;
  }
  function caretAtCenter() {
    var x = Math.floor(innerWidth / 2), r = null;
    // Probe outward from the vertical center for a TEXT node — a page's middle is
    // often whitespace (caret would land on an element, where word/line motions
    // can't move). Fall back to the first text node in the document.
    var ys = [0.5, 0.4, 0.6, 0.3, 0.7, 0.25, 0.75, 0.15, 0.85];
    for (var i = 0; i < ys.length && !r; i++) {
      var rr = rangeAt(x, Math.floor(innerHeight * ys[i]));
      if (rr && rr.startContainer && rr.startContainer.nodeType === 3) r = rr;
    }
    if (!r) {
      var root = document.body || document.documentElement;
      var tw = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, null);
      var n;
      while ((n = tw.nextNode())) { if (n.nodeValue && n.nodeValue.trim()) break; }
      r = document.createRange();
      if (n) r.setStart(n, 0); else r.selectNodeContents(root);
    }
    r.collapse(true);
    var s = sel(); s.removeAllRanges(); s.addRange(r);
  }
  window.__caretEnter = function (linewise) {
    on = true; visual = true; pend = '';
    caretAtCenter();
    if (linewise) { var s = sel(); s.modify('move','left','lineboundary'); s.modify('extend','right','lineboundary'); }
    updateBar();
  };
  window.__caretExit = function () {
    on = false; visual = false; pend = '';
    try { sel().removeAllRanges(); } catch (e) {}
    if (bar) bar.style.display = 'none';
  };
  window.__caretEsc = function () {
    if (!on) return;
    if (visual && !sel().isCollapsed) { sel().collapseToEnd(); visual = false; updateBar(); }
    else { window.__caretExit(); if (window.ipc) window.ipc.postMessage('caret-exit'); }
  };
  window.__caretYank = function () {
    var t = sel().toString();
    if (window.ipc) window.ipc.postMessage('caret-yank:' + t);
    sel().collapseToEnd(); visual = false; updateBar();
  };
  window.__caretKey = function (k) {
    if (!on) return;
    var s = sel(), alter = visual ? 'extend' : 'move';
    if (pend === 'g') { pend = ''; if (k === 'g') { s.modify(alter,'backward','documentboundary'); updateBar(); return; } }
    switch (k) {
      case 'h': s.modify(alter,'left','character'); break;
      case 'l': s.modify(alter,'right','character'); break;
      case 'j': s.modify(alter,'forward','line'); break;
      case 'k': s.modify(alter,'backward','line'); break;
      case 'w': s.modify(alter,'forward','word'); break;
      case 'e': s.modify(alter,'forward','word'); break;
      case 'b': s.modify(alter,'backward','word'); break;
      case '0': s.modify(alter,'left','lineboundary'); break;
      case '$': s.modify(alter,'right','lineboundary'); break;
      case 'G': s.modify(alter,'forward','documentboundary'); break;
      case 'g': pend = 'g'; break;
      case 'v': case 'V': visual = !visual; if (!visual) s.collapseToEnd(); break;
      default: break;
    }
    updateBar();
  };
  window.addEventListener('scroll', function () { if (on) updateBar(); }, true);
  window.addEventListener('resize', function () { if (on) updateBar(); });
})();
"#;

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

/// Events posted from webview IPC back into the event loop.
enum UserEvent {
    /// Leave insert/passthrough: move focus from the page back to the shell.
    ExitToNormal,
    /// Promote insert → passthrough (Ctrl+V while typing); the page keeps focus.
    InsertToPassthrough,
    /// Reclaim keyboard focus for the shell (e.g. after a page finishes loading
    /// and WebView2 has grabbed focus), unless the page should keep focus.
    FocusShell,
    /// The page grabbed keyboard focus (a click or a script `.focus()`) while in
    /// Normal mode — bounce it back so shell keys keep working (SPA focus trap).
    GrabFocus,
    /// A hint was activated (or the page asked to end hint mode).
    ExitHint,
    /// A hint selected an editable element: focus it and enter passthrough.
    HintEdit,
    /// A hint was activated in new-tab mode (`F`): open this URL in a new tab.
    HintOpen(String),
    /// A `:read` extraction finished: render this Document in an engine-free read
    /// tab. `replace` swaps the active read tab's doc in place (link-follow/reload)
    /// instead of opening a new tab.
    ReadReady { doc: Box<browser_core::Document>, replace: bool },
    /// A `:read` extraction failed.
    ReadFailed(String),
    /// Redirect the active tab to this URL (e.g. de-proxying a `translate.goog`
    /// navigation back to the original site).
    Navigate(String),
    /// A `:te` command finished: combined output and exit code.
    TermDone { cmd: String, output: String, code: Option<i32> },
    /// Raw output bytes from a terminal's PTY → feed to its native VT engine.
    TermOutput { id: u64, data: Vec<u8> },
    /// The terminal's shell exited (pty-host stdout EOF) → close that tab.
    TermClosed { id: u64 },
    /// Web caret mode yanked a selection → put this text on the clipboard.
    CaretYank(String),
    /// Web caret mode exited (Esc with no selection) → return the shell to Normal.
    CaretExit,
    /// The page entered (`true`) or left (`false`) HTML fullscreen (e.g. YouTube's
    /// fullscreen button) → match the window's fullscreen so the page fills the screen.
    PageFullscreen(bool),
    Quit,
}

#[derive(Clone, Copy, PartialEq)]
enum ModeKind {
    Normal,
    Command,
    /// Temporary typing in a field. The page types, but the shell still owns
    /// Escape (leave) and Ctrl+V (→ passthrough); auto-exits when focus leaves the
    /// field. Enter: `i` or a hint on an editable element.
    Insert,
    /// Every keystroke goes to the page, no exceptions; persists across clicks and
    /// navigation. Enter: Ctrl+V. Leave: Shift+Esc only.
    Passthrough,
    /// hjkl resize the window; Esc exits. Entered with `:resize`.
    Resize,
    /// hjkl move the window across the desktop; Esc exits. Entered with `:move`.
    Move,
    /// Link hints are shown; typed characters select one. Entered with `f`.
    Hint,
    /// Find-in-page: `/` opened a search prompt. Typing searches live; Enter keeps
    /// the highlights and returns to Normal (where `n`/`N` step through matches).
    Find,
    /// Caret/visual browsing on a WEB tab (`v`/`V`): the shell forwards vim motions
    /// to an injected page caret that moves/extends a real DOM Selection; `y` yanks,
    /// `Esc` collapses then exits. (Engine-free read tabs use `NativeRead.caret`.)
    Caret,
}

/// A find-in-page match on a native (read/vim) tab: a line index plus the char
/// range `[start, end)` within that line's plain text.
struct NativeMatch {
    line: usize,
    start: usize,
    end: usize,
}

/// Find-in-page state, shared across tab types. For web tabs the matches live in
/// the page (injected JS); for native read/vim tabs they live in `matches`.
#[derive(Default)]
struct FindState {
    /// The confirmed query (`n`/`N` navigate it while this is non-empty + `active`).
    query: String,
    /// Whether a search is live (highlights shown, `n`/`N` active).
    active: bool,
    /// Matches on a native tab; empty for web tabs (JS owns those).
    matches: Vec<NativeMatch>,
    /// Index of the current match within `matches`.
    current: usize,
}

/// Where a content webview gets its page from.
enum Source {
    Url(String),
    Html(String),
}

struct Tab {
    /// The engine. `None` for an engine-free read tab (rendered natively from a
    /// `Document` — no WebView2 process at all); `Some` for every other tab.
    webview: Option<WebView>,
    url: String,
    /// Whether this tab was opened with JavaScript disabled (hint mode needs JS).
    nojs: bool,
    /// Whether this is a readability "read mode" tab.
    read: bool,
    /// Whether this is a "research" tab: a normal page (JS on, images kept) with
    /// heavy media/embeds stripped on the fly for a lighter browse.
    research: bool,
    /// Present for an engine-free read tab: the extracted Document + its native
    /// layout/scroll state. Mutually exclusive with `webview`.
    native: Option<NativeRead>,
    /// Present for an engine-free `:error`/`:errors` tab: a read-only vim-style text
    /// buffer over the session error log. Mutually exclusive with `webview`.
    vim: Option<vim::TextBuffer>,
    /// Present if this tab is a native terminal (alacritty_terminal VT engine + PTY).
    term: Option<TermSession>,
}

/// State for an engine-free read tab: the extracted document, the vertical scroll
/// offset, and a cache of the laid-out lines (recomputed when the width/zoom
/// changes or the document is replaced by following a link).
struct NativeRead {
    doc: browser_core::Document,
    /// Top-of-viewport scroll offset in pixels (>= 0).
    scroll: i32,
    layout: read_view::Layout,
    /// Content width / font px the cached layout was built at; `dirty` forces a
    /// rebuild after the document is swapped (the width/px may be unchanged).
    layout_w: i32,
    layout_px: f32,
    dirty: bool,
    /// Vim caret/visual-selection state, when caret mode is active (`v`/`V`). Its
    /// `lines` mirror `layout.text_lines()` so a row maps 1:1 to a visual line;
    /// `None` means plain scroll mode.
    caret: Option<vim::TextBuffer>,
}

/// One recorded failure: when it happened, the command that triggered it (if
/// known), and the message. Rendered by `:error` / `:errors`.
struct ErrorEntry {
    /// Local wall-clock time, `HH:MM:SS`.
    time: String,
    /// The command line that raised it (e.g. `:open foo`), if any.
    command: Option<String>,
    message: String,
}

/// A placed hint label over a native read link: the typed label and target URL.
struct NativeHint {
    label: String,
    url: String,
    x: i32,
    y: i32,
}

/// A terminal tab's link to its companion `browser-pty-host` process. The ConPTY
/// lives entirely in that process; here we only hold a normal pipe + the process,
/// none of which can deadlock our exit.
struct TermSession {
    id: u64,
    child: Child,
    stdin: ChildStdin,
    /// Kill-on-close job containing the pty-host (and its conhost + shell), so
    /// closing it reaps the whole tree. 0 if jobs are unavailable.
    job: isize,
    reader: Option<JoinHandle<()>>,
    /// The native VT engine + grid this terminal renders from (no WebView2).
    pty: pty_term::PtyTerm,
}

impl TermSession {
    /// Send a framed message to the pty-host: `[kind:u8][len:u32 LE][payload]`.
    fn send(&mut self, kind: u8, payload: &[u8]) {
        let mut header = [0u8; 5];
        header[0] = kind;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let _ = self.stdin.write_all(&header);
        let _ = self.stdin.write_all(payload);
        let _ = self.stdin.flush();
    }

    /// Tear down: closing the job force-kills the pty-host + its conhost + shell;
    /// the reader then EOFs on the (normal) pipe. None of this can hang our process.
    fn shutdown(mut self) {
        #[cfg(windows)]
        if self.job != 0 {
            job::close(self.job);
        }
        drop(self.stdin); // EOF the pty-host's stdin as well
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

/// Windows job-object helpers: confine the pty-host to a kill-on-close job so the
/// OS reaps it (and its descendants) when we close the handle or the browser dies.
#[cfg(windows)]
mod job {
    use core::ffi::c_void;
    use std::mem::size_of;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Create a kill-on-close job and assign `process_handle` to it. Returns the
    /// job handle (as isize) to keep open; 0 on failure.
    pub fn create_for(process_handle: isize) -> isize {
        unsafe {
            let Ok(job) = CreateJobObjectW(None, windows::core::PCWSTR::null()) else {
                return 0;
            };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let _ = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if AssignProcessToJobObject(job, HANDLE(process_handle as *mut c_void)).is_err() {
                let _ = CloseHandle(job);
                return 0;
            }
            job.0 as isize
        }
    }

    pub fn close(job: isize) {
        unsafe {
            let _ = CloseHandle(HANDLE(job as *mut c_void));
        }
    }
}

struct App {
    window: Rc<Window>,
    // Kept alive for the lifetime of `surface`, which is created from it.
    _context: softbuffer::Context<Rc<Window>>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    painter: Painter,
    proxy: EventLoopProxy<UserEvent>,

    mode: ModeKind,
    command: String,
    /// Caret position within `command`, as a byte offset on a char boundary.
    command_cursor: usize,
    /// Selection anchor (byte offset). When `Some` and != cursor, the text between
    /// it and the caret is selected (Shift-movement extends it; typing replaces it).
    command_anchor: Option<usize>,
    /// Accumulated label characters while in Hint mode.
    hint_input: String,
    /// Whether the current hint will open its target in a NEW tab (entered with
    /// `F`, or set mid-typing when a label char is typed uppercase). Badges render
    /// uppercase as the visual cue. Links only; non-link targets click normally.
    hint_new_tab: bool,
    /// Placed hint labels for an engine-free read tab (web tabs hint via JS).
    native_hints: Vec<NativeHint>,
    status: String,
    /// Whether the current `status` is an error (rendered red instead of dim).
    status_is_error: bool,
    /// Session error log: every failure (message + the command that triggered it +
    /// a wall-clock timestamp), newest last. Inspected with `:error` (latest) and
    /// `:errors` (all), capped to avoid unbounded growth.
    errors: Vec<ErrorEntry>,
    /// The command line currently executing (`:open foo`), so a failure it raises
    /// can be attributed to it in the error log. `None` outside `run_command`.
    current_command: Option<String>,
    /// Find-in-page state (the `/` search and its matches).
    find: FindState,
    tabs: Vec<Tab>,
    active: Option<usize>,
    /// Current keyboard modifier state (tracked via ModifiersChanged).
    modifiers: ModifiersState,
    /// When true, new tabs are opened with JavaScript disabled.
    nojs: bool,
    /// When true, the uBlock-style content blocker ([`ADBLOCK_JS`]) is injected
    /// into web tabs. Toggled with `:ads`; persisted across sessions.
    adblock: bool,
    /// Live page-feature toggles ([`FEATURES_JS`]), applied to every web tab without
    /// a reload. `block_popups` neuters `window.open`; `mute` keeps all media muted;
    /// `no_css` disables every stylesheet. Session-only (reset to off each launch).
    block_popups: bool,
    mute: bool,
    no_css: bool,
    /// Shell command for `:te` (program + args), set via `:config`.
    term_command: Vec<String>,
    /// Search-engine URL template (`%s` = query) for a non-URL `:open`. Defaults
    /// to Google; change it with `:search <template>`.
    search_template: String,
    /// Monotonic id for routing PTY output to the right terminal tab.
    next_term_id: u64,
    /// Global UI zoom factor (1.0 = 100%). Scales native chrome, web content,
    /// and terminal font together.
    zoom: f64,
    /// Blink state for the command-bar cursor (toggled on a timer in Command mode).
    cursor_on: bool,
    quit: bool,
    /// Whether `teardown` has already run. It fires from multiple places (window
    /// close, `:q`, then `LoopDestroyed`); without this guard the second call would
    /// re-save the session with the now-cleared tab list, wiping the good snapshot.
    torn_down: bool,
    /// `:res` resource monitor: the previous per-pid (cpu_100ns, io_bytes) sample
    /// for computing CPU%/disk rate, and when that sample was taken. The monitor
    /// always auto-refreshes; `refresh_res` just freezes while text is selected.
    res_prev: std::collections::HashMap<u32, (u64, u64)>,
    res_at: Instant,
    /// Last known cursor position (physical px), tracked for tab-bar drag.
    cursor_pos: (f64, f64),
    /// When the window last gained focus — used to swallow the stray `Tab` that
    /// Alt+Tab delivers to a focused terminal.
    last_focus_gain: Instant,
    /// Visited URLs, most-recent first (deduped, capped). Drives command-bar
    /// autocomplete for `:open <partial>` and is persisted in the session.
    history: Vec<String>,
    /// Recently-closed restorable tabs (kind + url), newest last. `U` /
    /// Ctrl+Shift+T pops the most recent and reopens it. Internal pages (the error/
    /// res/version vim tabs, `browser://…`) are never recorded.
    closed_tabs: Vec<session::SavedTab>,
    /// True when the window's fullscreen was triggered by the page entering HTML
    /// fullscreen (YouTube's button), so leaving page fullscreen exits it again —
    /// without disturbing a fullscreen the user set manually with `:f`.
    fs_from_page: bool,
}

/// Tag this process with an explicit AppUserModelID so Windows (taskbar + Task
/// Manager) treats it and every process it spawns as one application. The id is
/// inherited by child processes, which is what collapses the WebView2 engine and
/// pty-host trees under the shell. Best-effort: any failure is ignored.
#[cfg(windows)]
fn set_app_user_model_id() {
    use windows::core::w;
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(w!("kayf.browser.Shell"));
    }
}

/// Put `text` on the system clipboard (best-effort; failures are ignored).
fn clipboard_set(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_string());
    }
}

/// Read UTF-8 text from the system clipboard, or `None` if unavailable/non-text.
fn clipboard_get() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

/// Format a "quick maths" result: drop the decimal point for whole numbers,
/// otherwise show up to 6 decimal places with trailing zeros trimmed.
fn format_number(n: f64) -> String {
    if n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Generate `n` fixed-width, prefix-free hint labels from the home-row charset
/// (matches the web HINT_JS scheme, so the muscle memory is the same).
fn hint_labels(n: usize) -> Vec<String> {
    const CH: &[u8] = b"asdfghjkl";
    if n == 0 {
        return Vec::new();
    }
    let (mut width, mut cap) = (1usize, CH.len());
    while cap < n {
        width += 1;
        cap *= CH.len();
    }
    (0..n)
        .map(|i| {
            let mut buf = vec![0u8; width];
            let mut x = i;
            for w in 0..width {
                buf[width - 1 - w] = CH[x % CH.len()];
                x /= CH.len();
            }
            String::from_utf8(buf).unwrap()
        })
        .collect()
}

/// Strip a leading `-t` / `--tab` flag from a command argument, returning
/// `(open_in_new_tab, remaining_target)`. Only matches the flag as a whole token, so
/// a target like `-test` or a URL is left untouched.
fn parse_tab_flag(rest: &str) -> (bool, &str) {
    for flag in ["-t", "--tab"] {
        if let Some(r) = rest.strip_prefix(flag) {
            if r.is_empty() || r.starts_with(char::is_whitespace) {
                return (true, r.trim_start());
            }
        }
    }
    (false, rest)
}

/// Build an engine-free native tab rendering `doc` (read mode when `read`). The
/// layout is left empty/dirty so it's laid out on the next draw at the live width.
fn native_read_tab(doc: browser_core::Document, url: String, read: bool) -> Tab {
    Tab {
        webview: None,
        url,
        nojs: false,
        read,
        research: false,
        native: Some(NativeRead {
            doc,
            scroll: 0,
            layout: read_view::Layout { lines: Vec::new(), line_h: 1, height: 0 },
            layout_w: -1,
            layout_px: 0.0,
            dirty: true,
            caret: None,
        }),
        vim: None,
        term: None,
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

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Decide up front whether we're restoring a session: only with no CLI target
    // and outside headless test runs. Load it here so the saved window geometry can
    // be applied at build time (no visible jump from the default size).
    let cli_arg = std::env::args().nth(1);
    let is_test = std::env::var("BROWSER_TEST_QUIT_MS").is_ok();
    let restore = if cli_arg.is_none() && !is_test { session::load() } else { None };

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
        hint_new_tab: false,
        native_hints: Vec::new(),
        status: String::new(),
        status_is_error: false,
        errors: Vec::new(),
        current_command: None,
        find: FindState::default(),
        tabs: Vec::new(),
        active: None,
        modifiers: ModifiersState::default(),
        nojs: false,
        adblock: true,
        block_popups: false,
        mute: false,
        no_css: false,
        term_command: vec!["nu".to_string()],
        search_template: browser_core::DEFAULT_SEARCH_URL.to_string(),
        next_term_id: 0,
        zoom: 1.0,
        cursor_on: true,
        quit: false,
        torn_down: false,
        res_prev: std::collections::HashMap::new(),
        res_at: Instant::now(),
        cursor_pos: (0.0, 0.0),
        last_focus_gain: Instant::now(),
        history: Vec::new(),
        closed_tabs: Vec::new(),
        fs_from_page: false,
    };

    // Optional: open a page immediately, e.g. `browser-desktop youtube.com`,
    // or run a command, e.g. `browser-desktop ":nojs youtube.com"`. An explicit
    // CLI target takes precedence over (and skips) session restore. With no
    // argument, restore the previous session's tabs + UI state (window geometry was
    // already applied at build time above).
    match cli_arg {
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
    }

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

    window.request_redraw();

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // Command-bar cursor blink: the WaitUntil deadline (set below while in
            // Command mode) wakes us here to flip the cursor and repaint.
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
                    app.cursor_on = !app.cursor_on;
                    app.window.request_redraw();
                } else if app.mode == ModeKind::Normal {
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
                WindowEvent::Focused(true) => app.last_focus_gain = Instant::now(),
                WindowEvent::CursorMoved { position, .. } => {
                    app.cursor_pos = (position.x, position.y);
                }
                // Clicks on the top tab bar: hit a tab label to switch to it, or
                // drag the borderless window by the empty strip (QoL — like a title
                // bar). The webview owns clicks below the bar.
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    button: MouseButton::Left,
                    ..
                } => {
                    if !app.tabs.is_empty() && app.cursor_pos.1 < app.tab_bar_h() as f64 {
                        if let Some(i) = app.tab_at_pixel(app.cursor_pos.0) {
                            app.jump_to(i);
                            app.window.request_redraw();
                        } else {
                            let _ = app.window.drag_window();
                        }
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
                WindowEvent::KeyboardInput { event: key, .. } => {
                    if key.state == ElementState::Pressed {
                        app.handle_key(&key);
                        if app.quit {
                            app.teardown();
                            *control_flow = ControlFlow::Exit;
                        }
                    }
                }
                _ => {}
            },
            Event::UserEvent(UserEvent::ExitToNormal) => app.exit_to_normal(),
            Event::UserEvent(UserEvent::InsertToPassthrough) => {
                app.mode = ModeKind::Passthrough;
                app.set_page_mode("passthrough");
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::FocusShell) => {
                match app.mode {
                    // Passthrough persists across navigation: re-assert it on the new
                    // page and keep the page focused.
                    ModeKind::Passthrough => {
                        app.set_page_mode("passthrough");
                        if let Some(wv) = app.active_webview() {
                            let _ = wv.focus();
                        }
                    }
                    // Insert and Caret are tied to the old page's DOM; a navigation
                    // ends them (the injected caret is gone on the new document).
                    ModeKind::Insert | ModeKind::Caret => {
                        app.mode = ModeKind::Normal;
                        app.window.set_focus();
                        app.window.request_redraw();
                    }
                    _ => app.window.set_focus(),
                }
                // A fresh navigation can reset the page's zoom factor — re-apply.
                app.apply_active_zoom();
                // Track the post-navigation URL in the status bar.
                app.refresh_active_url();
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::GrabFocus) => {
                // Only in Normal mode: the shell owns the keyboard there. In
                // Insert/Passthrough the page legitimately holds focus.
                if app.mode == ModeKind::Normal {
                    app.reclaim_shell_focus();
                }
            }
            Event::UserEvent(UserEvent::ExitHint) => {
                app.hint_input.clear();
                app.hint_new_tab = false;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::HintEdit) => {
                // The hint selected a text field: enter insert (temporary typing),
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
                app.hint_new_tab = false;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.open_tab(&url, app.nojs, true);
            }
            Event::UserEvent(UserEvent::ReadReady { doc, replace }) => {
                app.show_read_document(*doc, replace);
            }
            Event::UserEvent(UserEvent::ReadFailed(e)) => {
                app.set_error(format!("read failed: {e}"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::Navigate(url)) => {
                if let Some(i) = app.active {
                    if let Some(wv) = app.tabs.get(i).and_then(|t| t.webview.as_ref()) {
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
            Event::UserEvent(UserEvent::TermDone { cmd, output, code }) => {
                app.show_term_result(&cmd, &output, code);
            }
            Event::UserEvent(UserEvent::TermOutput { id, data }) => app.feed_terminal(id, &data),
            Event::UserEvent(UserEvent::CaretYank(text)) => {
                let n = text.chars().count();
                clipboard_set(&text);
                app.set_status(format!("yanked {n} chars"));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::CaretExit) => {
                if app.mode == ModeKind::Caret {
                    app.mode = ModeKind::Normal;
                    app.clear_status();
                    app.window.request_redraw();
                }
            }
            Event::UserEvent(UserEvent::PageFullscreen(on)) => app.set_page_fullscreen(on),
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
        // While typing a command, keep waking to blink the cursor (unless we're
        // already exiting). Outside Command mode we stay on plain Wait.
        if !matches!(*control_flow, ControlFlow::Exit | ControlFlow::ExitWithCode(_)) {
            if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
                // Blink the command-bar cursor.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
            } else if app.mode == ModeKind::Normal && app.active_has_webview() {
                // Poll to keep keyboard focus on the shell while a web tab is up (the
                // click-focus backstop). Idle otherwise — no wakeups on welcome/read tabs.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(300));
            } else if app.mode == ModeKind::Normal && app.active_is_res() {
                // Auto-refresh the live resource monitor about once a second.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(1000));
            }
        }
    });
}

impl App {
    // --- geometry -------------------------------------------------------------

    fn inner(&self) -> (u32, u32) {
        let s = self.window.inner_size();
        (s.width.max(1), s.height.max(1))
    }

    /// Scale a base (zoom-1.0) pixel metric by the current zoom factor.
    fn scaled(&self, base: u32) -> u32 {
        (base as f64 * self.zoom).round().max(1.0) as u32
    }

    /// Whether the window is in borderless fullscreen (`:f` / `:fullscreen`).
    fn is_fullscreen(&self) -> bool {
        self.window.fullscreen().is_some()
    }

    /// Whether the native chrome (tab bar + command/status bar) is hidden right now:
    /// true in fullscreen for an immersive page, EXCEPT while typing a command, so
    /// pressing `:` (or `/`) brings the bars back to interact, then they hide again.
    fn chrome_hidden(&self) -> bool {
        self.is_fullscreen() && !matches!(self.mode, ModeKind::Command | ModeKind::Find)
    }

    /// Command/status bar height at the current zoom (0 when chrome is hidden).
    fn bar_h(&self) -> u32 {
        if self.chrome_hidden() {
            0
        } else {
            self.scaled(BAR_H)
        }
    }

    /// Tab-bar height: present only while at least one tab is open and chrome shown.
    fn tab_bar_h(&self) -> u32 {
        if self.tabs.is_empty() || self.chrome_hidden() {
            0
        } else {
            self.scaled(TAB_BAR_H)
        }
    }

    /// Bounds for a content webview: full width, between the tab bar and command bar.
    fn content_rect(&self) -> Rect {
        let (w, h) = self.inner();
        let top = self.tab_bar_h();
        Rect {
            position: PhysicalPosition::new(0_i32, top as i32).into(),
            size: PhysicalSize::new(w, h.saturating_sub(top + self.bar_h())).into(),
        }
    }

    fn on_resize(&mut self, _w: u32, _h: u32) {
        let rect = self.content_rect();
        if let Some(wv) = self.active_webview() {
            let _ = wv.set_bounds(rect);
        }
        self.window.request_redraw();
    }

    /// Re-fit the active web tab to the current chrome layout and repaint. Called on
    /// transitions that change which bars are visible without resizing the window —
    /// entering/leaving the command bar while fullscreen — so the page grows to fill
    /// the freed space (or shrinks to make room for the bar). No-op for non-web tabs.
    fn relayout_active(&self) {
        let rect = self.content_rect();
        if let Some(wv) = self.active_webview() {
            let _ = wv.set_bounds(rect);
        }
        self.window.request_redraw();
    }

    /// Top/bottom y of the content band (between the tab bar and command bar), px.
    fn content_y_bounds(&self) -> (i32, i32) {
        let (_, h) = self.inner();
        (self.tab_bar_h() as i32, h as i32 - self.bar_h() as i32)
    }

    /// Visible height of the content band, in px (>= 1).
    fn content_view_h(&self) -> i32 {
        let (top, bottom) = self.content_y_bounds();
        (bottom - top).max(1)
    }

    /// (Re)build the active read tab's native layout when the width, zoom, or the
    /// document itself changed; then clamp the scroll to the new content height.
    /// Cheap no-op when the cache is still valid (called every frame).
    fn refresh_read_layout(&mut self) {
        let Some(i) = self.active else { return };
        if self.tabs[i].native.is_none() {
            return;
        }
        let (w, _) = self.inner();
        let cw = w as i32;
        let px = self.painter.px();
        let view = self.content_view_h();
        // Split borrow: `painter` and this tab's `native` are disjoint fields.
        let painter = &self.painter;
        let nr = self.tabs[i].native.as_mut().unwrap();
        if !nr.dirty && nr.layout_w == cw && (nr.layout_px - px).abs() < f32::EPSILON {
            return;
        }
        // Leave an 8px margin on each side (matches the draw offset).
        nr.layout = read_view::layout(&nr.doc, cw - 16, painter);
        nr.layout_w = cw;
        nr.layout_px = px;
        nr.dirty = false;
        // Re-wrapping changed the visual lines: refresh the caret's grid in place,
        // keeping its cursor/selection (clamped) so caret mode survives resize/zoom.
        if let Some(caret) = nr.caret.as_mut() {
            caret.set_lines(nr.layout.text_lines());
        }
        let max = (nr.layout.height - view).max(0);
        nr.scroll = nr.scroll.clamp(0, max);
    }

    // --- zoom -----------------------------------------------------------------

    fn zoom_by(&mut self, steps: i32) {
        self.set_zoom(self.zoom + steps as f64 * ZOOM_STEP);
    }

    fn zoom_reset(&mut self) {
        self.set_zoom(1.0);
    }

    /// Set the global zoom and apply it to every layer: the native chrome font
    /// (painter) and each web tab (WebView2 zoom). Native terminals scale with the
    /// painter too — their grid is re-sized to the new cell count on the next draw
    /// (`sync_active_term_size`). Bar/tab-bar heights scale, so the active webview is
    /// re-laid-out to fit between them.
    fn set_zoom(&mut self, factor: f64) {
        let z = ((factor.clamp(ZOOM_MIN, ZOOM_MAX)) * 100.0).round() / 100.0;
        self.zoom = z;
        self.painter.set_px(BASE_PX * z as f32);
        for tab in &self.tabs {
            if let Some(wv) = &tab.webview {
                let _ = wv.zoom(z);
            }
        }
        // Tab/command bars changed height → refit the visible page.
        let rect = self.content_rect();
        if let Some(wv) = self.active_webview() {
            let _ = wv.set_bounds(rect);
        }
        self.set_status(format!("zoom {}%", (z * 100.0).round() as i32));
        self.window.request_redraw();
    }

    /// Pull keyboard focus back to the shell window. After a click, the top-level
    /// window is still foreground (only the *child* webview HWND grabbed keyboard
    /// focus), and tao's `set_focus` is a no-op when already foreground — so we
    /// must `SetFocus` the parent HWND directly to take the keyboard off the child.
    #[cfg(windows)]
    fn reclaim_shell_focus(&self) {
        use tao::platform::windows::WindowExtWindows;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        let hwnd = self.window.hwnd();
        unsafe {
            let _ = SetFocus(Some(HWND(hwnd as *mut core::ffi::c_void)));
        }
    }

    #[cfg(not(windows))]
    fn reclaim_shell_focus(&self) {
        self.window.set_focus();
    }

    /// Backstop for the click-focus trap (TODO #1). In Normal mode the shell must
    /// own the keyboard, but a click can hand keyboard focus to the webview child
    /// and lock the user out of shell keys (`:` / `Esc` / `hjkl`) until they alt-tab.
    /// The injected `BRIDGE_JS` bounces most clicks back immediately, but it misses
    /// cases — clicks inside cross-origin iframes (no `window.ipc` there) and
    /// re-steals within its throttle window. This runs on a low-frequency timer
    /// (only while a web tab is active in Normal mode) and pulls focus back whenever
    /// we're the foreground app yet the shell window doesn't hold keyboard focus.
    /// No-op when we already have focus, so it's idle the rest of the time.
    #[cfg(windows)]
    fn reclaim_focus_tick(&self) {
        use tao::platform::windows::WindowExtWindows;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let hwnd = HWND(self.window.hwnd() as *mut core::ffi::c_void);
        unsafe {
            // Don't fight for focus while the user has switched to another app.
            if GetForegroundWindow() != hwnd {
                return;
            }
            // Already own keyboard focus → nothing to reclaim (the common case).
            if GetFocus() == hwnd {
                return;
            }
            let _ = SetFocus(Some(hwnd));
        }
    }

    #[cfg(not(windows))]
    fn reclaim_focus_tick(&self) {}

    /// Whether the active tab is a webview (web/research/nojs/terminal) — i.e. one
    /// that can trap keyboard focus on click. Engine-free read/error tabs and the
    /// empty welcome screen can't, so they don't need the focus backstop.
    fn active_has_webview(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.webview.is_some())
    }

    /// Re-assert the current zoom on the active web tab (e.g. after a navigation,
    /// which can reset the WebView2 zoom factor). No-op for terminal tabs.
    fn apply_active_zoom(&self) {
        if let Some(tab) = self.active.and_then(|i| self.tabs.get(i)) {
            if tab.term.is_none() {
                if let Some(wv) = &tab.webview {
                    let _ = wv.zoom(self.zoom);
                }
            }
        }
    }

    // --- tab access -----------------------------------------------------------

    fn active_webview(&self) -> Option<&WebView> {
        self.active.and_then(|i| self.tabs.get(i)).and_then(|t| t.webview.as_ref())
    }

    /// Mutable access to the active engine-free read tab's state, if any.
    fn active_native_mut(&mut self) -> Option<&mut NativeRead> {
        self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.native.as_mut())
    }

    /// Whether the active tab is an engine-free `:error`/`:errors` vim tab.
    fn active_is_vim(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.vim.is_some())
    }

    fn active_url(&self) -> Option<&str> {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.url.as_str())
    }

    /// The live URL of the active web tab (from WebView2, so it reflects in-page
    /// navigation), falling back to the stored URL. `None` for terminal tabs.
    fn current_url(&self) -> Option<String> {
        let tab = self.tabs.get(self.active?)?;
        if tab.term.is_some() {
            return None;
        }
        // Engine-free read tab: the stored url is the canonical document URL.
        let Some(wv) = &tab.webview else {
            return Some(tab.url.clone());
        };
        if let Ok(u) = wv.url() {
            if u.starts_with("http") {
                return Some(u);
            }
        }
        Some(tab.url.clone())
    }

    /// Refresh the stored URL of the active web tab from its live WebView2 URL,
    /// so the status bar tracks navigation. Skips terminal/internal/html tabs
    /// (their live URL is a data:/about: URL, not a real address).
    fn refresh_active_url(&mut self) {
        let mut visited = None;
        if let Some(tab) = self.active.and_then(|i| self.tabs.get_mut(i)) {
            if tab.term.is_some() {
                return;
            }
            let Some(wv) = &tab.webview else { return };
            if let Ok(u) = wv.url() {
                if u.starts_with("http") {
                    tab.url = u.clone();
                    visited = Some(u);
                }
            }
        }
        if let Some(u) = visited {
            self.record_history(&u);
        }
    }

    /// Record a visited URL for autocomplete: move it to the front (most recent),
    /// de-duplicated, and cap the list. Skips internal `browser://` pages.
    fn record_history(&mut self, url: &str) {
        if url.is_empty() || url.starts_with("browser://") {
            return;
        }
        self.history.retain(|u| u != url);
        self.history.insert(0, url.to_string());
        self.history.truncate(HISTORY_CAP);
    }

    // --- input ----------------------------------------------------------------

    fn handle_key(&mut self, key: &KeyEvent) {
        match self.mode {
            ModeKind::Command | ModeKind::Find => self.key_command(key),
            ModeKind::Resize => self.key_resize(key),
            ModeKind::Move => self.key_move(key),
            ModeKind::Hint => self.key_hint(key),
            ModeKind::Caret => self.key_caret(key),
            // In Insert/Passthrough the page normally has OS focus and the injected
            // bridge handles the shell keys; these arms are fallbacks for when the
            // shell still holds focus (e.g. right after entering the mode).
            ModeKind::Insert => {
                if matches!(key.logical_key, Key::Escape) && !self.modifiers.shift_key() {
                    self.exit_to_normal();
                } else if self.modifiers.control_key() && key.physical_key == KeyCode::KeyV {
                    self.enter_passthrough();
                }
            }
            ModeKind::Passthrough => {
                if matches!(key.logical_key, Key::Escape) && self.modifiers.shift_key() {
                    self.exit_to_normal();
                } else if self.active_is_term() {
                    // Native terminal: forward every key to the PTY, except a few
                    // shell-owned chords — the global zoom keys (Ctrl +/-/0) and
                    // Ctrl+V, which pastes the clipboard into the PTY (the Windows
                    // convention) rather than sending a literal ^V.
                    if self.modifiers.control_key() {
                        match key.physical_key {
                            KeyCode::KeyV => return self.term_paste(),
                            KeyCode::Equal => return self.zoom_by(1),
                            KeyCode::Minus => return self.zoom_by(-1),
                            KeyCode::Digit0 => return self.zoom_reset(),
                            _ => {}
                        }
                    }
                    self.key_term(key);
                }
                // For a webview in passthrough the page itself has focus and the
                // injected bridge handles keys; nothing to do here.
            }
            ModeKind::Normal => self.key_normal(key),
        }
        self.window.request_redraw();
    }

    fn key_normal(&mut self, key: &KeyEvent) {
        // Once a `/` search is live, `n`/`N` step through matches and Esc clears it
        // (qutebrowser-style) — in every tab type, so this takes precedence over both
        // the vim pager and the normal tab/scroll bindings.
        if self.find.active && !self.modifiers.control_key() {
            match &key.logical_key {
                Key::Character(s) if *s == "n" => {
                    self.find_step(true);
                    return;
                }
                Key::Character(s) if *s == "N" => {
                    self.find_step(false);
                    return;
                }
                Key::Escape => {
                    self.find_clear();
                    return;
                }
                _ => {}
            }
        }
        // Terminal copy/vi mode (Shift+Esc): vi motions move a cursor over the live
        // grid, v/V select, y yanks, i/Enter resumes. Unhandled keys (`:`/x/n/p) fall
        // through to the normal browser bindings.
        if self.active_term_vi() && self.key_term_vi(key) {
            return;
        }
        // A `:error`/`:errors` tab is a read-only vim pager: let it claim the motion/
        // visual/yank keys first; anything it doesn't want (`:`, n/p, x, …) falls
        // through to the normal browser bindings below.
        if self.active_is_vim() && self.key_vim(key) {
            return;
        }
        // Read-mode caret/visual selection (engine-free read tab): once active it
        // claims motion/visual/yank keys; `v`/`V` below enter it.
        if self.read_caret_active() && self.key_read_caret(key) {
            return;
        }
        // Chords (with Ctrl) take precedence over plain keys.
        if self.modifiers.control_key() {
            match key.physical_key {
                KeyCode::KeyV => self.enter_passthrough(),
                // Reopen the last closed tab (the familiar browser shortcut).
                KeyCode::KeyT if self.modifiers.shift_key() => self.reopen_closed(),
                // Half-page scroll (vim Ctrl+D / Ctrl+U).
                KeyCode::KeyD => self.scroll(self.half_page()),
                KeyCode::KeyU => self.scroll(-self.half_page()),
                // Browser-wide zoom (native chrome + web content + terminal).
                KeyCode::Equal => self.zoom_by(1),
                KeyCode::Minus => self.zoom_by(-1),
                KeyCode::Digit0 => self.zoom_reset(),
                _ => {}
            }
            return;
        }
        match &key.logical_key {
            Key::Character(s) => match *s {
                ":" => self.enter_command(""),
                // `o` opens in THIS tab; `O` opens in a new tab (prefills `:open -t`).
                "o" => self.enter_command("open "),
                "O" => self.enter_command("open -t "),
                "j" => self.scroll(80),
                "k" => self.scroll(-80),
                // Reopen the last closed tab (vim-style undo; also Ctrl+Shift+T).
                "u" => self.reopen_closed(),
                "g" => self.scroll_edge(false),
                "G" => self.scroll_edge(true),
                "/" => self.enter_find(),
                "i" => {
                    if self.active_is_term() {
                        self.enter_passthrough();
                    } else {
                        self.enter_insert();
                    }
                }
                "f" => self.enter_hint(false),
                // Shift+F: hints open the picked link in a NEW tab (like `:open -t`).
                "F" => self.enter_hint(true),
                // Caret/visual selection — highlight & yank text with vim motions.
                // Read tabs use the native caret; web tabs (open/research) use the
                // injected page caret. Terminal tabs are excluded.
                "v" => {
                    if self.active_is_read_native() {
                        self.enter_read_caret(false);
                    } else if self.active_webview().is_some() && !self.active_is_term() {
                        self.enter_web_caret(false);
                    }
                }
                "V" => {
                    if self.active_is_read_native() {
                        self.enter_read_caret(true);
                    } else if self.active_webview().is_some() && !self.active_is_term() {
                        self.enter_web_caret(true);
                    }
                }
                "x" => self.close_active(),
                "r" => self.reload_active(),
                "H" => self.history(false),
                "L" => self.history(true),
                "n" => self.switch_tab(1),
                "p" => self.switch_tab(-1),
                "<" => self.move_tab(-1),
                ">" => self.move_tab(1),
                d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
                    let n = (d.as_bytes()[0] - b'0') as usize;
                    if n >= 1 {
                        self.jump_to(n - 1);
                    }
                }
                _ => {}
            },
            Key::ArrowDown => self.scroll(80),
            Key::ArrowUp => self.scroll(-80),
            _ => {}
        }
    }

    /// Translate a raw key event into a vim-pager [`vim::Key`], or `None` if it
    /// doesn't map (so the shell can handle it). Ctrl+D/Ctrl+U are half-page; any
    /// other Ctrl chord (zoom, …) is left for the shell. Shared by the `:error`/
    /// `:res` pagers and the read-mode caret.
    fn map_vim_key(&self, key: &KeyEvent) -> Option<vim::Key> {
        if self.modifiers.control_key() {
            return match key.physical_key {
                KeyCode::KeyD => Some(vim::Key::HalfDown),
                KeyCode::KeyU => Some(vim::Key::HalfUp),
                _ => None,
            };
        }
        match &key.logical_key {
            Key::ArrowLeft => Some(vim::Key::Left),
            Key::ArrowRight => Some(vim::Key::Right),
            Key::ArrowUp => Some(vim::Key::Up),
            Key::ArrowDown => Some(vim::Key::Down),
            Key::Home => Some(vim::Key::Home),
            Key::End => Some(vim::Key::End),
            Key::Escape => Some(vim::Key::Esc),
            Key::Character(s) => {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(vim::Key::Char(c)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Feed a key to the active `:error`/`:errors`/`:res` vim pager. Returns `true`
    /// if the pager consumed it (the shell should then do nothing else with the key).
    fn key_vim(&mut self, key: &KeyEvent) -> bool {
        let Some(vk) = self.map_vim_key(key) else { return false };
        // Viewport in cells (monospace), matching the draw geometry below.
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = ((w as usize).saturating_sub(2 * 8)) / cw;
        let rows = (self.content_view_h() as usize) / line_h;
        let Some(buf) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.vim.as_mut())
        else {
            return false;
        };
        let res = buf.key(vk, rows, cols);
        if let Some(text) = res.yanked {
            let n = text.chars().count();
            clipboard_set(&text);
            self.set_status(format!("yanked {n} chars"));
        }
        if res.consumed {
            self.window.request_redraw();
        }
        res.consumed
    }

    /// Whether the active tab is an engine-free read tab (native render).
    fn active_is_read_native(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.native.is_some())
    }

    /// Whether read-mode caret/visual selection is currently active.
    fn read_caret_active(&self) -> bool {
        self.active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.native.as_ref())
            .is_some_and(|n| n.caret.is_some())
    }

    /// Enter read-mode caret/visual selection (`v` charwise, `V` linewise): build a
    /// caret over the read view's visual lines, place it near the middle of the
    /// viewport, and start a visual selection so motions immediately highlight text.
    fn enter_read_caret(&mut self, linewise: bool) {
        // Geometry first (immutable borrows of painter), before borrowing the tab.
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (((w as usize).saturating_sub(16)) / cw).max(1);
        let view = self.content_view_h();
        let rows = (view as usize / line_h).max(1);
        let Some(nr) = self.active_native_mut() else { return };
        let lines = nr.layout.text_lines();
        if lines.iter().all(|l| l.is_empty()) {
            return;
        }
        let n = lines.len();
        let lh = nr.layout.line_h.max(1);
        // Keep the view put: the caret starts at the line already at the middle of
        // the CURRENT viewport, so entering visual doesn't scroll (it's redundant to
        // re-center on a cursor we just placed mid-screen). Seed the buffer's own
        // scroll to match, and snap the read scroll to that line boundary.
        let top_line = (nr.scroll / lh).max(0) as usize;
        let mid_line = (top_line + rows / 2).min(n.saturating_sub(1));
        let mut tb = vim::TextBuffer::new(lines);
        tb.top = top_line;
        tb.place_cursor(mid_line, 0, rows, cols);
        tb.key(if linewise { vim::Key::Char('V') } else { vim::Key::Char('v') }, rows, cols);
        nr.scroll = tb.top as i32 * lh;
        nr.caret = Some(tb);
        self.set_status("[VISUAL]  motions select · y yank · Esc exit");
        self.window.request_redraw();
    }

    /// Place a selection-less read-mode caret at `(line, col)` so the cursor sits on
    /// a specific word (e.g. a find match) and `hjkl` move relative to it instead of
    /// scrolling the page. Keeps the current scroll context.
    fn place_read_caret_at(&mut self, line: usize, col: usize) {
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (((w as usize).saturating_sub(16)) / cw).max(1);
        let rows = (self.content_view_h() as usize / line_h).max(1);
        let Some(nr) = self.active_native_mut() else { return };
        let lines = nr.layout.text_lines();
        if lines.is_empty() {
            return;
        }
        let lh = nr.layout.line_h.max(1);
        let mut tb = vim::TextBuffer::new(lines);
        tb.top = (nr.scroll / lh).max(0) as usize;
        tb.place_cursor(line, col, rows, cols);
        nr.scroll = tb.top as i32 * lh;
        nr.caret = Some(tb);
    }

    /// Feed a key to the read-mode caret. Returns `true` if it was consumed. `Esc`
    /// with no selection leaves caret mode; the read view's pixel scroll is kept in
    /// sync with the caret's line so the cursor stays on-screen.
    fn key_read_caret(&mut self, key: &KeyEvent) -> bool {
        let Some(vk) = self.map_vim_key(key) else { return false };
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (((w as usize).saturating_sub(16)) / cw).max(1);
        let rows = (self.content_view_h() as usize / line_h).max(1);

        let mut yanked: Option<String> = None;
        let mut consumed = true;
        let mut exit = false;
        {
            let Some(nr) = self.active_native_mut() else { return false };
            let Some(buf) = nr.caret.as_mut() else { return false };
            if vk == vim::Key::Esc && !buf.has_selection() {
                exit = true;
            } else {
                let res = buf.key(vk, rows, cols);
                yanked = res.yanked;
                consumed = res.consumed;
                nr.scroll = buf.top as i32 * nr.layout.line_h.max(1);
            }
            if exit {
                nr.caret = None;
            }
        }
        if let Some(text) = yanked {
            let n = text.chars().count();
            clipboard_set(&text);
            self.set_status(format!("yanked {n} chars"));
        } else if exit {
            self.clear_status();
        }
        if consumed || exit {
            self.window.request_redraw();
        }
        consumed || exit
    }

    /// Enter caret/visual browsing on a WEB tab: tell the injected page caret to
    /// place itself at the viewport center and start a visual selection. The shell
    /// keeps keyboard focus (like hint mode) and forwards motions via `key_caret`.
    fn enter_web_caret(&mut self, linewise: bool) {
        let Some(wv) = self.active_webview() else { return };
        let _ = wv.evaluate_script(&format!(
            "window.__caretEnter&&window.__caretEnter({})",
            if linewise { "true" } else { "false" }
        ));
        self.mode = ModeKind::Caret;
        self.set_status("[CARET]  hjkl/w/b/0/$/gg/G move · v select · y yank · Esc exit");
        self.window.request_redraw();
    }

    /// Forward a key to the web tab's injected caret. Motions/visual go to
    /// `__caretKey`; `y` yanks; `Esc` collapses-then-exits (the page posts
    /// `caret-exit` when it actually leaves, which returns the shell to Normal).
    fn key_caret(&mut self, key: &KeyEvent) {
        let Some(wv) = self.active_webview() else {
            self.mode = ModeKind::Normal;
            return;
        };
        match &key.logical_key {
            Key::Escape => {
                let _ = wv.evaluate_script("window.__caretEsc&&window.__caretEsc()");
            }
            Key::Character(s) => {
                if *s == "y" {
                    let _ = wv.evaluate_script("window.__caretYank&&window.__caretYank()");
                } else if matches!(
                    *s,
                    "h" | "j" | "k" | "l" | "w" | "b" | "e" | "0" | "$" | "g" | "G" | "v" | "V"
                ) {
                    let _ = wv.evaluate_script(&format!("window.__caretKey&&window.__caretKey('{s}')"));
                }
            }
            _ => {}
        }
        self.window.request_redraw();
    }

    fn key_command(&mut self, key: &KeyEvent) {
        let shift = self.modifiers.shift_key();
        // Ctrl chords: clipboard, word movement/deletion, line editing. They take
        // precedence over text input. (Alt+Backspace is handled just below.)
        if self.modifiers.control_key() {
            match key.physical_key {
                // Clipboard. Ctrl+C copies the selection (or, with nothing selected,
                // cancels — the old behavior); Ctrl+X cuts; Ctrl+V pastes.
                KeyCode::KeyC => {
                    if let Some((a, b)) = self.sel_range() {
                        clipboard_set(&self.command[a..b]);
                    } else {
                        self.cancel_command();
                        return;
                    }
                }
                KeyCode::KeyX => {
                    if let Some((a, b)) = self.sel_range() {
                        clipboard_set(&self.command[a..b]);
                        self.delete_selection();
                    }
                }
                KeyCode::KeyV => {
                    if let Some(text) = clipboard_get() {
                        self.cmd_insert(&text);
                    }
                }
                KeyCode::KeyA => {
                    self.command_anchor = Some(0);
                    self.command_cursor = self.command.len();
                }
                // Word-wise caret movement (Ctrl+Shift extends the selection).
                KeyCode::ArrowLeft => {
                    let p = self.prev_word(self.command_cursor);
                    self.move_caret(p, shift);
                }
                // Ctrl+Right accepts the autocomplete suggestion (if any) — else moves
                // a word, as before.
                KeyCode::ArrowRight => {
                    if !self.accept_suggestion() {
                        let p = self.next_word(self.command_cursor);
                        self.move_caret(p, shift);
                    }
                }
                // Ctrl+W / Ctrl+Backspace: delete the word before the caret.
                KeyCode::KeyW | KeyCode::Backspace => self.cmd_delete_word(),
                // Ctrl+Delete: delete the word after the caret.
                KeyCode::Delete => self.cmd_delete_word_forward(),
                // Ctrl+U: delete from the caret back to the start of the line.
                KeyCode::KeyU => {
                    self.command.replace_range(0..self.command_cursor, "");
                    self.command_cursor = 0;
                    self.command_anchor = None;
                }
                KeyCode::KeyH => self.cmd_backspace(),
                _ => {}
            }
            self.cursor_on = true;
            if self.mode == ModeKind::Find {
                self.find_update();
            }
            return;
        }
        // Alt+Backspace: delete the word before the caret (a common alias).
        if self.modifiers.alt_key() && key.physical_key == KeyCode::Backspace {
            self.cmd_delete_word();
            self.cursor_on = true;
            if self.mode == ModeKind::Find {
                self.find_update();
            }
            return;
        }
        match &key.logical_key {
            Key::Enter if self.mode == ModeKind::Find => {
                self.find_confirm();
                return;
            }
            // Tab accepts the autocomplete suggestion (a no-op if there isn't one).
            Key::Tab => {
                self.accept_suggestion();
            }
            Key::Enter => {
                // Quick maths: if the line is an arithmetic expression, evaluate it
                // in place — replace the bar contents with the result so you can copy
                // it or keep calculating (`20*8` → `160` → `160+10`) instead of
                // running it as a command.
                if let Some(result) = self.math_preview() {
                    self.command = result;
                    self.command_cursor = self.command.len();
                    self.command_anchor = None;
                    self.cursor_on = true;
                    self.clear_status();
                    return;
                }
                let line = std::mem::take(&mut self.command);
                self.command_cursor = 0;
                self.command_anchor = None;
                self.mode = ModeKind::Normal;
                self.run_command(&line);
                // Back to Normal re-hides the bar over a fullscreen page; refit it.
                self.relayout_active();
            }
            Key::Escape => self.cancel_command(),
            Key::Backspace => {
                if !self.delete_selection() {
                    self.cmd_backspace();
                }
            }
            Key::Delete => {
                if !self.delete_selection() {
                    self.cmd_delete_forward();
                }
            }
            // Plain arrow with a selection collapses to that edge; otherwise moves a
            // character. Shift extends (or starts) the selection.
            Key::ArrowLeft => {
                if !shift {
                    if let Some((a, _)) = self.sel_range() {
                        self.command_cursor = a;
                        self.command_anchor = None;
                    } else {
                        self.move_caret(self.prev_char(self.command_cursor), false);
                    }
                } else {
                    self.move_caret(self.prev_char(self.command_cursor), true);
                }
            }
            Key::ArrowRight => {
                if !shift {
                    if let Some((_, b)) = self.sel_range() {
                        self.command_cursor = b;
                        self.command_anchor = None;
                    } else {
                        self.move_caret(self.next_char(self.command_cursor), false);
                    }
                } else {
                    self.move_caret(self.next_char(self.command_cursor), true);
                }
            }
            Key::Home => self.move_caret(0, shift),
            Key::End => self.move_caret(self.command.len(), shift),
            Key::Space => self.cmd_insert(" "),
            Key::Character(s) => self.cmd_insert(s),
            _ => {}
        }
        // Any edit should show the cursor immediately (don't wait for the blink).
        self.cursor_on = true;
        // In Find mode, search live as the query changes.
        if self.mode == ModeKind::Find {
            self.find_update();
        }
    }

    /// Leave the command bar, discarding the line (Esc / Ctrl+C with no selection).
    /// In Find mode this also drops the search and its highlights.
    fn cancel_command(&mut self) {
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        if self.mode == ModeKind::Find {
            self.find_clear();
        }
        self.mode = ModeKind::Normal;
        // Leaving the bar re-hides it over a fullscreen page; grow the page back.
        self.relayout_active();
    }

    /// The current selection as an ordered byte range, or `None` if empty.
    fn sel_range(&self) -> Option<(usize, usize)> {
        let a = self.command_anchor?;
        let c = self.command_cursor;
        (a != c).then(|| (a.min(c), a.max(c)))
    }

    /// Move the caret to `pos`. `extend` keeps/starts a selection (Shift held);
    /// otherwise the selection is dropped. A zero-width selection is normalized away.
    fn move_caret(&mut self, pos: usize, extend: bool) {
        if extend {
            if self.command_anchor.is_none() {
                self.command_anchor = Some(self.command_cursor);
            }
        } else {
            self.command_anchor = None;
        }
        self.command_cursor = pos;
        if self.command_anchor == Some(pos) {
            self.command_anchor = None;
        }
    }

    /// Replace the selection (if any) with `text`, then place the caret after it.
    /// Control characters (e.g. newlines from a paste) are dropped — it's one line.
    fn cmd_insert(&mut self, text: &str) {
        self.delete_selection();
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        self.command.insert_str(self.command_cursor, &clean);
        self.command_cursor += clean.len();
    }

    /// Remove the selection if there is one; returns whether anything was deleted.
    fn delete_selection(&mut self) -> bool {
        if let Some((a, b)) = self.sel_range() {
            self.command.replace_range(a..b, "");
            self.command_cursor = a;
            self.command_anchor = None;
            true
        } else {
            self.command_anchor = None;
            false
        }
    }

    /// Byte offset of the char before `pos` (or `pos` if at the start).
    fn prev_char(&self, pos: usize) -> usize {
        self.command[..pos].char_indices().next_back().map(|(i, _)| i).unwrap_or(pos)
    }

    /// Byte offset just after the char at `pos` (or `pos` if at the end).
    fn next_char(&self, pos: usize) -> usize {
        self.command[pos..].chars().next().map(|c| pos + c.len_utf8()).unwrap_or(pos)
    }

    /// Start of the word before `pos` in the command line (see [`prev_word_boundary`]).
    fn prev_word(&self, pos: usize) -> usize {
        prev_word_boundary(&self.command, pos)
    }

    /// End of the word after `pos` in the command line (see [`next_word_boundary`]).
    fn next_word(&self, pos: usize) -> usize {
        next_word_boundary(&self.command, pos)
    }

    /// Delete the character before the caret (or the selection, if any).
    fn cmd_backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.prev_char(self.command_cursor);
        if start != self.command_cursor {
            self.command.replace_range(start..self.command_cursor, "");
            self.command_cursor = start;
        }
    }

    /// Delete the character after the caret (or the selection, if any).
    fn cmd_delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.next_char(self.command_cursor);
        self.command.replace_range(self.command_cursor..end, "");
    }

    /// Delete the word before the caret (or the selection, if any).
    fn cmd_delete_word(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.prev_word(self.command_cursor);
        self.command.replace_range(start..self.command_cursor, "");
        self.command_cursor = start;
    }

    /// Delete the word after the caret (or the selection, if any).
    fn cmd_delete_word_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.next_word(self.command_cursor);
        self.command.replace_range(self.command_cursor..end, "");
    }

    // --- find in page ---------------------------------------------------------

    /// Open the `/` find prompt: searches live as you type; Enter keeps the
    /// highlights (then `n`/`N` step through matches), Esc cancels.
    fn enter_find(&mut self) {
        self.find_clear();
        self.mode = ModeKind::Find;
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        self.cursor_on = true;
        // Reveal the bar over a fullscreen page (it hides again on leaving Find).
        self.relayout_active();
    }

    /// Live-update the search from the current `/` input.
    fn find_update(&mut self) {
        let q = self.command.clone();
        self.find.query = q.clone();
        self.find_search(&q, true);
        self.window.request_redraw();
    }

    /// Confirm the search: keep the highlights and enable `n`/`N`, or clear it if
    /// the query is empty (or matched nothing on a native tab).
    fn find_confirm(&mut self) {
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        self.mode = ModeKind::Normal;
        let native_empty = self.active_webview().is_none() && self.find.matches.is_empty();
        if self.find.query.is_empty() || native_empty {
            self.find_clear();
        } else {
            self.find.active = true;
            if self.active_webview().is_none() {
                self.set_status(self.find_count_label());
            }
            // On a read tab, drop a caret on the matched word so hjkl move relative to
            // it (j below, k above) instead of scrolling the page from the top.
            if self.active_is_read_native() {
                if let Some(&NativeMatch { line, start, .. }) =
                    self.find.matches.get(self.find.current)
                {
                    self.place_read_caret_at(line, start);
                }
            }
        }
        // Leaving Find re-hides the bar over a fullscreen page; refit the page.
        self.relayout_active();
    }

    /// Drop the active search and clear any highlights (page-side and native).
    fn find_clear(&mut self) {
        self.find.active = false;
        self.find.query.clear();
        self.find.matches.clear();
        self.find.current = 0;
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script("window.__findClear&&window.__findClear()");
        }
        self.window.request_redraw();
    }

    /// Reset find state without touching page highlights — used when the active tab
    /// changes (the old page's highlights fade on its own; `n`/`N` shouldn't carry).
    fn find_reset(&mut self) {
        self.find.active = false;
        self.find.query.clear();
        self.find.matches.clear();
        self.find.current = 0;
    }

    /// Run a search for `q` on the active tab (web → injected JS; read/vim → a
    /// native match list). `reveal` scrolls/moves to the first match.
    fn find_search(&mut self, q: &str, reveal: bool) {
        self.find.matches.clear();
        self.find.current = 0;
        // Web tab: hand off to the page's injected search.
        if let Some(wv) = self.active_webview() {
            let js = if q.is_empty() {
                "window.__findClear&&window.__findClear()".to_string()
            } else {
                format!("window.__find&&window.__find({})", js_string(q))
            };
            let _ = wv.evaluate_script(&js);
            return;
        }
        if q.is_empty() {
            return;
        }
        // Native tab: collect this tab's lines and match against them.
        if let Some(lines) = self.find_native_lines() {
            self.find.matches = find_in_lines(&lines, q);
            if reveal && !self.find.matches.is_empty() {
                self.find_reveal_current();
            }
        }
    }

    /// Step to the next (`forward`) or previous match.
    fn find_step(&mut self, forward: bool) {
        if !self.find.active {
            return;
        }
        if let Some(wv) = self.active_webview() {
            let js = if forward {
                "window.__findNext&&window.__findNext()"
            } else {
                "window.__findPrev&&window.__findPrev()"
            };
            let _ = wv.evaluate_script(js);
            return;
        }
        let n = self.find.matches.len();
        if n == 0 {
            return;
        }
        self.find.current =
            if forward { (self.find.current + 1) % n } else { (self.find.current + n - 1) % n };
        self.find_reveal_current();
        // Keep the read-mode caret on the current match as you step with n/N.
        if self.active_is_read_native() && self.read_caret_active() {
            if let Some(&NativeMatch { line, start, .. }) = self.find.matches.get(self.find.current) {
                self.place_read_caret_at(line, start);
            }
        }
        self.set_status(self.find_count_label());
        self.window.request_redraw();
    }

    /// The active tab's searchable lines (read = laid-out lines, vim = buffer lines).
    fn find_native_lines(&mut self) -> Option<Vec<String>> {
        let i = self.active?;
        if self.tabs.get(i).is_some_and(|t| t.native.is_some()) {
            self.refresh_read_layout();
            let nr = self.tabs[i].native.as_ref()?;
            return Some(
                nr.layout
                    .lines
                    .iter()
                    .map(|l| l.runs.iter().map(|r| r.text.as_str()).collect::<String>())
                    .collect(),
            );
        }
        let vb = self.tabs.get(i)?.vim.as_ref()?;
        Some(vb.lines.iter().map(|l| l.iter().collect::<String>()).collect())
    }

    /// Scroll a read tab (or move a vim tab's cursor) so the current match shows.
    fn find_reveal_current(&mut self) {
        let Some(&NativeMatch { line, start, .. }) = self.find.matches.get(self.find.current) else {
            return;
        };
        let view = self.content_view_h();
        let Some(i) = self.active else { return };
        if let Some(nr) = self.tabs.get_mut(i).and_then(|t| t.native.as_mut()) {
            let line_h = nr.layout.line_h.max(1);
            let max = (nr.layout.height - view).max(0);
            nr.scroll = (line as i32 * line_h - view / 2).clamp(0, max);
            return;
        }
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (w as usize).saturating_sub(16) / cw;
        let rows = (view as usize) / line_h;
        if let Some(vb) = self.tabs.get_mut(i).and_then(|t| t.vim.as_mut()) {
            vb.place_cursor(line, start, rows, cols);
        }
    }

    /// `"/query  cur/total"` (or `no matches`) for the status bar on native tabs.
    fn find_count_label(&self) -> String {
        if self.find.matches.is_empty() {
            format!("/{}  no matches", self.find.query)
        } else {
            format!("/{}  {}/{}", self.find.query, self.find.current + 1, self.find.matches.len())
        }
    }

    fn enter_command(&mut self, prefill: &str) {
        self.mode = ModeKind::Command;
        self.command = prefill.to_string();
        self.command_cursor = self.command.len();
        self.command_anchor = None;
        self.cursor_on = true;
        self.clear_status();
        // In fullscreen the bars were hidden; showing the command bar shrinks the page.
        self.relayout_active();
    }

    /// Command-bar autocomplete: the FULL completed line for the current input, or
    /// `None`. Completes a verb (`ver`→`version`) before the first space, otherwise
    /// an `:open`-style argument from visited history (`open yout`→`open youtube.com`).
    /// Only when in Command mode with the caret at the end and no selection.
    fn command_suggestion(&self) -> Option<String> {
        if self.mode != ModeKind::Command
            || self.command_cursor != self.command.len()
            || self.command_anchor.is_some()
        {
            return None;
        }
        let cmd = &self.command;
        if cmd.is_empty() {
            return None;
        }
        match cmd.split_once(char::is_whitespace) {
            // Verb completion.
            None => COMMANDS
                .iter()
                .find(|c| c.len() > cmd.len() && c.starts_with(cmd.as_str()))
                .map(|c| (*c).to_string()),
            // Argument completion from history (open-like verbs only).
            Some((verb, rest)) => {
                let rest = rest.trim();
                if rest.is_empty()
                    || !matches!(
                        verb,
                        "open" | "o" | "read" | "research" | "rs" | "nojs" | "tabopen" | "t"
                    )
                {
                    return None;
                }
                let needle = rest.to_ascii_lowercase();
                let disp = self.history.iter().map(|u| history_display(u)).find(|d| {
                    let dl = d.to_ascii_lowercase();
                    dl.starts_with(&needle) || dl.contains(&format!("/{needle}"))
                })?;
                Some(format!("{verb} {disp}"))
            }
        }
    }

    /// Accept the current autocomplete suggestion into the command line (caret to
    /// end). Returns whether there was one to accept.
    fn accept_suggestion(&mut self) -> bool {
        let Some(sug) = self.command_suggestion() else { return false };
        self.command = sug;
        self.command_cursor = self.command.len();
        self.command_anchor = None;
        self.cursor_on = true;
        true
    }

    fn enter_insert(&mut self) {
        if self.active_webview().is_none() {
            self.set_status("no page — open one first");
            return;
        }
        self.mode = ModeKind::Insert;
        self.set_page_mode("insert");
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus();
        }
    }

    fn enter_passthrough(&mut self) {
        // Native terminal: Passthrough is the terminal's input mode, but the SHELL
        // keeps keyboard focus (there's no webview) and forwards keys to the PTY.
        if self.active_is_term() {
            // Leave copy/vi mode (if active) so the live grid takes input again.
            if let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term.as_mut())
            {
                if s.pty.is_vi() {
                    s.pty.toggle_vi();
                }
            }
            self.mode = ModeKind::Passthrough;
            self.window.set_focus();
            self.set_status("terminal — Shift+Esc returns to the shell");
            self.window.request_redraw();
            return;
        }
        if self.active_webview().is_none() {
            self.set_status("no page — open one first");
            return;
        }
        self.mode = ModeKind::Passthrough;
        self.set_page_mode("passthrough");
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus();
        }
    }

    /// Push the current mode name into the page so the injected bridge knows which
    /// keys to intercept. Called whenever the mode changes.
    fn set_page_mode(&self, mode: &str) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script(&format!("window.__mode={mode:?}"));
        }
    }

    fn exit_to_normal(&mut self) {
        // Leaving a terminal's input (Shift+Esc) enters COPY MODE: flip the engine
        // into Alacritty's own vi mode — a vi cursor over the LIVE colored grid, with
        // grid selection + `selection_to_string` to yank. `i`/`Enter` resumes.
        if self.active_is_term() {
            if let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term.as_mut())
            {
                if !s.pty.is_vi() {
                    s.pty.toggle_vi();
                }
            }
            self.mode = ModeKind::Normal;
            self.window.set_focus();
            self.clear_status();
            self.window.request_redraw();
            return;
        }
        self.set_page_mode("normal");
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus_parent();
        }
        self.mode = ModeKind::Normal;
        self.window.set_focus();
        self.window.request_redraw();
    }

    // --- hint mode ------------------------------------------------------------

    /// `new_tab`: enter with `F` to follow the picked link in a NEW tab (badges
    /// render uppercase). `f` (false) follows in the current tab.
    fn enter_hint(&mut self, new_tab: bool) {
        let Some(idx) = self.active else {
            self.set_status("no page — open one first");
            return;
        };
        self.hint_new_tab = new_tab;
        // Engine-free read tab: hints are computed and drawn natively.
        if self.tabs[idx].native.is_some() {
            self.hint_input.clear();
            self.mode = ModeKind::Hint;
            self.build_native_hints();
            if self.native_hints.is_empty() {
                self.mode = ModeKind::Normal;
                self.set_status("no links on screen");
            }
            self.window.request_redraw();
            return;
        }
        if self.tabs[idx].nojs {
            self.set_status("hint mode needs JavaScript (this tab is no-js)");
            return;
        }
        self.hint_input.clear();
        self.mode = ModeKind::Hint;
        if let Some(wv) = &self.tabs[idx].webview {
            let _ = wv.evaluate_script(&format!("window.__hintUpper={new_tab};"));
            let _ = wv.evaluate_script(HINT_JS);
        }
    }

    /// Place hint labels over the links currently visible in the native read tab.
    fn build_native_hints(&mut self) {
        self.native_hints.clear();
        let Some(i) = self.active else { return };
        let (top, bottom) = self.content_y_bounds();
        let painter = &self.painter;
        let Some(nr) = self.tabs[i].native.as_ref() else { return };
        let links = read_view::visible_links(&nr.layout, nr.scroll, top, bottom, painter);
        let labels = hint_labels(links.len());
        let mut hints = Vec::with_capacity(links.len());
        for ((id, x, y), label) in links.into_iter().zip(labels) {
            if let Some(url) = nr.doc.link_url(id) {
                // +8 to match the content's left draw margin.
                hints.push(NativeHint { label, url: url.to_string(), x: x + 8, y });
            }
        }
        self.native_hints = hints;
    }

    fn key_hint(&mut self, key: &KeyEvent) {
        let native = !self.native_hints.is_empty();
        match &key.logical_key {
            Key::Escape => self.exit_hint(),
            Key::Backspace => {
                self.hint_input.pop();
                if native {
                    self.window.request_redraw();
                } else {
                    self.hint_send();
                }
            }
            Key::Character(s) => {
                let c = *s;
                if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphabetic()) {
                    // Typing a label uppercase switches this pick to new-tab mode,
                    // even if hint mode was entered with plain `f`.
                    if c.chars().any(|ch| ch.is_ascii_uppercase()) {
                        self.hint_new_tab = true;
                    }
                    self.hint_input.push_str(&c.to_lowercase());
                    if native {
                        self.hint_match_native();
                    } else {
                        self.hint_send();
                    }
                }
            }
            _ => {}
        }
    }

    /// Live feedback while picking a hint: holding Shift flips every badge to
    /// UPPERCASE (and arms new-tab mode); releasing it returns to lowercase. The
    /// actual open then follows whatever Shift state is held when a label completes.
    fn on_modifiers_changed(&mut self) {
        if self.mode != ModeKind::Hint {
            return;
        }
        let shift = self.modifiers.shift_key();
        if shift == self.hint_new_tab {
            return;
        }
        self.hint_new_tab = shift;
        if self.native_hints.is_empty() {
            self.hint_send(); // repaint the page's badges in the new case
        } else {
            self.window.request_redraw();
        }
    }

    /// Forward the current label string to the page to filter/activate hints.
    fn hint_send(&self) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script(&format!(
                "window.__hintInput&&window.__hintInput({:?},{})",
                self.hint_input, self.hint_new_tab
            ));
        }
    }

    /// Native hint input: on an exact label match, follow the link (re-extract it
    /// into the current read tab); reset if the typed prefix matches nothing.
    fn hint_match_native(&mut self) {
        if let Some(h) = self.native_hints.iter().find(|h| h.label == self.hint_input) {
            let url = h.url.clone();
            let new_tab = self.hint_new_tab;
            self.exit_hint();
            // New-tab mode opens a fresh read tab; otherwise follow in place.
            self.start_read(&url, !new_tab);
            return;
        }
        if !self.native_hints.iter().any(|h| h.label.starts_with(&self.hint_input)) {
            self.hint_input.clear();
        }
        self.window.request_redraw();
    }

    fn exit_hint(&mut self) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script("window.__hintClear&&window.__hintClear()");
        }
        self.native_hints.clear();
        self.hint_input.clear();
        self.hint_new_tab = false;
        self.mode = ModeKind::Normal;
        self.window.request_redraw();
    }

    // --- window control (resize / move / fullscreen) --------------------------

    fn key_resize(&mut self, key: &KeyEvent) {
        const STEP: i32 = 40;
        match &key.logical_key {
            Key::Escape | Key::Enter => self.mode = ModeKind::Normal,
            Key::Character(s) => match *s {
                "h" => self.resize_window(-STEP, 0),
                "l" => self.resize_window(STEP, 0),
                "j" => self.resize_window(0, STEP),
                "k" => self.resize_window(0, -STEP),
                _ => {}
            },
            _ => {}
        }
    }

    fn key_move(&mut self, key: &KeyEvent) {
        const STEP: i32 = 40;
        match &key.logical_key {
            Key::Escape | Key::Enter => self.mode = ModeKind::Normal,
            Key::Character(s) => match *s {
                "h" => self.move_window(-STEP, 0),
                "l" => self.move_window(STEP, 0),
                "j" => self.move_window(0, STEP),
                "k" => self.move_window(0, -STEP),
                _ => {}
            },
            _ => {}
        }
    }

    fn resize_window(&self, dw: i32, dh: i32) {
        let s = self.window.inner_size();
        let w = (s.width as i32 + dw).max(240) as u32;
        let h = (s.height as i32 + dh).max(160) as u32;
        self.window.set_inner_size(tao::dpi::PhysicalSize::new(w, h));
    }

    fn move_window(&self, dx: i32, dy: i32) {
        if let Ok(p) = self.window.outer_position() {
            self.window
                .set_outer_position(tao::dpi::PhysicalPosition::new(p.x + dx, p.y + dy));
        }
    }

    fn toggle_fullscreen(&mut self) {
        use tao::window::Fullscreen;
        if self.window.fullscreen().is_some() {
            self.window.set_fullscreen(None);
        } else {
            self.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        // A manual toggle owns the fullscreen state — clear the page-initiated flag
        // so a later page fs-exit doesn't fight it.
        self.fs_from_page = false;
        // Entering fullscreen hides the bars (Normal mode); leaving restores them.
        // The window resize usually relayouts, but do it explicitly so the page
        // refits even if the inner size didn't change.
        self.relayout_active();
    }

    /// Sync the window's fullscreen to the page's HTML fullscreen (YouTube's button).
    /// Entering fullscreens the window (so the page fills the screen and the bars
    /// hide); leaving exits — but only if WE entered it for the page, so a manual
    /// `:f` fullscreen isn't undone when an unrelated element leaves fullscreen.
    fn set_page_fullscreen(&mut self, on: bool) {
        use tao::window::Fullscreen;
        if on {
            if !self.is_fullscreen() {
                self.window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                self.fs_from_page = true;
                self.relayout_active();
            }
        } else if self.fs_from_page {
            self.window.set_fullscreen(None);
            self.fs_from_page = false;
            self.relayout_active();
        }
    }

    // --- commands -------------------------------------------------------------

    /// Set an informational status message (rendered dim). Clears the error flag.
    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.status_is_error = false;
    }

    /// "Quick maths": if the command-bar line is an arithmetic expression, return
    /// its formatted result. Gated on the presence of a maths operator so plain
    /// inputs (a lone number, a URL, a command) don't show a spurious result.
    fn math_preview(&self) -> Option<String> {
        let line = self.command.trim();
        if !line.contains(['+', '-', '*', '/', '%', '^']) {
            return None;
        }
        browser_core::math_eval(line).map(format_number)
    }

    /// Clear the status line.
    fn clear_status(&mut self) {
        self.status.clear();
        self.status_is_error = false;
    }

    /// Record a failure: show it in the status bar (red) and append it to the
    /// session error log for `:error`/`:errors`.
    fn set_error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.errors.push(ErrorEntry {
            time: now_hms(),
            command: self.current_command.clone(),
            message: msg.clone(),
        });
        if self.errors.len() > ERROR_LOG_CAP {
            let overflow = self.errors.len() - ERROR_LOG_CAP;
            self.errors.drain(0..overflow);
        }
        self.status = msg;
        self.status_is_error = true;
    }

    fn run_command(&mut self, line: &str) {
        let line = line.trim();
        // Attribute any failure raised below to this command in the error log.
        self.current_command = if line.is_empty() { None } else { Some(format!(":{line}")) };
        let (verb, rest) = match line.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (line, ""),
        };
        match verb {
            // `:open`/`:o` open in THIS tab (replacing it); a leading `-t` opens a new
            // tab instead. `:tabopen`/`:t` always open a new tab (that's their meaning).
            "open" | "o" => {
                let (new_tab, rest) = parse_tab_flag(rest);
                self.open_tab(rest, self.nojs, new_tab);
            }
            "tabopen" | "t" => self.open_tab(rest, self.nojs, true),
            // Edit the current URL: drop into the command bar pre-filled with
            // `<verb> <current url>` so you can tweak and re-open it — `<verb>` is the
            // mode the tab was opened in (open/research/read/nojs) so e.g. editing a
            // `:research` tab re-opens with `:research`, not `:open`. Reads the LIVE
            // document URL (so in-page/SPA navigation gives the real address).
            "edit" | "e" => match self.current_url() {
                Some(url) => {
                    let verb = self.active_reopen_verb();
                    self.enter_command(&format!("{verb} {url}"));
                }
                None => self.set_status("no page to edit"),
            },
            // Yank (copy) the current URL to the system clipboard.
            "y" | "yank" => match self.current_url() {
                Some(url) => {
                    clipboard_set(&url);
                    self.set_status(format!("yanked {url}"));
                }
                None => self.set_status("no url to yank"),
            },
            "read" => {
                let (new_tab, rest) = parse_tab_flag(rest);
                if rest.is_empty() {
                    self.set_status("usage: :read [-t] <url>");
                } else {
                    // `replace` (open in this tab) is the default; `-t` opens a new tab.
                    self.start_read(rest, !new_tab);
                }
            }
            // Inspect this session's errors in an engine-free, scrollable tab:
            // `:error` shows the most recent one, `:errors` shows them all.
            "error" | "err" => self.open_error_page(false),
            "errors" | "errs" => self.open_error_page(true),
            // Like :open (URL or → search engine) but lighter: JS on, images kept,
            // heavy media/embeds stripped. For "how do I…" / "best way to…" lookups.
            "research" | "rs" => {
                let (new_tab, rest) = parse_tab_flag(rest);
                self.open_research(rest, new_tab);
            }
            "te" | "term" => {
                if rest.is_empty() {
                    self.open_terminal();
                } else {
                    self.run_term(rest);
                }
            }
            "shell" => {
                if rest.is_empty() {
                    self.set_status(format!("shell = {}", self.term_command.join(" ")));
                } else {
                    let parts: Vec<String> = rest.split_whitespace().map(String::from).collect();
                    // Verify the program exists before applying, so a typo doesn't
                    // silently break `:te` (which would fail to spawn the shell).
                    if program_exists(&parts[0]) {
                        self.term_command = parts;
                        self.set_status(format!("shell set to: {}", self.term_command.join(" ")));
                    } else {
                        self.set_error(format!("shell not found on PATH: {}", parts[0]));
                    }
                }
            }
            // Customize the search engine used when `:open <query>` isn't a URL.
            // Accepts a short engine NAME (`:search ddg`, `:search google`, `:search
            // wiki` — looked up in the bang table) OR a full `%s` URL template.
            "search" => {
                if rest.is_empty() {
                    self.set_status(format!("search = {}", self.search_template));
                } else if let Some(t) = search_template_for(rest) {
                    self.search_template = t;
                    self.set_status(format!("search engine set to: {}", self.search_template));
                } else {
                    self.set_error(format!(
                        "unknown engine '{rest}' — use a name (ddg/google/wiki/…) or a %s URL template"
                    ));
                }
            }
            // Toggle JavaScript live (reloads the current tab; applies to new tabs).
            "js" => self.toggle_js(),
            // `:nojs <url>` still opens a one-off JavaScript-disabled tab.
            "nojs" => {
                if rest.is_empty() {
                    self.toggle_js();
                } else {
                    let (new_tab, rest) = parse_tab_flag(rest);
                    self.open_tab(rest, true, new_tab);
                }
            }
            // Reopen the most recently closed tab (also `u` / Ctrl+Shift+T).
            "reopen" | "undo" => self.reopen_closed(),
            // Toggle the uBlock-style content blocker. Applies live to every open web
            // tab (no reload) and is the default for new tabs; persisted in the session.
            "ads" | "adblock" => self.toggle_adblock(),
            // Live page-feature toggles (apply instantly to every open web tab).
            "pops" | "popups" => {
                self.block_popups = !self.block_popups;
                self.broadcast_toggle("popups", self.block_popups);
                self.set_status(if self.block_popups { "popups blocked" } else { "popups allowed" });
                self.window.request_redraw();
            }
            "mute" | "audio" => {
                self.mute = !self.mute;
                self.broadcast_toggle("mute", self.mute);
                self.set_status(if self.mute { "audio muted" } else { "audio on" });
                self.window.request_redraw();
            }
            "css" => {
                self.no_css = !self.no_css;
                self.broadcast_toggle("css", self.no_css);
                self.set_status(if self.no_css { "CSS off" } else { "CSS on" });
                self.window.request_redraw();
            }
            "close" | "tabclose" | "bd" => self.close_active(),
            // Session is saved explicitly (vim-style): `:w` writes the current tabs +
            // UI state, `:q` quits without saving, `:wq`/`:x` writes then quits.
            "write" | "w" => {
                self.save_session();
                self.set_status("session written");
            }
            "wq" | "x" => {
                self.save_session();
                self.quit = true;
            }
            "quit" | "q" | "q!" => self.quit = true,
            "reload" | "r" => self.reload_active(),
            "next" | "tabnext" | "tn" => self.switch_tab(1),
            "prev" | "tabprev" | "tp" => self.switch_tab(-1),
            "back" => self.history(false),
            "forward" => self.history(true),
            "f" | "fullscreen" => self.toggle_fullscreen(),
            "resize" => {
                self.mode = ModeKind::Resize;
                self.clear_status();
            }
            "move" => {
                self.mode = ModeKind::Move;
                self.clear_status();
            }
            "commands" | "help" => self.open_local_page("commands", commands_document()),
            "version" => self.open_version_page(),
            // Total the browser's real footprint across its whole process tree
            // (browser.exe + WebView2 engine procs + pty-hosts), which Task Manager
            // scatters under a separate "WebView2 Manager" group.
            "res" | "resources" => self.open_resource_page(),
            "" => {}
            // A bare bang (`:!yt cats`) opens the whole line as a bang target (in
            // this tab, like `:open`).
            other if other.starts_with('!') => self.open_tab(line, self.nojs, false),
            other => self.set_error(format!("unknown command: {other}")),
        }
        self.current_command = None;
    }

    /// Turn a command-bar target into a URL the way `:open` does: a bare query
    /// (spaces, or no scheme/dot like `rustlang`) — or anything that won't parse as
    /// a URL — goes to the configured search engine; a real address opens directly.
    fn resolve_target(&self, target: &str) -> String {
        // DuckDuckGo-style bangs (`!yt cats`, `!osrs dragon`) take priority: they
        // redirect to a specific site's search regardless of the default engine.
        if let Some(url) = browser_core::expand_bang(target) {
            return url;
        }
        if browser_core::looks_like_query(target) {
            browser_core::search_url(&self.search_template, target)
        } else {
            browser_core::normalize_url(target)
                .unwrap_or_else(|| browser_core::search_url(&self.search_template, target))
        }
    }

    /// Open a page from a target (URL or query). `new_tab` opens it as a new tab;
    /// otherwise it replaces the active tab in place (`:open` default, `o`). With no
    /// active tab the two are equivalent (a fresh tab is created).
    fn open_tab(&mut self, target: &str, disable_js: bool, new_tab: bool) {
        let url = self.resolve_target(target);
        self.record_history(&url);
        match self.build_content_webview(Source::Url(url.clone()), disable_js, "") {
            Ok(webview) => {
                let tab = Tab {
                    webview: Some(webview),
                    url,
                    nojs: disable_js,
                    read: false,
                    research: false,
                    native: None,
                    vim: None,
                    term: None,
                };
                self.place_tab(tab, new_tab);
                // Keep the keyboard on the shell; the page-load handler re-asserts
                // this once navigation finishes (which is when focus tends to move).
                self.window.set_focus();
                self.set_status(if disable_js { "(no-js)" } else { "" });
            }
            Err(e) => self.set_error(format!("failed to open: {e:#}")),
        }
    }

    /// Open a "research" tab: like `:open` (URL or → search engine), but JS-on with
    /// the [`RESEARCH_JS`] pruner injected so video/audio/embeds are stripped while
    /// images and text stay. A lighter browse for "how do I…" lookups. `new_tab` as
    /// in [`open_tab`].
    fn open_research(&mut self, target: &str, new_tab: bool) {
        let url = self.resolve_target(target);
        match self.build_content_webview(Source::Url(url.clone()), false, RESEARCH_JS) {
            Ok(webview) => {
                let tab = Tab {
                    webview: Some(webview),
                    url,
                    nojs: false,
                    read: false,
                    research: true,
                    native: None,
                    vim: None,
                    term: None,
                };
                self.place_tab(tab, new_tab);
                self.window.set_focus();
                self.set_status("(research — media stripped)");
            }
            Err(e) => self.set_error(format!("failed to open: {e:#}")),
        }
    }

    /// Insert `tab`, either as a NEW tab at the end (`new_tab`, or when nothing is
    /// active) or REPLACING the active tab in place (`:open`/`o` default). The tab it
    /// evicts is recorded on the closed-tab stack (so `U` can bring it back) and its
    /// terminal, if any, is shut down deterministically first.
    fn place_tab(&mut self, tab: Tab, new_tab: bool) {
        match self.active {
            Some(i) if !new_tab && i < self.tabs.len() => {
                self.record_closed(i);
                if let Some(session) = self.tabs[i].term.take() {
                    session.shutdown();
                }
                self.tabs[i] = tab;
                self.active = Some(i);
            }
            _ => {
                self.tabs.push(tab);
                self.active = Some(self.tabs.len() - 1);
            }
        }
        self.find_reset();
        self.refresh_visibility();
        self.window.request_redraw();
    }

    /// Record tab `i` on the closed-tab stack so `U` / Ctrl+Shift+T can reopen it.
    /// Mirrors the session's restorable-tab rules: live `browser://…` pages and the
    /// error/res vim pagers are session-specific and skipped.
    fn record_closed(&mut self, i: usize) {
        let Some(t) = self.tabs.get(i) else { return };
        if t.url.starts_with("browser://") || t.vim.is_some() {
            return;
        }
        let kind = if t.term.is_some() {
            "term"
        } else if t.read || t.native.is_some() {
            "read"
        } else if t.research {
            "research"
        } else if t.nojs {
            "nojs"
        } else {
            "open"
        };
        self.closed_tabs.push(session::SavedTab { kind: kind.to_string(), url: t.url.clone() });
        if self.closed_tabs.len() > CLOSED_CAP {
            self.closed_tabs.remove(0);
        }
    }

    /// Reopen the most recently closed tab (`U` / Ctrl+Shift+T), as a new tab. Read
    /// tabs are re-fetched and terminals reopened fresh, matching session restore.
    fn reopen_closed(&mut self) {
        let Some(c) = self.closed_tabs.pop() else {
            self.set_status("no closed tab to reopen");
            return;
        };
        match c.kind.as_str() {
            "term" => self.open_terminal(),
            "read" => self.start_read(&c.url, false),
            "research" => self.open_research(&c.url, true),
            "nojs" => self.open_tab(&c.url, true, true),
            _ => self.open_tab(&c.url, false, true),
        }
    }

    /// Build a child webview from either a URL or an inline HTML document, with
    /// the full shell bridge (keybindings, focus reclaim, hint mode).
    fn build_content_webview(
        &self,
        source: Source,
        disable_js: bool,
        extra_init: &str,
    ) -> Result<WebView> {
        let ipc_proxy = self.proxy.clone();
        let load_proxy = self.proxy.clone();
        let nav_proxy = self.proxy.clone();
        let mut builder = WebViewBuilder::new();
        builder = match source {
            Source::Url(u) => builder.with_url(u),
            Source::Html(h) => builder.with_html(h),
        };
        builder = builder
            .with_bounds(self.content_rect())
            .with_focused(false)
            // Disable Chromium's built-in accelerators (Shift+Esc task manager,
            // Ctrl+F/P, F12, …) so our own keybindings own the keyboard. Standard
            // editing keys (Ctrl+C/V/X) are unaffected.
            .with_browser_accelerator_keys(false)
            // Browser process flags — see BROWSER_ARGS. MUST match every other
            // webview (terminal included) or WebView2 creation fails with 0x8007139F.
            .with_additional_browser_args(BROWSER_ARGS)
            // The shell bridge always loads; the adblocker loads too but starts in
            // the shell's current state (baked as `__adblockDefault`), so `:ads` can
            // flip it live (`__setAdblock`) without a reload. `extra_init` (e.g.
            // research-mode DOM pruning) is appended last. All run in the same
            // document-create pass.
            .with_initialization_script({
                let ab = self.adblock;
                // Page-feature toggles start in the shell's current state too, so a
                // tab opened while a toggle is active is already in that state.
                let (p, m, c) = (self.block_popups, self.mute, self.no_css);
                let mut init = format!(
                    "{BRIDGE_JS}\n{FIND_JS}\n{CARET_JS}\n\
                     window.__adblockDefault={ab};\n{ADBLOCK_JS}\n\
                     window.__featureDefaults={{popups:{p},mute:{m},css:{c}}};\n{FEATURES_JS}"
                );
                if !extra_init.is_empty() {
                    init.push('\n');
                    init.push_str(extra_init);
                }
                init
            })
            .with_ipc_handler(move |req| match req.body().as_str() {
                "leave-passthrough" | "insert-escape" | "insert-blur" => {
                    let _ = ipc_proxy.send_event(UserEvent::ExitToNormal);
                }
                "to-passthrough" => {
                    let _ = ipc_proxy.send_event(UserEvent::InsertToPassthrough);
                }
                "page-ready" => {
                    let _ = ipc_proxy.send_event(UserEvent::FocusShell);
                }
                "grab-focus" => {
                    let _ = ipc_proxy.send_event(UserEvent::GrabFocus);
                }
                "hint-exit" => {
                    let _ = ipc_proxy.send_event(UserEvent::ExitHint);
                }
                "hint-edit" => {
                    let _ = ipc_proxy.send_event(UserEvent::HintEdit);
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
                    // A hint in new-tab mode resolved to a link: `hint-open:<href>`.
                    } else if let Some(href) = body.strip_prefix("hint-open:") {
                        let _ = ipc_proxy.send_event(UserEvent::HintOpen(href.to_string()));
                    }
                }
            })
            // Backup focus reclaim for non-JS tabs (page-ready covers JS tabs).
            .with_on_page_load_handler(move |event, _url| {
                if matches!(event, PageLoadEvent::Finished) {
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
                true
            });
        if disable_js {
            builder = builder.with_javascript_disabled();
        }
        builder
            .build_as_child(&*self.window)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Kick off a background readability extraction into the Document model; the
    /// result arrives as a ReadReady/ReadFailed user event so the UI stays
    /// responsive. `replace` swaps the active read tab's doc in place (link-follow
    /// / reload) rather than opening a new tab.
    fn start_read(&mut self, target: &str, replace: bool) {
        let proxy = self.proxy.clone();
        // A plain query (no `!bang`, not a URL) is run through the SEARCH backend,
        // which returns a clean, followable results document — readability can't parse
        // a live search-results page (it errors "failed to grab the article"). A bang
        // or a real address is fetched + run through readability as before.
        let bang = browser_core::expand_bang(target);
        if bang.is_none() && browser_core::looks_like_query(target) {
            let query = target.to_string();
            let config = browser_core::SearchConfig::default(); // DDG-lite, zero-config
            self.set_status(format!("searching {query} …"));
            std::thread::spawn(move || {
                let event = match browser_backend_search::search_blocking(&query, config) {
                    Ok(doc) => UserEvent::ReadReady { doc: Box::new(doc), replace },
                    Err(e) => UserEvent::ReadFailed(format!("{e:#}")),
                };
                let _ = proxy.send_event(event);
            });
        } else {
            let url = bang
                .or_else(|| browser_core::normalize_url(target))
                .unwrap_or_else(|| target.to_string());
            self.set_status(format!("reading {url} …"));
            std::thread::spawn(move || {
                let event = match browser_backend_text::fetch_document_blocking(&url) {
                    Ok(doc) => UserEvent::ReadReady { doc: Box::new(doc), replace },
                    Err(e) => UserEvent::ReadFailed(format!("{e:#}")),
                };
                let _ = proxy.send_event(event);
            });
        }
        self.window.request_redraw();
    }

    /// Run a local shell command in the background. Result arrives as TermDone.
    /// Strictly shell-initiated — never reachable from page content.
    fn run_term(&mut self, cmd: &str) {
        self.set_status(format!("$ {cmd}"));
        let proxy = self.proxy.clone();
        let cmd = cmd.to_string();
        std::thread::spawn(move || {
            let (output, code) = exec_command(&cmd);
            let _ = proxy.send_event(UserEvent::TermDone { cmd, output, code });
        });
        self.window.request_redraw();
    }

    fn active_is_term(&self) -> bool {
        self.active
            .and_then(|i| self.tabs.get(i))
            .map(|t| t.term.is_some())
            .unwrap_or(false)
    }

    /// Monospace cell size (width, height) in px at the current zoom.
    fn term_cell(&self) -> (i32, i32) {
        (self.painter.measure("M").max(1) as i32, self.painter.line_height().max(1) as i32)
    }

    /// The terminal grid size (cols, rows) that fits the current content band.
    fn term_grid_size(&self) -> (usize, usize) {
        let (cw, ch) = self.term_cell();
        let (w, _) = self.inner();
        let cols = (((w as i32 - 2 * TERM_PAD) / cw).max(1)) as usize;
        let rows = ((self.content_view_h() / ch).max(1)) as usize;
        (cols, rows)
    }

    /// Open a native terminal tab (no WebView2). The ConPTY + shell run in the
    /// `browser-pty-host` companion (so they can't deadlock our exit); its raw
    /// output is parsed by an in-process `alacritty_terminal` engine and painted by
    /// our own renderer. Enters Passthrough (shell keeps keyboard focus and forwards
    /// every key to the PTY; Shift+Esc returns to Normal).
    fn open_terminal(&mut self) {
        let shell = if self.term_command.is_empty() {
            vec!["cmd".to_string()]
        } else {
            self.term_command.clone()
        };

        let Some(host) = pty_host_path() else {
            self.set_error("could not locate browser-pty-host");
            return;
        };

        let (cols, rows) = self.term_grid_size();
        let mut command = Command::new(&host);
        command
            .arg(cols.to_string())
            .arg(rows.to_string())
            .args(&shell)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // NOTE: do NOT pass CREATE_NO_WINDOW here. A console-less host can fail to
        // back its ConPTY (the shell starts but no output flows). The console popup
        // is suppressed by building browser-pty-host as a GUI-subsystem binary.
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.set_error(format!("failed to start pty-host: {e}"));
                return;
            }
        };

        // Confine the pty-host (and its conhost + shell) to a kill-on-close job so
        // closing the handle — or the browser dying — reaps the whole tree.
        #[cfg(windows)]
        let job = {
            use std::os::windows::io::AsRawHandle;
            job::create_for(child.as_raw_handle() as isize)
        };
        #[cfg(not(windows))]
        let job = 0isize;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let id = self.next_term_id;
        self.next_term_id += 1;

        // Pump the pty-host's stdout (raw PTY bytes) to the UI thread → VT parser.
        let proxy = self.proxy.clone();
        let reader_handle = std::thread::spawn(move || {
            let mut stdout = stdout;
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = proxy.send_event(UserEvent::TermClosed { id });
                        break;
                    }
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        if proxy.send_event(UserEvent::TermOutput { id, data }).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        self.tabs.push(Tab {
            webview: None,
            url: format!("term: {}", shell[0]),
            nojs: false,
            read: false,
            research: false,
            native: None,
            vim: None,
            term: Some(TermSession {
                id,
                child,
                stdin,
                job,
                reader: Some(reader_handle),
                pty: pty_term::PtyTerm::new(cols, rows),
            }),
        });
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.mode = ModeKind::Passthrough; // terminal input mode; shell keeps focus
        self.window.set_focus();
        self.set_status("terminal — Shift+Esc returns to the shell");
        self.window.request_redraw();
    }

    fn term_session_mut(&mut self, id: u64) -> Option<&mut TermSession> {
        self.tabs.iter_mut().find_map(|t| t.term.as_mut().filter(|s| s.id == id))
    }

    /// Feed raw PTY output bytes to a terminal's VT engine and repaint it. Any reply
    /// the engine produced (e.g. the `ESC[6n` cursor-position answer the shell waits
    /// for) is written straight back to the PTY.
    fn feed_terminal(&mut self, id: u64, data: &[u8]) {
        if let Some(s) = self.term_session_mut(id) {
            s.pty.feed(data);
            let reply = s.pty.take_reply();
            if !reply.is_empty() {
                s.send(0, &reply);
            }
            self.window.request_redraw();
        }
    }

    /// Match the active terminal's grid to the current window/zoom, resizing the PTY
    /// if the cell count changed.
    fn sync_active_term_size(&mut self) {
        let (cols, rows) = self.term_grid_size();
        let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term.as_mut())
        else {
            return;
        };
        if s.pty.resize(cols, rows) {
            let mut p = [0u8; 4];
            p[0..2].copy_from_slice(&(cols as u16).to_le_bytes());
            p[2..4].copy_from_slice(&(rows as u16).to_le_bytes());
            s.send(1, &p);
        }
    }

    /// Encode a key for the active terminal and write it to the PTY.
    fn key_term(&mut self, key: &KeyEvent) {
        // Swallow the stray `Tab` that Alt+Tab delivers right as the window regains
        // focus — otherwise it gets typed into the shell.
        if matches!(key.logical_key, Key::Tab)
            && self.last_focus_gain.elapsed() < Duration::from_millis(150)
        {
            return;
        }
        let app_cursor = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.term.as_ref())
            .is_some_and(|s| s.pty.app_cursor());
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let shift = self.modifiers.shift_key();
        if let Some(bytes) = encode_term_key(&key.logical_key, ctrl, alt, shift, app_cursor) {
            if let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term.as_mut())
            {
                s.send(0, &bytes);
            }
        }
    }

    /// Paste the clipboard into the active terminal's PTY (Ctrl+V in a `:te` tab).
    /// Newlines are normalized to CR (what Enter sends), and the text is wrapped in
    /// bracketed-paste markers when the program asked for them, so a shell treats a
    /// multi-line paste as literal input instead of executing each line.
    fn term_paste(&mut self) {
        let Some(text) = clipboard_get() else { return };
        if text.is_empty() {
            return;
        }
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term.as_mut())
        else {
            return;
        };
        let mut out = Vec::new();
        let bracketed = s.pty.bracketed_paste();
        if bracketed {
            out.extend_from_slice(b"\x1b[200~");
        }
        out.extend_from_slice(text.as_bytes());
        if bracketed {
            out.extend_from_slice(b"\x1b[201~");
        }
        s.send(0, &out);
    }

    /// Whether the active terminal is in copy/vi mode.
    fn active_term_vi(&self) -> bool {
        self.active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.term.as_ref())
            .is_some_and(|s| s.pty.is_vi())
    }

    /// Drive Alacritty's vi/copy mode from a key. Returns `true` if consumed;
    /// unhandled keys fall through to the normal browser bindings.
    fn key_term_vi(&mut self, key: &KeyEvent) -> bool {
        use pty_term::ViMotion as M;
        let ctrl = self.modifiers.control_key();
        let mut yank: Option<String> = None;
        let mut exit = false;
        let mut consumed = true;
        {
            let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term.as_mut())
            else {
                return false;
            };
            let pty = &mut s.pty;
            let half = (pty.rows as i32 / 2).max(1);
            let page = (pty.rows as i32 - 1).max(1);
            if ctrl {
                match key.physical_key {
                    KeyCode::KeyU => pty.vi_scroll(-half),
                    KeyCode::KeyD => pty.vi_scroll(half),
                    _ => consumed = false,
                }
            } else {
                match &key.logical_key {
                    Key::Escape => {
                        if !pty.clear_selection() {
                            exit = true;
                        }
                    }
                    Key::Enter => exit = true,
                    Key::ArrowLeft => pty.vi_motion(M::Left),
                    Key::ArrowRight => pty.vi_motion(M::Right),
                    Key::ArrowUp => pty.vi_motion(M::Up),
                    Key::ArrowDown => pty.vi_motion(M::Down),
                    Key::PageUp => pty.vi_scroll(-page),
                    Key::PageDown => pty.vi_scroll(page),
                    Key::Character(c) => match *c {
                        "h" => pty.vi_motion(M::Left),
                        "j" => pty.vi_motion(M::Down),
                        "k" => pty.vi_motion(M::Up),
                        "l" => pty.vi_motion(M::Right),
                        "w" => pty.vi_motion(M::WordRight),
                        "b" => pty.vi_motion(M::WordLeft),
                        "e" => pty.vi_motion(M::WordRightEnd),
                        "0" => pty.vi_motion(M::First),
                        "$" => pty.vi_motion(M::Last),
                        "^" => pty.vi_motion(M::FirstOccupied),
                        "H" => pty.vi_motion(M::High),
                        "M" => pty.vi_motion(M::Middle),
                        "L" => pty.vi_motion(M::Low),
                        "G" => pty.vi_bottom(),
                        "g" => pty.vi_top(),
                        "v" => {
                            if !pty.clear_selection() {
                                pty.start_selection(false);
                            }
                        }
                        "V" => {
                            if !pty.clear_selection() {
                                pty.start_selection(true);
                            }
                        }
                        "y" => yank = pty.yank(),
                        "i" => exit = true,
                        _ => consumed = false,
                    },
                    _ => consumed = false,
                }
            }
        }
        if let Some(text) = yank {
            let n = text.chars().count();
            clipboard_set(&text);
            self.set_status(format!("yanked {n} chars"));
        }
        if exit {
            self.enter_passthrough(); // leaves vi mode + resumes the live shell
        }
        if consumed || exit {
            self.window.request_redraw();
        }
        consumed || exit
    }

    /// Present a finished command vim-style: the result replaces the command-bar
    /// text (collapsed to one line).
    fn show_term_result(&mut self, _cmd: &str, output: &str, code: Option<i32>) {
        let trimmed = output.trim();
        let msg = if trimmed.is_empty() {
            let codestr = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            format!("(exit {codestr})")
        } else {
            trimmed.replace(['\r', '\n'], " ")
        };
        self.set_status(msg);
        self.window.request_redraw();
    }

    /// Render an extracted Document in an engine-free read tab (no WebView2).
    /// `replace` means "open in the current tab": if the active tab is ALREADY a read
    /// tab its document is swapped in place (link-follow/reload, keeping the tab); if
    /// it's some other tab type, that tab is replaced with a fresh read tab. Without
    /// `replace` (`:read -t`), a new read tab is opened.
    fn show_read_document(&mut self, doc: browser_core::Document, replace: bool) {
        let url = doc.url.clone();
        if replace {
            if let Some(i) = self.active {
                // Fast path: active is already a read tab → swap its doc, keep the tab.
                if self.tabs.get(i).is_some_and(|t| t.native.is_some()) {
                    let nr = self.tabs[i].native.as_mut().unwrap();
                    nr.doc = doc;
                    nr.scroll = 0;
                    nr.dirty = true;
                    nr.caret = None; // the text changed; drop any caret/selection
                    self.tabs[i].url = url;
                    self.clear_status();
                    self.window.request_redraw();
                    return;
                }
                // Active is a web/terminal tab → replace it in place with a read tab.
                let tab = native_read_tab(doc, url, true);
                self.place_tab(tab, false);
                self.window.set_focus();
                self.clear_status();
                return;
            }
        }
        self.push_native_tab(doc, url, true);
    }

    /// Open a new engine-free native tab rendering `doc` (no WebView2 process). Used
    /// by read mode (`read = true`, tinted green + `f` hint) and the `:error(s)`
    /// pages (`read = false`). Activates and focuses the new tab.
    fn push_native_tab(&mut self, doc: browser_core::Document, url: String, read: bool) {
        self.tabs.push(native_read_tab(doc, url, read));
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.window.set_focus();
        self.clear_status();
        self.window.request_redraw();
    }

    /// Open the `:error` / `:errors` page: render the session error log in an
    /// engine-free, read-only **vim-style** tab so the full text (which may be long,
    /// like the WebView2 HRESULT messages) is readable, navigable, and — crucially —
    /// selectable/yankable without retyping. `all = false` shows just the most recent
    /// error; `all = true` shows every error this session, newest first.
    fn open_error_page(&mut self, all: bool) {
        if self.errors.is_empty() {
            self.set_status("no errors this session");
            return;
        }
        let lines = error_lines(&self.errors, all);
        self.tabs.push(Tab {
            webview: None,
            url: "browser://error".into(),
            nojs: false,
            read: false,
            research: false,
            native: None,
            vim: Some(vim::TextBuffer::new(lines)),
            term: None,
        });
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.window.set_focus();
        self.clear_status();
        self.window.request_redraw();
    }

    /// `:res` — the browser's whole-tree resource usage (browser.exe + WebView2
    /// engine procs + pty-hosts): memory, CPU%, and disk I/O per process plus a
    /// grand total, in an engine-free vim-style tab. Task Manager won't give this in
    /// one place (it scatters WebView2 under its own group). Auto-refreshes ~1×/sec
    /// and freezes automatically while you're selecting text (so you can copy figures
    /// with vim motions without it shifting).
    fn open_resource_page(&mut self) {
        // No previous sample yet, so the first frame shows memory immediately and
        // CPU/disk fill in on the next refresh.
        self.res_prev.clear();
        self.res_at = Instant::now();
        let lines = self.sample_res_lines();
        if lines.is_empty() {
            self.set_status("resource info unavailable");
            return;
        }
        self.tabs.push(Tab {
            webview: None,
            url: "browser://res".into(),
            nojs: false,
            read: false,
            research: false,
            native: None,
            vim: Some(vim::TextBuffer::new(lines)),
            term: None,
        });
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.window.set_focus();
        self.clear_status();
        self.window.request_redraw();
    }

    /// Whether the active tab is the `:res` resource monitor.
    fn active_is_res(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.url == "browser://res")
    }

    /// If the active tab is the resource monitor, re-sample and update its buffer
    /// **in place** — keeping the cursor, selection, and scroll — so a live refresh
    /// never disturbs navigation/copy. Freezes (skips the update) while a visual
    /// selection is active, so the highlighted text can't shift mid-copy; the
    /// refresh resumes once the selection is cleared (after yank/Esc). Called on the
    /// ~1s tick and on pause/resume.
    fn refresh_res(&mut self) {
        if !self.active_is_res() {
            return;
        }
        let selecting = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.vim.as_ref())
            .is_some_and(|b| b.has_selection());
        if selecting {
            return;
        }
        let lines = self.sample_res_lines();
        if let Some(buf) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.vim.as_mut())
        {
            buf.set_lines(lines);
        }
    }

    /// Take a fresh process-tree sample, fold in CPU%/disk-rate deltas against the
    /// previous sample, format the breakdown, and update `res_prev`/`res_at`.
    fn sample_res_lines(&mut self) -> Vec<String> {
        let sample = procmon::tree_sample();
        if sample.is_empty() {
            return Vec::new();
        }
        let elapsed = self.res_at.elapsed().as_secs_f64();
        let ncores = procmon::cpu_count() as f64;
        let have_prev = !self.res_prev.is_empty() && elapsed > 0.05;

        // Per-process CPU% and disk B/s from the cumulative-counter deltas.
        let rate = |s: &procmon::ProcSample| -> (Option<f64>, Option<f64>) {
            if !have_prev {
                return (None, None);
            }
            match self.res_prev.get(&s.pid) {
                Some(&(pc, pio)) => {
                    let cpu = (s.cpu_100ns.saturating_sub(pc)) as f64
                        / (elapsed * 1e7 * ncores)
                        * 100.0;
                    let disk = (s.io_bytes.saturating_sub(pio)) as f64 / elapsed;
                    (Some(cpu), Some(disk))
                }
                None => (None, None),
            }
        };

        let total_mem: u64 = sample.iter().map(|p| p.working_set).sum();
        let (mut total_cpu, mut total_disk) = (0.0f64, 0.0f64);
        let mut rows = Vec::with_capacity(sample.len());
        for p in &sample {
            let (cpu, disk) = rate(p);
            total_cpu += cpu.unwrap_or(0.0);
            total_disk += disk.unwrap_or(0.0);
            let cpu_s = cpu.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "—".into());
            let disk_s = disk.map(procmon::fmt_rate).unwrap_or_else(|| "—".into());
            rows.push(format!(
                "{:>9}  {:>6}  {:>9}  {:<22} {}",
                procmon::fmt_bytes(p.working_set),
                cpu_s,
                disk_s,
                p.name,
                p.pid
            ));
        }

        let cpu_total = if have_prev { format!("CPU {total_cpu:.1}%") } else { "CPU —".into() };
        let disk_total =
            if have_prev { format!("disk {}", procmon::fmt_rate(total_disk)) } else { "disk —".into() };
        let mut lines = Vec::with_capacity(rows.len() + 5);
        lines.push(format!("browser — {} processes    (live; select to freeze)", sample.len()));
        lines.push(format!("{} · {} · {}", procmon::fmt_bytes(total_mem), cpu_total, disk_total));
        lines.push(String::new());
        lines.push(format!("{:>9}  {:>6}  {:>9}  {:<22} {}", "MEM", "CPU", "DISK", "PROCESS", "PID"));
        lines.extend(rows);

        // Roll the sample forward for the next delta.
        self.res_prev = sample.iter().map(|p| (p.pid, (p.cpu_100ns, p.io_bytes))).collect();
        self.res_at = Instant::now();
        lines
    }

    /// `:version` — build/runtime details in an engine-free vim pager (no WebView2),
    /// so the text is navigable and yankable with the same motions as `:error`/`:res`.
    fn open_version_page(&mut self) {
        self.tabs.push(Tab {
            webview: None,
            url: "browser://version".into(),
            nojs: false,
            read: false,
            research: false,
            native: None,
            vim: Some(vim::TextBuffer::new(version_lines())),
            term: None,
        });
        self.active = Some(self.tabs.len() - 1);
        self.refresh_visibility();
        self.window.set_focus();
        self.clear_status();
        self.window.request_redraw();
    }

    /// Open an internal HTML page (e.g. `:commands`) in a new tab.
    fn open_local_page(&mut self, label: &str, html: String) {
        match self.build_content_webview(Source::Html(html), false, "") {
            Ok(webview) => {
                self.tabs.push(Tab {
                    webview: Some(webview),
                    url: format!("browser://{label}"),
                    nojs: false,
                    read: false,
                    research: false,
                    native: None,
                    vim: None,
                    term: None,
                });
                self.active = Some(self.tabs.len() - 1);
                self.refresh_visibility();
                self.window.set_focus();
                self.clear_status();
            }
            Err(e) => self.set_error(format!("failed to open {label}: {e:#}")),
        }
    }

    fn close_active(&mut self) {
        let Some(i) = self.active else {
            self.set_status("no tab to close");
            return;
        };
        // Remember it so `U` / Ctrl+Shift+T can reopen it (internal pages are skipped
        // by record_closed). Shut a terminal down deterministically (kill shell, close
        // PTY, join reader) before dropping the tab; dropping the WebView frees the renderer.
        self.record_closed(i);
        if let Some(session) = self.tabs[i].term.take() {
            session.shutdown();
        }
        let _ = self.tabs.remove(i);
        self.active = if self.tabs.is_empty() {
            None
        } else {
            Some(i.min(self.tabs.len() - 1))
        };
        self.find_reset();
        self.refresh_visibility();
        self.window.set_focus();
    }

    /// Close the tab whose terminal has the given id (its shell exited). Behaves
    /// like `x`, but only disturbs focus/mode if that tab was the active one.
    fn close_term_tab(&mut self, id: u64) {
        let Some(i) = self.tabs.iter().position(|t| t.term.as_ref().map(|s| s.id) == Some(id))
        else {
            return;
        };
        let was_active = self.active == Some(i);
        if let Some(session) = self.tabs[i].term.take() {
            session.shutdown();
        }
        self.tabs.remove(i);
        self.active = if self.tabs.is_empty() {
            None
        } else {
            let a = self.active.unwrap_or(0);
            Some(if a > i { a - 1 } else { a.min(self.tabs.len() - 1) })
        };
        if was_active {
            self.mode = ModeKind::Normal;
            self.window.set_focus();
        }
        self.refresh_visibility();
    }

    fn switch_tab(&mut self, delta: i32) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as i32;
        let cur = self.active.unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(n) as usize;
        self.active = Some(next);
        self.find_reset();
        self.refresh_visibility();
        self.window.set_focus();
    }

    /// Jump directly to a zero-based tab index (bound to keys 1..9).
    fn jump_to(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = Some(index);
            self.find_reset();
            self.refresh_visibility();
            self.window.set_focus();
        }
    }

    /// Map an x pixel coordinate on the tab bar to a tab index, for click-to-switch.
    /// Mirrors `draw_tab_bar`'s layout (start at x=8, each label `+6` gap), including
    /// its right-edge truncation, so the clickable regions match what's drawn.
    fn tab_at_pixel(&self, px: f64) -> Option<usize> {
        let p = &self.painter;
        let labels = self.tab_labels();
        let limit = self.inner().0 as usize;
        let px = px.max(0.0) as usize;
        let mut x = 8usize;
        for (i, (label, active, _)) in labels.iter().enumerate() {
            let text = if *active {
                format!("[{}:{}]", i + 1, label)
            } else {
                format!(" {}:{} ", i + 1, label)
            };
            let end = x + p.measure(&text) + 6;
            if px >= x && px < end {
                return Some(i);
            }
            x = end;
            if x > limit.saturating_sub(40) {
                break;
            }
        }
        None
    }

    /// Mouse-wheel scroll. `dy_lines` > 0 means the wheel rolled up (toward older
    /// terminal output / the top of a page). Routes to the terminal's scrollback or
    /// the native read-tab scroll offset; web tabs handle the wheel themselves.
    fn on_wheel(&mut self, dy_lines: f64) {
        if self.active_is_term() {
            // If the program enabled mouse reporting (vim `mouse=a`, less, tmux…),
            // hand it the wheel so IT scrolls; otherwise page our own scrollback.
            if self.term_mouse_wheel(dy_lines) {
                return;
            }
            let lines = (dy_lines * 3.0).round() as i32;
            if lines != 0 {
                if let Some(s) =
                    self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term.as_mut())
                {
                    s.pty.scroll_display(lines);
                    self.window.request_redraw();
                }
            }
            return;
        }
        let dy = (-dy_lines * 80.0).round() as i32;
        if dy != 0 {
            self.scroll(dy);
        }
    }

    /// Forward a wheel notch to a terminal program that turned on mouse reporting,
    /// as a mouse wheel-button event at the cell under the cursor. Returns `false`
    /// (so the caller scrolls our scrollback instead) when no program wants mice.
    fn term_mouse_wheel(&mut self, dy_lines: f64) -> bool {
        let (cw, ch) = self.term_cell();
        let top = self.tab_bar_h() as i32;
        let (px, py) = (self.cursor_pos.0 as i32, self.cursor_pos.1 as i32);
        let Some(s) =
            self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term.as_mut())
        else {
            return false;
        };
        if !s.pty.mouse_mode() {
            return false;
        }
        let (cols, rows) = (s.pty.cols as i32, s.pty.rows as i32);
        let col = (((px - TERM_PAD) / cw) + 1).clamp(1, cols.max(1));
        let row = (((py - top) / ch) + 1).clamp(1, rows.max(1));
        // xterm wheel buttons: 64 = up, 65 = down.
        let button = if dy_lines > 0.0 { 64 } else { 65 };
        let sgr = s.pty.sgr_mouse();
        let notches = (dy_lines.abs().round() as i32).max(1);
        let mut out = Vec::new();
        for _ in 0..notches {
            out.extend_from_slice(&encode_mouse_wheel(sgr, button, col, row));
        }
        s.send(0, &out);
        self.window.request_redraw();
        true
    }

    /// Move the active tab one position left (-1) or right (+1).
    fn move_tab(&mut self, delta: i32) {
        let Some(i) = self.active else { return };
        let j = i as i32 + delta;
        if j < 0 || j as usize >= self.tabs.len() {
            return;
        }
        let j = j as usize;
        self.tabs.swap(i, j);
        self.active = Some(j);
        self.refresh_visibility();
    }

    fn refresh_visibility(&mut self) {
        let rect = self.content_rect();
        for (i, tab) in self.tabs.iter().enumerate() {
            let Some(wv) = &tab.webview else { continue };
            let visible = Some(i) == self.active;
            let _ = wv.set_visible(visible);
            if visible {
                let _ = wv.set_bounds(rect);
            }
        }
        self.window.request_redraw();
    }

    /// Shut down terminals (kill shells, close PTYs, join readers) and drop every
    /// webview before exiting — so WebView2 processes and ConPTYs close cleanly
    /// rather than leaving a stuck thread that deadlocks process teardown.
    fn teardown(&mut self) {
        // Idempotent: only the first call tears down (see `torn_down`). The session is
        // NOT auto-saved here — saving is explicit (`:w` / `:wq`), vim-style, so just
        // closing the window or `:q` leaves the last written session untouched.
        if self.torn_down {
            return;
        }
        self.torn_down = true;
        for tab in &mut self.tabs {
            if let Some(session) = tab.term.take() {
                session.shutdown();
            }
        }
        self.tabs.clear();
        self.active = None;
    }

    /// Snapshot the open tabs + UI state to disk so the next launch restores them.
    /// Explicit only — run by `:w` / `:wq` (vim-style), never automatically on exit.
    /// Internal pages (`browser://…`, the `:error(s)` log) are session-specific and
    /// skipped. No-op during headless test runs so they don't clobber a real session.
    fn save_session(&self) {
        if std::env::var("BROWSER_TEST_QUIT_MS").is_ok() {
            return;
        }
        let mut tabs = Vec::new();
        let mut active = 0;
        for (i, tab) in self.tabs.iter().enumerate() {
            if tab.url.starts_with("browser://") || tab.vim.is_some() {
                continue;
            }
            let kind = if tab.term.is_some() {
                "term"
            } else if tab.read || tab.native.is_some() {
                "read"
            } else if tab.research {
                "research"
            } else if tab.nojs {
                "nojs"
            } else {
                "open"
            };
            if Some(i) == self.active {
                active = tabs.len();
            }
            tabs.push(session::SavedTab { kind: kind.to_string(), url: tab.url.clone() });
        }
        // Remember the window placement (outer position + inner size) so it reopens
        // exactly where it was.
        let window = self.window.outer_position().ok().map(|p| {
            let s = self.window.inner_size();
            session::WindowGeom { x: p.x, y: p.y, w: s.width, h: s.height }
        });
        session::save(&session::Session {
            zoom: self.zoom,
            nojs: self.nojs,
            adblock: self.adblock,
            search_template: self.search_template.clone(),
            term_command: self.term_command.clone(),
            active,
            history: self.history.clone(),
            window,
            tabs,
        });
    }

    /// Reopen the tabs + UI state saved by a previous run. Read tabs are re-fetched
    /// (so they may arrive slightly out of order, since fetching is async) and
    /// terminals are reopened fresh.
    fn restore_session(&mut self, s: session::Session) {
        self.search_template = s.search_template;
        if !s.term_command.is_empty() {
            self.term_command = s.term_command;
        }
        self.history = s.history;
        self.history.truncate(HISTORY_CAP);
        self.nojs = s.nojs;
        self.adblock = s.adblock;
        if s.zoom != 1.0 {
            self.set_zoom(s.zoom);
        }
        for tab in &s.tabs {
            // Each restored tab is a NEW tab (push), so they don't replace each other.
            match tab.kind.as_str() {
                "term" => self.open_terminal(),
                "read" => self.start_read(&tab.url, false),
                "research" => self.open_research(&tab.url, true),
                "nojs" => self.open_tab(&tab.url, true, true),
                _ => self.open_tab(&tab.url, false, true),
            }
        }
        if !self.tabs.is_empty() {
            self.active = Some(s.active.min(self.tabs.len() - 1));
            self.refresh_visibility();
        }
        self.clear_status();
        self.window.request_redraw();
    }

    /// Half the visible content height, in px (the Ctrl+D / Ctrl+U scroll step).
    fn half_page(&self) -> i32 {
        let (_, h) = self.inner();
        ((h as i32 - self.bar_h() as i32).max(40)) / 2
    }

    fn scroll(&mut self, dy: i32) {
        // Engine-free read tab: move the native scroll offset, clamped to content.
        let view = self.content_view_h();
        if let Some(nr) = self.active_native_mut() {
            let max = (nr.layout.height - view).max(0);
            nr.scroll = (nr.scroll + dy).clamp(0, max);
            self.window.request_redraw();
            return;
        }
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script(&format!("window.scrollBy(0,{dy});"));
        }
    }

    /// Jump to the top (`g`) or bottom (`G`) of the active page/document.
    fn scroll_edge(&mut self, bottom: bool) {
        let view = self.content_view_h();
        if let Some(nr) = self.active_native_mut() {
            nr.scroll = if bottom { (nr.layout.height - view).max(0) } else { 0 };
            self.window.request_redraw();
            return;
        }
        if let Some(wv) = self.active_webview() {
            let js = if bottom {
                "window.scrollTo(0,document.body.scrollHeight);"
            } else {
                "window.scrollTo(0,0);"
            };
            let _ = wv.evaluate_script(js);
        }
    }

    fn history(&mut self, forward: bool) {
        if let Some(wv) = self.active_webview() {
            let js = if forward { "history.forward();" } else { "history.back();" };
            let _ = wv.evaluate_script(js);
        }
    }

    /// Toggle the ad/tracker blocker. Flips the live `on` flag inside every open
    /// web tab via `__setAdblock` (cosmetic hiding + DOM sweep + YouTube skip update
    /// immediately; already-loaded requests aren't undone — reload for a clean slate)
    /// and updates the default baked into newly opened tabs.
    fn toggle_adblock(&mut self) {
        self.adblock = !self.adblock;
        let on = self.adblock;
        for tab in &self.tabs {
            if let Some(wv) = &tab.webview {
                let _ = wv.evaluate_script(&format!("window.__setAdblock&&window.__setAdblock({on})"));
            }
        }
        self.set_status(if on { "adblock ON — ads blocked" } else { "adblock OFF — ads allowed" });
        self.window.request_redraw();
    }

    /// Flip a live page-feature toggle ([`FEATURES_JS`]) on every open web tab via
    /// `__setToggle` (no reload). `name` is `popups` | `mute` | `css`.
    fn broadcast_toggle(&self, name: &str, on: bool) {
        for tab in &self.tabs {
            if let Some(wv) = &tab.webview {
                let _ = wv.evaluate_script(&format!(
                    "window.__setToggle&&window.__setToggle('{name}',{on})"
                ));
            }
        }
    }

    /// Toggle JavaScript globally. Unlike the page-feature toggles, JS can only be
    /// switched per-webview at build time, so this rebuilds the ACTIVE web tab with
    /// the new setting (a reload) and applies to newly opened tabs; background tabs
    /// adopt it when next reloaded.
    fn toggle_js(&mut self) {
        self.nojs = !self.nojs;
        let on = !self.nojs;
        self.set_status(format!(
            "JavaScript {} (this tab reloaded; applies to new tabs)",
            if on { "ON" } else { "OFF" }
        ));
        let Some(i) = self.active else { return };
        let Some(t) = self.tabs.get(i) else { return };
        if t.webview.is_none() {
            return; // only web tabs have JS to toggle
        }
        let url = t.url.clone();
        let research = t.research;
        let extra = if research { RESEARCH_JS } else { "" };
        match self.build_content_webview(Source::Url(url.clone()), self.nojs, extra) {
            Ok(webview) => {
                let nojs = self.nojs;
                if let Some(session) = self.tabs[i].term.take() {
                    session.shutdown();
                }
                self.tabs[i] = Tab {
                    webview: Some(webview),
                    url,
                    nojs,
                    read: false,
                    research,
                    native: None,
                    vim: None,
                    term: None,
                };
                self.refresh_visibility();
                self.window.set_focus();
            }
            Err(e) => self.set_error(format!("failed to reload: {e:#}")),
        }
    }

    /// Reload the active tab: re-extract for an engine-free read tab, else reload
    /// the webview.
    fn reload_active(&mut self) {
        if let Some(url) = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.native.as_ref())
            .map(|nr| nr.doc.url.clone())
        {
            // Internal native pages (e.g. `:error`) have nothing to re-extract.
            if !url.starts_with("browser://") {
                self.start_read(&url, true);
            }
            return;
        }
        if let Some(wv) = self.active_webview() {
            let _ = wv.reload();
        }
    }

    // --- rendering ------------------------------------------------------------

    fn draw(&mut self) -> Result<()> {
        // Keep the engine-free read layout current (cheap no-op unless something
        // that affects layout changed) before we read it for painting.
        self.refresh_read_layout();
        // Keep the active terminal's grid matched to the window/zoom.
        self.sync_active_term_size();
        let (w, h) = self.inner();
        // Gather all dynamic text + zoom-scaled metrics up front, while we can
        // still borrow &self.
        let tab_labels = self.tab_labels();
        let welcome = self.active.is_none();
        let native_active = self
            .active
            .and_then(|i| self.tabs.get(i))
            .is_some_and(|t| t.native.is_some());
        let vim_active = self
            .active
            .and_then(|i| self.tabs.get(i))
            .is_some_and(|t| t.vim.is_some());
        let term_active = self
            .active
            .and_then(|i| self.tabs.get(i))
            .is_some_and(|t| t.term.is_some());
        let bar_h = self.bar_h() as usize;
        let tab_h = self.tab_bar_h() as usize;
        // Minimized / degenerate size: skip the frame. The tab + command bars are a
        // FIXED height; in a window shorter than they are (e.g. during Show Desktop /
        // minimize, where the client area collapses) drawing them writes past the
        // bottom of the pixel buffer (buf.len() == w*h) and panics. Nothing is
        // visible when minimized anyway, so there's nothing to paint.
        if (h as usize) < tab_h + bar_h + 4 || w < 8 {
            return Ok(());
        }
        // Command bar: draw the `:`-line with horizontal scroll-to-caret so editing
        // a long URL (e.g. after `:edit`, caret parked at the end) keeps the caret —
        // and the tail of the URL — visible. `cmd` is Some((line, scroll_px)) in
        // Command mode and replaces the normal segment list; `caret` is the lit
        // block cursor (x, width) already shifted by the same scroll.
        const MARGIN: i32 = 8;
        let cw = ((self.zoom * 2.0).round() as i32).max(2);
        let (segments, cmd, caret, sel) = if matches!(self.mode, ModeKind::Command | ModeKind::Find)
        {
            // `:` for a command, `/` for a find-in-page search.
            let pre = if self.mode == ModeKind::Find { '/' } else { ':' };
            let line = format!("{pre}{}", self.command);
            let prefix = format!("{pre}{}", &self.command[..self.command_cursor]);
            let caret_un = MARGIN + self.painter.measure(&prefix) as i32;
            // Keep the caret a hair inside the right edge; scroll only when it would
            // otherwise fall off the end of the bar.
            let right_bound = (w as i32 - cw - 2).max(MARGIN);
            let scroll = (caret_un - right_bound).max(0);
            let caret = if self.cursor_on {
                Some(((caret_un - scroll) as usize, cw as usize))
            } else {
                None
            };
            // Selection highlight rect (x range), in the same scrolled coordinates,
            // clipped to the left margin.
            let sel = self.sel_range().map(|(a, b)| {
                let x_of = |k: usize| {
                    MARGIN - scroll
                        + self.painter.measure(&format!("{pre}{}", &self.command[..k])) as i32
                };
                (x_of(a).max(MARGIN).max(0) as usize, x_of(b).max(0) as usize)
            });
            (Vec::new(), Some((line, scroll)), caret, sel)
        } else {
            (self.bar_segments(), None, None, None)
        };
        // Quick-maths live result, shown right-aligned in the command bar while the
        // typed line is an arithmetic expression (`:20*8` → `= 160`).
        let math = (self.mode == ModeKind::Command)
            .then(|| self.math_preview())
            .flatten()
            .map(|r| format!("= {r}"));
        // Autocomplete ghost: the un-typed tail of the suggestion, drawn dim after
        // the command text (Tab / Ctrl+Right accepts it).
        let cmd_suffix = self
            .command_suggestion()
            .and_then(|s| s.strip_prefix(self.command.as_str()).map(str::to_string))
            .filter(|t| !t.is_empty());

        self.surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .map_err(|e| anyhow::anyhow!("resize: {e}"))?;
        let mut buf = self
            .surface
            .buffer_mut()
            .map_err(|e| anyhow::anyhow!("buffer: {e}"))?;

        // `p` is a disjoint field borrow from `self.surface`, so it coexists with `buf`.
        let p = &self.painter;
        let (wz, hz) = (w as usize, h as usize);
        let bar_top = hz.saturating_sub(bar_h);

        let baseline = bar_top + (bar_h * 2 / 3);
        // Draw the opaque command bar; called LAST so nothing bleeds through it.
        let draw_bar = |buf: &mut [u32]| {
            draw::fill_band(buf, wz, hz, bar_top, hz, draw::BAR_BG);
            if let Some((text, scroll)) = &cmd {
                // Selection highlight first, so the text paints on top of it.
                if let Some((sx0, sx1)) = sel {
                    let lh = p.line_height();
                    let y0 = baseline.saturating_sub(lh * 3 / 4);
                    let y1 = (baseline + lh / 6).min(hz);
                    draw::fill_rect(buf, wz, hz, sx0, y0, sx1, y1, draw::SEL);
                }
                // Command line, scrolled left by `scroll` px; clip at the left
                // margin so scrolled-off text doesn't bleed into the edge.
                let endx =
                    p.text_clipped(buf, wz, hz, MARGIN - *scroll, baseline, text, draw::BAR_FG, MARGIN);
                // Autocomplete ghost text (dim) continuing from the caret.
                if let Some(sfx) = &cmd_suffix {
                    p.text_clipped(buf, wz, hz, endx, baseline, sfx, draw::DIM, MARGIN);
                }
            } else {
                let mut x = 8;
                for (text, color) in &segments {
                    x = p.text(buf, wz, hz, x, baseline, text, *color) + 6;
                }
            }
            if let Some((cx, cw)) = caret {
                let lh = p.line_height();
                let y0 = baseline.saturating_sub(lh * 3 / 4);
                let y1 = (baseline + lh / 6).min(hz);
                draw::fill_rect(buf, wz, hz, cx, y0, cx + cw, y1, draw::BAR_FG);
            }
            // Right-aligned quick-maths result, painted last so it sits on top.
            if let Some(text) = &math {
                let tw = p.measure(text) as i32;
                let x = (wz as i32 - MARGIN - tw).max(0) as usize;
                p.text(buf, wz, hz, x, baseline, text, draw::ACCENT);
            }
        };

        if welcome {
            // No engine running: paint the welcome screen, THEN the bar on top so a
            // long welcome list can't bleed into the command bar.
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, draw::BG);
            draw_welcome(p, &mut buf, wz, hz, self.zoom as f32);
            draw_bar(&mut buf);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else if native_active {
            // Engine-free read tab: paint the document ourselves. Content is drawn
            // first; the opaque tab bar and command bar are painted on top, so lines
            // scrolled past either edge are simply covered (no per-line y-clipping).
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, draw::BG);
            let content_top = tab_h as i32;
            if let Some(nr) =
                self.active.and_then(|i| self.tabs.get(i)).and_then(|t| t.native.as_ref())
            {
                let line_h = nr.layout.line_h;
                for (li, line) in nr.layout.lines.iter().enumerate() {
                    let y_top = content_top - nr.scroll + li as i32 * line_h;
                    if y_top + line_h < content_top || y_top > bar_top as i32 {
                        continue;
                    }
                    if line.rule {
                        let ry = y_top + line_h / 2;
                        if ry >= 0 {
                            draw::fill_rect(
                                &mut buf, wz, hz, MARGIN as usize, ry as usize,
                                wz.saturating_sub(MARGIN as usize), ry as usize + 1, draw::DIM,
                            );
                        }
                        continue;
                    }
                    let baseline = y_top + line_h * 3 / 4;
                    if baseline < 0 {
                        continue;
                    }
                    // Find-in-page highlights behind this line's matches — shown both
                    // once confirmed (find.active) AND live while typing the `/` query.
                    if self.find.active || self.mode == ModeKind::Find {
                        let chars: Vec<char> =
                            line.runs.iter().flat_map(|r| r.text.chars()).collect();
                        let base = MARGIN + line.indent;
                        for (mi, m) in self.find.matches.iter().enumerate() {
                            if m.line != li {
                                continue;
                            }
                            let s = m.start.min(chars.len());
                            let e = m.end.min(chars.len());
                            let x0 = line_col_x(&line.runs, s, base, p);
                            let x1 = line_col_x(&line.runs, e, base, p);
                            let col =
                                if mi == self.find.current { draw::FIND_CUR } else { draw::FIND };
                            draw::fill_rect(
                                &mut buf, wz, hz, x0.max(0) as usize, y_top.max(0) as usize,
                                x1.max(0) as usize, (y_top + line_h).max(0) as usize, col,
                            );
                        }
                    }
                    // Read-mode caret: visual-selection highlight (behind the text).
                    if let Some(caret) = &nr.caret {
                        if let Some((s0, s1)) = caret.selection_on_row(li) {
                            let base = MARGIN + line.indent;
                            let x0 = line_col_x(&line.runs, s0, base, p).max(MARGIN);
                            let x1 = line_col_x(&line.runs, s1, base, p).max(MARGIN);
                            if x1 > x0 {
                                draw::fill_rect(
                                    &mut buf, wz, hz, x0 as usize, y_top.max(0) as usize,
                                    x1 as usize, (y_top + line_h).max(0) as usize, draw::SEL,
                                );
                            }
                        }
                    }
                    let mut x = MARGIN + line.indent;
                    for run in &line.runs {
                        x = p.text_clipped(
                            &mut buf, wz, hz, x, baseline as usize, &run.text, run.color, MARGIN,
                        );
                    }
                    // Read-mode caret: block cursor (inverse cell) on the cursor row.
                    if let Some(caret) = &nr.caret {
                        if li == caret.cy {
                            let chars: Vec<char> =
                                line.runs.iter().flat_map(|r| r.text.chars()).collect();
                            let cx0 = line_col_x(&line.runs, caret.cx, MARGIN + line.indent, p);
                            let cwid = p.measure("M").max(1) as i32;
                            draw::fill_rect(
                                &mut buf, wz, hz, cx0.max(MARGIN) as usize, y_top.max(0) as usize,
                                (cx0 + cwid) as usize, (y_top + line_h).max(0) as usize, draw::ACCENT,
                            );
                            if let Some(ch) = chars.get(caret.cx) {
                                p.text(
                                    &mut buf, wz, hz, cx0.max(MARGIN) as usize, baseline as usize,
                                    &ch.to_string(), draw::BG,
                                );
                            }
                        }
                    }
                }
            }
            // Hint labels over the visible links (filtered by what's been typed).
            if self.mode == ModeKind::Hint {
                let lh = p.line_height();
                for hint in &self.native_hints {
                    if !hint.label.starts_with(&self.hint_input) {
                        continue;
                    }
                    // New-tab mode (`F`) shows labels uppercase as the cue.
                    let label = if self.hint_new_tab {
                        hint.label.to_uppercase()
                    } else {
                        hint.label.clone()
                    };
                    let lw = p.measure(&label);
                    let bx = hint.x.max(0) as usize;
                    let by = (hint.y - (lh as i32) * 3 / 4).max(0) as usize;
                    draw::fill_rect(&mut buf, wz, hz, bx, by, bx + lw + 4, by + lh, (0xff, 0xd4, 0x00));
                    p.text(&mut buf, wz, hz, bx + 2, hint.y.max(0) as usize, &label, (0x10, 0x10, 0x10));
                }
            }
            // Tab bar + command bar painted on top of the content.
            draw::fill_band(&mut buf, wz, hz, 0, tab_h, draw::BAR_BG);
            draw_tab_bar(p, &mut buf, wz, tab_h, &tab_labels);
            draw_bar(&mut buf);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else if vim_active {
            // Engine-free `:error`/`:errors` vim pager: a monospace text grid with a
            // block cursor and (in visual mode) a selection highlight. Content first,
            // then the opaque bars on top.
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, draw::BG);
            let content_top = tab_h as i32;
            let line_h = p.line_height() as i32;
            let cw = p.measure("M").max(1) as i32;
            if let Some(vb) =
                self.active.and_then(|i| self.tabs.get(i)).and_then(|t| t.vim.as_ref())
            {
                // Pixel x of (absolute) column `col`, measured from the actual glyph
                // advances of the visible slice — so the cursor/selection line up with
                // the text exactly, at any zoom (a fixed cell width drifts on long
                // lines). Columns past end-of-line use the nominal cell width.
                let left = vb.left;
                let col_x = |line: &[char], col: usize| -> i32 {
                    if col <= left {
                        return MARGIN;
                    }
                    let end = col.min(line.len());
                    let slice: String = line[left..end].iter().collect();
                    let mut x = MARGIN + p.measure(&slice) as i32;
                    if col > line.len() {
                        x += (col - line.len()) as i32 * cw;
                    }
                    x
                };
                for r in vb.top..vb.lines.len() {
                    let y_top = content_top + (r - vb.top) as i32 * line_h;
                    if y_top >= bar_top as i32 {
                        break;
                    }
                    let line = &vb.lines[r];
                    let (yt, yb) = (y_top.max(0) as usize, (y_top + line_h).max(0) as usize);
                    // Selection highlight band for this row (visual mode).
                    if let Some((s0, s1)) = vb.selection_on_row(r) {
                        let x0 = col_x(line, s0).max(MARGIN) as usize;
                        let x1 = col_x(line, s1).max(MARGIN) as usize;
                        if x1 > x0 {
                            draw::fill_rect(&mut buf, wz, hz, x0, yt, x1, yb, draw::SEL);
                        }
                    }
                    // Find-in-page highlights for this row (confirmed or live-typing).
                    if self.find.active || self.mode == ModeKind::Find {
                        for (mi, m) in self.find.matches.iter().enumerate() {
                            if m.line != r {
                                continue;
                            }
                            let x0 = col_x(line, m.start).max(MARGIN) as usize;
                            let x1 = col_x(line, m.end).max(MARGIN) as usize;
                            if x1 > x0 {
                                let col =
                                    if mi == self.find.current { draw::FIND_CUR } else { draw::FIND };
                                draw::fill_rect(&mut buf, wz, hz, x0, yt, x1, yb, col);
                            }
                        }
                    }
                    // The visible slice of the line, scrolled left by `vb.left` cols.
                    let baseline = (y_top + line_h * 3 / 4) as usize;
                    if vb.left < line.len() {
                        let text: String = line[vb.left..].iter().collect();
                        p.text_clipped(&mut buf, wz, hz, MARGIN, baseline, &text, draw::FG, MARGIN);
                    }
                    // Block cursor (inverse cell) on the cursor row.
                    if r == vb.cy {
                        let cx0 = col_x(line, vb.cx);
                        let cx1 = col_x(line, vb.cx + 1).max(cx0 + cw);
                        draw::fill_rect(&mut buf, wz, hz, cx0 as usize, yt, cx1 as usize, yb, draw::FG);
                        if let Some(ch) = line.get(vb.cx) {
                            p.text(&mut buf, wz, hz, cx0 as usize, baseline, &ch.to_string(), draw::BG);
                        }
                    }
                }
            }
            draw::fill_band(&mut buf, wz, hz, 0, tab_h, draw::BAR_BG);
            draw_tab_bar(p, &mut buf, wz, tab_h, &tab_labels);
            draw_bar(&mut buf);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else if term_active {
            // Native terminal: paint the VT grid ourselves (no WebView2). Fill the
            // content band with the terminal background, draw the cells, then the
            // opaque bars on top.
            draw::fill_band(&mut buf, wz, hz, 0, bar_top, pty_term::BG);
            let content_top = tab_h as i32;
            let (cw, ch) = (p.measure("M").max(1) as i32, p.line_height() as i32);
            if let Some(s) = self.active.and_then(|i| self.tabs.get(i)).and_then(|t| t.term.as_ref())
            {
                pty_term::render(
                    &s.pty, p, &mut buf, wz, hz, TERM_PAD, content_top, cw, ch, bar_top as i32,
                );
            }
            draw::fill_band(&mut buf, wz, hz, 0, tab_h, draw::BAR_BG);
            draw_tab_bar(p, &mut buf, wz, tab_h, &tab_labels);
            draw_bar(&mut buf);
            buf.present().map_err(|e| anyhow::anyhow!("present: {e}"))?;
        } else {
            draw_bar(&mut buf);
            // A webview covers the middle; redraw only the top tab bar and the
            // bottom command bar so we never paint over the live page. In fullscreen
            // both bars are hidden (height 0) and the page covers the whole window —
            // present nothing native (and skip the bottom rect, which would otherwise
            // sit at y == height, out of the buffer).
            draw::fill_band(&mut buf, wz, hz, 0, tab_h, draw::BAR_BG);
            draw_tab_bar(p, &mut buf, wz, tab_h, &tab_labels);
            let mut damage = Vec::new();
            if tab_h > 0 {
                damage.push(softbuffer::Rect {
                    x: 0,
                    y: 0,
                    width: NonZeroU32::new(w).unwrap(),
                    height: NonZeroU32::new(tab_h as u32).unwrap(),
                });
            }
            if bar_h > 0 {
                damage.push(softbuffer::Rect {
                    x: 0,
                    y: bar_top as u32,
                    width: NonZeroU32::new(w).unwrap(),
                    height: NonZeroU32::new(bar_h as u32).unwrap(),
                });
            }
            buf.present_with_damage(&damage).map_err(|e| anyhow::anyhow!("present: {e}"))?;
        }
        Ok(())
    }

    /// (label, is_active, color) for each open tab, in order.
    fn tab_labels(&self) -> Vec<(String, bool, draw::Rgb)> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let active = Some(i) == self.active;
                let color = if t.term.is_some() {
                    draw::TERM
                } else if t.vim.is_some() {
                    draw::ERR
                } else if t.read {
                    draw::READ
                } else if t.research {
                    draw::RESEARCH
                } else if active {
                    draw::ACCENT
                } else {
                    draw::DIM
                };
                // Terminals label themselves by the running program's OSC title
                // (vim → the open file, Claude Code → "Claude Code"); fall back to
                // the shell name until something sets a title.
                let label = t
                    .term
                    .as_ref()
                    .and_then(|s| s.pty.title())
                    .map(|title| term_label(&title))
                    .unwrap_or_else(|| short_label(&t.url));
                (label, active, color)
            })
            .collect()
    }

    /// Whether the active tab is a read-mode tab.
    fn active_is_read(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.read).unwrap_or(false)
    }

    /// Whether the active tab is a research-mode tab.
    fn active_is_research(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).map(|t| t.research).unwrap_or(false)
    }

    /// The command verb that re-opens the active tab in its own mode, for `:edit`.
    fn active_reopen_verb(&self) -> &'static str {
        match self.active.and_then(|i| self.tabs.get(i)) {
            Some(t) if t.research => "research",
            Some(t) if t.read => "read",
            Some(t) if t.nojs => "nojs",
            _ => "open",
        }
    }

    /// Build the bar as a sequence of (text, color) segments drawn left to right.
    fn bar_segments(&self) -> Vec<(String, draw::Rgb)> {
        match self.mode {
            // The blinking caret is drawn separately (at the byte cursor), so the
            // text segment is just the literal command line. (Command/Find are drawn
            // via the dedicated caret path in `draw`, so these arms are unreached.)
            ModeKind::Command => vec![(format!(":{}", self.command), draw::BAR_FG)],
            ModeKind::Find => vec![(format!("/{}", self.command), draw::BAR_FG)],
            ModeKind::Resize => vec![
                ("[RESIZE]".into(), draw::ACCENT),
                ("  hjkl resize window · Esc done".into(), draw::DIM),
            ],
            ModeKind::Move => vec![
                ("[MOVE]".into(), draw::ACCENT),
                ("  hjkl move window · Esc done".into(), draw::DIM),
            ],
            ModeKind::Hint => vec![
                (if self.hint_new_tab { "[HINT ↗]" } else { "[HINT]" }.into(), draw::ACCENT),
                (format!(" {}", self.hint_input), draw::BAR_FG),
                (
                    if self.hint_new_tab {
                        "   label opens a new tab · Esc cancel".into()
                    } else {
                        "   type a label (UPPERCASE = new tab) · Esc cancel".into()
                    },
                    draw::DIM,
                ),
            ],
            ModeKind::Caret => vec![
                ("[CARET]".into(), draw::ACCENT),
                ("  hjkl/w/b/0/$/gg/G move · v select · y yank · Esc exit".into(), draw::DIM),
            ],
            ModeKind::Insert => {
                let url = self.active_url().unwrap_or("").to_string();
                vec![
                    ("[INSERT]".into(), draw::ACCENT),
                    (url, draw::BAR_FG),
                    ("   (Esc normal · Ctrl+V passthrough)".into(), draw::DIM),
                ]
            }
            ModeKind::Passthrough => {
                // A native terminal's input mode reads as [TERM]; a web page as [PASS].
                if self.active_is_term() {
                    vec![
                        ("[TERM]".into(), draw::TERM),
                        ("   typing to the shell · Ctrl+V paste · Shift+Esc to leave".into(), draw::DIM),
                    ]
                } else {
                    let url = self.active_url().unwrap_or("").to_string();
                    vec![
                        ("[PASS]".into(), draw::ACCENT),
                        (url, draw::BAR_FG),
                        ("   (Shift+Esc to exit)".into(), draw::DIM),
                    ]
                }
            }
            ModeKind::Normal => {
                let label = match self.active_url() {
                    Some(url) => {
                        let n = self.tabs.len();
                        let i = self.active.map(|i| i + 1).unwrap_or(0);
                        format!("{i}/{n}  {url}")
                    }
                    None => ":open <url>  (or press o)".to_string(),
                };
                let mut segs = vec![("[N]".into(), draw::ACCENT), (label, draw::BAR_FG)];
                if self.active_is_read() {
                    segs.push(("   [read]".into(), draw::READ));
                    // Read-mode caret: show [VISUAL]/[VISUAL LINE] (or [CARET]) + hint.
                    if let Some(caret) = self
                        .active
                        .and_then(|i| self.tabs.get(i))
                        .and_then(|t| t.native.as_ref())
                        .and_then(|n| n.caret.as_ref())
                    {
                        let m = caret.mode_label().unwrap_or("CARET");
                        segs.push((format!("   [{m}]"), draw::ACCENT));
                        segs.push(("  motions select · y yank · Esc exit".into(), draw::DIM));
                    }
                }
                if self.active_is_research() {
                    segs.push(("   [research]".into(), draw::RESEARCH));
                }
                // Terminal: [term] live (i types), [COPY] in vi/copy mode.
                if self.active_is_term() {
                    if self.active_term_vi() {
                        segs.push((
                            "   [COPY]  hjkl/w/b move · v select · y yank · i resume".into(),
                            draw::TERM,
                        ));
                    } else {
                        segs.push(("   [term]  i: type · Shift+Esc: copy-mode".into(), draw::TERM));
                    }
                }
                // Vim pager tabs (`:error`/`:errors`, `:res`): show [VISUAL]/[VISUAL
                // LINE] while selecting, else a hint keyed to the tab — the red [error]
                // hint must NOT bleed onto the `:res` monitor.
                if let Some(t) = self.active.and_then(|i| self.tabs.get(i)) {
                    if let Some(vb) = t.vim.as_ref() {
                        match vb.mode_label() {
                            Some(m) => segs.push((format!("   [{m}]"), draw::ACCENT)),
                            None if t.url == "browser://error" => segs.push((
                                "   [error]  v select · y yank · yi( inner ()".into(),
                                draw::ERR,
                            )),
                            None => segs.push(("   v select · y yank".into(), draw::DIM)),
                        }
                    }
                }
                if self.nojs {
                    segs.push(("   [no-js]".into(), draw::ACCENT));
                }
                // Active find-in-page: show the query and (for native tabs) the
                // current/total match counter; `n`/`N` step, Esc clears.
                if self.find.active {
                    let label = if self.active_webview().is_some() {
                        format!("   /{}  · n/N · Esc", self.find.query)
                    } else {
                        format!("   {}  · n/N · Esc", self.find_count_label())
                    };
                    segs.push((label, draw::FIND_CUR));
                }
                if !self.status.is_empty() {
                    let color = if self.status_is_error { draw::ERR } else { draw::DIM };
                    segs.push((format!("   {}", self.status), color));
                }
                segs
            }
        }
    }
}

/// Cap captured command output so a runaway command can't balloon memory.
const TERM_OUTPUT_CAP: usize = 200_000;

/// Maximum number of past errors kept in the session log (oldest dropped first).
const ERROR_LOG_CAP: usize = 200;

/// Run `cmd` through the platform shell, returning combined stdout+stderr and the
/// exit code. Blocking — call from a background thread.
fn exec_command(cmd: &str) -> (String, Option<i32>) {
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    match command.output() {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&err);
            }
            if s.len() > TERM_OUTPUT_CAP {
                s.truncate(TERM_OUTPUT_CAP);
                s.push_str("\n… (output truncated)");
            }
            (s, out.status.code())
        }
        Err(e) => (format!("failed to run command: {e}"), None),
    }
}

/// Stylesheet for the internal `:commands` / `:version` pages.
const HELP_CSS: &str = "html{background:#1e1e1e;color:#d0d0d0}body{margin:0}\
main{max-width:820px;margin:40px auto;padding:0 22px;\
font:16px/1.6 -apple-system,Segoe UI,Roboto,sans-serif}\
h1{color:#fff;font-size:1.7em;margin:0 0 .2em}h2{color:#6cb6ff;font-size:1.1em;\
margin:1.6em 0 .4em;border-bottom:1px solid #333;padding-bottom:.2em}\
p.sub{color:#888;margin:0 0 1em}table{border-collapse:collapse;width:100%}\
td{padding:3px 10px 3px 0;vertical-align:top}td.k{white-space:nowrap;color:#e6a55e;\
font-family:Consolas,monospace;width:1%}kbd{background:#2a2a2a;border:1px solid #444;\
border-radius:4px;padding:1px 6px;font-family:Consolas,monospace;font-size:.9em;color:#f0f0f0}\
td.d{color:#cfcfcf}";

/// Minimal HTML-escaping for text interpolated into the internal pages.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Render rows of (key, description) into a `<table>`, escaping both columns.
fn help_table(rows: &[(&str, &str)]) -> String {
    let mut s = String::from("<table>");
    for (k, d) in rows {
        s.push_str(&format!(
            "<tr><td class=\"k\">{}</td><td class=\"d\">{}</td></tr>",
            html_escape(k),
            html_escape(d)
        ));
    }
    s.push_str("</table>");
    s
}

/// Build the plain-text lines shown by `:error` / `:errors` in the vim pager. Each
/// error becomes a header line (`[HH:MM:SS] :command — error N`) followed by its
/// message (split on newlines), with a blank line between entries. `all = false`
/// renders only the most recent error; `all = true` renders every logged error in
/// chronological order (oldest first, newest last). The text is intentionally flat
/// so vim motions/text-objects work cleanly over it (e.g. `yi(` to grab a
/// `HRESULT(0x…)` token).
fn error_lines(errors: &[ErrorEntry], all: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut emit = |n: usize, e: &ErrorEntry| {
        let cmd = e.command.as_deref().unwrap_or("(no command)");
        out.push(format!("[{}] {} — error {}", e.time, cmd, n + 1));
        for line in e.message.lines() {
            out.push(line.to_string());
        }
        out.push(String::new());
    };
    if all {
        for (n, e) in errors.iter().enumerate() {
            emit(n, e);
        }
    } else if let Some(e) = errors.last() {
        emit(errors.len() - 1, e);
    }
    // Drop the trailing blank so the buffer doesn't end on an empty line.
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// A "word" character for command-bar word motions (`Ctrl+W`, `Ctrl+←/→`):
/// alphanumerics and `_`. Everything else — `/ . : - ? & = # @ ~ + …` — is a
/// separator, so word jumps/deletes stop at URL and path boundaries.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offset of the start of the word before `pos`: skip trailing separators,
/// then the word run. So `Ctrl+W` on `…/foo/bar` erases `bar` (then `/`, then
/// `foo`), not the entire URL.
fn prev_word_boundary(s: &str, pos: usize) -> usize {
    let trimmed = s[..pos].trim_end_matches(|c| !is_word_char(c));
    trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| !is_word_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

/// Byte offset of the end of the word after `pos`: skip leading separators, then
/// the word run.
fn next_word_boundary(s: &str, pos: usize) -> usize {
    let rest = &s[pos..];
    let after_sep = rest.trim_start_matches(|c| !is_word_char(c));
    let sep = rest.len() - after_sep.len();
    let word = after_sep.find(|c| !is_word_char(c)).unwrap_or(after_sep.len());
    pos + sep + word
}

/// Encode `s` as a JavaScript string literal (quoted, with control/quote/`<`
/// escaped) for safe interpolation into an `evaluate_script` call.
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Case-insensitive (ASCII) find-in-page over `lines`, returning every match as a
/// `(line, char-range)`. Matches don't span lines (each visual/buffer line is
/// searched independently), which is fine for the short queries this is for.
fn find_in_lines(lines: &[String], q: &str) -> Vec<NativeMatch> {
    let needle: Vec<char> = q.chars().map(|c| c.to_ascii_lowercase()).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let hay: Vec<char> = line.chars().map(|c| c.to_ascii_lowercase()).collect();
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()] == needle[..] {
                out.push(NativeMatch { line: li, start: i, end: i + needle.len() });
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Whether `url` points at Google Translate's page-proxy (`*.translate.goog`).
fn is_translate_proxy(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.ends_with(".translate.goog")))
        .unwrap_or(false)
}

/// Reverse a `*.translate.goog` proxy URL back to the original site. Google encodes
/// the host (`.`→`-`, `-`→`--`) and appends `_x_tr_*` query params; we decode the
/// host, keep the path and any genuine query params, and drop the `_x_tr_*` ones.
/// Returns `None` if `url` isn't a translate-proxy URL we can decode.
fn deproxy_translate(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let encoded = parsed.host_str()?.strip_suffix(".translate.goog")?;
    // '--' is a literal '-'; a lone '-' is a '.'.
    let mut decoded = String::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' {
            if chars.peek() == Some(&'-') {
                chars.next();
                decoded.push('-');
            } else {
                decoded.push('.');
            }
        } else {
            decoded.push(c);
        }
    }
    let mut out = parsed.clone();
    out.set_host(Some(&decoded)).ok()?;
    // The proxy serves over its own host on 443; the original default port applies.
    let _ = out.set_port(None);
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("_x_tr"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        out.set_query(None);
    } else {
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in &kept {
            ser.append_pair(k, v);
        }
        out.set_query(Some(&ser.finish()));
    }
    Some(out.to_string())
}

/// Local wall-clock time as `HH:MM:SS`, for stamping logged errors.
#[cfg(windows)]
fn now_hms() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
}

#[cfg(not(windows))]
fn now_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// The `:commands` page: every keybind and command (not customizable yet).
fn commands_document() -> String {
    let normal = help_table(&[
        (":", "open the command bar"),
        ("o / O", "open a page in THIS tab / in a new tab (prefills “open ” / “open -t ”)"),
        ("j / k", "scroll down / up"),
        ("Ctrl+D / Ctrl+U", "scroll half a page down / up"),
        ("g / G", "jump to top / bottom"),
        ("/", "find in page — type to search live; works on web, read & error tabs"),
        ("n / N", "next / previous match (while a search is active); Esc clears"),
        ("i", "insert mode (passthrough on a terminal tab)"),
        ("f / F", "hint mode — label every link, type the label to follow (F: open in a new tab)"),
        ("v / V", "caret/visual select on read & web tabs — hjkl/w/b move, y yank, Esc exits"),
        ("x", "close the current tab"),
        ("u / Ctrl+Shift+T", "reopen the last closed tab"),
        ("r", "reload the page"),
        ("H / L", "history back / forward"),
        ("n / p", "next / previous tab"),
        ("1 – 9", "jump straight to tab N"),
        ("< / >", "move the current tab left / right"),
        ("Ctrl+V", "passthrough mode (every key to the page)"),
        ("Ctrl +/-/0", "zoom the whole UI in / out / reset"),
    ]);
    let cmdline = help_table(&[
        ("Enter", "run the command"),
        ("Esc / Ctrl+C", "cancel (Ctrl+C copies first if text is selected)"),
        ("Left / Right", "move the caret a character"),
        ("Ctrl+Left / Right", "move the caret a word"),
        ("Home / End", "jump to start / end of line"),
        ("Tab / Ctrl+Right", "accept the autocomplete suggestion (verb, or :open URL from history)"),
        ("Shift+ movement", "extend the selection (with arrows, Ctrl+arrows, Home/End)"),
        ("Ctrl+A", "select the whole line"),
        ("Ctrl+C / Ctrl+X / Ctrl+V", "copy / cut / paste"),
        ("Backspace / Delete", "delete back / forward (or the selection)"),
        ("Ctrl+W · Ctrl/Alt+Backspace", "delete the previous word"),
        ("Ctrl+Delete", "delete the next word"),
        ("Ctrl+U", "delete to the start of the line"),
    ]);
    let modes = help_table(&[
        ("Insert", "type into a field; Esc or click-away leaves, Ctrl+V → passthrough"),
        ("Passthrough", "every key goes to the page; Shift+Esc leaves"),
        ("Hint", "type a label to follow it (type it UPPERCASE to open in a new tab); Esc cancels"),
        ("Resize / Move", "hjkl to size / reposition the window; Esc finishes"),
    ]);
    let vimpager = help_table(&[
        ("h j k l · arrows", "move the cursor"),
        ("w / b / e", "next / previous / end of word"),
        ("0 / ^ / $", "start / first non-blank / end of line"),
        ("f / t  (F / T)", "jump to / before a char forward (back); ; , repeat"),
        ("gg / G", "top / bottom; Ctrl+D / Ctrl+U half-page"),
        ("v / V", "charwise / linewise visual select"),
        ("y", "yank: the selection, or with a motion (yy, yw, y$, yf), yt;)"),
        ("yiw · yi( · ya\"", "yank inner/around a text object (word, (), {}, [], <>, quotes)"),
    ]);
    let cmds = help_table(&[
        (":open <url|query> · :o", "open in THIS tab (non-URL → search engine); add -t for a new tab"),
        (":tabopen · :t", "open in a new tab (same as :open -t)"),
        (":reopen · :undo", "reopen the last closed tab (also U / Ctrl+Shift+T)"),
        (":research <url|query> · :rs", "lighter browse: JS on, images kept, media/embeds stripped (-t = new tab)"),
        (":edit · :e", "edit the current URL (re-opens in the tab's own mode)"),
        (":y · :yank", "copy the current URL to the clipboard"),
        (":read <url|query>", "engine-free reader (no WebView2) in this tab; -t = new tab; non-URL → search"),
        (":search [name|template]", "show/set the search engine — a name (ddg/google/wiki…) or a %s URL"),
        (":te", "native terminal (Ctrl+V pastes · Shift+Esc → vim copy-mode: navigate/yank, i resumes)"),
        (":te <command>", "run a local command, result in the command bar"),
        (":shell <program>", "set the terminal shell (e.g. :shell nu, :shell bash)"),
        (":js", "toggle JavaScript (reloads this tab; applies to new tabs)"),
        (":nojs <url>", "open a single page with JavaScript disabled"),
        (":ads · :adblock", "toggle the ad/tracker blocker (live, all tabs; on by default)"),
        (":pops · :popups", "toggle blocking scripted pop-ups (live, all tabs)"),
        (":mute · :audio", "toggle muting all page audio/video (live, all tabs)"),
        (":css", "toggle page styling off/on (live, all tabs)"),
        (":close · :bd", "close the current tab"),
        (":reload · :r", "reload"),
        (":tabnext · :tn · :tabprev · :tp", "switch tabs"),
        (":back · :forward", "history navigation"),
        (":f · :fullscreen", "toggle fullscreen (hides the bars; `:` brings them back). YouTube's fullscreen button does this too"),
        (":resize · :move", "window-control modes (then hjkl, Esc)"),
        (":error · :err", "latest error in a read-only vim tab (v/y to select & copy)"),
        (":errors · :errs", "every error this session (newest first), same vim tab"),
        (":res · :resources", "live memory/CPU/disk across the whole browser tree (freezes while you select)"),
        (":commands · :help", "this page"),
        (":version", "version and build information"),
        (":w · :write", "save the current session (open tabs + UI state) to disk"),
        (":wq · :x", "save the session, then quit"),
        (":quit · :q", "quit WITHOUT saving (the last :w'd session is kept)"),
    ]);
    // Bangs: build `!key → description` rows from the core table.
    let bang_rows: Vec<(String, &str)> =
        browser_core::bang_list().into_iter().map(|(k, d)| (format!("!{k} <query>"), d)).collect();
    let bangs = help_table(
        &bang_rows.iter().map(|(k, d)| (k.as_str(), *d)).collect::<Vec<_>>(),
    );
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>commands</title><style>{HELP_CSS}</style></head><body><main>\
         <h1>Commands &amp; keybindings</h1>\
         <p class=\"sub\">Not customizable yet — these are the built-in bindings.</p>\
         <h2>Normal mode</h2>{normal}\
         <h2>Command-line editing</h2>{cmdline}\
         <h2>Other modes</h2>{modes}\
         <h2>Vim pager (:error · :errors · :res · :version · read-mode v/V)</h2>{vimpager}\
         <h2>Commands</h2>{cmds}\
         <h2>Bangs</h2>\
         <p class=\"sub\">A <code>!key</code> token in any open/search target jumps to that \
         site's search (no query → the site's home). Trailing form works too: \
         <code>dragon scimitar !osrs</code>.</p>{bangs}\
         <h2>Quick maths</h2>\
         <p class=\"sub\">Type an arithmetic expression in the command bar \
         (<code>+ - * / %  ^</code>, parentheses) to see the result live, e.g. \
         <code>:20*8</code> → <code>= 160</code>. Press Enter to replace the line with the \
         result so you can copy it or keep calculating (<code>160+10</code>).</p>\
         </main></body></html>"
    )
}

/// The `:version` page: build/runtime details about this browser.
/// Plain-text lines for the `:version` pager (navigable/yankable with vim motions).
fn version_lines() -> Vec<String> {
    let kv = [
        ("Name", env!("CARGO_PKG_NAME")),
        ("Version", env!("CARGO_PKG_VERSION")),
        ("Description", env!("CARGO_PKG_DESCRIPTION")),
        ("Authors", env!("CARGO_PKG_AUTHORS")),
        ("Engine", "WebView2 (Chromium) via wry 0.55 — loaded on demand"),
        ("Windowing", "tao 0.35 + softbuffer/fontdue native chrome"),
        ("Terminal", "native alacritty_terminal VT engine + a browser-pty-host companion (ConPTY)"),
        ("Platform", std::env::consts::OS),
        ("Architecture", std::env::consts::ARCH),
    ];
    let mut lines = vec![
        format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        String::new(),
    ];
    for (k, v) in kv {
        lines.push(format!("  {:<14}{}", format!("{k}:"), v));
    }
    lines.push(String::new());
    lines.push("A modal, mode-dispatching browser — only what's needed, when needed.".into());
    lines
}

/// Whether `program` resolves to an executable: an explicit path that exists, or a
/// bare name found on `PATH` (trying `PATHEXT` extensions on Windows, so `:shell nu`
/// matches `nu.exe`). Used to reject a `:shell` typo before it breaks `:te`.
fn program_exists(program: &str) -> bool {
    use std::path::{Path, PathBuf};
    let exts: Vec<String> = if cfg!(windows) {
        let mut v = vec![String::new()];
        let pe = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        v.extend(pe.split(';').filter(|s| !s.is_empty()).map(|s| s.to_lowercase()));
        v
    } else {
        vec![String::new()]
    };
    let exists_with_ext = |base: &Path| -> bool {
        exts.iter().any(|ext| {
            let cand = if ext.is_empty() {
                base.to_path_buf()
            } else {
                PathBuf::from(format!("{}{}", base.display(), ext))
            };
            cand.is_file()
        })
    };
    if program.contains(['/', '\\']) {
        return exists_with_ext(Path::new(program));
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| exists_with_ext(&dir.join(program)))
    })
}

/// Locate the companion `browser-pty-host` binary next to our own executable.
fn pty_host_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "browser-pty-host.exe"
    } else {
        "browser-pty-host"
    };
    Some(exe.parent()?.join(name))
}

/// Resolve a `:search` argument into a search-URL template: a bare engine name
/// (looked up in the bang table — `ddg`, `google`, `wiki`, `yt`, …) maps to its
/// template; anything containing `%s` or `://` is taken as a literal template.
/// `None` for an unrecognized name with no `%s`.
fn search_template_for(arg: &str) -> Option<String> {
    let a = arg.trim();
    if a.contains("%s") || a.contains("://") {
        return Some(a.to_string());
    }
    browser_core::bang_search_template(a).map(|t| t.to_string())
}

/// Pixel x of character column `col` within a read-view line, computed by
/// replicating exactly how the line is painted: the pen runs in f32 within a run
/// but is floored to an `i32` at every run boundary (because `text_clipped` takes
/// and returns an `i32` x). Using `measure()` of the flattened prefix instead drifts
/// to the right as the column and zoom grow (TODO #8) — the per-run floors add up.
fn line_col_x(runs: &[read_view::Run], col: usize, base: i32, p: &Painter) -> i32 {
    let mut x = base as f32;
    let mut c = 0usize;
    for run in runs {
        for ch in run.text.chars() {
            if c == col {
                return x as i32;
            }
            x += p.advance(ch);
            c += 1;
        }
        x = x.floor(); // text_clipped returns `pen as i32` between runs
    }
    x as i32
}

/// Encode a key event into the bytes a PTY expects (the inverse of what xterm.js
/// Encode a single mouse-button event for a terminal in mouse-reporting mode.
/// `button` is the xterm button code (64/65 = wheel up/down), `col`/`row` are
/// 1-based cell coordinates. Uses the SGR (1006) form when the program asked for
/// it, else the legacy X10 byte form. Wheel events are press-only (no release).
fn encode_mouse_wheel(sgr: bool, button: u8, col: i32, row: i32) -> Vec<u8> {
    if sgr {
        format!("\x1b[<{button};{col};{row}M").into_bytes()
    } else {
        // X10: each field is its value + 32, clamped to one byte.
        let cb = 32u8.saturating_add(button);
        let cx = (32 + col).clamp(32, 255) as u8;
        let cy = (32 + row).clamp(32, 255) as u8;
        vec![0x1b, b'[', b'M', cb, cx, cy]
    }
}

/// used to do). Covers printable input, Ctrl-combos → control codes, Enter/Tab/
/// Backspace/Esc, and the cursor/navigation keys (honoring DECCKM app-cursor mode).
/// `None` for keys with no terminal meaning. Alt prefixes the sequence with ESC.
fn encode_term_key(key: &Key, ctrl: bool, alt: bool, shift: bool, app_cursor: bool) -> Option<Vec<u8>> {
    let cursor = |c: u8| -> Vec<u8> {
        let mut v = if app_cursor { vec![0x1b, b'O'] } else { vec![0x1b, b'['] };
        v.push(c);
        v
    };
    let mut out: Vec<u8> = match key {
        Key::Character(s) => {
            if ctrl {
                vec![ctrl_byte(s.chars().next()?)?]
            } else {
                s.as_bytes().to_vec()
            }
        }
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![0x7f],
        // Shift+Tab is the "back-tab" CSI Z — TUIs (Claude Code's mode switch,
        // form/field navigation) need it to move backward; plain Tab otherwise.
        Key::Tab => {
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        Key::Escape => vec![0x1b],
        Key::Space => vec![b' '],
        Key::ArrowUp => cursor(b'A'),
        Key::ArrowDown => cursor(b'B'),
        Key::ArrowRight => cursor(b'C'),
        Key::ArrowLeft => cursor(b'D'),
        Key::Home => cursor(b'H'),
        Key::End => cursor(b'F'),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::Insert => b"\x1b[2~".to_vec(),
        _ => return None,
    };
    // Alt = Meta: prefix with ESC (unless the sequence already starts with one).
    if alt && out.first() != Some(&0x1b) {
        let mut v = vec![0x1b];
        v.append(&mut out);
        out = v;
    }
    Some(out)
}

/// Control code for Ctrl+<char>: `Ctrl+A`→0x01 … `Ctrl+Z`→0x1a, `Ctrl+[`→ESC,
/// `Ctrl+Space`→NUL, etc. `None` for non-controllable keys.
fn ctrl_byte(c: char) -> Option<u8> {
    if !c.is_ascii() {
        return None;
    }
    let u = c.to_ascii_uppercase() as u8;
    if (0x40..=0x5f).contains(&u) {
        Some(u & 0x1f)
    } else if u == b' ' {
        Some(0)
    } else {
        None
    }
}

/// Compact display form of a URL for autocomplete: scheme + `www.` + trailing
/// slash stripped (e.g. `https://www.youtube.com/` → `youtube.com`). The result is
/// still openable (`resolve_target` re-adds the scheme).
fn history_display(url: &str) -> String {
    let s = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    let s = s.strip_prefix("www.").unwrap_or(s);
    s.trim_end_matches('/').to_string()
}

/// A short tab label: the host without scheme/`www.`, truncated.
fn short_label(url: &str) -> String {
    let s = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = s.split('/').next().unwrap_or(s);
    let host = host.strip_prefix("www.").unwrap_or(host);
    truncate_label(host)
}

/// Turn a terminal's raw OSC title into a tidy tab label. Strips the editor suffix
/// vim/nvim append (`<file> (<dir>) - VIM`) and reduces a bare path to its final
/// component, so a shell sitting in `C:\projects\browser` reads as `browser` and an
/// open file reads as just its name; anything else (e.g. `Claude Code`) is kept.
fn term_label(title: &str) -> String {
    let t = title.trim();
    // vim/nvim: "<file> (<dir>) - VIM"/"- NVIM" → keep the part before " - ".
    let t = t.split(" - ").next().unwrap_or(t).trim();
    // Drop the trailing "(<dir>)" annotation vim appends after the filename.
    let t = t.split(" (").next().unwrap_or(t).trim();
    // A bare path → its last segment; otherwise leave the text alone.
    let last = t.rsplit(['\\', '/']).next().filter(|s| !s.is_empty()).unwrap_or(t);
    truncate_label(if last.is_empty() { t } else { last })
}

/// Cap a tab label at 22 characters, appending an ellipsis when it overflows.
fn truncate_label(s: &str) -> String {
    let mut label = s.to_string();
    if label.chars().count() > 22 {
        label = label.chars().take(21).collect::<String>();
        label.push('…');
    }
    label
}

/// Draw the top tab bar: `[1:host]` for the active tab, ` 2:host ` for others.
/// Read-mode tabs are tinted green.
fn draw_tab_bar(p: &Painter, buf: &mut [u32], w: usize, h: usize, labels: &[(String, bool, draw::Rgb)]) {
    let baseline = h * 2 / 3;
    let mut x = 8;
    for (i, (label, active, color)) in labels.iter().enumerate() {
        let color = *color;
        let text = if *active {
            format!("[{}:{}]", i + 1, label)
        } else {
            format!(" {}:{} ", i + 1, label)
        };
        x = p.text(buf, w, h, x, baseline, &text, color) + 6;
        if x > w.saturating_sub(40) {
            p.text(buf, w, h, x, baseline, "…", draw::DIM);
            break;
        }
    }
}

/// Paint the engine-free welcome screen: title + a key/command cheat-sheet.
/// `scale` is the global zoom factor so column offsets track the scaled font.
fn draw_welcome(p: &Painter, buf: &mut [u32], w: usize, h: usize, _scale: f32) {
    let lh = p.line_height();
    // A clean splash: the name + tagline centered, with one quiet hint below.
    let name = "browser";
    let tag = "  — lightweight modal shell";
    let title_w = p.measure(name) + p.measure(tag);
    let tx = w.saturating_sub(title_w) / 2;
    let ty = h / 2 - lh;
    let after = p.text(buf, w, h, tx, ty, name, draw::ACCENT);
    p.text(buf, w, h, after, ty, tag, draw::DIM);
    let hint = ":open <url> to start   ·   :commands for all keybindings";
    let hint_w = p.measure(hint);
    p.text(buf, w, h, w.saturating_sub(hint_w) / 2, ty + lh * 2, hint, draw::DIM);
}

#[cfg(test)]
mod tests {
    use super::vim::{Key, TextBuffer};
    use super::{
        deproxy_translate, error_lines, is_translate_proxy, next_word_boundary, parse_tab_flag,
        prev_word_boundary, ErrorEntry,
    };

    #[test]
    fn tab_flag_only_matches_whole_token() {
        // `-t` / `--tab` as a leading token → new tab, flag stripped.
        assert_eq!(parse_tab_flag("-t youtube.com"), (true, "youtube.com"));
        assert_eq!(parse_tab_flag("--tab youtube.com"), (true, "youtube.com"));
        assert_eq!(parse_tab_flag("-t"), (true, ""));
        // No flag → opens in the current tab, target untouched.
        assert_eq!(parse_tab_flag("youtube.com"), (false, "youtube.com"));
        // A target that merely starts with `-t` is NOT the flag.
        assert_eq!(parse_tab_flag("-test query"), (false, "-test query"));
        assert_eq!(parse_tab_flag("rust -t async"), (false, "rust -t async"));
    }

    #[test]
    fn ctrl_w_deletes_one_url_segment_at_a_time() {
        let url = "https://example.com/foo/bar";
        // Caret at the end: prev word is `bar`, leaving the trailing slash.
        let p1 = prev_word_boundary(url, url.len());
        assert_eq!(&url[..p1], "https://example.com/foo/");
        // Again from there: skip the `/`, delete `foo`.
        let p2 = prev_word_boundary(url, p1);
        assert_eq!(&url[..p2], "https://example.com/");
        // Not the whole thing in one go.
        assert_ne!(p1, 0);
    }

    #[test]
    fn word_motions_step_over_separators() {
        let s = "ab.cd";
        assert_eq!(next_word_boundary(s, 0), 2); // end of `ab`
        assert_eq!(next_word_boundary(s, 2), 5); // skip `.`, end of `cd`
        assert_eq!(prev_word_boundary(s, 5), 3); // start of `cd`
    }

    #[test]
    fn find_in_lines_is_case_insensitive_and_per_line() {
        let lines = vec!["The Rust language".to_string(), "rust rust".to_string()];
        let m = super::find_in_lines(&lines, "rust");
        // One on line 0 (case-insensitive), two on line 1.
        assert_eq!(m.len(), 3);
        assert_eq!((m[0].line, m[0].start, m[0].end), (0, 4, 8));
        assert_eq!((m[1].line, m[1].start), (1, 0));
        assert_eq!((m[2].line, m[2].start), (1, 5));
    }

    #[test]
    fn js_string_escapes_quotes_newlines_and_angle() {
        assert_eq!(super::js_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(super::js_string("a\nb"), r#""a\nb""#);
        // `<` becomes the unicode escape < (guard against `</script>`).
        assert!(super::js_string("x<y").contains("\\u003c"));
    }

    #[test]
    fn detects_and_deproxies_translate_urls() {
        let proxied = "https://monsterhunterrise-wiki-fextralife-com.translate.goog/Monster+Hunter+Rise+Wiki?_x_tr_sl=en&_x_tr_tl=pt&_x_tr_hl=pt-BR";
        assert!(is_translate_proxy(proxied));
        assert_eq!(
            deproxy_translate(proxied).as_deref(),
            Some("https://monsterhunterrise.wiki.fextralife.com/Monster+Hunter+Rise+Wiki")
        );
    }

    #[test]
    fn deproxy_keeps_real_query_and_decodes_literal_dashes() {
        // `my--site` decodes to `my-site` (a literal dash); `q=1` is a real param.
        let url = "https://my--site-com.translate.goog/p?q=1&_x_tr_sl=en";
        assert_eq!(deproxy_translate(url).as_deref(), Some("https://my-site.com/p?q=1"));
    }

    #[test]
    fn non_translate_urls_are_left_alone() {
        assert!(!is_translate_proxy("https://example.com/translate"));
        assert_eq!(deproxy_translate("https://example.com/"), None);
    }

    fn entry(time: &str, command: Option<&str>, message: &str) -> ErrorEntry {
        ErrorEntry {
            time: time.into(),
            command: command.map(Into::into),
            message: message.into(),
        }
    }

    #[test]
    fn latest_error_has_header_then_message_lines() {
        let errs = vec![
            entry("00:00:01", Some(":open a"), "boom"),
            entry("00:00:02", None, "bad\nthings"),
        ];
        let lines = error_lines(&errs, false);
        assert_eq!(lines[0], "[00:00:02] (no command) — error 2");
        assert_eq!(&lines[1..], &["bad".to_string(), "things".to_string()]);
    }

    #[test]
    fn all_errors_are_oldest_first_with_command_and_time() {
        let errs = vec![entry("00:00:01", Some(":open a"), "e1"), entry("00:00:09", Some(":bad"), "e2")];
        let lines = error_lines(&errs, true);
        assert_eq!(lines[0], "[00:00:01] :open a — error 1");
        assert_eq!(lines[1], "e1");
        assert_eq!(lines[2], ""); // blank separator
        assert_eq!(lines[3], "[00:00:09] :bad — error 2");
        assert_eq!(lines[4], "e2");
    }

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
