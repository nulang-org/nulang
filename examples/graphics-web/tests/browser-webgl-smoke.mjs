// Strict real-browser WebGL 2 smoke test for compiler-produced Nulang output.
// Requires: nula build --web, playwright-core, and a local Chromium installation.
import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = resolve(fileURLToPath(new URL('../dist/', import.meta.url)));
const requiredAssets = new Map([
  ['/index.html', 'text/html; charset=utf-8'],
  ['/app.client.js', 'text/javascript; charset=utf-8'],
  ['/demo.js', 'text/javascript; charset=utf-8'],
  ['/graphics.js', 'text/javascript; charset=utf-8'],
  ['/style.css', 'text/css; charset=utf-8'],
]);

async function run() {
  for (const asset of requiredAssets.keys()) {
    const s = await stat(join(dist, asset.slice(1)));
    if (!s.isFile()) throw new Error('Expected built asset: ' + asset);
  }
  let chromium;
  try {
    ({ chromium } = await import('playwright-core'));
  } catch {
    throw new Error('Install optional browser dependency: npm install --no-save --package-lock=false playwright-core');
  }
  const browserPath = process.env.CHROMIUM_EXECUTABLE;
  if (!browserPath) throw new Error('Set CHROMIUM_EXECUTABLE to your local Chromium binary');

  const server = createServer(async (request, response) => {
    const pathname = new URL(request.url || '/', 'http://127.0.0.1').pathname;
    const asset = pathname === '/' ? '/index.html' : pathname;
    // Fixed allowlist: never expose build metadata or arbitrary filesystem files.
    if (!requiredAssets.has(asset)) { response.writeHead(404).end(); return; }
    try {
      const bytes = await readFile(join(dist, asset.slice(1)));
      response.writeHead(200, { 'Content-Type': requiredAssets.get(asset), 'Cache-Control': 'no-store' });
      response.end(bytes);
    } catch { response.writeHead(500).end(); }
  });
  let browser;
  try {
    await new Promise((ok, fail) => { server.once('error', fail); server.listen(0, '127.0.0.1', ok); });
    browser = await chromium.launch({ executablePath: browserPath, headless: true });
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', error => errors.push(String(error)));
    page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
    await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'networkidle' });
    await page.waitForFunction(() => document.querySelector('#backend')?.textContent !== 'Initializing renderer');
    const status = await page.locator('#backend').innerText();
    if (status !== 'Rendering with webgl2') {
      throw new Error('WEBGL2 NOT VERIFIED: ' + status + ' (fallback is not a passing result)');
    }
    for (const count of ['5000', '10000']) {
      await page.locator('#density').fill(count);
      await page.waitForFunction(expected => document.querySelector('#point-count')?.textContent === expected, count);
    }
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => resolve())));
    const state = await page.locator('#plot').evaluate(canvas => {
      const gl = canvas.getContext('webgl2');
      return { width: canvas.width, height: canvas.height,
        contextLost: gl?.isContextLost(), error: gl?.getError(), noError: gl?.NO_ERROR };
    });
    if (!state.width || !state.height || state.contextLost || state.error !== state.noError) {
      throw new Error('WebGL drawing failed: ' + JSON.stringify(state));
    }
    if (errors.length) throw new Error('Browser errors: ' + errors.join(' | '));
    console.log(JSON.stringify({ result: 'pass', backend: 'webgl2', status, width: state.width,
      height: state.height, points: 10000, browserErrors: 0 }));
  } finally {
    await browser?.close();
    if (server.listening) await new Promise((ok, fail) => server.close(e => e ? fail(e) : ok()));
  }
}

run().catch(error => { console.error(error.message); process.exitCode = 1; });
