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
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::keyboard::ModifiersState;
use tao::window::WindowBuilder;

mod app;
mod blocklist;
mod chrome;
mod commands;
mod draw;
mod find;
mod hints;
mod keys;
mod navguard;
mod pages;
mod panes;
mod procmon;
mod pty_term;
mod read_view;
mod session;
mod tabs;
mod term;
mod vim;
use draw::Painter;
use app::{clipboard_get, clipboard_set, App, ModeKind, UserEvent};
use commands::COMMANDS;
use find::FindState;
use tabs::{js_string, parse_tab_flag, Source, Tab};
use pages::commands_document;
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
/// rest type; in `passthrough` it takes only the leave chord (Ctrl+S or Shift+Esc)
/// and lets every other key reach the page. In insert it also reports when focus leaves the editable element,
/// so the shell can drop back to normal when you click away.
const BRIDGE_JS: &str = r#"
(function () {
  if (window.__shellBridge) return;
  window.__shellBridge = true;
  if (typeof window.__mode === 'undefined') window.__mode = 'normal';
  function post(m) { window.__post(m); }
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
        window.__post('hint-edit');
      } else if (nt && href) {
        // New-tab mode on a real link: let the shell open it as a new tab.
        window.__post('hint-open:' + href);
      } else {
        activate(el);
        window.__post('hint-exit');
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

/// Hostnames / URL fragments the blocker drops (substring match, lower-cased).
/// Kept to well-known ad-exchange, analytics and tracker endpoints to avoid false
/// hits. This is the single source of truth: it's baked into the page blocker as
/// `window.__adHosts` (see [`ad_hosts_js`]) AND consulted by the native navigation
/// guard ([`url_is_ad_host`]) so a forced redirect to any of these hosts is stopped
/// regardless of how it was triggered (script, `<meta refresh>`, or a server 3xx).
/// Entries containing `/` are path fragments (sub-resource paths) and are ignored
/// by the host-only native guard.
pub(crate) const AD_HOSTS: &[&str] = &[
    "doubleclick.net", "googlesyndication.com", "googleadservices.com",
    "google-analytics.com", "googletagmanager.com", "googletagservices.com",
    "adservice.google.", "pagead2.googlesyndication", "amazon-adsystem.com",
    "adnxs.com", "adsrvr.org", "rubiconproject.com", "pubmatic.com", "openx.net",
    "criteo.com", "criteo.net", "taboola.com", "outbrain.com", "scorecardresearch.com",
    "quantserve.com", "moatads.com", "adcolony.com", "applovin.com", "zedo.com",
    "bidswitch.net", "casalemedia.com", "sharethrough.com", "smartadserver.com",
    "teads.tv", "3lift.com", "yieldmo.com", "contextweb.com", "gumgum.com",
    "indexww.com", "media.net", "mgid.com", "revcontent.com", "adform.net",
    "adroll.com", "bluekai.com", "demdex.net", "everesttech.net", "rlcdn.com",
    "agkn.com", "crwdcntrl.net", "mathtag.com", "adsafeprotected.com",
    "serving-sys.com", "flashtalking.com", "servedbyadbutler.com",
    "hotjar.com", "mixpanel.com", "segment.io", "amplitude.com", "branch.io",
    "onesignal.com", "clarity.ms", "fullstory.com", "heap.io", "nr-data.net",
    "bugsnag.com", "optimizely.com", "chartbeat.com", "parsely.com",
    "permutive.com", "cxense.com", "facebook.net/en_us/fbevents", "analytics.tiktok",
    "ads.linkedin.com", "ads.pinterest.com", "ads.yahoo.com",
    "/pagead/", "/adsbygoogle", "/gampad/", "/securepubads",
    "youtube.com/api/stats/ads", "youtube.com/ptracking", "/get_midroll_",
];

/// Render [`AD_HOSTS`] as a JS array literal for `window.__adHosts`. The entries are
/// static and contain no `"`/`\`, so a plain quote-join is safe.
fn ad_hosts_js() -> String {
    let mut s = String::from("[");
    for (i, h) in AD_HOSTS.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        s.push_str(h);
        s.push('"');
    }
    s.push(']');
    s
}

/// uBlock-style content blocker, injected at document-start into every web tab
/// while adblock is on. wry exposes no sub-resource request blocker (see
/// [`RESEARCH_JS`]), so we do it page-side in four layers:
///   * network — wrap `fetch`/`XHR`/`sendBeacon` so requests to known ad/tracker
///     hosts resolve empty instead of hitting the network;
///   * DOM — a MutationObserver removes script/iframe/img/ins nodes that load from
///     those hosts (and `ins.adsbygoogle`) as the page builds itself;
///   * cosmetic — a `<style>` hides generic ad containers (EasyList-ish), plus
///     YouTube's ad slots, and a 400ms tick skips/seeks past YouTube video ads;
///   * redirects — trap scripted navigation (`location` href/assign/replace) and
///     `<meta refresh>` to stop forced/auto redirects (see the guard near the end).
///
/// All layers honour a live `on` flag: the shell flips it via `window.__setAdblock`
/// on `:ads` (no reload needed), and bakes the initial value as `__adblockDefault`
/// per tab so new tabs start in the current state. The host list arrives as
/// `window.__adHosts` (baked from Rust's [`AD_HOSTS`], the single source of truth).
const ADBLOCK_JS: &str = r#"
(function () {
  if (window.__adblockInit) return;
  window.__adblockInit = true;
  var on = (typeof window.__adblockDefault === 'undefined') ? true : !!window.__adblockDefault;

  // Hostnames / URL fragments to drop (substring match, lower-cased). Baked from
  // Rust's AD_HOSTS (single source of truth, also used by the native redirect guard).
  var HOSTS = (window.__adHosts && window.__adHosts.length) ? window.__adHosts : [];
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
  // scummy streaming sites. Neuter scripted opens to ad hosts / cross-origin while
  // blocking is on. The shell's native new-window handler is the primary guard (it
  // also catches frames and about:blank shells); this is the in-page backstop. Runs
  // in every frame, so the ad iframe's own window.open is stopped at the source.
  var _winOpen = window.open ? window.open.bind(window) : null;
  if (_winOpen) {
    window.open = function (u) {
      if (on && (!u || blocked(u) || crossOrigin(u))) { reportPopup(u || 'about:blank'); return null; }
      return _winOpen.apply(null, arguments);
    };
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
  if (window.__featuresInit) return;
  window.__featuresInit = true;
  var d = window.__featureDefaults || {};
  var muted = !!d.mute, noCss = !!d.css;

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
    if (name === 'mute') { muted = val; applyMute(document, val); }
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
    else { window.__caretExit(); window.__post('caret-exit'); }
  };
  window.__caretYank = function () {
    var t = sel().toString();
    window.__post('caret-yank:' + t);
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

/// pty-host trees under the shell. Best-effort: any failure is ignored.
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
        adblock_on: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        allow_risky_downloads: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        nav_intent: std::sync::Arc::new(std::sync::Mutex::new(None)),
        blocker: blocklist::new_shared(),
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

    // Compile the ad/redirect blocklist engine off-thread; it goes live a beat after
    // launch (BlocklistReady), and navigations use the timing heuristic until then.
    blocklist::spawn_build(app.blocker.clone(), app.proxy.clone());

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
                WindowEvent::Focused(focused) => {
                    if focused {
                        app.last_focus_gain = Instant::now();
                    }
                }
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
                    // Shell-driven `load_url` reads as not-user-initiated; stamp intent so
                    // the native guard lets this de-proxy redirect through.
                    navguard::mark(&app.nav_intent);
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
            Event::UserEvent(UserEvent::BlocklistReady) => {
                // Quiet by default (don't clobber a useful status); the engine simply
                // starts catching navigations from here on.
            }
            Event::UserEvent(UserEvent::DownloadBlocked(name)) => {
                let short: String = name.chars().take(60).collect();
                app.set_error(format!(
                    "blocked download of {short} — executable/installer. :downloads to allow"
                ));
                app.window.request_redraw();
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
