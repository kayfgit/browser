// Standard web APIs Servo 0.6 doesn't implement yet, filled in before page scripts
// run (every document and frame). Each shim steps aside once Servo has the real API.
(function () {
  // requestIdleCallback: React and many sites schedule work with it, and GitHub's
  // app fails without it. Run the callback soon, with a typical 50 ms budget.
  if (typeof window.requestIdleCallback !== 'function') {
    window.requestIdleCallback = function (callback) {
      return setTimeout(function () {
        var start = performance.now();
        callback({
          didTimeout: false,
          timeRemaining: function () { return Math.max(0, 50 - (performance.now() - start)); }
        });
      }, 1);
    };
    window.cancelIdleCallback = function (id) { clearTimeout(id); };
  }

  // Page visibility: Servo reports every document "hidden" until its load event
  // (servo#32687), while browsers report a foreground tab visible from the start.
  // Sites that defer rendering in hidden tabs (YouTube keeps its results hidden) then
  // wait forever. Report what the shell knows: the pane is shown unless it says not.
  // Getters via defineProperty, not wrapped natives: YouTube rejects patched natives.
  try {
    Object.defineProperty(Document.prototype, 'visibilityState', {
      configurable: true, enumerable: true,
      get: function () { return window.__paneHidden ? 'hidden' : 'visible'; }
    });
    Object.defineProperty(Document.prototype, 'hidden', {
      configurable: true, enumerable: true,
      get: function () { return !!window.__paneHidden; }
    });
  } catch (e) {}
})();
