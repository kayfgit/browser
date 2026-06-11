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

use std::rc::Rc;

use anyhow::{Context as _, Result};
use std::time::{Duration, Instant};

use tao::event::{ElementState, Event, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::keyboard::ModifiersState;
use tao::window::{Window, WindowBuilder};
use wry::dpi::{PhysicalPosition, PhysicalSize};
use wry::{PageLoadEvent, Rect, WebView, WebViewBuilder, WebViewBuilderExtWindows};

mod chrome;
mod draw;
mod find;
mod hints;
mod keys;
mod pages;
mod panes;
mod procmon;
mod pty_term;
mod read_view;
mod session;
mod term;
mod vim;
use draw::Painter;
use panes::{PaneNode, PaneRect, SplitDir, FOCUS_BORDER};
use find::FindState;
use hints::NativeHint;
use pages::{commands_document, now_hms, ErrorEntry, ERROR_LOG_CAP};
use term::{program_exists, TermSession};

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
    "vsplit", "split", "write", "wq", "quit",
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
/// rest type; in `passthrough` it takes only the leave chord (Ctrl+S or Shift+Esc)
/// and lets every other key reach the page. In insert it also reports when focus leaves the editable element,
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
      // Ctrl+S (easy reach) or Shift+Esc (legacy) leaves passthrough.
      if ((e.ctrlKey && (e.key === 's' || e.key === 'S')) || (e.key === 'Escape' && e.shiftKey)) {
        e.preventDefault(); e.stopPropagation(); post('leave-passthrough');
      }
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
  // A real pointer press anywhere in this page tells the shell to focus THIS pane
  // (when split). Fired on pointerdown — before any link navigation — so clicking a
  // non-focused web pane switches focus to it instead of stranding the keyboard.
  document.addEventListener('pointerdown', function () { post('pane-click'); }, true);
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
  // Right-click menu: WebView2's default menu is full of options that don't work
  // here (and flickered shut). Replace it with our own one-item menu — "Open in
  // new tab" — shown only over a real link; the shell opens it via `hint-open`.
  var __ctxMenu = null;
  function ctxClose() { if (__ctxMenu) { __ctxMenu.remove(); __ctxMenu = null; } }
  document.addEventListener('contextmenu', function (e) {
    e.preventDefault(); // always kill the broken native menu
    ctxClose();
    var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    var href = (a && a.href && !/^javascript:/i.test(a.href)) ? a.href : null;
    if (!href) return; // nothing actionable under the cursor
    var menu = document.createElement('div');
    menu.style.cssText = 'position:fixed;z-index:2147483647;left:' + e.clientX + 'px;top:' +
      e.clientY + 'px;background:#222;color:#eee;font:13px sans-serif;border:1px solid #444;' +
      'border-radius:4px;padding:4px 0;box-shadow:0 2px 8px rgba(0,0,0,.5);min-width:150px;';
    var item = document.createElement('div');
    item.textContent = 'Open in new tab';
    item.style.cssText = 'padding:6px 14px;white-space:nowrap;cursor:pointer;';
    item.addEventListener('mouseenter', function () { item.style.background = '#0a84ff'; });
    item.addEventListener('mouseleave', function () { item.style.background = ''; });
    item.addEventListener('click', function (ev) {
      ev.stopPropagation(); ctxClose(); post('hint-open:' + href);
    });
    menu.appendChild(item);
    // Clamp to the viewport so a menu near the edges stays fully on-screen.
    document.documentElement.appendChild(menu);
    var r = menu.getBoundingClientRect();
    if (r.right > innerWidth) menu.style.left = Math.max(0, innerWidth - r.width) + 'px';
    if (r.bottom > innerHeight) menu.style.top = Math.max(0, innerHeight - r.height) + 'px';
    __ctxMenu = menu;
  }, true);
  // Dismiss the menu on an outside click (but not a click INSIDE it — that path
  // runs the item's own handler), on scroll, or Escape. (No window-blur close: the
  // shell's focus-reclaim blurs the webview routinely, which would shut it early.)
  document.addEventListener('click', function (e) {
    if (__ctxMenu && !__ctxMenu.contains(e.target)) ctxClose();
  }, true);
  document.addEventListener('scroll', ctxClose, true);
  document.addEventListener('keydown', function (e) { if (e.key === 'Escape') ctxClose(); }, true);
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
    /// A web pane was clicked (pointerdown): focus the pane under the cursor.
    PaneClick,
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
    /// navigation. Enter: Ctrl+V. Leave: Ctrl+S (or Shift+Esc).
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

impl Tab {
    /// A blank tab: no engine, no content. Used to fill a freshly `:split` pane —
    /// it paints an empty "open something" prompt and is replaced in place by the
    /// first `:open`/`:te`/`:read`/… run while it's focused.
    fn blank() -> Tab {
        Tab {
            webview: None,
            url: BLANK_URL.to_string(),
            nojs: false,
            read: false,
            research: false,
            native: None,
            vim: None,
            term: None,
        }
    }

    /// Whether this is a blank (empty) pane placeholder.
    fn is_blank(&self) -> bool {
        self.webview.is_none()
            && self.native.is_none()
            && self.vim.is_none()
            && self.term.is_none()
    }
}

/// Sentinel URL for a blank pane (see [`Tab::blank`]).
const BLANK_URL: &str = "browser://blank";

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
    /// tmux-style pane layout tiling the content area. `None` = a single pane (the
    /// active tab fills the content band, exactly as without splits). `Some(tree)`
    /// once the user `:split`s: leaves reference distinct tab indices, the focused
    /// leaf is the active tab, and Ctrl+W h/j/k/l moves focus between panes.
    split: Option<PaneNode>,
    /// True after Ctrl+W in Normal mode: the next h/j/k/l moves pane focus and
    /// s/v splits (vim-window style). Cleared by the following key.
    pending_window_key: bool,
    /// Cached by `refresh_visibility()`: the focused pane shows a webview, which can
    /// trap keyboard focus on click — arms the fast (300 ms) focus backstop.
    active_pane_is_webview: bool,
    /// Cached by `refresh_visibility()`: a webview is visible in some *non-focused*
    /// pane. It can still steal focus on a stray click, but that's rare — a slow
    /// (1 s) backstop tier is enough, so a focused native pane stays near-idle.
    background_webview_visible: bool,
    /// Scrollback lines kept per terminal (memory scales with it; see
    /// [`pty_term::DEFAULT_SCROLLBACK`]).
    term_scrollback: usize,
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
            layout: read_view::Layout {
                lines: Vec::new(),
                line_h: 1,
                height: 0,
                text: Vec::new(),
            },
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
        split: None,
        pending_window_key: false,
        active_pane_is_webview: false,
        background_webview_visible: false,
        term_scrollback: pty_term::DEFAULT_SCROLLBACK,
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
                    } else if app.split.is_some() {
                        // Click a (native) pane below the tab bar to focus it. Web
                        // panes consume the click in their own HWND, so this only
                        // fires for terminal/read/vim/blank panes — Ctrl+W covers the
                        // rest.
                        if let Some((tab, _)) =
                            app.pane_at_pixel(app.cursor_pos.0, app.cursor_pos.1)
                        {
                            app.set_active_pane(tab);
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
            Event::UserEvent(UserEvent::PaneClick) => {
                // Clicking inside a web pane focuses it (web panes consume the click in
                // their HWND, so this is the only way they reach us). Use the OS cursor
                // position, since CursorMoved isn't delivered over a child webview.
                if app.split.is_some() {
                    if let Some((x, y)) = app.cursor_client_pos() {
                        if let Some((tab, _)) = app.pane_at_pixel(x as f64, y as f64) {
                            app.set_active_pane(tab);
                        }
                    }
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
            } else if app.mode == ModeKind::Normal && app.active_pane_is_webview {
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
        self.refresh_visibility();
        self.window.request_redraw();
    }

    /// Re-fit every visible web pane to the current chrome layout and repaint. Called
    /// on transitions that change which bars are visible without resizing the window —
    /// entering/leaving the command bar while fullscreen — so pages grow to fill the
    /// freed space (or shrink to make room for the bar).
    fn relayout_active(&mut self) {
        self.refresh_visibility();
    }

    /// Top/bottom y of the FOCUSED pane (the whole content band when not split) —
    /// drives the active tab's scroll clamp, hint placement, and terminal rows.
    fn content_y_bounds(&self) -> (i32, i32) {
        let r = self.focused_pane_rect();
        (r.y, r.y + r.h)
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
        let px = self.painter.px();
        // Lay out every visible read pane to ITS pane width (so a split read tab
        // wraps to its column); with no split this is just the active read tab.
        let (panes, _) = self.pane_layout();
        for (tab, rect) in panes {
            let is_read = matches!(self.tabs.get(tab), Some(t) if t.native.is_some());
            if !is_read {
                continue;
            }
            let cw = rect.w;
            let view = rect.h;
            // Split borrow: `painter` and this tab's `native` are disjoint fields.
            let painter = &self.painter;
            let nr = self.tabs[tab].native.as_mut().unwrap();
            if !nr.dirty && nr.layout_w == cw && (nr.layout_px - px).abs() < f32::EPSILON {
                continue;
            }
            // Leave an 8px margin on each side (matches the draw offset).
            nr.layout = read_view::layout(&nr.doc, cw - 16, painter);
            nr.layout_w = cw;
            nr.layout_px = px;
            nr.dirty = false;
            // Re-wrapping changed the visual lines: refresh the caret's grid in place,
            // keeping its cursor/selection (clamped) so caret mode survives resize/zoom.
            if let Some(caret) = nr.caret.as_mut() {
                caret.set_lines(nr.layout.text_lines().to_vec());
            }
            let max = (nr.layout.height - view).max(0);
            nr.scroll = nr.scroll.clamp(0, max);
        }
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

    /// The OS cursor position in window client (physical) pixels — needed for
    /// pane-click routing, since CursorMoved isn't delivered while the pointer is
    /// over a child webview, leaving `cursor_pos` stale there.
    #[cfg(windows)]
    fn cursor_client_pos(&self) -> Option<(i32, i32)> {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut pt = POINT::default();
        unsafe { GetCursorPos(&mut pt).ok()? };
        // `inner_position` is the client area's top-left in screen (physical) pixels,
        // so subtracting it turns the screen cursor into client coordinates — matching
        // the pane rects — without needing the Win32 GDI feature for ScreenToClient.
        let origin = self.window.inner_position().ok()?;
        Some((pt.x - origin.x, pt.y - origin.y))
    }

    #[cfg(not(windows))]
    fn cursor_client_pos(&self) -> Option<(i32, i32)> {
        Some((self.cursor_pos.0 as i32, self.cursor_pos.1 as i32))
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
            // tmux-style panes: split the focused pane (side-by-side / stacked) into a
            // new blank pane. Navigate between them with Ctrl+W h/j/k/l.
            "vsplit" | "vs" | "vsp" => self.split_pane(SplitDir::Row),
            "split" | "sp" | "hsplit" => self.split_pane(SplitDir::Col),
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
    ///
    /// Under a split, new content ALWAYS lands in the focused pane (in place), since
    /// a background tab would have no pane to show it — `new_tab` is ignored there.
    fn place_tab(&mut self, tab: Tab, new_tab: bool) {
        let replace = (!new_tab || self.split.is_some()) && self.active.is_some();
        match self.active {
            Some(i) if replace && i < self.tabs.len() => {
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
                "pane-click" => {
                    let _ = ipc_proxy.send_event(UserEvent::PaneClick);
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
        // place_tab is split-aware: a new tab normally, or the focused pane in place.
        self.place_tab(native_read_tab(doc, url, read), true);
        self.window.set_focus();
        self.clear_status();
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
        // Drop the tab and fix the pane tree (prune its leaf, collapse to single-pane
        // when one remains); focus the surviving pane.
        self.active = self.drop_tab(i);
        self.find_reset();
        self.mode = ModeKind::Normal;
        self.refresh_visibility();
        self.window.set_focus();
    }

    /// Remove tab `i` from `tabs` and repair the pane tree: prune its leaf if it was
    /// in a pane, shift higher leaf indices down, and collapse to a single pane (no
    /// split) when only one leaf remains. Returns the tab index that should take
    /// focus if the *focused* pane was the one closed (else the caller keeps its own
    /// active tab, adjusted for the shift). `None` when no tabs remain.
    fn drop_tab(&mut self, i: usize) -> Option<usize> {
        self.tabs.remove(i);
        let collapsed_focus = if let Some(tree) = self.split.take() {
            match tree.prune(i) {
                Some(mut t) => {
                    t.shift_after_remove(i);
                    let mut leaves = Vec::new();
                    t.leaves(&mut leaves);
                    let focus = t.first_leaf();
                    self.split = (leaves.len() > 1).then_some(t);
                    Some(focus)
                }
                None => None, // the pruned leaf was the whole tree
            }
        } else {
            None
        };
        if self.tabs.is_empty() {
            return None;
        }
        Some(collapsed_focus.unwrap_or_else(|| i.min(self.tabs.len() - 1)))
    }

    fn switch_tab(&mut self, delta: i32) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as i32;
        let cur = self.active.unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(n) as usize;
        self.show_tab(next);
    }

    /// Jump directly to a zero-based tab index (bound to keys 1..9).
    fn jump_to(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.show_tab(index);
        }
    }

    /// Show tab `target` as the active one. With no split it simply becomes active;
    /// while split it's loaded into the FOCUSED pane (swapping panes if it's already
    /// shown in another), so the layout never points at a tab outside the tiling.
    fn show_tab(&mut self, target: usize) {
        if self.split.is_some() {
            let Some(cur) = self.active else { return };
            if cur != target {
                if let Some(tree) = self.split.as_mut() {
                    let mut leaves = Vec::new();
                    tree.leaves(&mut leaves);
                    if leaves.contains(&target) {
                        tree.swap_leaves(cur, target);
                    } else {
                        tree.retarget(cur, target);
                    }
                }
            }
        }
        self.active = Some(target);
        self.find_reset();
        self.refresh_visibility();
        self.window.set_focus();
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
        // Scroll the pane UNDER the cursor (web panes get the wheel via their own
        // HWND, so they never reach here — only native panes and the bars do).
        let Some((tab, rect)) = self.pane_at_pixel(self.cursor_pos.0, self.cursor_pos.1) else {
            return;
        };
        if self.tabs.get(tab).is_some_and(|t| t.term.is_some()) {
            // If the program enabled mouse reporting (vim `mouse=a`, less, tmux…),
            // hand it the wheel so IT scrolls; otherwise page our own scrollback.
            if self.term_mouse_wheel(tab, rect, dy_lines) {
                return;
            }
            let lines = (dy_lines * 3.0).round() as i32;
            if lines != 0 {
                if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term.as_mut()) {
                    s.pty.scroll_display(lines);
                    self.window.request_redraw();
                }
            }
            return;
        }
        // Native read pane: scroll its own offset, clamped to its pane height.
        let dy = (-dy_lines * 80.0).round() as i32;
        if dy != 0 {
            if let Some(nr) = self.tabs.get_mut(tab).and_then(|t| t.native.as_mut()) {
                let max = (nr.layout.height - rect.h).max(0);
                nr.scroll = (nr.scroll + dy).clamp(0, max);
                self.window.request_redraw();
            }
        }
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
        // Keep each pane showing the same content after the index swap.
        if let Some(tree) = self.split.as_mut() {
            tree.swap_leaves(i, j);
        }
        self.active = Some(j);
        self.refresh_visibility();
    }

    fn refresh_visibility(&mut self) {
        // A web tab is visible iff it occupies a pane; position it at that pane's
        // rect. With no split this is just the active tab filling the band.
        let (panes, _) = self.pane_layout();
        let active = self.active;
        let split = self.split.is_some();
        for (i, tab) in self.tabs.iter().enumerate() {
            let Some(wv) = &tab.webview else { continue };
            match panes.iter().find(|(t, _)| *t == i) {
                Some((_, r)) => {
                    let mut rect = *r;
                    // Inset the FOCUSED web pane by the border width so the accent
                    // border we paint on our surface shows around it (the webview HWND
                    // otherwise covers it). Native panes draw their border on top.
                    if split && Some(i) == active {
                        let b = FOCUS_BORDER;
                        rect = PaneRect {
                            x: rect.x + b,
                            y: rect.y + b,
                            w: (rect.w - 2 * b).max(1),
                            h: (rect.h - 2 * b).max(1),
                        };
                    }
                    let _ = wv.set_visible(true);
                    let _ = wv.set_bounds(wry_rect(rect));
                }
                None => {
                    let _ = wv.set_visible(false);
                }
            }
        }
        // Cache the focus-backstop facts for the event-loop timer tail. Every layout
        // mutation (open/close/switch/split/move/resize/restore) funnels through here,
        // so the idle path can read two bools instead of walking the pane tree.
        self.active_pane_is_webview = active
            .and_then(|i| self.tabs.get(i))
            .is_some_and(|t| t.webview.is_some());
        self.background_webview_visible = panes.iter().any(|(t, _)| {
            Some(*t) != active && self.tabs.get(*t).is_some_and(|tab| tab.webview.is_some())
        });
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

/// Convert an internal [`PaneRect`] to the wry `Rect` used for webview bounds.
fn wry_rect(r: PaneRect) -> Rect {
    Rect {
        position: PhysicalPosition::new(r.x, r.y).into(),
        size: PhysicalSize::new(r.w.max(1) as u32, r.h.max(1) as u32).into(),
    }
}

#[cfg(test)]
mod tests {
    use super::vim::{Key, TextBuffer};
    use super::{deproxy_translate, is_translate_proxy, parse_tab_flag};

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
