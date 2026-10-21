import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const here = fileURLToPath(new URL('..', import.meta.url));
const vendor = resolve(here, 'node_modules/maplibre-gl/dist/maplibre-gl.js');
const host = resolve(here, 'index.mjs');
const html = `<!doctype html><meta charset="utf-8"><style>
body{margin:0}#map{width:640px;height:480px}
</style><div id="map"></div><script src="/maplibre.js"></script>
<script type="module">
import { createMapHost } from '/host.mjs';
window.__mounts = 0;
const original = window.maplibregl;
const injected = {...original, Map: class extends original.Map {
  constructor(options) { super(options); window.__mounts += 1; window.__map = this; }
}};
window.__host = createMapHost({ maplibregl: injected, container: document.querySelector('#map'),
  style: {version: 8, sources: {}, layers: []}, center: [-47.9, -15.8], zoom: 8 });
window.__host.dispatch({type:'source.upsert', id:'sites', source:{type:'geojson', data:{
  type:'FeatureCollection', features:[{type:'Feature', id:7, properties:{},
    geometry:{type:'Point', coordinates:[-47.9,-15.8]}}]}}});
window.__host.dispatch({type:'layer.add', layer:{id:'pins',type:'circle',source:'sites',
  paint:{'circle-color':'#f43f5e','circle-radius':7}}});
</script>`;

const server = createServer((req, res) => {
  if (req.url === '/') {res.writeHead(200, {'Content-Type':'text/html'}); res.end(html); return;}
  if (req.url === '/host.mjs' || req.url === '/maplibre.js') {
    res.writeHead(200, {'Content-Type': req.url.endsWith('.mjs') ? 'text/javascript' : 'application/javascript'});
    res.end(readFileSync(req.url === '/host.mjs' ? host : vendor)); return;
  }
  res.writeHead(404); res.end();
});
await new Promise((ok,fail) => { server.once('error', fail); server.listen(0,'127.0.0.1',ok); });
let browser;
try {
  browser = await chromium.launch({headless:true,args:['--use-gl=angle','--use-angle=swiftshader','--enable-webgl','--ignore-gpu-blocklist']});
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  const port = server.address().port;
  await page.goto(`http://127.0.0.1:${port}/`, {waitUntil:'load'});
  await page.waitForFunction(() => window.__map?.getLayer('pins') && window.__map?.getSource('sites'), {timeout:20000});
  assert.equal(await page.evaluate(() => window.__mounts), 1);

  await page.evaluate(() => {
    window.__host.dispatch({type:'feature-state.set',target:{source:'sites',id:7},state:{selected:true}});
    window.__host.dispatch({type:'source.upsert',id:'sites',source:{type:'geojson',data:{type:'FeatureCollection',features:[]}}});
  });
  await page.evaluate(() => window.__map.setStyle({version:8,sources:{},layers:[]}));
  await page.waitForFunction(() => window.__map?.getLayer('pins') && window.__map?.getSource('sites'), {timeout:20000});
  assert.equal(await page.evaluate(() => window.__mounts), 1);
  await page.evaluate(() => { window.__host.destroy(); window.__host.destroy(); });
  assert.equal(await page.locator('.maplibregl-canvas').count(), 0);
  assert.deepEqual(errors, []);
  console.log('MapLibre Chromium smoke: OK');
} finally {
  await browser?.close();
  await new Promise(ok => server.close(ok));
}
