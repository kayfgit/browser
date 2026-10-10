(function () {
  // Keyed on the DOCUMENT, not the window: the init script runs first against the initial
  // about:blank document, whose window Chromium reuses for the first real page, so a
  // window-keyed once-guard would leave the listeners below on that dead document.
  if (window.__navguardDoc === document) return;
  var firstRun = !window.__navguardDoc;
  window.__navguardDoc = document;
  // Shared across documents of this window: the `window.open` wrapper below is installed
  // once per window but must follow the toggle of whichever document is live.
  var state = firstRun ? (window.__navguardState = {}) : window.__navguardState;
  state.on = (typeof window.__adblockDefault === 'undefined') ? true : !!window.__adblockDefault;

  // Forced cross-site redirects (the "click the video → bounced to a scam" hijack) are
  // cancelled natively by the shell's intent-gate guard, which denies any cross-site TOP
  // navigation that no trusted gesture asked for — see `navguard`. The page side's job is
  // to REPORT that trusted intent (below) so genuine link clicks are allowed, and to
  // neuter popunder `window.open`s. `crossOrigin` backs both.
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
  function reportIntent(t) { if (state.on && isTop && navTargetCrossSite(t)) window.__post('nav-intent'); }
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
  var _winOpen = firstRun && window.open ? window.open.bind(window) : null;
  if (_winOpen) {
    window.open = function (u) {
      if (state.on && (!u || crossOrigin(u))) { reportPopup(u || 'about:blank'); return null; }
      return _winOpen.apply(null, arguments);
    };
  }

  // Live toggle from the shell (`:ads`). Also passed on to the page-side cosmetic blocker
  // where one runs (Servo, `ADBLOCK_JS`).
  window.__setAdblock = function (v) {
    state.on = !!v;
    if (window.__adblockCosmetic) window.__adblockCosmetic(state.on);
  };
})();
