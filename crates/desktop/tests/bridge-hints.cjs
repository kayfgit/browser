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
  const timers = [];
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
    documentElement: { appendChild(el) { appended.push(el); } },
  };
  const appended = [];
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
    setTimeout(fn) { timers.push(fn); },
  });
  vm.runInContext(script('BRIDGE_JS'), context);
  return { context, window, document, button, messages, clicks, timers, appended };
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
  // ...and shows its own menu once the page has had its turn.
  p.timers.splice(0).forEach((fn) => fn());
  assert.equal(p.appended.length, 1);
});

test('a page with its own right-click menu gets only that menu', () => {
  const p = page();
  // The page's own handler (YouTube's player) cancels the event to show its menu.
  p.document.addEventListener('contextmenu', (e) => e.preventDefault());
  let prevented = false;
  p.button.dispatchEvent({ type: 'contextmenu', preventDefault() { prevented = true; } });
  assert.equal(prevented, true, 'the native menu is still cancelled');
  p.timers.splice(0).forEach((fn) => fn());
  assert.equal(p.appended.length, 0, "no shell menu on top of the page's");
  assert.deepEqual(p.messages, ['page-hold'], "the page keeps focus, so its menu stays open");
});

// A page with real hit-testing: `stack` lists elements topmost first, and
// elementFromPoint returns the first one under the point that takes clicks.
function hitPage({ clickables = [], iframes = [], turnstile = [], stack }) {
  const messages = [];
  const timers = [];
  const document = {
    querySelector() { return null; },
    querySelectorAll(sel) {
      if (sel === 'iframe') return iframes;
      if (sel.includes('cf-turnstile')) return turnstile;
      return clickables;
    },
    getElementById() { return null; },
    createElement() { return { style: {}, appendChild() {}, remove() {} }; },
    elementFromPoint(x, y) {
      return stack.find((e) => {
        const r = e.getBoundingClientRect();
        return e.style.getPropertyValue('pointer-events') !== 'none' &&
          x >= r.left && x < r.right && y >= r.top && y < r.bottom;
      }) || null;
    },
    documentElement: { appendChild() {} },
    body: {},
  };
  const window = { __post(message) { messages.push(message); } };
  window.top = window;
  const context = vm.createContext({
    window, document, MouseEvent: class {}, location: { href: 'https://example.test/' },
    innerWidth: 800, innerHeight: 600,
    getComputedStyle() { return { visibility: 'visible', display: 'block' }; },
    setTimeout(fn) { timers.push(fn); },
  });
  vm.runInContext(script('HINT_JS'), context);
  return { window, messages, timers };
}

function element(tagName, rect, extra = {}) {
  const css = {};
  const e = {
    tagName,
    parentElement: null,
    style: {
      setProperty(k, v, p) { css[k] = [v, p || '']; },
      getPropertyValue(k) { return (css[k] || [''])[0]; },
      getPropertyPriority(k) { return (css[k] || ['', ''])[1]; },
    },
    getAttribute() { return null; },
    getBoundingClientRect() {
      return { ...rect, right: rect.left + rect.width, bottom: rect.top + rect.height };
    },
    contains(other) {
      for (let x = other; x; x = x.parentElement) if (x === e) return true;
      return false;
    },
    closest() { return null; },
    focus() {},
    dispatchEvent() { return true; },
    ...extra,
  };
  return e;
}

test('a button under a full overlay gets a trusted click through the overlay', () => {
  const button = element('BUTTON', { left: 10, top: 10, width: 100, height: 30 });
  const overlay = element('DIV', { left: 0, top: 0, width: 200, height: 100 });
  const p = hitPage({ clickables: [button], stack: [overlay, button] });
  p.window.__hintInput('a', 'follow');
  assert.deepEqual(p.messages, ['hint-click:60,25', 'hint-exit']);
  assert.equal(overlay.style.getPropertyValue('pointer-events'), 'none');
  p.timers.forEach((fn) => fn());
  assert.equal(overlay.style.getPropertyValue('pointer-events'), '');
});

test('a partly covered button is clicked on its uncovered part, overlay untouched', () => {
  const button = element('BUTTON', { left: 10, top: 10, width: 100, height: 30 });
  const overlay = element('DIV', { left: 40, top: 0, width: 100, height: 100 });
  const p = hitPage({ clickables: [button], stack: [overlay, button] });
  p.window.__hintInput('a', 'follow');
  assert.deepEqual(p.messages, ['hint-click:35,25', 'hint-exit']);
  assert.deepEqual(p.timers, []);
});

test('a captcha frame is labelled and clicked on its checkbox', () => {
  const frame = element('IFRAME', { left: 20, top: 100, width: 300, height: 65 },
    { src: 'https://challenges.cloudflare.com/cdn-cgi/challenge-platform/turnstile/if/ov2' });
  const p = hitPage({ iframes: [frame], stack: [frame] });
  p.window.__hintInput('a', 'follow');
  assert.deepEqual(p.messages, ['hint-click:48,132.5', 'hint-exit']);
});

test('a Turnstile widget in a closed shadow root is clicked through its host', () => {
  const host = element('DIV', { left: 0, top: 0, width: 300, height: 65 });
  const response = element('INPUT', { left: 0, top: 0, width: 0, height: 0 });
  response.parentElement = host;
  const p = hitPage({ turnstile: [response], stack: [host] });
  p.window.__hintInput('a', 'follow');
  assert.deepEqual(p.messages, ['hint-click:28,32.5', 'hint-exit']);
});

test('invisible reCAPTCHA badges and image challenges get no label', () => {
  const badge = element('IFRAME', { left: 0, top: 500, width: 256, height: 60 },
    { src: 'https://www.google.com/recaptcha/api2/anchor?k=x&size=invisible' });
  const grid = element('IFRAME', { left: 0, top: 0, width: 400, height: 580 },
    { src: 'https://www.google.com/recaptcha/api2/bframe?k=x' });
  const p = hitPage({ iframes: [badge, grid], stack: [badge, grid] });
  assert.deepEqual(Object.keys(p.window.__hintMap), []);
});

// Insert mode: Esc must reach the page (GitHub's search popup closes on it) and then
// leave the field and Insert, reported once.
function escapeInField(p) {
  let prevented = false;
  let blurred = 0;
  const field = { tagName: 'INPUT', getAttribute: () => 'search', blur() { blurred++; } };
  p.document.activeElement = field;
  p.window.__mode = 'insert';
  p.button.dispatchEvent({
    type: 'keydown', key: 'Escape', shiftKey: false,
    preventDefault() { prevented = true; }, stopPropagation() { prevented = true; },
  });
  return { prevented: () => prevented, blurred: () => blurred };
}

test('Esc in Insert reaches the page, then leaves the field and Insert', () => {
  const p = page();
  const esc = escapeInField(p);
  assert.equal(esc.prevented(), false, 'the page must see the Esc');
  assert.deepEqual(p.messages, [], 'the exit waits until the page has handled it');
  p.timers.splice(0).forEach((fn) => fn());
  assert.equal(esc.blurred(), 1);
  assert.equal(p.window.__mode, 'normal');
  p.timers.splice(0).forEach((fn) => fn()); // the focusout check, if any
  assert.deepEqual(p.messages, ['insert-escape']);
});

test('Esc leaves Insert alone if the page already left it', () => {
  const p = page();
  const esc = escapeInField(p);
  p.window.__mode = 'normal'; // e.g. the shell switched modes meanwhile
  p.timers.splice(0).forEach((fn) => fn());
  assert.equal(esc.blurred(), 0);
  assert.deepEqual(p.messages, []);
});
