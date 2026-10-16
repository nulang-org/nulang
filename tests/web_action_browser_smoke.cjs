// Dependency-free Chromium regression for the compiler-emitted Web action runtime.
// Run: node tests/web_action_browser_smoke.cjs
// Requires Chrome/Chromium in PATH (preinstalled on GitHub ubuntu-latest).
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { pathToFileURL } = require('node:url');

function clientRuntime() {
  const source = fs.readFileSync(
    path.join(__dirname, '../src/web/reactivity.rs'), 'utf8'
  );
  const start = source.indexOf('pub fn generate_client_runtime() -> String {');
  assert.notEqual(start, -1, 'Web micro-runtime generator not found');
  const open = source.indexOf('r#"', start);
  const close = source.indexOf('"#', open + 3);
  assert.ok(open !== -1 && close > open, 'Web micro-runtime raw string not found');
  const runtime = source.slice(open + 3, close);
  assert.ok(!runtime.toLowerCase().includes('</script'), 'unsafe closing script tag in emitted JS');
  return runtime;
}

function browserBinary() {
  const candidates = [
    process.env.CHROME_BIN,
    'google-chrome',
    'google-chrome-stable',
    'chromium',
    'chromium-browser'
  ].filter(Boolean);
  for (const candidate of candidates) {
    const result = spawnSync(candidate, ['--version'], {
      encoding: 'utf8', timeout: 5000
    });
    if (!result.error && result.status === 0) return candidate;
  }
  throw new Error('Chrome/Chromium is required for the Web action browser smoke test');
}

const html = `<!doctype html>
<html><head><meta charset="utf-8"></head><body>
  <form id="editor" data-action="save" data-action-placement="server">
    <label for="message">Message</label>
    <input id="message" name="message" value="Hello">
    <button type="submit" id="save">Save</button>
  </form>
  <button type="button" id="client" data-action="ping" data-action-placement="client">Ping</button>
  <script>
    window.__requests = [];
    window.__clientCalls = 0;
    window.nulangActions = { ping: () => { window.__clientCalls++; } };
    window.fetch = async function (url, options) {
      const message = JSON.parse(options.body.get('__nulang_ui_message'));
      window.__requests.push({
        method: options.method,
        action: message.request.action_id,
        value: message.request.payload.value.message?.value
      });
      // No navigation: deliberately return an error after capturing the POST.
      return { ok: false, status: 409 };
    };
  </script>
  <script>${clientRuntime()}</script>
  <script>
    window.addEventListener('load', async function () {
      const sleep = () => new Promise(resolve => setTimeout(resolve, 0));
      const check = (condition, message) => {
        if (!condition) throw new Error(message);
      };
      try {
        const form = document.getElementById('editor');
        const input = document.getElementById('message');
        const label = document.querySelector('label');
        input.click();
        label.click();
        await sleep();
        check(window.__requests.length === 0,
          'input/label clicks must not dispatch a server mutation');
        document.getElementById('client').click();
        check(window.__clientCalls === 1, 'non-form click action did not run');
        document.getElementById('save').click();
        await sleep();
        check(window.__requests.length === 1,
          'submit-button click must dispatch exactly one POST');
        check(window.__requests[0].method === 'POST' &&
          window.__requests[0].action === 'save' &&
          window.__requests[0].value === 'Hello',
          'button submission must preserve action and form payload');
        form.requestSubmit();
        await sleep();
        check(window.__requests.length === 2,
          'native requestSubmit must dispatch one additional POST');
        document.body.dataset.webActionSmoke = 'PASS';
      } catch (error) {
        document.body.dataset.webActionSmoke = 'FAIL';
        document.body.dataset.webActionError = String(error.message);
      }
    });
  </script>
</body></html>`;

const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'nulang-web-action-'));
try {
  const filename = path.join(directory, 'smoke.html');
  fs.writeFileSync(filename, html);
  const browser = browserBinary();
  const result = spawnSync(browser, [
    '--headless',
    '--no-sandbox',
    '--disable-gpu',
    '--disable-dev-shm-usage',
    '--disable-background-networking',
    '--no-first-run',
    '--virtual-time-budget=3000',
    '--dump-dom',
    pathToFileURL(filename).href
  ], { encoding: 'utf8', timeout: 30000, maxBuffer: 3 * 1024 * 1024 });
  if (result.error) throw result.error;
  assert.equal(result.status, 0,
    `Browser failed: ${(result.stderr || '').slice(-1200)}`);
  const body = result.stdout.match(/<body[^>]*>/i)?.[0] || '';
  assert.match(body, /data-web-action-smoke="PASS"/i,
    `Web action browser smoke failed: ${body.slice(0, 800)}`);
  console.log('PASS Web action browser smoke (input/label, submit, requestSubmit, client click)');
} finally {
  fs.rmSync(directory, { recursive: true, force: true });
}
