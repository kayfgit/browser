//! browser-desktop — a lightweight, keyboard-driven shell that boots a WebView2
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
mod chrome;
mod commands;
mod config;
mod data;
mod draw;
mod extensions;
mod find;
mod freeze;
mod hints;
mod keys;
mod khook;
mod navguard;
mod netblock;
mod pages;
mod panes;
mod proc_cwd;
mod procmon;
mod pty_term;
mod read_view;
mod schemes;
mod session;
mod tabs;
mod term;
mod vim;
use draw::Painter;
use app::{clipboard_get, clipboard_set, AdblockMode, App, ExtInfo, ModeKind, UserEvent};
use commands::COMMANDS;
use find::FindState;
use tabs::{js_string, parse_open_flags, parse_tab_flag, Source, Tab};
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
/// rest type; in `passthrough` it takes only the leave chord (Ctrl+S) and lets every
/// other key — including Esc — reach the page. In insert it also reports when focus leaves the editable element,
/// so the shell can drop back to normal when you click away.
const BRIDGE_JS: &str = r#"
(function () {
  // Re-run per DOCUMENT, not once per window. wry injects init scripts on "document
  // created", which can fire first against the INITIAL EMPTY (about:blank) document —
  // and Chromium then REUSES that window for the first real page. A window-keyed
  // once-guard left every `document.addEventListener` below attached to that dead
  // initial document (and the old history wrapper bound to its stale History, whose
  // pushState then throws SecurityError — the "YouTube SPA navigation dies at the
  // progress bar" bug). Keying the guard on the document re-wires the listeners for
  // the real page; window-level work (window listeners, the History.prototype patch)
  // still runs once per window via `firstRun`.
  if (window.__shellBridge === document) return;
  var firstRun = !window.__shellBridge;
  window.__shellBridge = document;
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
      // Light field typing: Esc leaves. Ctrl+V is left alone so it pastes into the field
      // (to enter passthrough, leave Insert first, then Ctrl+V).
      if (e.key === 'Escape' && !e.shiftKey) { e.preventDefault(); e.stopPropagation(); post('insert-escape'); }
    } else if (m === 'passthrough') {
      // Sticky: only Ctrl+S or Shift+Esc leaves. Plain Esc is left for the page (a web
      // SSH/vim needs it), so passthrough survives clicks, focus changes and bare Esc.
      if ((e.ctrlKey && (e.key === 's' || e.key === 'S')) || (e.key === 'Escape' && e.shiftKey)) {
        e.preventDefault(); e.stopPropagation(); post('leave-passthrough');
      }
    }
  }, true);
  // Insert auto-leaves when focus leaves the field (clicking / tabbing away). A
  // fullscreen transition also shuffles focus (e.g. onto a video element), so suppress
  // the auto-leave while an element is fullscreen and for a short beat around every
  // fullscreen change — otherwise a page that fullscreens right after you focus a field
  // would drop Insert spuriously. `__fsChangeAt` is stamped by `fsPost` below. (Sticky
  // passthrough never auto-leaves — only Ctrl+S / Shift+Esc — so it's unaffected here.)
  var __fsChangeAt = 0;
  document.addEventListener('focusout', function () {
    if (window.__mode !== 'insert') return;
    setTimeout(function () {
      if (document.fullscreenElement || Date.now() - __fsChangeAt < 700) return;
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
  // Decide what a Normal-mode click should do with keyboard focus, on `click` (the
  // END of the gesture, so the page's own handlers run first):
  //   * a text field  → 'page-edit', the shell enters Insert so you can type;
  //   * a control/menu/link → 'page-hold', let the PAGE keep focus so its popover
  //     stays open (the old blanket grab-back blurred YouTube menus shut on click);
  //   * empty page area → grabBack(), reclaim the shell keyboard as before.
  // Esc (caught by the keyboard hook) snaps the shell back from a hold/edit. A script
  // `.focus()` with no click is still caught by the shell's periodic reclaim tick.
  function onClick(e) {
    if (window.__mode && window.__mode !== 'normal') return;
    var t = e.target;
    var field = (t && t.closest) ? t.closest('input,textarea,[contenteditable]') : null;
    if (field && editable(field)) { post('page-edit'); return; }
    var ctrl = (t && t.closest) ? t.closest(
      "a[href],button,select,summary,label,[role='button'],[role='link']," +
      "[role='menuitem'],[role='tab'],[role='checkbox'],[role='radio']," +
      "[role='option'],[role='switch'],[role='combobox']," +
      "[onclick],[tabindex]:not([tabindex='-1'])") : null;
    // Component frameworks (YouTube, Gmail, …) wire clicks onto custom elements
    // with addEventListener and none of the above attributes, so the selector
    // misses them and the old fall-through to grabBack() blurred the webview the
    // instant you clicked — snapping the control's just-opened menu/popover shut
    // (the "can't press YouTube buttons; a double-click opens then closes" bug).
    // Such controls almost always style themselves `cursor: pointer`, so treat
    // that as the catch-all clickability signal and let the PAGE keep focus.
    if (!ctrl && t && t.nodeType === 1) {
      try { if (getComputedStyle(t).cursor === 'pointer') ctrl = t; } catch (_) {}
    }
    if (ctrl) { post('page-hold'); return; }
    grabBack();
  }
  document.addEventListener('click', onClick, true);
  // A real pointer press anywhere in this page tells the shell to focus THIS pane
  // (when split). Fired on pointerdown — before any link navigation — so clicking a
  // non-focused web pane switches focus to it instead of stranding the keyboard.
  document.addEventListener('pointerdown', function () { post('pane-click'); }, true);
  // Link-hover readout: report the href under the pointer so the shell can show it
  // on the right of the command bar (like a browser status bar). Posted only when
  // the target link CHANGES (mouseover bubbles, so this is event-delegated and
  // cheap); cleared when the pointer leaves a link or the document entirely.
  var __hoverHref = '';
  function reportHover(href) {
    if (href === __hoverHref) return;
    __hoverHref = href;
    post('link-hover:' + href);
  }
  document.addEventListener('mouseover', function (e) {
    var a = (e.target && e.target.closest) ? e.target.closest('a[href]') : null;
    var href = (a && a.href && !/^javascript:/i.test(a.href)) ? a.href : '';
    reportHover(href);
  }, true);
  document.addEventListener('mouseout', function (e) {
    // Leaving an element with no related link target underneath clears the readout.
    var to = e.relatedTarget;
    var a = (to && to.closest) ? to.closest('a[href]') : null;
    if (!a) reportHover('');
  }, true);
  if (firstRun) window.addEventListener('blur', function () { reportHover(''); });
  // Tell the shell once the page is up so it can reclaim keyboard focus — works
  // for both URL and with_html content, independent of native load events.
  if (firstRun) window.addEventListener('load', function () { post('page-ready'); });
  // Mirror HTML fullscreen (e.g. clicking YouTube's fullscreen button) to the
  // shell: it fullscreens the window so the page fills the screen and the bars
  // hide. wry exposes no native fullscreen-element event on Windows, so we detect
  // it here. (`webkit`-prefixed for older players that fire only that.)
  function fsPost() { __fsChangeAt = Date.now(); post(document.fullscreenElement ? 'fs-enter' : 'fs-exit'); }
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
  // SPA navigations (history.pushState / back-forward / hash jumps) never fire a
  // page-load event, so without this the shell never sees the URL change and H/L
  // has no back-stack to walk (YouTube video → video, for one). Report them over
  // IPC: 'url-changed' records a back/forward step, 'url-replaced' only syncs the
  // shown URL (replaceState adds no history entry — recording it would spam the
  // stack with scroll/query rewrites and drift from the engine's own history).
  //
  // Patch History.PROTOTYPE with a dynamic `this` — NEVER `history.pushState
  // .bind(history)`. An instance-bound wrapper captured on the initial empty
  // document keeps validating URLs against that stale `about:blank` document after
  // Chromium reuses the window for the real page, so the page's own pushState
  // throws SecurityError and its SPA router dies mid-navigation (YouTube: red bar
  // stuck at ~75%, URL never updates, page half-hydrated). The prototype method
  // with the caller's own `this` always resolves against the live document.
  if (firstRun) {
    var __pushState = History.prototype.pushState;
    var __replaceState = History.prototype.replaceState;
    History.prototype.pushState = function () {
      var r = __pushState.apply(this, arguments); post('url-changed'); return r;
    };
    History.prototype.replaceState = function () {
      var r = __replaceState.apply(this, arguments); post('url-replaced'); return r;
    };
    window.addEventListener('popstate', function () { post('url-changed'); });
    window.addEventListener('hashchange', function () { post('url-changed'); });
  }
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

/// Well-known ad-exchange / analytics / tracker hostnames, lower-cased. Consulted by the
/// native navigation guard ([`url_is_ad_host`]) as the tiny always-on fallback that stops
/// a forced top-level redirect to one of these hosts during the brief window before the
/// full EasyList [`Engine`](crate::blocklist) finishes compiling off-thread at startup.
/// Sub-resource blocking ([`netblock`](crate::netblock)) and the page-side cosmetic layer
/// no longer read this list — the engine is the source of truth there. Matched as a host
/// substring, so `adservice.google.` catches `adservice.google.com`.
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
    "permutive.com", "cxense.com", "nitropay.com", "nitrocnct.com", "analytics.tiktok",
    "ads.linkedin.com", "ads.pinterest.com", "ads.yahoo.com",
    // NOTE: YouTube's own first-party ad telemetry (`/api/stats/ads`, `/ptracking`,
    // `/get_midroll_`) is DELIBERATELY absent — blocking it trips YouTube's anti-adblock
    // detector, which then serves the "Ad blockers violate ToS" enforcement wall. The ads
    // are killed by pruning the player-response JSON instead (see ADBLOCK_JS). `netblock`
    // enforces the same carve-out for sub-resource requests to YouTube's own hosts.
];

/// uBlock-style content blocker, injected at document-start into every web tab while
/// adblock is on. NETWORK-level blocking — stopping the ad/scam scripts, iframes and
/// XHRs from ever loading — is done natively against the full EasyList engine by the
/// WebView2 sub-resource blocker ([`netblock`](crate::netblock)); this page-side script
/// handles only what must run inside the page:
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
  // first place) is done natively against the full EasyList engine — see `netblock.rs`.
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

// Page-side caret/visual browsing (vim selection on live web pages). The shell
// forwards motions here; we drive a real DOM Selection via `Selection.modify`
// (Chromium supports character/word/line/lineboundary/documentboundary), draw a
// vim-style block cursor over the character at the focus end, and post the yanked
// text back over IPC. `__caretEnter` places the caret at the viewport center
// WITHOUT selecting — press `v`/`V` again for charwise/linewise visual; `__caretKey`
// moves or extends and scrolls the window when the cursor nears a viewport edge
// (vim scrolloff); `__caretYank` copies; `__caretEsc` collapses then exits (posting
// `caret-exit` so the shell leaves caret mode).
const CARET_JS: &str = r#"
(function () {
  if (window.__caretInit) return;
  window.__caretInit = true;
  var on = false, visual = false, pend = '', bar = null, lineBar = null;
  // Affinity of the LAST motion. At a soft-wrap boundary the same (node, offset)
  // renders on two lines; without this a `j` that logically moved down can paint
  // the block at the end of the line ABOVE (the "j went up" visual glitch).
  var dirBack = false;
  var BLINK = '__caretBlink 1.06s step-end infinite';
  function sel() { return window.getSelection(); }
  function ensureBar() {
    if (bar && bar.isConnected && lineBar && lineBar.isConnected) return bar;
    var host = document.body || document.documentElement;
    if (!document.getElementById('__caret-css')) {
      var st = document.createElement('style');
      st.id = '__caret-css';
      st.textContent = '@keyframes __caretBlink{0%,52%{opacity:1}53%,100%{opacity:.08}}';
      (document.head || host).appendChild(st);
    }
    if (lineBar) lineBar.remove();
    if (bar) bar.remove();
    // Vim 'cursorline': a faint full-width tint on the cursor's line (translucent
    // gray so it reads on both light and dark pages). Steady — only the block blinks.
    lineBar = document.createElement('div');
    lineBar.style.cssText = 'position:fixed;left:0;width:100vw;z-index:2147483646;' +
      'background:rgba(128,128,128,.16);pointer-events:none;display:none';
    host.appendChild(lineBar);
    bar = document.createElement('div');
    bar.style.cssText = 'position:fixed;z-index:2147483647;background:rgba(108,182,255,.45);' +
      'outline:1px solid #6cb6ff;pointer-events:none;display:none;animation:' + BLINK;
    host.appendChild(bar);
    return bar;
  }
  // Rect of the single character [i, i+1) of text node n. A range that spans a
  // soft wrap yields one fragment per line — `last` picks the lower one (forward
  // affinity), else the upper. Returns null for characters that render nowhere
  // (collapsed whitespace), so callers skip them instead of drawing a block at
  // some stale end-of-line position.
  function charRect(n, i, last) {
    var r = document.createRange();
    r.setStart(n, i); r.setEnd(n, i + 1);
    var cs = r.getClientRects();
    var rc = cs.length ? cs[last ? cs.length - 1 : 0] : r.getBoundingClientRect();
    return rc && rc.height && rc.width ? rc : null;
  }
  // Resolve an element-boundary focus (nodeType 1 + child offset) down to a real
  // text position. Without this the fallback drew the block at the parent
  // element's bounding-box TOP — a paragraph-sized upward jump mid-motion.
  function textPosIn(el, off) {
    function edgeText(node, first) {
      var tw = document.createTreeWalker(node, NodeFilter.SHOW_TEXT, null), t, hit = null;
      while ((t = tw.nextNode())) {
        if (!t.nodeValue.length) continue;
        hit = t;
        if (first) break;
      }
      return hit;
    }
    var kids = el.childNodes, i, t;
    for (i = off; i < kids.length; i++) {
      t = kids[i].nodeType === 3 ? (kids[i].nodeValue.length ? kids[i] : null) : edgeText(kids[i], true);
      if (t) return { n: t, o: 0 };
    }
    for (i = Math.min(off, kids.length) - 1; i >= 0; i--) {
      t = kids[i].nodeType === 3 ? (kids[i].nodeValue.length ? kids[i] : null) : edgeText(kids[i], false);
      if (t) return { n: t, o: t.nodeValue.length };
    }
    return null;
  }
  // The rect of the character AT the focus end — what the block cursor sits on.
  // Probing a single character keeps the cursor one line tall no matter how big
  // the selection is. width 0 means the caret sits past the last character of a
  // line (block gets a default width). Wrap-ambiguous positions resolve along
  // `dirBack` so the block always lands on the line the motion moved to.
  function focusRect() {
    var s = sel();
    if (s.rangeCount === 0 || !s.focusNode) return null;
    var n = s.focusNode, o = s.focusOffset, rc;
    try {
      if (n.nodeType === 1) {
        var p = textPosIn(n, o);
        if (p) { n = p.n; o = p.o; }
      }
      if (n.nodeType === 3) {
        var len = n.nodeValue.length;
        var after = o < len ? charRect(n, o, true) : null;
        var before = o > 0 ? charRect(n, o - 1, false) : null;
        // The neighbours render nowhere (caret inside a run of collapsed
        // whitespace): walk toward the nearest visible character in the motion's
        // direction and sit the block there.
        if (!after && !before) {
          var i;
          if (dirBack) { for (i = o - 1; i > 0 && !before; i--) before = charRect(n, i - 1, false); }
          else { for (i = o + 1; i < len && !after; i++) after = charRect(n, i, true); }
        }
        // Both neighbours visible but on different lines — the caret sits exactly
        // on a wrap. Forward motions show it at the start of the lower line,
        // backward ones at the end of the upper.
        if (after && before && after.top - before.top > 1) {
          if (dirBack) after = null; else before = null;
        }
        if (after) return after;
        if (before) return { left: before.right, top: before.top, width: 0, height: before.height };
      }
      // Focus on an element boundary with no text anywhere near: use the selection
      // edge's own line box, not the element's whole rect.
      var rr = s.getRangeAt(0), rects = rr.getClientRects();
      if (rects.length) {
        var atEnd = n === rr.endContainer && o === rr.endOffset;
        rc = rects[atEnd ? rects.length - 1 : 0];
        return { left: atEnd ? rc.right : rc.left, top: rc.top, width: 0, height: rc.height };
      }
      var el = n.nodeType === 1 ? n : n.parentElement;
      if (!el) return null;
      rc = el.getBoundingClientRect();
      var lh = parseFloat(getComputedStyle(el).lineHeight) || 16;
      return { left: rc.left, top: rc.top, width: 0, height: Math.min(rc.height || lh, lh) };
    } catch (e) { return null; }
  }
  function updateBar() {
    var b = ensureBar();
    if (!on) { b.style.display = 'none'; lineBar.style.display = 'none'; return; }
    var rc = focusRect();
    if (!rc) { b.style.display = 'none'; lineBar.style.display = 'none'; return; }
    var h = rc.height || 16;
    b.style.display = 'block';
    b.style.left = rc.left + 'px';
    b.style.top = rc.top + 'px';
    // Exactly the glyph's width (the old 0.55·line-height floor overhung narrow
    // glyphs to the right); width 0 = the empty end-of-line slot, half a cell.
    b.style.width = (rc.width > 0 ? rc.width : h * 0.45) + 'px';
    b.style.height = h + 'px';
    // Restart the blink so the block is solid right after a motion (vim-style).
    b.style.animation = 'none';
    void b.offsetWidth;
    b.style.animation = BLINK;
    lineBar.style.display = 'block';
    lineBar.style.top = rc.top + 'px';
    lineBar.style.height = h + 'px';
  }
  // Vim scrolloff: when a motion pushes the cursor within ~2 lines of the top or
  // bottom edge, scroll the window so the text around it stays visible (h/l past
  // the sides likewise). The scroll listener repaints the block afterwards.
  function keepInView() {
    var rc = focusRect();
    if (!rc) return;
    var h = rc.height || 16, pad = h * 2, bot = rc.top + h;
    if (bot > innerHeight - pad) scrollBy(0, Math.ceil(bot - (innerHeight - pad)));
    else if (rc.top < pad) scrollBy(0, Math.floor(rc.top - pad));
    if (rc.left > innerWidth) scrollBy(Math.ceil(rc.left - innerWidth + 40), 0);
    else if (rc.left < 0) scrollBy(Math.floor(rc.left - 40), 0);
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
  window.__caretEnter = function () {
    on = true; visual = false; pend = ''; dirBack = false;
    caretAtCenter();
    updateBar();
  };
  window.__caretExit = function () {
    on = false; visual = false; pend = '';
    try { sel().removeAllRanges(); } catch (e) {}
    if (bar) bar.style.display = 'none';
    if (lineBar) lineBar.style.display = 'none';
  };
  window.__caretEsc = function () {
    if (!on) return;
    if (visual && !sel().isCollapsed) { sel().collapseToEnd(); visual = false; updateBar(); }
    else { window.__caretExit(); window.__post('caret-exit'); }
  };
  window.__caretYank = function () {
    var s = sel(), t = '';
    try {
      if (visual && s.rangeCount) {
        // Vim-inclusive selection: the char under the block cursor belongs to the
        // yank, but DOM selections end BEFORE their focus/anchor boundary (l→t on
        // "localhost" yanked "localhos"). Normalize the range forward (addRange
        // makes anchor=start) and extend its end one character; this also makes a
        // bare v+y yank the single char under the block, like vim.
        var r = s.getRangeAt(0).cloneRange();
        s.removeAllRanges();
        s.addRange(r);
        s.modify('extend', 'right', 'character');
        t = s.toString();
        // Vim leaves the cursor at the selection's start after a yank.
        s.collapseToStart();
        dirBack = false;
      } else {
        t = s.toString();
        s.collapseToEnd();
      }
    } catch (e) { t = s.toString(); }
    window.__post('caret-yank:' + t);
    visual = false; updateBar();
  };
  window.__caretKey = function (k) {
    if (!on) return;
    var s = sel(), alter = visual ? 'extend' : 'move';
    // Remember the motion's direction — focusRect uses it to resolve positions
    // that render on two lines (soft wraps).
    if ('hkb0'.indexOf(k) >= 0) dirBack = true;
    else if ('ljweG$'.indexOf(k) >= 0) dirBack = false;
    if (pend === 'g') { pend = ''; if (k === 'g') { dirBack = true; s.modify(alter,'backward','documentboundary'); keepInView(); updateBar(); return; } }
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
      case 'v':
        visual = !visual; if (!visual) s.collapseToEnd();
        break;
      case 'V':
        // Linewise visual: anchor at the line start, focus at its end; j/k then
        // extend by whole lines. Pressed while already selecting, it collapses.
        visual = !visual;
        if (visual) { s.modify('move','left','lineboundary'); s.modify('extend','right','lineboundary'); }
        else s.collapseToEnd();
        break;
      default: break;
    }
    keepInView();
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
        // Default to the uBlock Origin extension (native blocker off); session restore may
        // override the mode below. `adblock`/`adblock_on` mirror "native on" = false here.
        adblock_mode: AdblockMode::Ubo,
        extensions: Vec::new(),
        adblock: false,
        adblock_on: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        allow_risky_downloads: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        nav_intent: std::sync::Arc::new(std::sync::Mutex::new(None)),
        blocker: blocklist::new_shared(),
        mute: false,
        no_css: false,
        no_video: false,
        no_scrollbar: false,
        term_command: vec!["nu".to_string()],
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
        acting_ai: None,
        config: config::load(),
        theme: draw::Theme::default(),
        last_focus_gain: Instant::now(),
        history: Vec::new(),
        history_at: Vec::new(),
        closed_tabs: Vec::new(),
        fs_from_page: false,
        windows: Vec::new(),
        pending_window_key: false,
        pending_window_at: Instant::now(),
        pane_resize_at: Instant::now(),
        pane_move_orig: None,
        active_pane_is_webview: false,
        background_webview_visible: false,
        term_scrollback: pty_term::DEFAULT_SCROLLBACK,
        term_style: pty_term::TermStyle::default(),
        term_painter: None,
        nav_replaying: false,
        term_find_pending: None,
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

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // Command-bar cursor blink: the WaitUntil deadline (set below while in
            // Command mode) wakes us here to flip the cursor and repaint.
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                // A transient status flash (e.g. a finished background `:ai`) times out.
                app.expire_status_flash();
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
                } else if app.mode == ModeKind::Normal {
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
                }
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
                    } else if app.is_split() {
                        // Click a (native) pane below the tab bar to focus it. Web
                        // panes consume the click in their own HWND, so this only
                        // fires for terminal/read/vim/blank panes — Ctrl+W covers the
                        // rest. These have no child HWND (keys already route through the
                        // shell), so focusing to Normal here is the expected behaviour;
                        // the web two-click problem is handled on the PaneClick path.
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
                    // Insert and Caret are tied to the old page's DOM; a navigation ends
                    // them (the field/caret is gone on the new document).
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
            Event::UserEvent(UserEvent::PaneClick) => {
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
                app.hint_new_tab = false;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.window.request_redraw();
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
                app.hint_new_tab = false;
                app.mode = ModeKind::Normal;
                app.window.set_focus();
                app.open_tab(&url, app.nojs, true);
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
            Event::UserEvent(UserEvent::OpenPopupTab(url)) => {
                // A real new-tab click the popup guard cleared: re-open it as a managed
                // tab (new tabs here live in our tab strip, not as OS popups).
                app.open_tab(&url, app.nojs, true);
            }
            Event::UserEvent(UserEvent::BlocklistReady) => {
                // Quiet by default (don't clobber a useful status); the engine simply
                // starts catching navigations from here on.
            }
            Event::UserEvent(UserEvent::ExtensionsListed(list)) => {
                // The async extension query finished: cache it and (re)render `:extensions`.
                app.extensions = list;
                app.show_extensions_page();
            }
            Event::UserEvent(UserEvent::DownloadBlocked(name)) => {
                let short: String = name.chars().take(60).collect();
                app.set_error(format!(
                    "blocked download of {short} — executable/installer. :downloads to allow"
                ));
                app.window.request_redraw();
            }
            Event::UserEvent(UserEvent::DataCleared { label, ai_id }) => {
                // The async erase finished. An empty label is a silent bonus clear
                // (engine history alongside the shell's own list) — don't announce it.
                if !label.is_empty() {
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
        khook::set_mode(app.hook_mode_code());
        // While typing a command, keep waking to blink the cursor (unless we're
        // already exiting). Outside Command mode we stay on plain Wait.
        if !matches!(*control_flow, ControlFlow::Exit | ControlFlow::ExitWithCode(_)) {
            if matches!(app.mode, ModeKind::Command | ModeKind::Find) {
                // Blink the command-bar cursor.
                *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(530));
            } else if app.mode == ModeKind::Normal && app.read_caret_active() {
                // Blink the read-mode caret's block cursor.
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
            } else if app.mode == ModeKind::PaneResize {
                // Wake at the resize-mode idle deadline so it can auto-exit.
                *control_flow =
                    ControlFlow::WaitUntil(app.pane_resize_at + crate::app::PANE_RESIZE_TIMEOUT);
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
