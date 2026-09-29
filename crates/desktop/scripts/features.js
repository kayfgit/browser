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
