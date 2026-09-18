// Run with: node --test crates/desktop/tests/*.cjs
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const { test } = require('node:test');
const assert = require('node:assert/strict');

function page() {
  const messages = [], elements = [], listeners = new Map(), observers = [];
  function element(options = {}) {
    const el = {
      tagName: 'DIV', isConnected: true, style: {}, children: [],
      clientWidth: 200, clientHeight: 100, scrollWidth: 200, scrollHeight: 500,
      scrollLeft: 0, scrollTop: 0,
      computed: { overflowX: 'visible', overflowY: 'auto', visibility: 'visible', display: 'block' },
      rect: { left: 20, top: 20, right: 220, bottom: 120, width: 200, height: 100 },
      getBoundingClientRect() { return this.rect; },
      appendChild(child) { this.children.push(child); child.parentElement = this; },
      remove() { this.isConnected = false; this.children.forEach(child => child.remove()); },
      scrollTo({ left, top }) {
        this.scrollLeft = Math.max(0, Math.min(left, this.scrollWidth - this.clientWidth));
        this.scrollTop = Math.max(0, Math.min(top, this.scrollHeight - this.clientHeight));
      },
      ...options,
    };
    elements.push(el);
    return el;
  }
  const html = element({ computed: { overflowX: 'visible', overflowY: 'visible' } });
  const document = {
    documentElement: html, scrollingElement: html,
    querySelector() { return null; },
    querySelectorAll() { return elements.filter(el => el.isConnected && !el.overlay && !el.inShadow); },
    createElement() { return element({ overlay: true }); },
    getElementById(id) { return elements.find(el => el.id === id && el.isConnected); },
  };
  const window = {
    __post(message) { messages.push(message); },
    addEventListener(type, fn) { listeners.set(type, fn); },
    removeEventListener(type) { listeners.delete(type); },
  };
  class MutationObserver {
    constructor(fn) { this.fn = fn; observers.push(this); }
    observe() { this.active = true; }
    disconnect() { this.active = false; }
  }
  const context = vm.createContext({
    window, document, MutationObserver, innerWidth: 800, innerHeight: 600,
    getComputedStyle(el) { return el.computed; },
  });
  function run(name) {
    vm.runInContext(readFileSync(join(__dirname, '../scripts', name), 'utf8'), context);
  }
  function hint() {
    run('scroll.js');
    window.__hintMode = 'scroll';
    run('hints.js');
  }
  return { window, document, html, elements, element, messages, listeners, observers, hint };
}

test('scroll hints include vertical and horizontal overflow, but exclude the page and invisible boxes', () => {
  const p = page();
  const vertical = p.element();
  const horizontal = p.element({ scrollHeight: 100, scrollWidth: 600,
    computed: { overflowX: 'auto', overflowY: 'hidden' } });
  p.element({ scrollHeight: 100 }); // Fits exactly.
  p.element({ computed: { overflowX: 'hidden', overflowY: 'hidden' } });
  p.element({ computed: { overflowY: 'auto', visibility: 'hidden' } });
  p.element({ rect: { left: 20, right: 220, top: 700, bottom: 800 } });
  const clipper = p.element({ scrollHeight: 100,
    computed: { overflowX: 'hidden', overflowY: 'hidden' } });
  p.element({ parentElement: clipper,
    rect: { left: 20, right: 220, top: 200, bottom: 300 } });
  p.hint();
  const hints = Object.values(p.window.__hintMap);
  assert.equal(hints.length, 2);
  assert.equal(hints[0].el, vertical);
  assert.equal(hints[1].el, horizontal);
});

test('selecting even an editable box locks scrolling without clicking or focusing it', () => {
  const p = page();
  const box = p.element({ tagName: 'TEXTAREA',
    focus() { assert.fail('must not enter Insert mode'); },
    dispatchEvent() { assert.fail('must not click the box'); },
  });
  p.hint();
  p.window.__hintInput('A', 'scroll');
  assert.deepEqual(p.messages, ['scroll-selected']);
  assert.equal(p.window.__hintMap, null);
  p.window.__scrollMove('down');
  assert.equal(box.scrollTop, 80);
  p.window.__scrollMove('half-down');
  assert.equal(box.scrollTop, 130); // Half the box, not half the viewport.
  p.window.__scrollMove('page-down');
  assert.equal(box.scrollTop, 230);
  p.window.__scrollMove('bottom');
  p.window.__scrollMove('down');
  assert.equal(box.scrollTop, 400);
  assert.equal(p.html.scrollTop, 0);
  p.window.__scrollMove('top');
  p.window.__scrollMove('up');
  assert.equal(box.scrollTop, 0);
  p.window.__scrollMove('page-down');
  p.window.__scrollMove('half-up');
  assert.equal(box.scrollTop, 50);
  p.window.__scrollMove('page-up');
  assert.equal(box.scrollTop, 0);
});

test('horizontal movement stays inside the chosen nested box at either edge', () => {
  const p = page();
  const outer = p.element({ scrollWidth: 700 });
  const inner = p.element({ parentElement: outer, scrollWidth: 500 });
  p.hint();
  p.window.__hintInput('s', 'scroll');
  for (let i = 0; i < 10; i++) p.window.__scrollMove('right');
  assert.equal(inner.scrollLeft, 300);
  assert.equal(outer.scrollLeft, 0);
  assert.equal(p.html.scrollLeft, 0);
  for (let i = 0; i < 10; i++) p.window.__scrollMove('left');
  assert.equal(inner.scrollLeft, 0);
});

test('no eligible boxes exits hint mode immediately', () => {
  const p = page();
  p.hint();
  assert.deepEqual(p.messages, ['scroll-exit']);
});

test('clearing a selection removes its outline and listeners and prevents further scrolling', () => {
  const p = page();
  const box = p.element();
  p.hint();
  p.window.__hintInput('a', 'scroll');
  p.window.__scrollClear();
  assert.equal(p.listeners.size, 0);
  assert.ok(p.observers.every(o => !o.active));
  assert.ok(p.elements.filter(el => el.overlay).every(el => !el.isConnected));
  p.window.__scrollMove('down');
  assert.equal(box.scrollTop, 0);
});

test('removing the selected element ends scroll mode without scrolling the page', () => {
  const p = page();
  const box = p.element();
  p.hint();
  p.window.__hintInput('a', 'scroll');
  box.remove();
  p.observers.find(o => o.active).fn();
  assert.deepEqual(p.messages, ['scroll-selected', 'scroll-exit']);
  assert.equal(p.listeners.size, 0);
  assert.equal(p.html.scrollTop, 0);
});

test('open shadow roots expose their scroll containers', () => {
  const p = page();
  const box = p.element({ inShadow: true });
  p.element({ scrollHeight: 100, shadowRoot: { querySelectorAll() { return [box]; } } });
  p.hint();
  assert.equal(Object.values(p.window.__hintMap)[0].el, box);
});

test('hint filtering and backspace preserve the selectable targets', () => {
  const p = page();
  for (let i = 0; i < 10; i++) p.element();
  p.hint();
  p.window.__hintInput('s', 'scroll');
  assert.equal(p.window.__hintMap.aa.badge.style.display, 'none');
  assert.equal(p.window.__hintMap.sa.badge.style.display, '');
  p.window.__hintInput('', 'scroll');
  assert.equal(p.window.__hintMap.aa.badge.style.display, '');
  p.window.__hintInput('aa', 'scroll');
  assert.deepEqual(p.messages, ['scroll-selected']);
});
