// Real Chromium modal regression without npm dependencies.
// Runs a separate loopback-only child HTTP server; no network beyond localhost.
import assert from 'node:assert/strict';
import {readFileSync, mkdtempSync, writeFileSync, rmSync, existsSync} from 'node:fs';
import {spawn, spawnSync} from 'node:child_process';
import {tmpdir} from 'node:os';
import {join} from 'node:path';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'Nulang generated browser runtime');
const chrome = process.env.CHROME_BIN ||
  ['/usr/bin/google-chrome', '/usr/bin/chromium-browser', '/usr/bin/chromium'].find(existsSync);
assert.ok(chrome, 'Chromium required for native modal smoke test');
const temp = mkdtempSync(join(tmpdir(), 'nulang-overlay-browser-'));
let server;
try {
  const html = String.raw`<!doctype html><html><body>
<button id="opener" type="button">Edit</button>
<dialog id="editor" data-nulang-overlay=""></dialog>
<pre id="smoke-result">pending</pre>
<script>__NULANG_RUNTIME__</script>
<script>
(async function () {
  const flags = {};
  const opener = document.querySelector('#opener');
  const dialog = document.querySelector('#editor');
  const result = document.querySelector('#smoke-result');
  const wait = async condition => {
    for (let i = 0; i < 80; i++) {
      if (condition()) return true;
      await new Promise(resolve => setTimeout(resolve, 20));
    }
    return false;
  };
  try {
    await new Promise(resolve => document.readyState === 'loading'
      ? document.addEventListener('DOMContentLoaded', resolve, {once:true}) : resolve());
    opener.focus();
    const pending = window.nulang.overlay.open('#editor','/fragment.html');
    flags.opened = await wait(() => dialog.open);
    flags.nativeModal = dialog.matches(':modal');
    flags.fragment = dialog.querySelector('#inside')?.textContent === 'Rendered by server';
    flags.firstFocus = dialog.contains(document.activeElement);
    flags.complete = window.nulang.overlay.complete('#editor',{type:'string',value:'ok'});
    const outcome = await pending;
    flags.typed = outcome.protocol === 'nulang-ui-msg/1' &&
      outcome.overlay_id === '#editor' && outcome.value.type === 'string' &&
      outcome.value.value === 'ok' && outcome.status === 'completed';
    flags.closed = !dialog.open && !dialog.matches(':modal');
    flags.restored = document.activeElement === opener;
    const canceled = window.nulang.overlay.open('#editor','/fragment.html');
    canceled.catch(()=>{});
    flags.reopened = await wait(() => dialog.open);
    dialog.close();
    flags.nativeClose = await Promise.race([
      canceled.then(() => false, () => true),
      new Promise(resolve=>setTimeout(()=>resolve(false),500))
    ]);
    flags.releasedOwnership = window.nulang.overlay.cancel('#editor') === false;
    flags.closedAgain = !dialog.open;
    flags.focusAgain = document.activeElement === opener;
  } catch (e) {
    flags.error = String(e?.stack || e);
  }
  const failed = Object.keys(flags).filter(key => flags[key] !== true);
  result.dataset.pass = failed.length === 0 ? 'true' : 'false';
  result.textContent = JSON.stringify({flags,failed});
})();
</script></body></html>`;
  writeFileSync(join(temp,'index.html'), html.replace('__NULANG_RUNTIME__',match[1]));
  writeFileSync(join(temp,'fragment.html'), String.raw`<!doctype html><html><body>
<dialog id="editor" data-nulang-overlay=""><label for="field">Name</label>
<input id="field" autofocus value="Test"><span id="inside">Rendered by server</span></dialog>
</body></html>`);
  const service = String.raw`import http from 'node:http';
import {readFileSync, writeFileSync} from 'node:fs';
import {join} from 'node:path';
const dir = process.argv[2], ready = process.argv[3];
const srv = http.createServer((req,res) => {
  if (req.method !== 'GET' || !['/','/index.html','/fragment.html'].includes(req.url)) {
    res.writeHead(404);res.end();return;
  }
  res.setHeader('Content-Type','text/html; charset=utf-8');
  res.setHeader('Cache-Control','no-store');
  res.end(readFileSync(join(dir, req.url === '/fragment.html' ? 'fragment.html' : 'index.html')));
});
srv.listen(0,'127.0.0.1',()=>writeFileSync(ready,String(srv.address().port)));`;
  writeFileSync(join(temp,'server.mjs'),service);
  const ready = join(temp,'port.txt');
  server = spawn(process.execPath,[join(temp,'server.mjs'),temp,ready], {stdio:'ignore'});
  for (let i=0; i<100 && !existsSync(ready); i++)
    await new Promise(resolve=>setTimeout(resolve,20));
  assert.ok(existsSync(ready),'local fixture server failed to start');
  const port=Number(readFileSync(ready,'utf8'));
  const processResult = spawnSync(chrome,[
    '--headless','--no-sandbox','--disable-gpu','--disable-dev-shm-usage',
    '--no-first-run','--disable-background-networking',
    '--user-data-dir='+join(temp,'profile'),'--virtual-time-budget=3500',
    '--dump-dom','http://127.0.0.1:'+port+'/index.html'
  ],{encoding:'utf8',timeout:15000,maxBuffer:2*1024*1024});
  assert.equal(processResult.status,0,processResult.stderr?.slice(-1600));
  const marker=processResult.stdout.match(/<pre id="smoke-result"[^>]*>(.*?)<\/pre>/s);
  assert.ok(marker,'browser overlay marker missing: '+processResult.stdout.slice(-1000));
  assert.match(marker[0],/data-pass="true"/,'native dialog browser regression: '+marker[1]);
  console.log('Nulang Web: real Chromium overlay open, native modal, typed close, focus restoration and native close verified');
} finally {
  if (server) server.kill();
  rmSync(temp,{recursive:true,force:true});
}
