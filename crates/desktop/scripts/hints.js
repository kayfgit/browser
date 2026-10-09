
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
  // Copy mode (`yf`) has nothing to say about buttons and text fields, so it labels
  // real links only — far fewer badges, hence shorter labels to type.
  var copyMode = window.__hintMode === 'copy';
  var scrollMode = window.__hintMode === 'scroll';
  var sel = copyMode ? "a[href]"
          : "a[href], button, input:not([type=hidden]):not([disabled]), textarea, " +
            "select, [onclick], [role='button'], [role='link'], [tabindex]:not([tabindex='-1'])";
  function onScreen(el) {
    var r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return false;
    if (r.bottom < 0 || r.right < 0 || r.top > innerHeight || r.left > innerWidth) return false;
    var st = getComputedStyle(el);
    return st.visibility !== 'hidden' && st.display !== 'none';
  }
  var els = scrollMode ? window.__scrollCandidates() : Array.prototype.slice.call(document.querySelectorAll(sel)).filter(function (el) {
    if (!onScreen(el)) return false;
    // A `javascript:` link has no address worth copying.
    return !copyMode || (el.href && !/^javascript:/i.test(el.href));
  });
  // Captcha checkboxes live in a cross-origin frame (and Cloudflare's behind a closed
  // shadow root), out of reach of the query above. Label the widget itself and aim
  // the click at its checkbox, which all of them draw ~28px in from the left edge.
  // Only checkbox-sized widgets: an image-grid challenge can't be solved by one click,
  // and the invisible reCAPTCHA badge does nothing when clicked.
  var points = new Map();
  if (!copyMode && !scrollMode) {
    var captcha = /recaptcha|hcaptcha|turnstile|challenges\.cloudflare\.com|captcha/i;
    var widgets = Array.prototype.slice.call(document.querySelectorAll('iframe')).filter(function (f) {
      var src = f.src || '';
      return captcha.test(src + ' ' + (f.title || '') + ' ' + (f.name || '')) &&
        !/size=invisible/.test(src);
    });
    Array.prototype.forEach.call(document.querySelectorAll(
      '.cf-turnstile, input[name="cf-turnstile-response"], input[id^="cf-chl-widget"]'), function (n) {
      var host = n.tagName === 'INPUT' ? n.parentElement : n;
      if (host && widgets.indexOf(host) === -1 &&
          !widgets.some(function (w) { return host.contains(w) || w.contains(host); })) {
        widgets.push(host);
      }
    });
    widgets.forEach(function (w) {
      if (!onScreen(w) || els.indexOf(w) !== -1) return;
      var r = w.getBoundingClientRect();
      if (r.height > 160 || r.width < 40) return;
      points.set(w, { x: r.left + Math.min(28, r.width / 2), y: r.top + r.height / 2 });
      els.push(w);
    });
  }
  if (scrollMode && !els.length) { window.__post('scroll-exit'); return; }
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
    var r = scrollMode ? window.__scrollRect(els[i]) : els[i].getBoundingClientRect();
    var b = document.createElement('span');
    // New-tab mode (`F`) shows labels uppercase as a cue; matching stays lowercase.
    b.textContent = window.__hintMode === 'newtab' ? labels[i].toUpperCase() : labels[i];
    b.style.cssText = 'position:fixed;left:' + Math.max(0, r.left) + 'px;top:' + Math.max(0, r.top) +
      'px;z-index:2147483647;background:' + (copyMode || scrollMode ? '#00e5ff' : '#ffd400') +
      ';color:#000;font:bold 11px monospace;padding:0 3px;' +
      'border:1px solid #000;border-radius:3px;line-height:14px;pointer-events:none;';
    box.appendChild(b);
    map[labels[i]] = { el: els[i], badge: b };
  }
  // A fullscreen element (a YouTube player) is drawn in the top layer, above
  // everything else — badges outside it would be hidden behind the video.
  (document.fullscreenElement || document.documentElement).appendChild(box);
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
    // Prefer a trusted click from the shell: a click dispatched from script isn't a
    // user gesture, so the page refuses it things like a clipboard write (a "copy"
    // button would do nothing), and some controls ignore it outright (YouTube's
    // skip-ad button). The shell clicks a POINT, so find one that lands on the element.
    var pt = window.top === window && clickPoint(el);
    if (pt) {
      window.__hintClickEl = el;
      window.__hintClickAt = Date.now();
      window.__post('hint-click:' + pt.x + ',' + pt.y);
      return;
    }
    fireClick(el);
  }
  function hits(el, p) {
    var hit = document.elementFromPoint(p.x, p.y);
    return !!hit && (hit === el || el.contains(hit));
  }
  // A viewport point where a click reaches `el`: its aim point (captcha checkbox) or
  // center, else any uncovered spot on it. If an overlay covers all of them (YouTube
  // lays a transparent layer over its skip-ad button), let clicks fall through that
  // overlay for a moment, so the trusted click lands on the element it's labelled for.
  function clickPoint(el) {
    var r = el.getBoundingClientRect();
    var first = points.get(el) || { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    var spots = [first];
    [0.5, 0.25, 0.75, 0.1, 0.9].forEach(function (fy) {
      [0.5, 0.25, 0.75, 0.1, 0.9].forEach(function (fx) {
        spots.push({ x: r.left + r.width * fx, y: r.top + r.height * fy });
      });
    });
    for (var i = 0; i < spots.length; i++) {
      var p = spots[i];
      if (p.x >= 0 && p.y >= 0 && p.x < innerWidth && p.y < innerHeight && hits(el, p)) return p;
    }
    var peeled = [];
    for (var n = 0; n < 8; n++) {
      var top = document.elementFromPoint(first.x, first.y);
      if (!top || top === el || el.contains(top)) break;
      // An ancestor on top means `el` itself takes no clicks: nothing to peel.
      if (top.contains(el) || top === document.body || top === document.documentElement) break;
      peeled.push([top, top.style.getPropertyValue('pointer-events'),
                   top.style.getPropertyPriority('pointer-events')]);
      top.style.setProperty('pointer-events', 'none', 'important');
    }
    function restore() {
      peeled.forEach(function (o) { o[0].style.setProperty('pointer-events', o[1], o[2]); });
    }
    if (peeled.length && hits(el, first)) {
      // The shell's click is asynchronous; put the overlays back once it's landed.
      setTimeout(restore, 1000);
      return first;
    }
    restore();
    return null;
  }
  // The shell couldn't inject the trusted click: fall back to the synthetic one.
  window.__hintFallback = function () {
    var el = window.__hintClickEl;
    window.__hintClickEl = null;
    if (el) fireClick(el);
  };
  // `mode` is 'follow' | 'newtab' | 'copy' | 'scroll' — the shell re-sends it on every
  // keystroke, since holding Shift flips follow↔newtab mid-pick.
  window.__hintInput = function (s, mode) {
    var m = window.__hintMap; if (!m) return;
    window.__hintMode = mode;
    var nt = mode === 'newtab';
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
      if (scrollMode) {
        window.__hintClear();
        window.__scrollSelect(el);
        return;
      }
      var edit = mode !== 'copy' && editable(el);
      // For new-tab and copy modes, resolve the link href before clearing badges.
      var a = !edit && el.closest ? el.closest('a[href]') : (el.tagName === 'A' ? el : null);
      var href = (a && a.href && !/^javascript:/i.test(a.href)) ? a.href : null;
      window.__hintClear();
      if (edit) {
        // Defer focusing until the shell has handed the webview OS focus, so the
        // field (not the document body) ends up focused; then enter passthrough.
        window.__hintTarget = el;
        window.__post('hint-edit');
      } else if (mode === 'copy') {
        // Copy mode only ever labels links, so `href` is set — but if a page mutated
        // the element out from under us, leave the clipboard alone and just exit.
        window.__post(href ? 'hint-copy:' + href : 'hint-exit');
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
