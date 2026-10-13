// Dependency-free Chromium DevTools smoke test for the built Astro homepage.
// CI uses the Chrome installation on ubuntu-latest and Node.js 22+.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { spawn, spawnSync } from 'node:child_process';
import { mkdtemp, readFile, rm, mkdir, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve, join, extname } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';

const root = resolve('dist');
const artifacts = resolve('browser-artifacts');
assert.ok(existsSync(join(root, 'index.html')), 'Build docs/ before running the browser smoke test');
const samples = [
  ['hello', 'Hello, Nulang!\nHello, World!\nThe answer is 42'],
  ['recursion', 'fib(10) = 55\n5! = 120'],
  ['closures', 'add10(32) = 42\nBlock result: 30'],
];
const mime = { '.html':'text/html', '.js':'text/javascript', '.css':'text/css',
  '.wasm':'application/wasm', '.svg':'image/svg+xml', '.png':'image/png' };
function findChrome() {
  for (const name of [process.env.CHROME_BIN, 'google-chrome', 'google-chrome-stable',
    'chromium', 'chromium-browser'].filter(Boolean)) {
    const binary = name.includes('/') ? name : spawnSync('which', [name], { encoding:'utf8' }).stdout?.trim();
    if (binary && existsSync(binary)) return binary;
  }
  throw new Error('Google Chrome or Chromium must be installed for browser smoke tests');
}
const server = createServer(async (req, res) => {
  try {
    const pathname = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
    const file = resolve(root, '.' + pathname, pathname.endsWith('/') ? 'index.html' : '');
    if (!file.startsWith(root + '/')) throw new Error('File outside docs/dist');
    const bytes = await readFile(file);
    res.writeHead(200, { 'content-type': mime[extname(file)] || 'application/octet-stream' });
    res.end(bytes);
  } catch { res.writeHead(404); res.end('Not found'); }
});
await new Promise(ok => server.listen(0, '127.0.0.1', ok));
const url = 'http://127.0.0.1:' + server.address().port + '/';
const profile = await mkdtemp(join(tmpdir(), 'nulang-browser-'));
let chrome;
let socket;
try {
  chrome = spawn(findChrome(), ['--headless=new', '--disable-gpu', '--disable-dev-shm-usage',
    '--no-first-run', '--no-default-browser-check',
    ...(process.getuid?.() === 0 ? ['--no-sandbox'] : []),
    '--user-data-dir=' + profile, '--remote-debugging-port=0', 'about:blank'], { stdio:'ignore' });
  let port;
  for (let i=0; i<100; i++) {
    try {
      port = (await readFile(join(profile, 'DevToolsActivePort'), 'utf8')).split('\n')[0];
      break;
    } catch {
      if (chrome.exitCode !== null) throw new Error('Chrome exited before startup');
      await sleep(150);
    }
  }
  assert.ok(port, 'Chrome did not open a DevTools port');
  const tabs = await (await fetch('http://127.0.0.1:' + port + '/json/list')).json();
  const page = tabs.find(tab => tab.type === 'page');
  assert.ok(page?.webSocketDebuggerUrl, 'Chrome did not expose a page debugger');
  socket = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((ok, fail) => {
    socket.addEventListener('open', ok, { once:true });
    socket.addEventListener('error', fail, { once:true });
  });
  const pending = new Map();
  const exceptions = [];
  let nextId = 0;
  socket.addEventListener('message', event => {
    const msg = JSON.parse(event.data);
    if (msg.method === 'Runtime.exceptionThrown') exceptions.push(msg.params.exceptionDetails.text);
    const request = pending.get(msg.id);
    if (!request) return;
    pending.delete(msg.id);
    if (msg.error) request.fail(new Error(request.method + ': ' + msg.error.message));
    else request.ok(msg.result);
  });
  function command(method, params = {}) {
    return new Promise((ok, fail) => {
      const id = ++nextId;
      pending.set(id, { ok, fail, method });
      socket.send(JSON.stringify({ id, method, params }));
    });
  }
  async function evaluate(expression) {
    const result = await command('Runtime.evaluate', {
      expression, returnByValue:true, awaitPromise:true,
    });
    if (result.exceptionDetails) throw new Error(result.exceptionDetails.text);
    return result.result.value;
  }
  async function waitFor(expression, description, timeout = 45000) {
    const deadline = Date.now() + timeout;
    while (Date.now() < deadline) {
      if (await evaluate(expression)) return;
      await sleep(150);
    }
    const diagnostics = await evaluate('({url:location.href, output:document.querySelector(".hero-output")?.textContent})');
    throw new Error('Timed out ' + description + ': ' + JSON.stringify(diagnostics));
  }
  async function screenshot(filename) {
    await mkdir(artifacts, { recursive:true });
    const capture = await command('Page.captureScreenshot', { format:'png', captureBeyondViewport:false });
    await writeFile(join(artifacts, filename), Buffer.from(capture.data, 'base64'));
  }
  await command('Page.enable');
  await command('Runtime.enable');
  const navigation = await command('Page.navigate', { url });
  assert.ok(!navigation.errorText, 'Browser navigation failed: ' + navigation.errorText);
  await waitFor('document.readyState === "complete" && !!document.querySelector("[data-hero-playground]")',
    'for homepage to load');
  assert.equal(await evaluate('document.querySelectorAll("[data-hero-example]").length'), 3);
  assert.equal(await evaluate('!!document.querySelector("label[for=hero-source]")'), true);
  assert.equal(await evaluate('document.querySelector(".hero-output").getAttribute("aria-live")'), 'polite');
  assert.equal(await evaluate('document.querySelector(".hero-core-note").textContent.includes("native runtime")'), true);

  for (let i=0; i<samples.length; i++) {
    await evaluate('document.querySelectorAll("[data-hero-example]")[' + i +
      '].click(); document.querySelector(".hero-run").click()');
    await waitFor('document.querySelector(".hero-output").textContent.includes(' +
      JSON.stringify(samples[i][1]) + ')', 'executing ' + samples[i][0]);
    assert.equal(await evaluate('document.querySelectorAll("[data-hero-example]")[' +
      i + '].getAttribute("aria-pressed")'), 'true');
    console.log('PASS: browser CoreVM ' + samples[i][0]);
  }

  await evaluate('document.querySelectorAll("[data-hero-example]")[0].click(); ' +
    'document.querySelector("#hero-source").focus()');
  await command('Input.dispatchKeyEvent', {
    type:'keyDown', key:'Enter', code:'Enter', windowsVirtualKeyCode:13, modifiers:2,
  });
  await command('Input.dispatchKeyEvent', {
    type:'keyUp', key:'Enter', code:'Enter', windowsVirtualKeyCode:13,
  });
  await waitFor('document.querySelector(".hero-output").textContent.includes(' +
    JSON.stringify(samples[0][1]) + ')', 'executing with Ctrl+Enter');
  console.log('PASS: keyboard shortcut');
  await screenshot('homepage-desktop.png');

  await command('Emulation.setDeviceMetricsOverride', {
    width:390, height:844, deviceScaleFactor:1, mobile:true,
  });
  await sleep(350);
  const mobile = await evaluate('(() => {' +
    'const targets=[...document.querySelectorAll(".hero-example, .hero-run")];' +
    'return {width:window.innerWidth,overflow:document.documentElement.scrollWidth-window.innerWidth,' +
    'boxes:targets.map(b=>({name:b.textContent.trim(),height:b.getBoundingClientRect().height}))};' +
    '})()');
  assert.equal(mobile.width, 390, 'mobile viewport should be 390 CSS pixels');
  assert.ok(mobile.overflow <= 2, 'mobile horizontal overflow: ' + JSON.stringify(mobile));
  assert.ok(mobile.boxes.every(b => b.height >= 44), 'touch target below 44px: ' + JSON.stringify(mobile.boxes));
  await screenshot('homepage-mobile.png');
  assert.deepEqual(exceptions, [], 'uncaught browser errors: ' + exceptions.join('; '));
  console.log('PASS: mobile layout, hit targets and browser exceptions');
} finally {
  socket?.close();
  if (chrome) {
    const exited = chrome.exitCode !== null;
    if (!exited) {
      const completion = new Promise(ok => chrome.once('exit', ok));
      chrome.kill('SIGTERM');
      await Promise.race([completion, sleep(2500)]);
    }
  }
  await rm(profile, { recursive:true, force:true, maxRetries:5, retryDelay:100 });
  await new Promise(ok => server.close(ok));
}
