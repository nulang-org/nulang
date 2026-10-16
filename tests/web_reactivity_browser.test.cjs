// Browser integration smoke for Nulang's Rust-emitted signal runtime.
// Uses Node 22+ and installed Chrome/Chromium; no browser-test dependencies.
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
const test = require('node:test');

function runtime() {
  const source = fs.readFileSync(path.join(__dirname, '../src/web/reactivity.rs'), 'utf8');
  const start = source.indexOf('pub fn generate_client_runtime() -> String {');
  assert.ok(start >= 0);
  const begin = source.indexOf('r#"', start) + 3;
  const end = source.indexOf('"#', begin);
  assert.ok(begin >= 3 && end > begin);
  return source.slice(begin, end);
}
function chromeBinary() {
  for (const candidate of [process.env.CHROMIUM_BIN, process.env.CHROME_BIN,
    '/usr/bin/google-chrome', '/usr/bin/chromium']) {
    if (candidate && fs.existsSync(candidate)) return candidate;
  }
  for (const name of ['google-chrome', 'chromium']) {
    const found = spawnSync('which', [name], { encoding: 'utf8' });
    if (found.status === 0) return found.stdout.trim();
  }
  throw Error('Chrome/Chromium required for browser smoke test');
}
async function devtoolsPort(dir, process) {
  for (let i = 0; i < 100; i++) {
    if (process.exitCode !== null) throw Error('Chromium exited early');
    try { return Number(fs.readFileSync(path.join(dir, 'DevToolsActivePort'), 'utf8').split('\n')[0]); }
    catch { await new Promise(resolve => setTimeout(resolve, 100)); }
  }
  throw Error('Chromium failed to start DevTools');
}
async function connect(port) {
  const targets = await (await fetch('http://127.0.0.1:' + port + '/json')).json();
  const page = targets.find(item => item.type === 'page');
  assert.ok(page?.webSocketDebuggerUrl);
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true });
    ws.addEventListener('error', reject, { once: true });
  });
  let nextId = 0;
  const pending = new Map();
  ws.addEventListener('message', event => {
    const result = JSON.parse(event.data);
    const callbacks = pending.get(result.id);
    if (!callbacks) return;
    pending.delete(result.id);
    if (result.error) callbacks.reject(Error(result.error.message));
    else callbacks.resolve(result.result);
  });
  return {
    async eval(source) {
      const result = await new Promise((resolve, reject) => {
        const id = ++nextId;
        pending.set(id, { resolve, reject });
        ws.send(JSON.stringify({ id, method: 'Runtime.evaluate',
          params: { expression: source, returnByValue: true } }));
      });
      if (result.exceptionDetails) throw Error(result.exceptionDetails.text + ': ' +
        (result.exceptionDetails.exception?.description || 'unknown'));
      return result.result.value;
    },
    close() { ws.close(); }
  };
}

function browserChecks() {
  const assert = (ok, message) => { if (!ok) throw Error(message); };
  const api = window.nulang;
  const span = document.getElementById('state');
  const button = document.getElementById('button');
  const link = document.getElementById('link');
  const special = document.getElementById('special');
  assert(api.signal('status').get() === 'Initial', 'SSR text state');
  assert(api.signal('__proto__').get() === 'Safe', 'special key');
  const writes = [];
  const original = button.setAttribute.bind(button);
  button.setAttribute = function(name, value) { writes.push(name); return original(name, value); };
  api.signal('status').set('Initial');
  assert(writes.length === 0, 'same-value write');
  const result = api.batch(() => {
    api.signal('status').set('One');
    api.batch(() => api.signal('status').set('Two'));
    assert(span.textContent === 'Initial' && button.title === 'Initial', 'early flush');
    assert(api.signal('status').get() === 'Two', 'stale read');
    return 42;
  });
  assert(result === 42 && writes.length === 2, 'batch coalescing');
  assert(span.textContent === 'Two' && button.title === 'Two' &&
    button.getAttribute('aria-label') === 'Two', 'DOM hydration');
  const payload = '<img src=x onerror=alert(1)>';
  api.signal('status').set(payload);
  assert(span.textContent === payload && span.querySelector('img') === null, 'HTML injection');
  assert(button.title === payload && button.textContent === 'Static', 'attribute safety');
  try { api.batch(() => { api.signal('status').set('Recovered'); throw Error('expected'); }); }
  catch (error) { assert(error.message === 'expected', 'exception propagation'); }
  assert(span.textContent === 'Recovered', 'exception flush');
  api.signal('__proto__').set('Updated');
  assert(special.title === 'Updated', 'prototype-safe key');
  assert(link.href === 'https://example.test/unchanged', 'unsafe URL mutated');
  return true;
}

test('real Chromium DOM hydration, batching, and safe attributes', { timeout: 25000 }, async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'nulang-web-'));
  let chrome;
  let cdp;
  try {
    const js = runtime();
    assert.ok(!js.toLowerCase().includes('</script'));
    const html = '<span id="state" data-signal="status">Initial</span>' +
      '<button id="button" title="Initial" aria-label="Initial" data-nulang-bind-title="status" data-nulang-bind-aria-label="status">Static</button>' +
      '<a id="link" href="https://example.test/unchanged">Link</a>' +
      '<span id="special" title="Safe" data-nulang-bind-title="__proto__"></span>' +
      '<script>' + js + '</script>';
    chrome = spawn(chromeBinary(), [ '--headless=new', '--no-sandbox', '--disable-gpu',
      '--disable-dev-shm-usage', '--disable-background-networking',
      '--remote-debugging-port=0', '--user-data-dir=' + dir, 'about:blank' ], { stdio: 'ignore' });
    cdp = await connect(await devtoolsPort(dir, chrome));
    await cdp.eval('document.open(); document.write(' + JSON.stringify(html) + '); document.close();');
    assert.equal(await cdp.eval('(' + browserChecks.toString() + ')()'), true);
  } finally {
    cdp?.close();
    chrome?.kill('SIGTERM');
    if (chrome && chrome.exitCode === null) {
      await Promise.race([new Promise(resolve => chrome.once('exit', resolve)),
        new Promise(resolve => setTimeout(resolve, 1000))]);
    }
    fs.rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});
