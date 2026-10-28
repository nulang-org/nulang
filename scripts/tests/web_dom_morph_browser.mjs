// Dependency-free real Chromium smoke test for Nulang's generated browser runtime.
// Usage: CHROME_BIN=/path/to/chrome node scripts/tests/web_dom_morph_browser.mjs
import assert from 'node:assert/strict';
import { readFileSync, mkdtempSync, writeFileSync, rmSync, existsSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"(.*?)"#\s*\.to_string\(\)/s);
assert.ok(match, 'expected generated Nulang browser runtime');

const page = String.raw`<!doctype html><html><body>
<div id="status" data-version="old">
  <input id="draft" name="draft" value="server-old">
  <input id="flag" type="checkbox" checked>
  <textarea id="notes">notes-old</textarea>
  <select id="choice"><option value="a" selected>A</option><option value="b">B</option></select>
  <ul id="items"><li data-nulang-key="a">A</li><li data-nulang-key="b">B</li></ul>
  <span id="label">Old</span>
</div>
<form id="save-form" data-action="update" data-action-placement="server" data-nulang-target="#status">
  <button type="submit">Save</button>
</form>
<pre id="smoke-result">pending</pre>
<script>__NULANG_RUNTIME__</script>
<script>
(function () {
  const originalRoot = document.querySelector('#status');
  const originalDraft = document.querySelector('#draft');
  const originalFlag = document.querySelector('#flag');
  const originalNotes = document.querySelector('#notes');
  const originalChoice = document.querySelector('#choice');
  const originalA = document.querySelector('[data-nulang-key="a"]');
  const originalB = document.querySelector('[data-nulang-key="b"]');
  let clicks = 0;
  let pings = 0;
  originalB.addEventListener('click', function () { clicks++; });
  originalDraft.value = 'edited locally';
  originalDraft.focus();
  originalDraft.setSelectionRange(3, 3);
  originalFlag.checked = false;
  originalNotes.value = 'draft notes';
  originalChoice.value = 'b';
  window.nulangActions = { ping: function () { pings++; } };
  window.fetch = async function () {
    return {
      ok: true,
      headers: { get: function () { return 'text/html; charset=utf-8'; } },
      text: async function () {
        return '<div id="status" data-version="new">' +
          '<input id="draft" name="draft" value="server-new">' +
          '<input id="flag" type="checkbox" checked>' +
          '<textarea id="notes">notes-new</textarea>' +
          '<select id="choice"><option value="a" selected>A</option><option value="b">B</option></select>' +
          '<ul id="items"><li data-nulang-key="b">B updated</li><li data-nulang-key="a">A updated</li><li data-nulang-key="c">C</li></ul>' +
          '<span id="label">Updated</span>' +
          '<button id="ping" data-action="ping" data-action-placement="client">Ping</button>' +
          '</div>';
      }
    };
  };
  document.querySelector('#save-form').requestSubmit();
  setTimeout(function () {
    const root = document.querySelector('#status');
    document.querySelector('[data-nulang-key="b"]').click();
    document.querySelector('#ping')?.click();
    const keys = Array.from(document.querySelectorAll('[data-nulang-key]'), n => n.getAttribute('data-nulang-key')).join(',');
    const results = {
      rootPreserved: root === originalRoot,
      draftPreserved: document.querySelector('#draft') === originalDraft,
      dirtyValue: document.querySelector('#draft').value === 'edited locally',
      focus: document.activeElement === originalDraft,
      selection: originalDraft.selectionStart === 3,
      checkbox: document.querySelector('#flag') === originalFlag && !originalFlag.checked,
      textarea: document.querySelector('#notes') === originalNotes && originalNotes.value === 'draft notes',
      select: document.querySelector('#choice') === originalChoice && originalChoice.value === 'b',
      bPreserved: document.querySelector('[data-nulang-key="b"]') === originalB,
      aPreserved: document.querySelector('[data-nulang-key="a"]') === originalA,
      existingListener: clicks === 1,
      newAction: pings === 1,
      order: keys === 'b,a,c',
      newText: document.querySelector('#label').textContent === 'Updated',
      attribute: root.getAttribute('data-version') === 'new'
    };
    const failures = Object.entries(results).filter(function (entry) { return !entry[1]; }).map(function (entry) { return entry[0]; });
    const marker = document.querySelector('#smoke-result');
    marker.dataset.pass = failures.length === 0 ? 'true' : 'false';
    marker.textContent = JSON.stringify({ results: results, failures: failures });
  }, 100);
})();
</script></body></html>`;
const chrome = process.env.CHROME_BIN ||
  ['/usr/bin/google-chrome', '/usr/bin/chromium-browser', '/usr/bin/chromium']
    .find(existsSync);
assert.ok(chrome, 'Chrome/Chromium is required for the real-browser morph regression');
const temp = mkdtempSync(join(tmpdir(), 'nulang-web-morph-'));
try {
  const path = join(temp, 'test.html');
  writeFileSync(path, page.replace('__NULANG_RUNTIME__', match[1]));
  const result = spawnSync(chrome, [
    '--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--disable-background-networking', '--virtual-time-budget=1500', '--dump-dom', 'file://' + path
  ], { encoding: 'utf8', timeout: 16000, maxBuffer: 2 * 1024 * 1024 });
  assert.equal(result.status, 0, result.stderr?.slice(-2000) || 'Chrome exited unsuccessfully');
  const marker = result.stdout.match(/<pre id="smoke-result"[^>]*>(.*?)<\/pre>/s);
  assert.ok(marker, 'test marker missing from Chrome DOM: ' + result.stdout.slice(0, 900) + ' STDERR: ' + result.stderr.slice(-400));
  assert.match(marker[0], /data-pass="true"/, 'browser morph smoke failed: ' + marker[1]);
  console.log('Nulang Web: real Chromium morphing preserves focus, dirty inputs, keyed nodes, and actions');
} finally {
  rmSync(temp, { recursive: true, force: true });
}
