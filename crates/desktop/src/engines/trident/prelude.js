// Runs before the shell's page scripts in Trident (IE11's MSHTML): the page-to-shell
// channel, plus the handful of DOM APIs those scripts use that IE11 lacks. ES5 only.
(function () {
  if (window.__tridentPrelude === document) return;
  window.__tridentPrelude = document;
  window.__post = function (m) {
    try { window.external.post(String(m)); } catch (e) {}
  };
  var E = window.Element && Element.prototype;
  if (E && !E.matches) E.matches = E.msMatchesSelector;
  if (E && !E.closest) {
    E.closest = function (selector) {
      for (var node = this; node && node.nodeType === 1; node = node.parentNode) {
        if (node.matches(selector)) return node;
      }
      return null;
    };
  }
  if (E && !E.remove) {
    E.remove = function () { if (this.parentNode) this.parentNode.removeChild(this); };
  }
  if (window.Node && !('isConnected' in Node.prototype)) {
    Object.defineProperty(Node.prototype, 'isConnected', {
      get: function () { return document.documentElement.contains(this); }
    });
  }
  if (window.NodeList && !NodeList.prototype.forEach) {
    NodeList.prototype.forEach = Array.prototype.forEach;
  }
  // scrollBy / scrollTo with an options object: IE11 only takes (x, y).
  function scrollShim(original, relative) {
    return function (a, b) {
      if (a && typeof a === 'object') {
        var x = a.left, y = a.top;
        if (relative) return original.call(this, x || 0, y || 0);
        return original.call(this,
          x === undefined ? (this.pageXOffset || this.scrollLeft || 0) : x,
          y === undefined ? (this.pageYOffset || this.scrollTop || 0) : y);
      }
      return original.call(this, a, b);
    };
  }
  window.scrollBy = scrollShim(window.scrollBy, true);
  window.scrollTo = scrollShim(window.scrollTo, false);
  if (E && !E.scrollBy) {
    E.scrollBy = function (a, b) {
      var x = typeof a === 'object' ? a.left || 0 : a || 0;
      var y = typeof a === 'object' ? a.top || 0 : b || 0;
      this.scrollLeft += x; this.scrollTop += y;
    };
  }
  if (E && !E.scrollTo) {
    E.scrollTo = function (a, b) {
      if (typeof a === 'object') {
        if (a.left !== undefined) this.scrollLeft = a.left;
        if (a.top !== undefined) this.scrollTop = a.top;
      } else { this.scrollLeft = a; this.scrollTop = b; }
    };
  }
  // Event.isTrusted (IE11 has none; the bridge only acts on real input): every event a
  // script makes comes from createEvent, so mark those and call the rest real.
  var D = window.Document && Document.prototype;
  if (D && D.createEvent && !('isTrusted' in Event.prototype)) {
    var create = D.createEvent;
    D.createEvent = function (type) {
      var ev = create.call(this, type);
      try { ev.__synthetic = true; } catch (e) {}
      return ev;
    };
    Object.defineProperty(Event.prototype, 'isTrusted', {
      configurable: true,
      get: function () { return !this.__synthetic; }
    });
  }
  // `new MouseEvent(type, init)`: IE11 only has document.createEvent.
  try { new MouseEvent('click'); } catch (e) {
    var Native = window.MouseEvent;
    var Shim = function (type, init) {
      init = init || {};
      var ev = document.createEvent('MouseEvent');
      ev.initMouseEvent(type, !!init.bubbles, !!init.cancelable, window, init.detail || 0,
        init.screenX || 0, init.screenY || 0, init.clientX || 0, init.clientY || 0,
        !!init.ctrlKey, !!init.altKey, !!init.shiftKey, !!init.metaKey,
        init.button || 0, init.relatedTarget || null);
      return ev;
    };
    if (Native) Shim.prototype = Native.prototype;
    window.MouseEvent = Shim;
  }
  // IE11's KeyboardEvent.key uses pre-standard names ("Esc", "Left", "Spacebar").
  var KEYS = {
    Esc: 'Escape', Left: 'ArrowLeft', Right: 'ArrowRight', Up: 'ArrowUp', Down: 'ArrowDown',
    Spacebar: ' ', Del: 'Delete', Apps: 'ContextMenu', Win: 'Meta', Scroll: 'ScrollLock'
  };
  try {
    var K = KeyboardEvent.prototype;
    var desc = Object.getOwnPropertyDescriptor(K, 'key');
    if (desc && desc.get) {
      Object.defineProperty(K, 'key', {
        configurable: true,
        get: function () {
          var k = desc.get.call(this);
          return KEYS.hasOwnProperty(k) ? KEYS[k] : k;
        }
      });
    }
  } catch (e) {}
})();
