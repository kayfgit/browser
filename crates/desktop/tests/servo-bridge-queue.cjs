const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const script = readFileSync(join(__dirname, '../src/engines/servo/bridge.js'), 'utf8');
function page() {
  const context = vm.createContext({ window: {}, document: {} });
  vm.runInContext(script, context);
  return context;
}
function read(context, doc = '', ack = 0) {
  return JSON.parse(context.window.__servoShellRead(doc, ack));
}
test('reads retry until acknowledged; wrong-document ack cannot erase messages', () => {
  const ctx = page();
  ctx.window.__post('page-ready');
  ctx.window.__post('hint-exit');
  const first = read(ctx);
  assert.deepEqual(first.messages, [[1, 'page-ready'], [2, 'hint-exit']]);
  assert.deepEqual(read(ctx).messages, first.messages);
  assert.deepEqual(read(ctx, 'old-document', 999).messages, first.messages);
  assert.deepEqual(read(ctx, first.document, 1).messages, [[2, 'hint-exit']]);
  assert.deepEqual(read(ctx, first.document, 2).messages, []);
});
test('new document replaces transport even when Window is reused', () => {
  const ctx = page();
  const oldRead = ctx.window.__servoShellRead;
  ctx.window.__post('old');
  const old = read(ctx);
  ctx.document = {};
  vm.runInContext(script, ctx);
  ctx.window.__post('new');
  assert.equal(oldRead('', 0), 'null');
  assert.notEqual(read(ctx).document, old.document);
  assert.deepEqual(read(ctx, old.document, 999).messages, [[1, 'new']]);
});
test('queue and batches remain bounded and overflow is explicit', () => {
  const ctx = page();
  for (let i = 0; i < 300; i++) ctx.window.__post('message');
  let result = read(ctx);
  assert.equal(result.messages.length, 32);
  assert.equal(result.dropped, 44);
  result = read(ctx, result.document, 32);
  assert.equal(result.messages[0][0], 33);
  ctx.window.__post('x'.repeat(8193));
  assert.equal(read(ctx).dropped, 45);
});
test('aggregate text size is bounded and non-string messages are ignored', () => {
  const ctx = page();
  ctx.window.__post({ command: 'ignored' });
  for (let i = 0; i < 9; i++) ctx.window.__post('x'.repeat(8192));
  const result = read(ctx);
  assert.equal(result.messages.length, 8);
  assert.equal(result.dropped, 1);
});
