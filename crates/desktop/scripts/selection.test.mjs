// Run: node crates/desktop/scripts/selection.test.mjs [path-to-chromium]
// Uses real Chromium layout and Selection.modify without npm dependencies.
import { readFileSync, writeFileSync, mkdtempSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';

const browser = process.argv[2] || [
  'C:/Program Files/Google/Chrome/Application/chrome.exe',
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  '/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/google-chrome',
].find(existsSync);
if (!browser) throw new Error('Pass a Chromium executable as the first argument.');
const script = readFileSync(new URL('./selection.js', import.meta.url), 'utf8');
const dir = mkdtempSync(join(tmpdir(), 'browser-selection-test-'));
const html = `<!doctype html><meta charset="utf-8"><style>
body { margin:60px; font:20px Arial; } p { margin:0 0 20px; }
</style><main id="fixture"></main><pre id="results"></pre>
<script>${script}</script><script>
const results = [];
let copied = '', exited = false;
window.__post = text => {
  if (text.startsWith('caret-yank:')) copied = text.slice(11);
  if (text === 'caret-exit') exited = true;
};
function equal(actual, expected) {
  if (actual !== expected) throw new Error(JSON.stringify({ actual, expected }));
}
function test(name, run) {
  try { run(); results.push({ name, pass:true }); }
  catch (error) { results.push({ name, pass:false, error:String(error) }); }
}
function start(html, offset=0, selector='p') {
  __caretExit(); copied = ''; exited = false;
  fixture.innerHTML = html;
  document.caretRangeFromPoint = () => {
    const r = document.createRange();
    r.setStart(fixture.querySelector(selector).firstChild, offset); r.collapse(true); return r;
  };
  __caretEnter();
}
function keys(sequence) { for (const key of sequence) __caretKey(key); }
function selected() { return getSelection().toString(); }
function block() { return [...document.body.children].find(el => el.style.zIndex === '2147483647'); }
function cursorOn(offset) {
  const r = document.createRange();
  r.setStart(fixture.querySelector('p').firstChild, offset);
  r.setEnd(fixture.querySelector('p').firstChild, offset + 1);
  if (Math.abs(parseFloat(block().style.left) - r.getBoundingClientRect().left) > 0.01) {
    throw new Error('block is not on character ' + offset);
  }
}
test('Lupa: line-end selection parks on a, then h parks on p', () => {
  start('<p>Lupa</p>'); keys('v$'); equal(selected(), 'Lupa'); cursorOn(3);
  keys('h'); equal(selected(), 'Lup'); cursorOn(2);
});
test('Lupa: word-end selection parks on a, then h parks on p', () => {
  start('<p>Lupa next</p>'); keys('ve'); equal(selected(), 'Lupa'); cursorOn(3);
  keys('h'); equal(selected(), 'Lup'); cursorOn(2);
});
test('repeated word-end motions reach successive words', () => {
  start('<p>Lupa next x final</p>'); keys('ve'); equal(selected(), 'Lupa');
  keys('e'); equal(selected(), 'Lupa next');
  keys('e'); equal(selected(), 'Lupa next x');
  keys('e'); equal(selected(), 'Lupa next x final');
});
test('entering at the end of a line selects the last character immediately', () => {
  start('<p>Lupa</p>', 4); keys('v'); equal(selected(), 'a'); cursorOn(3);
});
test('narrow fixed block on p does not reach the adjacent a', () => {
  start('<p style="font:32px Arial">Lupa</p>', 2); keys('v');
  const r = document.createRange(); r.setStart(fixture.firstChild.firstChild, 3); r.setEnd(fixture.firstChild.firstChild, 4);
  if (block().getBoundingClientRect().right > r.getBoundingClientRect().left) throw new Error('block overlaps a');
  equal(getComputedStyle(block()).outlineStyle, 'none');
});
test('v immediately highlights and yanks the current character', () => {
  start('<p>apple</p>'); keys('v'); equal(selected(), 'a');
  __caretYank(); equal(copied, 'a'); equal(getSelection().rangeCount, 0);
});
test('yank exits selection, hides both overlays, and ignores further motions', () => {
  for (const selection of ['vll', 'V']) {
    start('<p>apple</p>'); keys(selection); __caretYank();
    equal(copied, selection === 'V' ? 'apple' : 'app');
    equal(getSelection().rangeCount, 0);
    equal(block().style.display, 'none');
    const line = [...document.body.children].find(el => el.style.zIndex === '2147483646');
    equal(line.style.display, 'none');
    keys('lv'); equal(getSelection().rangeCount, 0);
    __caretYank(); equal(copied, selection === 'V' ? 'apple' : 'app');
    __caretEnter(); keys('v'); equal(selected(), 'a');
  }
});
test('forward selection includes the final e without moving the block', () => {
  start('<p>apple</p>'); keys('vllll'); equal(selected(), 'apple');
  const r = document.createRange(); r.setStart(fixture.firstChild.firstChild, 4); r.setEnd(fixture.firstChild.firstChild, 5);
  const block = [...document.body.children].find(el => el.style.zIndex === '2147483647');
  equal(parseFloat(block.style.left), r.getBoundingClientRect().left);
  __caretYank(); equal(copied, 'apple');
});
test('reverse selection includes the anchor and can cross it', () => {
  start('<p>apple</p>', 2); keys('vhh'); equal(selected(), 'app');
  keys('llll'); equal(selected(), 'ple'); __caretYank(); equal(copied, 'ple');
});
test('cursor width stays fixed across i, d, W and end of line', () => {
  start('<p>idW</p>');
  const block = [...document.body.children].find(el => el.style.zIndex === '2147483647');
  const width = block.style.width;
  for (let i=0;i<3;i++) { keys('l'); equal(block.style.width, width); }
});
test('linewise yank excludes the next paragraph', () => {
  start('<p>hello, this is a test</p><p>next paragraph</p>', 4);
  keys('V'); equal(selected(), 'hello, this is a test');
  __caretYank(); equal(copied, 'hello, this is a test');
});
test('linewise motions include whole lines in both directions', () => {
  start('<p>first line</p><p>second line</p><p>third line</p>', 3);
  keys('Vj'); equal(selected(), 'first line\\n\\nsecond line');
  keys('k'); equal(selected(), 'first line');
  start('<p>first line</p><p>second line</p>', 3, 'p:nth-child(2)');
  keys('Vk'); equal(selected(), 'first line\\n\\nsecond line');
});
test('character selection across inline elements', () => {
  start('<p>ap<strong>pl</strong>e</p>'); keys('vllll');
  equal(selected(), 'apple'); __caretYank(); equal(copied, 'apple');
  start('<p>ap<strong>pl</strong>e</p>', 0, 'strong'); keys('vhh');
  equal(selected(), 'app'); __caretYank(); equal(copied, 'app');
});
test('soft-wrapped text stays inclusive and linewise selection follows layout', () => {
  start('<p style="font:20px monospace;width:6ch">apple berry cherry</p>', 6);
  keys('v'); equal(selected(), 'b'); __caretYank(); equal(copied, 'b');
  __caretEnter();
  keys('V'); equal(selected().trim(), 'berry');
  keys('j'); equal(selected().trim(), 'berry cherry');
  keys('k'); equal(selected().trim(), 'berry');
});
test('h onto the first character of a wrapped line paints that character', () => {
  start('<p style="font:20px monospace;width:6ch">apple berry cherry</p>', 7);
  keys('vh'); equal(selected(), 'be'); cursorOn(6);
  const r = document.createRange(); r.setStart(fixture.firstChild.firstChild, 6); r.setEnd(fixture.firstChild.firstChild, 7);
  equal(parseFloat(block().style.top), r.getBoundingClientRect().top);
});
test('linewise selection handles explicit line breaks and empty lines', () => {
  start('<p>apple<br><br>berry</p>'); keys('Vj');
  __caretYank(); equal(copied, 'apple');
  start('<p>apple<br>berry</p>'); keys('Vj');
  __caretYank(); equal(copied, 'apple\\nberry');
});
test('line end does not add a character from the following block', () => {
  start('<p>apple</p><p>banana</p>'); keys('v$');
  __caretYank(); equal(copied, 'apple');
});
test('trailing padding is removed without flattening code', () => {
  start('<pre>  hello, this is a test        \\n\\n</pre>', 0, 'pre'); keys('vG');
  __caretYank(); equal(copied, '  hello, this is a test');
  start('<pre>  first\\n\\n    second        </pre>', 0, 'pre'); keys('vG');
  __caretYank(); equal(copied, '  first\\n\\n    second');
});
test('collapsed HTML whitespace does not leak into copy', () => {
  start('<p>hello,    this is a test        \\n\\n</p><p>next</p>'); keys('V');
  __caretYank(); equal(copied, 'hello, this is a test');
});
test('single character selection respects a Unicode grapheme', () => {
  start('<p>😀éx</p>'); keys('v'); __caretYank(); equal(copied, '😀');
  __caretEnter(); keys('lv'); __caretYank(); equal(copied, 'é');
});
test('Escape cancels at the cursor, then exits on the second press', () => {
  start('<p>apple</p>'); keys('vll'); __caretEsc();
  equal(selected(), ''); equal(getSelection().focusOffset, 2); equal(exited, false);
  __caretEsc(); equal(exited, true);
});
test('v and V switch between character and line selection', () => {
  start('<p>apple</p>', 2); keys('vV'); equal(selected(), 'apple');
  keys('v'); equal(selected(), 'p'); keys('v'); equal(selected(), '');
});
document.getElementById('results').textContent = JSON.stringify(results);
</script>`;
try {
  const page = join(dir, 'test.html'); writeFileSync(page, html);
  const run = spawnSync(browser, ['--headless', '--disable-gpu', '--no-first-run',
    '--no-default-browser-check', '--allow-file-access-from-files',
    `--user-data-dir=${join(dir, 'profile')}`, '--dump-dom', pathToFileURL(page).href],
    { encoding:'utf8', timeout:60000, maxBuffer:4 * 1024 * 1024 });
  if (run.error || run.status !== 0) throw run.error || new Error(run.stderr);
  const match = run.stdout.match(/<pre id="results">([\s\S]*?)<\/pre>/);
  if (!match || !match[1]) throw new Error('No browser test results: ' + run.stderr);
  const results = JSON.parse(match[1].replaceAll('&quot;', '"').replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&'));
  for (const result of results) console.log(`${result.pass ? 'PASS' : 'FAIL'} ${result.name}${result.error ? ': ' + result.error : ''}`);
  if (results.some(r => !r.pass)) process.exitCode = 1;
} finally {
  rmSync(dir, { recursive:true, force:true, maxRetries:3 });
}
