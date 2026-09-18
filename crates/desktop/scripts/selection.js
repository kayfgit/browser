(function () {
  if (window.__caretInit) return;
  window.__caretInit = true;
  var on = false, visual = false, pend = '', bar = null, lineBar = null;
  // Logical character positions stay separate from the DOM's exclusive range
  // endpoints. This lets the highlighted range include BOTH cursor and anchor.
  var cursor = null, anchor = null;
  // A scroll-selected box optionally bounds both the caret and visual ranges.
  var scope = null;
  // Affinity of the LAST motion. At a soft-wrap boundary the same (node, offset)
  // renders on two lines; without this a `j` that logically moved down can paint
  // the block at the end of the line ABOVE (the "j went up" visual glitch).
  var dirBack = false;
  var BLINK = '__caretBlink 1.06s step-end infinite';
  function sel() { return window.getSelection(); }
  function position() { var s = sel(); return bounded({ n: s.focusNode, o: s.focusOffset }); }
  function collapse(p) { sel().collapse(p.n, p.o); }
  function pointRange(p) {
    var r = document.createRange(); r.setStart(p.n, p.o); r.collapse(true); return r;
  }
  function compare(a, b) { return pointRange(a).compareBoundaryPoints(Range.START_TO_START, pointRange(b)); }
  function scopeEdge(last) {
    var tw = document.createTreeWalker(scope, NodeFilter.SHOW_TEXT, null), n, hit = null;
    while ((n = tw.nextNode())) {
      if (!n.nodeValue.trim()) continue;
      var st = getComputedStyle(n.parentElement);
      if (st.visibility === 'hidden' || st.display === 'none') continue;
      var r = document.createRange(); r.selectNodeContents(n);
      if (!r.getClientRects().length) continue;
      hit = n;
      if (!last) break;
    }
    return hit ? { n: hit, o: last ? hit.nodeValue.length : 0 } : { n: scope, o: 0 };
  }
  function bounded(p) {
    if (!scope || !p.n || scope.contains(p.n)) return p;
    var first = scopeEdge(false);
    return compare(p, first) < 0 ? first : scopeEdge(true);
  }
  function lineEdge(p, direction) {
    collapse(p); sel().modify('move', direction, 'lineboundary'); return position();
  }
  function normalizeCursor() {
    // DOM carets can sit AFTER a line's last character. A Vim block must sit
    // ON that character, and the next h must move from that same logical place.
    var end = lineEdge(cursor, 'right'), start = lineEdge(cursor, 'left');
    collapse(cursor);
    if (compare(cursor, end) === 0 && compare(start, end) < 0) {
      sel().modify('move', 'backward', 'character'); cursor = position();
    }
  }
  function wordEnd() {
    var s = sel(), origin = position();
    // Chromium's word boundary may include the following space (platform
    // dependent). Walk back to the last non-whitespace grapheme of that word.
    for (var attempt = 0; attempt < 2; attempt++) {
      s.modify('move', 'forward', 'word');
      var boundary = position(), candidate = boundary;
      while (compare(candidate, origin) > 0) {
        s.modify('extend', 'backward', 'character');
        var text = s.toString(); candidate = position(); collapse(candidate);
        if (/\S/.test(text)) break;
      }
      if (compare(candidate, origin) > 0 || compare(boundary, origin) <= 0) return;
      collapse(boundary);
    }
  }
  function characterEnd(p) {
    collapse(p);
    // Do not extend an empty/end-of-line slot into the next block. In particular,
    // V+y must not copy the following paragraph's newline or first character.
    var end = lineEdge(p, 'right');
    collapse(p);
    if (compare(p, end) < 0) sel().modify('extend', 'forward', 'character');
    return position();
  }
  function renderSelection() {
    if (!cursor || !cursor.n.isConnected) return;
    if (!visual || !anchor || !anchor.n.isConnected) { collapse(cursor); return; }
    var backward = compare(cursor, anchor) < 0;
    var start = backward ? cursor : anchor, end = backward ? anchor : cursor;
    if (visual === 'line') {
      start = lineEdge(start, 'left'); end = lineEdge(end, 'right');
    } else {
      end = characterEnd(end);
    }
    var s = sel();
    collapse(backward ? end : start);
    var focus = backward ? start : end;
    s.extend(focus.n, focus.o);
  }
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
      'box-shadow:inset 0 0 0 1px #6cb6ff;pointer-events:none;display:none;animation:' + BLINK;
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
  // The rect of the logical cursor, independent of the inclusive DOM selection.
  // Probing a single character keeps the cursor one line tall no matter how big
  // the selection is. width 0 means the caret sits past the last character of a
  // line (block gets a default width). Wrap-ambiguous positions resolve along
  // `dirBack` so the block always lands on the line the motion moved to.
  function focusRect() {
    if (!cursor || !cursor.n.isConnected) return null;
    var n = cursor.n, o = cursor.o, rc;
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
      var rr = pointRange(cursor), rects = rr.getClientRects();
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
    // One fixed cell for this text size, independent of the glyph under it.
    b.style.width = Math.max(1, Math.round(h * 0.38)) + 'px';
    b.style.height = h + 'px';
    // Restart the blink so the block is solid right after a motion (vim-style).
    b.style.animation = 'none';
    void b.offsetWidth;
    b.style.animation = BLINK;
    lineBar.style.display = 'block';
    lineBar.style.top = rc.top + 'px';
    lineBar.style.height = h + 'px';
    lineBar.style.left = '0'; lineBar.style.width = '100vw';
    if (scope) {
      var box = scopeRect();
      lineBar.style.left = box.left + 'px'; lineBar.style.width = box.width + 'px';
      var top = Math.max(rc.top, box.top), bottom = Math.min(rc.top + h, box.top + box.height);
      lineBar.style.top = b.style.top = top + 'px';
      lineBar.style.height = b.style.height = Math.max(0, bottom - top) + 'px';
      var left = Math.max(rc.left, box.left);
      var right = Math.min(rc.left + Math.max(1, Math.round(h * 0.38)), box.left + box.width);
      b.style.left = left + 'px'; b.style.width = Math.max(0, right - left) + 'px';
      if (bottom <= top || right <= left) b.style.display = 'none';
    }
  }
  function scopeRect() {
    if (window.__scrollRect) return window.__scrollRect(scope);
    var r = scope.getBoundingClientRect();
    var left = Math.max(0, r.left), top = Math.max(0, r.top);
    return { left: left, top: top, width: Math.min(innerWidth, r.right) - left,
      height: Math.min(innerHeight, r.bottom) - top };
  }
  // Vim scrolloff: when a motion pushes the cursor within ~2 lines of the top or
  // bottom edge, scroll the window so the text around it stays visible (h/l past
  // the sides likewise). The scroll listener repaints the block afterwards.
  function keepInView() {
    var rc = focusRect();
    if (!rc) return;
    var h = rc.height || 16, pad = h * 2, bot = rc.top + h;
    if (scope) {
      var box = scopeRect(), dx = 0, dy = 0;
      pad = Math.min(pad, Math.max(0, (box.height - h) / 2));
      if (bot > box.top + box.height - pad) dy = Math.ceil(bot - (box.top + box.height - pad));
      else if (rc.top < box.top + pad) dy = Math.floor(rc.top - (box.top + pad));
      if (rc.left + rc.width > box.left + box.width) dx = Math.ceil(rc.left + rc.width - box.left - box.width);
      else if (rc.left < box.left) dx = Math.floor(rc.left - box.left);
      scope.scrollBy({ left: dx, top: dy, behavior: 'instant' });
      return;
    }
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
    var box = scope ? scopeRect() : { left: 0, top: 0, width: innerWidth, height: innerHeight };
    var x = Math.floor(box.left + box.width / 2), r = null;
    // Probe outward from the vertical center for a TEXT node — a page's middle is
    // often whitespace (caret would land on an element, where word/line motions
    // can't move). Fall back to the first text node in the document.
    var ys = [0.5, 0.4, 0.6, 0.3, 0.7, 0.25, 0.75, 0.15, 0.85];
    for (var i = 0; i < ys.length && !r; i++) {
      var rr = rangeAt(x, Math.floor(box.top + box.height * ys[i]));
      if (rr && rr.startContainer && rr.startContainer.nodeType === 3
          && (!scope || scope.contains(rr.startContainer))) r = rr;
    }
    if (!r) {
      var root = scope || document.body || document.documentElement;
      var tw = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, null);
      var n;
      while ((n = tw.nextNode())) { if (n.nodeValue && n.nodeValue.trim()) break; }
      r = document.createRange();
      if (n) r.setStart(n, 0); else r.selectNodeContents(root);
    }
    r.collapse(true);
    var s = sel(); s.removeAllRanges(); s.addRange(r);
  }
  window.__caretEnter = function (root) {
    scope = root || null;
    on = true; visual = false; pend = ''; dirBack = false;
    caretAtCenter();
    cursor = position(); anchor = null;
    normalizeCursor();
    updateBar();
  };
  window.__caretExit = function () {
    on = false; visual = false; pend = '';
    scope = null;
    cursor = anchor = null;
    try { sel().removeAllRanges(); } catch (e) {}
    if (bar) bar.style.display = 'none';
    if (lineBar) lineBar.style.display = 'none';
  };
  window.__caretEsc = function () {
    if (!on) return;
    if (visual) { visual = false; anchor = null; collapse(cursor); updateBar(); }
    else { window.__caretExit(); window.__post('caret-exit'); }
  };
  window.__caretYank = function () {
    if (!on) return;
    var s = sel(), t = '';
    try {
      renderSelection();
      t = s.toString();
    } catch (e) { t = s.toString(); }
    // Keep internal whitespace (including code indentation and paragraph breaks),
    // but discard padding and blank lines at the end of the copied selection.
    window.__caretExit();
    // The shell handles this single event by copying and returning to Normal,
    // keeping the yank confirmation visible after the mode change.
    window.__post('caret-yank:' + t.trimEnd());
  };
  window.__caretKey = function (k) {
    if (!on) return;
    var s = sel(), alter = 'move';
    collapse(cursor);
    // Remember the motion's direction — focusRect uses it to resolve positions
    // that render on two lines (soft wraps).
    if ('hkb0'.indexOf(k) >= 0) dirBack = true;
    else if ('ljweG$'.indexOf(k) >= 0) dirBack = false;
    if (pend === 'g') { pend = ''; if (k === 'g') { dirBack = true; if (scope) collapse(scopeEdge(false)); else s.modify(alter,'backward','documentboundary'); cursor = position(); renderSelection(); keepInView(); updateBar(); return; } }
    switch (k) {
      case 'h': s.modify(alter,'left','character'); break;
      case 'l': s.modify(alter,'right','character'); break;
      case 'j': s.modify(alter,'forward','line'); break;
      case 'k': s.modify(alter,'backward','line'); break;
      case 'w': s.modify(alter,'forward','word'); break;
      case 'e': wordEnd(); break;
      case 'b': s.modify(alter,'backward','word'); break;
      case '0': s.modify(alter,'left','lineboundary'); break;
      case '$': s.modify(alter,'right','lineboundary'); break;
      case 'G': if (scope) collapse(scopeEdge(true)); else s.modify(alter,'forward','documentboundary'); break;
      case 'g': pend = 'g'; break;
      case 'v':
        visual = visual === 'char' ? false : 'char';
        if (visual && !anchor) anchor = cursor;
        if (!visual) anchor = null;
        break;
      case 'V':
        visual = visual === 'line' ? false : 'line';
        if (visual && !anchor) anchor = cursor;
        if (!visual) anchor = null;
        break;
      default: break;
    }
    cursor = position();
    normalizeCursor();
    renderSelection();
    keepInView();
    updateBar();
  };
  // Selection.modify can reveal its endpoint by scrolling ancestor containers.
  // Restore those positions so a boxed selection never drags the outer page along.
  ['__caretEnter', '__caretKey', '__caretEsc', '__caretYank'].forEach(function (name) {
    var run = window[name];
    window[name] = function () {
      var root = name === '__caretEnter' ? arguments[0] : scope;
      if (!root) return run.apply(this, arguments);
      var positions = [], pageX = window.scrollX, pageY = window.scrollY;
      for (var p = root.parentElement || root.getRootNode().host; p;
           p = p.parentElement || p.getRootNode().host) {
        positions.push({ el: p, x: p.scrollLeft, y: p.scrollTop });
      }
      try { return run.apply(this, arguments); }
      finally {
        positions.forEach(function (p) {
          if (p.el.scrollLeft !== p.x || p.el.scrollTop !== p.y)
            p.el.scrollTo({ left: p.x, top: p.y, behavior: 'instant' });
        });
        if (window.scrollX !== pageX || window.scrollY !== pageY)
          window.scrollTo({ left: pageX, top: pageY, behavior: 'instant' });
        if (on) updateBar();
      }
    };
  });
  window.addEventListener('scroll', function () { if (on) updateBar(); }, true);
  window.addEventListener('resize', function () { if (on) updateBar(); });
})();
