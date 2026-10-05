// Run with: node --test crates/desktop/tests/bridge-hints.cjs
// Exercise the actual injected scripts without starting a browser/profile.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const assert = require('node:assert/strict');

function script(name) {
  const files = { BRIDGE_JS: 'bridge.js', HINT_JS: 'hints.js' };
  return readFileSync(join(__dirname, '../scripts', files[name]), 'utf8');
}

function page() {
  const listeners = new Map();
  const messages = [];
  const clicks = [];
  const document = {
    addEventListener(type, fn) {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(fn);
    },
    querySelector() { return null; },
    querySelectorAll() { return [button]; },
    getElementById() { return null; },
    createElement() {
      return {
        style: {}, appendChild() {}, remove() {}, addEventListener() {}, contains() { return false; },
        getBoundingClientRect() { return { left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0 }; },
      };
    },
    documentElement: { appendChild() {} },
  };
  const button = {
    tagName: 'BUTTON',
    closest(selector) { return selector.split(',').includes('button') ? this : null; },
    getAttribute() { return null; },
    getBoundingClientRect() {
      return { left: 10, top: 10, width: 100, height: 30, right: 110, bottom: 40 };
    },
    focus() {},
    dispatchEvent(event) {
      event.target = this;
      for (const listener of listeners.get(event.type) || []) listener(event);
      if (event.type === 'click') clicks.push(event);
      return true;
    },
  };
  class MouseEvent {
    constructor(type, options) {
      Object.assign(this, options, { type, isTrusted: false });
    }
  }
  class History {
    pushState() {}
    replaceState() {}
  }
  const window = {
    __post(message) { messages.push(message); },
    addEventListener() {},
    PointerEvent: MouseEvent,
  };
  const context = vm.createContext({
    window, document, MouseEvent, History,
    location: { href: 'https://example.test/' },
    innerWidth: 800, innerHeight: 600,
    getComputedStyle() { return { visibility: 'visible', display: 'block' }; },
    setTimeout() {},
  });
  vm.runInContext(script('BRIDGE_JS'), context);
  return { context, window, button, messages, clicks };
}

test('hinted button still clicks without selecting the pane under the mouse', () => {
  const p = page();
  p.window.__mode = 'hint';
  vm.runInContext(script('HINT_JS'), p.context);
  p.window.__hintInput('a', 'follow');
  assert.equal(p.clicks.length, 1);
  assert.deepEqual(p.messages, ['hint-exit']);
});

test('real pointer press still selects its pane and holds control focus', () => {
  const p = page();
  p.button.dispatchEvent({ type: 'pointerdown', isTrusted: true });
  assert.deepEqual(p.messages, ['pane-click', 'page-hold']);
});

test('synthetic pointer press does not change pane or gesture focus in normal mode', () => {
  const p = page();
  p.button.dispatchEvent({ type: 'pointerdown', isTrusted: false });
  assert.deepEqual(p.messages, []);
});

test('providers with native context menus keep their contextmenu event', () => {
  const p = page();
  p.window.__shellNativeContextMenu = true;
  let prevented = false;
  p.button.dispatchEvent({ type: 'contextmenu', preventDefault() { prevented = true; } });
  assert.equal(prevented, false);
});

test('default provider still suppresses the native context menu', () => {
  const p = page();
  let prevented = false;
  p.button.dispatchEvent({ type: 'contextmenu', preventDefault() { prevented = true; } });
  assert.equal(prevented, true);
});
