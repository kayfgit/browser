
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
  // What a Normal-mode click at `t` should do with keyboard focus:
  //   'field' → the shell enters Insert so you can type;
  //   'ctrl'  → a control/menu/link: let the PAGE keep focus so its popover stays
  //             open (a blanket grab-back blurs component menus shut);
  //   ''      → empty page area: the shell reclaims the keyboard.
  function classify(t) {
    if (!t || !t.closest) return '';
    var field = t.closest('input,textarea,[contenteditable]');
    if (field && editable(field)) return 'field';
    var ctrl = t.closest(
      "a[href],button,select,summary,label,[role='button'],[role='link']," +
      "[role='menuitem'],[role='tab'],[role='checkbox'],[role='radio']," +
      "[role='option'],[role='switch'],[role='combobox']," +
      "[onclick],[tabindex]:not([tabindex='-1'])");
    // Component frameworks (YouTube, Gmail, …) wire clicks onto custom elements
    // with addEventListener and none of the above attributes, so the selector
    // misses them and a fall-through to grabBack() blurred the webview the
    // instant you clicked — snapping the control's just-opened menu/popover shut
    // (the "can't press YouTube buttons; a double-click opens then closes" bug).
    // Such controls almost always style themselves `cursor: pointer`, so treat
    // that as the catch-all clickability signal and let the PAGE keep focus.
    if (!ctrl && t.nodeType === 1) {
      try { if (getComputedStyle(t).cursor === 'pointer') ctrl = t; } catch (_) {}
    }
    return ctrl ? 'ctrl' : '';
  }
  // Act on the click (the END of the gesture, so the page's own handlers run first).
  // The keyboard hook takes the keyboard back from a hold on Esc or on any shell key,
  // and from a script `.focus()` (no click) on the next key.
  function onClick(e) {
    if (window.__mode && window.__mode !== 'normal') return;
    var kind = classify(e.target);
    if (kind === 'field') { post('page-edit'); return; }
    if (kind === 'ctrl') { post('page-hold'); return; }
    grabBack();
  }
  document.addEventListener('click', onClick, true);
  // A real pointer press anywhere in this page tells the shell to focus THIS pane
  // (when split). Fired on pointerdown — before any link navigation — so clicking a
  // non-focused web pane switches focus to it instead of stranding the keyboard.
  // It ALSO opens the shell's mid-gesture focus grace (see `page_gesture_at`).
  document.addEventListener('pointerdown', function (e) {
    // Hint mode dispatches synthetic pointer events to activate page controls.
    // They must not select the pane under the unrelated physical mouse cursor.
    if (!e.isTrusted) return;
    post('pane-click');
    // Report a control press HERE, at the start of the gesture, not on the click that
    // ends it. The webview takes OS keyboard focus on mousedown, and the shell's
    // periodic reclaim poll pulls it straight back unless it has been told the page
    // should keep it — so reporting only at `click` left the entire press as a window
    // in which the page could be blurred mid-gesture. A press held even slightly (or
    // a control whose menu opens asynchronously, like YouTube's account avatar, which
    // fetches its menu before showing it) lost focus in that window and the popover
    // never appeared: the button looked dead. Posting at pointerdown closes it.
    if (window.__mode && window.__mode !== 'normal') return;
    if (classify(e.target) === 'ctrl') { __ctrlPressAt = Date.now(); post('page-hold'); }
  }, true);
  // A control that moves focus into a text field (a search icon that opens a search
  // box) means the user is about to type there: enter Insert, exactly as if the field
  // had been clicked. Otherwise the keyboard hook would treat those keystrokes as shell
  // commands. Limited to just after a real press on a control, so a page autofocusing
  // an input by itself never flips the mode.
  var __ctrlPressAt = 0;
  document.addEventListener('focusin', function (e) {
    if (window.__mode && window.__mode !== 'normal') return;
    if (Date.now() - __ctrlPressAt > 1500 || !editable(e.target)) return;
    __ctrlPressAt = 0;
    post('page-edit');
  }, true);
  // Normal mode means the shell owns the keyboard, but after a click on a page control
  // (or a script .focus(), or an SPA navigation) the PAGE holds it, and every key went
  // to the page: the "frozen command bar" bug. A key reaching the page in Normal mode
  // is therefore a shell key that took a wrong turn. Hand it to the shell, which takes
  // focus back and replays it (`shell-key:<keyCode>,<shift>,<ctrl>`), so `:` opens
  // the command bar on the first press. Keys that drive the page's own menus and
  // focus (arrows, Enter, Space, Tab, paging) and the usual editing chords stay with
  // the page; Esc just returns the keyboard; typing into a field enters Insert.
  // Window + capture: runs before any of the page's own key handlers.
  var PAGE_KEYS = {
    Tab: 1, Enter: 1, ' ': 1, PageUp: 1, PageDown: 1, Home: 1, End: 1,
    ArrowUp: 1, ArrowDown: 1, ArrowLeft: 1, ArrowRight: 1
  };
  var MODIFIERS = { Shift: 1, Control: 1, Alt: 1, Meta: 1, AltGraph: 1, CapsLock: 1, NumLock: 1, ScrollLock: 1 };
  var EDIT_CHORDS = { a: 1, c: 1, x: 1, z: 1, y: 1 };
  function shellKey(e) {
    if ((window.__mode || 'normal') !== 'normal' || !e.isTrusted) return;
    if (e.altKey || e.metaKey || MODIFIERS[e.key]) return;
    if (e.ctrlKey && EDIT_CHORDS[(e.key || '').toLowerCase()]) return;
    var t = e.composedPath ? e.composedPath()[0] : e.target;
    if (editable(t)) { post('page-edit'); return; }
    if (PAGE_KEYS[e.key] && !e.ctrlKey) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    if (e.key === 'Escape') { post('reclaim'); return; }
    post('shell-key:' + e.keyCode + ',' + (e.shiftKey ? 1 : 0) + ',' + (e.ctrlKey ? 1 : 0));
  }
  if (firstRun) window.addEventListener('keydown', shellKey, true);
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
  // here (and flickered shut). Replace it with our own, built from what is actually
  // under (or selected by) the pointer: Copy for a text selection, and for a link
  // "Open in new tab" (`hint-open`) plus "Copy link address". Copy items hand the
  // text to the SHELL over IPC (`clip:`) — the page can't reach the real clipboard
  // in Normal mode, and the shell owns it anyway (`y`, caret yank, terminal select).
  // With nothing actionable under the cursor we just eat the event: no empty menu.
  var __ctxMenu = null;
  function ctxClose() { if (__ctxMenu) { __ctxMenu.remove(); __ctxMenu = null; } }
  document.addEventListener('contextmenu', function (e) {
    if (window.__shellNativeContextMenu) return;
    e.preventDefault(); // always kill the broken native menu
    ctxClose();
    var items = [];
    var sel = '';
    try { sel = String(window.getSelection ? window.getSelection() : ''); } catch (err) {}
    if (sel.trim()) items.push(['Copy', 'clip:' + sel]);
    var a = e.target && e.target.closest ? e.target.closest('a[href]') : null;
    var href = (a && a.href && !/^javascript:/i.test(a.href)) ? a.href : null;
    if (href) {
      items.push(['Open in new tab', 'hint-open:' + href]);
      items.push(['Copy link address', 'clip:' + href]);
    }
    var img = e.target && e.target.closest ? e.target.closest('img[src]') : null;
    var isrc = img ? (img.currentSrc || img.src) : '';
    if (isrc && !/^data:/i.test(isrc)) items.push(['Copy image address', 'clip:' + isrc]);
    if (!items.length) return; // nothing actionable under the cursor
    var menu = document.createElement('div');
    menu.style.cssText = 'position:fixed;z-index:2147483647;left:' + e.clientX + 'px;top:' +
      e.clientY + 'px;background:#222;color:#eee;font:13px sans-serif;border:1px solid #444;' +
      'border-radius:4px;padding:4px 0;box-shadow:0 2px 8px rgba(0,0,0,.5);min-width:150px;';
    items.forEach(function (spec) {
      var item = document.createElement('div');
      item.textContent = spec[0];
      item.style.cssText = 'padding:6px 14px;white-space:nowrap;cursor:pointer;';
      item.addEventListener('mouseenter', function () { item.style.background = '#0a84ff'; });
      item.addEventListener('mouseleave', function () { item.style.background = ''; });
      item.addEventListener('click', function (ev) {
        ev.stopPropagation(); ctxClose(); post(spec[1]);
      });
      menu.appendChild(item);
    });
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
