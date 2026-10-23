
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
  // Glance mode (`q`) opens the link in a glance: links only, like copy mode.
  var glanceMode = window.__hintMode === 'glance';
  var linksOnly = copyMode || glanceMode;
  var scrollMode = window.__hintMode === 'scroll';
  // Zap mode (`z`) labels the page's visible boxes; picking one hides it.
  var zapMode = window.__hintMode === 'zap';
  var sel = linksOnly ? "a[href]"
          : "a[href], button, input:not([type=hidden]):not([disabled]), textarea, " +
            "select, [onclick], [role='button'], [role='link'], [tabindex]:not([tabindex='-1'])";
  // Widgets that take a click but are often plain divs, reachable only by arrow keys
  // (tabindex -1): Discord's server icons are `role=treeitem`. Many wrap a link or
  // button already labelled, so these count only when they hold none.
  var roles = linksOnly ? null
            : "[role='treeitem'], [role='tab'], [role='option'], [role='menuitem'], " +
              "[role='menuitemcheckbox'], [role='menuitemradio'], [role='checkbox'], " +
              "[role='radio'], [role='switch']";
  function onScreen(el) {
    var r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return false;
    if (r.bottom < 0 || r.right < 0 || r.top > innerHeight || r.left > innerWidth) return false;
    var st = getComputedStyle(el);
    return st.visibility !== 'hidden' && st.display !== 'none';
  }
  // Web components keep their insides in shadow roots, which querySelectorAll never
  // enters: Reddit's search box is an <input> two shadow roots down. Query every
  // open shadow root too (closed ones are unreachable by design).
  function deepQuery(root, selector, out) {
    Array.prototype.push.apply(out, root.querySelectorAll(selector));
    Array.prototype.forEach.call(root.querySelectorAll('*'), function (el) {
      if (el.shadowRoot) deepQuery(el.shadowRoot, selector, out);
    });
    return out;
  }
  // Zap candidates: every visible box big enough to matter — blocks, flex/grid
  // items, images, video, frames. Inline text runs are skipped (their block is
  // the thing to hide), and so is a box exactly the size of its parent, which
  // keeps the outer one, so a column of nested divs gets one label, not five. A box
  // that covers the whole viewport would blank the page, so it gets none either.
  function zapCandidates() {
    var media = /^(IMG|VIDEO|IFRAME|CANVAS|SVG|PICTURE|EMBED|OBJECT)$/i;
    function same(a, b) {
      return Math.abs(a.left - b.left) < 2 && Math.abs(a.top - b.top) < 2 &&
             Math.abs(a.right - b.right) < 2 && Math.abs(a.bottom - b.bottom) < 2;
    }
    return deepQuery(document, 'body *', []).filter(function (el) {
      var r = el.getBoundingClientRect();
      if (r.width < 40 || r.height < 16) return false;
      if (r.bottom < 0 || r.right < 0 || r.top > innerHeight || r.left > innerWidth) return false;
      if (r.left <= 0 && r.top <= 0 && r.right >= innerWidth && r.bottom >= innerHeight) return false;
      var st = getComputedStyle(el);
      if (st.display === 'none' || st.display === 'contents' || st.visibility === 'hidden') return false;
      if (st.display === 'inline' && !media.test(el.tagName)) return false;
      var up = el.parentElement;
      return !(up && up !== document.body && same(up.getBoundingClientRect(), r));
    });
  }
  function hintable() {
    var found = deepQuery(document, sel, []);
    if (roles) deepQuery(document, roles, []).forEach(function (el) {
      if (found.indexOf(el) === -1 && !el.querySelector(sel)) found.push(el);
    });
    return found.filter(function (el) {
      if (!onScreen(el)) return false;
      // A `javascript:` link has no address worth copying or glancing at.
      return !linksOnly || (el.href && !/^javascript:/i.test(el.href));
    });
  }
  var els = zapMode ? zapCandidates() : scrollMode ? window.__scrollCandidates() : hintable();
  // Captcha checkboxes live in a cross-origin frame (and Cloudflare's behind a closed
  // shadow root), out of reach of the query above. Label the widget itself and aim
  // the click at its checkbox, which all of them draw ~28px in from the left edge.
  // Only checkbox-sized widgets: an image-grid challenge can't be solved by one click,
  // and the invisible reCAPTCHA badge does nothing when clicked.
  var points = new Map();
  if (!linksOnly && !scrollMode && !zapMode) {
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
  // Nested zap boxes often share a corner; nudge a badge right until it clears
  // the ones already placed, so every label stays readable.
  var placed = [];
  function spot(x, y) {
    for (var tries = 0; tries < 20; tries++) {
      var hit = placed.some(function (p) { return Math.abs(p.y - y) < 15 && Math.abs(p.x - x) < 26; });
      if (!hit) break;
      x += 26;
    }
    placed.push({ x: x, y: y });
    return { x: x, y: y };
  }
  for (var i = 0; i < els.length; i++) {
    var r = scrollMode ? window.__scrollRect(els[i]) : els[i].getBoundingClientRect();
    var at = zapMode ? spot(Math.max(0, r.left), Math.max(0, r.top))
                     : { x: Math.max(0, r.left), y: Math.max(0, r.top) };
    var b = document.createElement('span');
    // New-tab mode (`F`) shows labels uppercase as a cue; matching stays lowercase.
    b.textContent = window.__hintMode === 'newtab' ? labels[i].toUpperCase() : labels[i];
    b.style.cssText = 'position:fixed;left:' + at.x + 'px;top:' + at.y +
      'px;z-index:2147483647;background:' +
      (zapMode ? '#ff6b6b' : glanceMode ? '#c69cf6' : copyMode || scrollMode ? '#00e5ff' : '#ffd400') +
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
  // The real link a hinted element is (or sits in), or null.
  function linkOf(el) {
    var a = el.closest ? el.closest('a[href]') : (el.tagName === 'A' ? el : null);
    return a && a.href && !/^javascript:/i.test(a.href) ? a : null;
  }
  // Follow a link without clicking it: by NAVIGATING to its href, which a synthetic
  // click can't be relied on for (YouTube's thumbnails ignore one). A full page load,
  // though, so it's only the fallback for when the shell can't click.
  function follow(el, a) {
    var here = location.href.split('#')[0];
    // A same-page hash link: let the click scroll instead of reloading.
    if (a.href.indexOf('#') !== -1 && a.href.split('#')[0] === here) { fireClick(el); return; }
    location.href = a.href;
  }
  // Activate a hinted target the way a mouse click would.
  function activate(el) {
    var a = linkOf(el);
    // A link meant for another window would open a pop-up; `f` follows it here.
    if (a && a.target && !/^_(self|top|parent)$/i.test(a.target)) { follow(el, a); return; }
    try { el.focus(); } catch (e) {}
    // Prefer a trusted click from the shell: a click dispatched from script isn't a
    // user gesture, so the page refuses it things like a clipboard write (a "copy"
    // button would do nothing), and some controls ignore it outright (YouTube's
    // skip-ad button). Links get one too, so a single-page app's router takes it and
    // switches in place (Discord reloads the whole app on a plain navigation).
    // The shell clicks a POINT, so find one that lands on the element.
    var pt = window.top === window && clickPoint(el);
    if (pt) {
      window.__hintClickEl = el;
      window.__hintClickAt = Date.now();
      window.__post('hint-click:' + pt.x + ',' + pt.y);
      return;
    }
    if (a) follow(el, a); else fireClick(el);
  }
  // The element at a point, descending into shadow roots (the document only reports
  // the outermost shadow host).
  function deepAt(x, y) {
    var hit = document.elementFromPoint(x, y);
    while (hit && hit.shadowRoot) {
      var inner = hit.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === hit) break;
      hit = inner;
    }
    return hit;
  }
  // `node` is `el` or inside it, counting a shadow root as inside its host.
  function within(el, node) {
    for (var n = node; n; n = n.parentNode || n.host) if (n === el) return true;
    return false;
  }
  function hits(el, p) {
    return within(el, deepAt(p.x, p.y));
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
      var top = deepAt(first.x, first.y);
      if (!top || within(el, top)) break;
      // An ancestor on top means `el` itself takes no clicks: nothing to peel.
      if (within(top, el) || top === document.body || top === document.documentElement) break;
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
  // Hide a picked box, remembering its own inline `display` so it can come back.
  // The list lives on the window: it outlasts this hint pass, and goes away with
  // the document (a reload brings everything back anyway).
  window.__zapped = window.__zapped || [];
  window.__zap = function (el) {
    window.__zapped.push([el, el.style.getPropertyValue('display'), el.style.getPropertyPriority('display')]);
    el.style.setProperty('display', 'none', 'important');
    window.__post('hint-zapped:' + window.__zapped.length);
  };
  // Bring back the last hidden box, or all of them; reports `<brought back>,<still hidden>`.
  window.__zapUndo = function (all) {
    var z = window.__zapped, n = 0;
    while (z.length && (all || n === 0)) {
      var e = z.pop();
      e[0].style.setProperty('display', e[1], e[2]);
      n++;
    }
    window.__post('zap-undone:' + n + ',' + z.length);
  };
  // The shell couldn't inject the trusted click: fall back to the synthetic one.
  window.__hintFallback = function () {
    var el = window.__hintClickEl;
    window.__hintClickEl = null;
    if (!el) return;
    var a = linkOf(el);
    if (a) follow(el, a); else fireClick(el);
  };
  // `mode` is 'follow' | 'newtab' | 'copy' | 'scroll' — the shell re-sends it on every
  // keystroke, since holding Shift flips follow↔newtab mid-pick.
  // Outline the boxes whose labels still match what's been typed, so you can see
  // what a zap would take before finishing the label. Only once few are left.
  function outline(boxes) {
    var old = document.getElementById('__zap_outlines');
    if (old) old.remove();
    if (!boxes.length || boxes.length > 12) return;
    var layer = document.createElement('div');
    layer.id = '__zap_outlines';
    boxes.forEach(function (el) {
      var r = el.getBoundingClientRect();
      var o = document.createElement('div');
      o.style.cssText = 'position:fixed;left:' + r.left + 'px;top:' + r.top + 'px;width:' + r.width +
        'px;height:' + r.height + 'px;outline:2px dashed #ff6b6b;outline-offset:-2px;' +
        'background:rgba(255,107,107,.08);pointer-events:none;z-index:2147483646;';
      layer.appendChild(o);
    });
    box.appendChild(layer);
  }
  window.__hintInput = function (s, mode) {
    var m = window.__hintMap; if (!m) return;
    window.__hintMode = mode;
    var nt = mode === 'newtab';
    s = (s || '').toLowerCase();
    var exact = null, live = [];
    for (var k in m) {
      // Keep the badge text in sync with the mode (UPPERCASE once new-tab is set),
      // so a plain-`f` hint that flips to new-tab mid-typing repaints its labels.
      m[k].badge.textContent = nt ? k.toUpperCase() : k;
      if (k.indexOf(s) === 0) {
        m[k].badge.style.display = '';
        live.push(m[k].el);
        if (k === s) exact = m[k];
      } else { m[k].badge.style.display = 'none'; }
    }
    if (zapMode && !exact) outline(s ? live : []);
    if (exact) {
      var el = exact.el;
      if (zapMode) {
        window.__hintClear();
        window.__zap(el);
        return;
      }
      if (scrollMode) {
        window.__hintClear();
        window.__scrollSelect(el);
        return;
      }
      var edit = mode !== 'copy' && mode !== 'glance' && editable(el);
      // For new-tab and copy modes, resolve the link href before clearing badges.
      var a = edit ? null : linkOf(el);
      var href = a ? a.href : null;
      window.__hintClear();
      if (edit) {
        // Defer focusing until the shell has handed the webview OS focus, so the
        // field (not the document body) ends up focused; then enter passthrough.
        window.__hintTarget = el;
        window.__post('hint-edit');
      } else if (mode === 'glance') {
        // Glance mode only labels links; open the picked one in a glance.
        window.__post(href ? 'hint-glance:' + href : 'hint-exit');
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
