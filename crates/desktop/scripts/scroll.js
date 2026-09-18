// Scroll hints keep an explicit DOM target; reaching an edge never scrolls its parent.
(function () {
  if (window.__scrollClear) window.__scrollClear();
  var target = null, outline = null, observer = null;
  window.__scrollTarget = function () { return target; };
  function parent(el) {
    return el.parentElement || (el.getRootNode && el.getRootNode().host);
  }
  function visibleRect(el) {
    var r = el.getBoundingClientRect();
    var left = Math.max(0, r.left), top = Math.max(0, r.top);
    var right = Math.min(innerWidth, r.right), bottom = Math.min(innerHeight, r.bottom);
    for (var p = parent(el); p; p = parent(p)) {
      var st = getComputedStyle(p), pr = p.getBoundingClientRect();
      if (st.overflowX !== 'visible') { left = Math.max(left, pr.left); right = Math.min(right, pr.right); }
      if (st.overflowY !== 'visible') { top = Math.max(top, pr.top); bottom = Math.min(bottom, pr.bottom); }
    }
    return { left: left, top: top, width: right - left, height: bottom - top };
  }
  window.__scrollRect = visibleRect;
  window.__scrollCandidates = function () {
    var candidates = [];
    function scan(root) {
      Array.prototype.forEach.call(root.querySelectorAll('*'), function (el) {
        if (el.shadowRoot) scan(el.shadowRoot);
        if (el === document.scrollingElement || el === document.documentElement) return;
        var st = getComputedStyle(el);
        if (st.visibility === 'hidden' || st.visibility === 'collapse' || st.display === 'none') return;
        var vertical = /^(auto|scroll|overlay)$/.test(st.overflowY) && el.scrollHeight > el.clientHeight + 1;
        var horizontal = /^(auto|scroll|overlay)$/.test(st.overflowX) && el.scrollWidth > el.clientWidth + 1;
        if (!vertical && !horizontal) return;
        var r = visibleRect(el);
        if (r.width > 0 && r.height > 0) candidates.push(el);
      });
    }
    scan(document);
    return candidates;
  };
  window.__scrollClear = function () {
    if (target && window.__caretExit) window.__caretExit();
    target = null;
    if (outline) outline.remove();
    outline = null;
    if (observer) observer.disconnect();
    observer = null;
    window.removeEventListener('scroll', paint, true);
    window.removeEventListener('resize', paint);
    window.removeEventListener('pagehide', leave);
  };
  function leave() {
    window.__scrollClear();
    window.__post('scroll-exit');
  }
  function paint() {
    if (!target || !target.isConnected) { leave(); return; }
    var r = visibleRect(target);
    outline.style.cssText = 'position:fixed;pointer-events:none;z-index:2147483646;' +
      'box-sizing:border-box;border:2px solid #00e5ff;border-radius:3px;' +
      'left:' + r.left + 'px;top:' + r.top + 'px;width:' + Math.max(0, r.width) +
      'px;height:' + Math.max(0, r.height) + 'px;';
  }
  window.__scrollSelect = function (el) {
    window.__scrollClear();
    if (!el.isConnected) { leave(); return; }
    target = el;
    outline = document.createElement('div');
    document.documentElement.appendChild(outline);
    window.addEventListener('scroll', paint, true);
    window.addEventListener('resize', paint);
    window.addEventListener('pagehide', leave);
    observer = new MutationObserver(function () {
      if (!target || !target.isConnected) leave();
    });
    observer.observe(document, { childList: true, subtree: true });
    // Document observers do not cross shadow boundaries.
    for (var node = el; node && node.getRootNode;) {
      var root = node.getRootNode();
      if (!root.host) break;
      observer.observe(root, { childList: true, subtree: true });
      node = root.host;
    }
    paint();
    window.__post('scroll-selected');
  };
  window.__scrollMove = function (action) {
    if (!target || !target.isConnected) { leave(); return; }
    var x = target.scrollLeft, y = target.scrollTop;
    switch (action) {
      case 'down': y += 80; break;
      case 'up': y -= 80; break;
      case 'left': x -= 80; break;
      case 'right': x += 80; break;
      case 'half-down': y += target.clientHeight / 2; break;
      case 'half-up': y -= target.clientHeight / 2; break;
      case 'page-down': y += target.clientHeight; break;
      case 'page-up': y -= target.clientHeight; break;
      case 'top': y = 0; break;
      case 'bottom': y = target.scrollHeight; break;
    }
    target.scrollTo({ left: x, top: y, behavior: 'instant' });
    paint();
  };
})();
