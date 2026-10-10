// Run with: node --test crates/desktop/tests/navguard.cjs
// Exercise the injected redirect/popup guard without starting a browser.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const assert = require('node:assert/strict');

const NAVGUARD_JS = readFileSync(join(__dirname, '../scripts/navguard.js'), 'utf8');

function doc() {
  const listeners = {};
  return {
    listeners,
    baseURI: 'https://site.test/page',
    addEventListener(type, fn) { (listeners[type] = listeners[type] || []).push(fn); },
  };
}

// A top-level window on https://site.test whose real `window.open` records its calls.
function page() {
  const messages = [];
  const opened = [];
  const window = {
    __post(m) { messages.push(m); },
    open(u) { opened.push(u); return {}; },
  };
  window.top = window;
  const context = vm.createContext({ window, document: doc(), location: { origin: 'https://site.test' }, URL });
  const run = () => vm.runInContext(NAVGUARD_JS, context);
  return { context, window, messages, opened, run };
}

test('cross-origin window.open is neutered and reported while blocking is on', () => {
  const p = page();
  p.run();
  assert.equal(p.window.open('https://ads.test/pop'), null);
  assert.notEqual(p.window.open('/same-site'), null);
  assert.deepEqual(p.opened, ['/same-site']);
  assert.deepEqual(p.messages, ['popup-blocked:https://ads.test/pop']);
});

test('the shell toggle turns the guard off and is passed on to a cosmetic blocker', () => {
  const p = page();
  const cosmetic = [];
  p.window.__adblockCosmetic = (v) => cosmetic.push(v);
  p.run();
  p.window.__setAdblock(false);
  p.window.open('https://elsewhere.test/');
  assert.deepEqual(p.opened, ['https://elsewhere.test/']);
  assert.deepEqual(cosmetic, [false]);
});

test('a document starting off stays off, and a reused window follows the new document', () => {
  const p = page();
  p.window.__adblockDefault = true;
  p.run();
  // Chromium reuses the initial document's window for the first real page: the script
  // runs again for the new document, which must re-attach its listeners and adopt its
  // own starting state — without wrapping `window.open` a second time.
  const wrapped = p.window.open;
  p.context.document = doc();
  p.window.__adblockDefault = false;
  p.run();
  assert.equal(p.window.open, wrapped);
  assert.ok(p.context.document.listeners.pointerdown, 'listeners re-attached to the new document');
  p.window.open('https://elsewhere.test/');
  assert.deepEqual(p.opened, ['https://elsewhere.test/']);
});

test('a trusted press on a cross-site link reports navigation intent', () => {
  const p = page();
  p.run();
  const link = { href: 'https://other.test/', closest: (s) => (s === 'a[href]' ? link : null) };
  p.context.document.listeners.pointerdown[0]({ isTrusted: true, target: link });
  p.context.document.listeners.pointerdown[0]({ isTrusted: false, target: link });
  assert.deepEqual(p.messages, ['nav-intent']);
});
