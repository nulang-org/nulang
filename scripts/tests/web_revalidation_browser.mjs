// Dependency-free Chromium HTTP regression for compiled revalidation and navigation.
// Run CHROME_BIN=/path/to/chrome node scripts/tests/web_revalidation_browser.mjs
import assert from 'node:assert/strict';
import { readFileSync, existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'generated browser runtime exists');
const chrome = process.env.CHROME_BIN ||
  ['/usr/bin/google-chrome', '/usr/bin/chromium-browser', '/usr/bin/chromium'].find(existsSync);
assert.ok(chrome, 'Chrome/Chromium required');

let saved = 0;
let posts = 0;
let refreshed = 0;
let nextGets = 0;
const compiled = {
  loaders: [{ id: 'summary', path: '/', target: '#summary', reads: ['tasks'] }],
  mutations: [{ path: '/', invalidates: ['tasks'], targets: ['#summary'] }]
};

const browserTest = [
  "document.addEventListener('DOMContentLoaded', function() {",
  "  document.querySelector('#save').requestSubmit();",
  "  let navigated = false;",
  "  const interval = setInterval(function() {",
  "    const result = document.querySelector('#smoke-result');",
  "    if (!result) { clearInterval(interval); return; }",
  "    if (!navigated && document.querySelector('#detail').textContent === 'Saved' &&",
  "        document.querySelector('#summary').textContent === 'Summary 1') {",
  "      navigated = true;",
  "      document.querySelector('#next-link').click();",
  "    }",
  "    if (navigated && location.pathname === '/next' &&",
  "        document.querySelector('#page-content').textContent === 'Next page body' &&",
  "        document.title === 'Next page' && window.__nulangRetained === 1) {",
  "      result.setAttribute('data-pass', 'true');",
  "      result.textContent = 'PASS: POST+GET and native navigation';",
  "      clearInterval(interval);",
  "    }",
  "  }, 40);",
  "  setTimeout(function() {",
  "    const result = document.querySelector('#smoke-result');",
  "    if (result && !result.hasAttribute('data-pass')) {",
  "      result.setAttribute('data-pass', 'false');",
  "      result.textContent = 'FAIL: timed out';",
  "    }",
  "    clearInterval(interval);",
  "  }, 4500);",
  "});"
].join('\n');

const page = () => [
  '<!doctype html><html><head><title>Original page</title></head><body>',
  '<div id="page-content">Original page body</div>',
  '<div id="detail">Old detail</div><div id="summary">Summary ' + saved + '</div>',
  '<form id="save" method="post" data-action="save" data-action-placement="server" data-nulang-target="#detail">',
  '<input name="message" value="test"><button type="submit">Save</button></form>',
  '<a id="next-link" href="/next" data-nulang-nav-target="#page-content">Next</a>',
  '<pre id="smoke-result">pending</pre>',
  '<script>window.__nulangRetained = 1;</script>',
  '<script>', match[1], '\nwindow.nulangCompiledData = ',
  JSON.stringify(compiled), ';</script>',
  '<script>', browserTest, '</script></body></html>'
].join('');

const server = createServer((req, res) => {
  res.setHeader('Content-Type', 'text/html; charset=utf-8');
  res.setHeader('Cache-Control', 'no-store');
  if (req.method === 'POST' && req.url === '/') {
    posts++;
    saved++;
    req.resume();
    res.end('<div id="detail">Saved</div>');
  } else if (req.method === 'GET' && req.url === '/') {
    if (posts) refreshed++;
    res.end(page());
  } else if (req.method === 'GET' && req.url === '/next') {
    nextGets++;
    res.end('<!doctype html><html><head><title>Next page</title></head>' +
      '<body><div id="page-content">Next page body</div></body></html>');
  } else {
    res.statusCode = 404;
    res.end('not found');
  }
});

const temp = mkdtempSync(join(tmpdir(), 'nulang-web-http-'));
try {
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  const args = [
    '--headless', '--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage',
    '--user-data-dir=' + join(temp, 'chrome-profile'), '--no-first-run',
    '--disable-background-networking', '--virtual-time-budget=6500',
    '--dump-dom', 'http://127.0.0.1:' + address.port + '/'
  ];
  const browser = spawn(chrome, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '';
  let stderr = '';
  browser.stdout.on('data', bytes => { stdout += bytes.toString(); });
  browser.stderr.on('data', bytes => { stderr += bytes.toString(); });
  const code = await new Promise((resolve, reject) => {
    const watchdog = setTimeout(() => {
      browser.kill('SIGKILL');
      reject(new Error('Chromium timed out: ' + stderr.slice(-800)));
    }, 18000);
    browser.once('error', err => { clearTimeout(watchdog); reject(err); });
    browser.once('close', code => { clearTimeout(watchdog); resolve(code); });
  });
  assert.equal(code, 0, 'Chromium failed: ' + stderr.slice(-1200));
  assert.match(stdout, /<pre id="smoke-result" data-pass="true">PASS:/,
    'Browser failure: ' +
    stdout.match(/<pre id="smoke-result"[^>]*>.*?<\/pre>/s)?.[0] +
    '; stderr: ' + stderr.slice(-800));
  assert.equal(posts, 1, 'exactly one POST');
  assert.ok(refreshed >= 1, 'canonical GET used for revalidation');
  assert.ok(nextGets >= 1, 'Navigation API fetched destination');
  console.log('Nulang Web: real Chromium HTTP POST revalidation and native navigation passed');
} finally {
  server.close();
  rmSync(temp, { recursive: true, force: true });
}
