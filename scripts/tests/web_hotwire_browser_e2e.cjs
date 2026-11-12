// Test-only browser acceptance: actual guarded Nulang WebDevServer + official Turbo.
// Run: cargo build --locked --no-default-features --example web_hotwire_browser_test_server
//      npm ci --prefix scripts/tests/hotwire-browser
//      npx --prefix scripts/tests/hotwire-browser playwright install chromium
//      node scripts/tests/web_hotwire_browser_e2e.cjs
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {spawn, execFileSync} = require('node:child_process');
const {request: httpRequest} = require('node:http');
const assert = require('node:assert/strict');
const rootDir = path.resolve(__dirname, '../..');
const playwrightPath = process.env.NULANG_PLAYWRIGHT_MODULE ||
  path.join(__dirname, 'hotwire-browser/node_modules/playwright');
const {chromium} = require(playwrightPath);
const turboBundle = process.env.NULANG_TURBO_UMD ||
  path.join(__dirname, 'hotwire-browser/node_modules/@hotwired/turbo/dist/turbo.es2017-umd.js');

const targetDirectory = JSON.parse(execFileSync('cargo', ['metadata','--format-version','1','--no-deps'],
  {cwd:rootDir, encoding:'utf8', stdio:['ignore','pipe','pipe']})).target_directory;
const binary = process.env.NULANG_HOTWIRE_SERVER_BIN ||
  path.join(targetDirectory, 'debug/examples/web_hotwire_browser_test_server');
const root = fs.mkdtempSync(path.join(os.tmpdir(),'nulang-hotwire-audited-'));
fs.copyFileSync(turboBundle,path.join(root,'turbo.js'));
fs.writeFileSync(path.join(root,'index.html'), `<!doctype html><html><head>
  <meta charset="utf-8"><script defer src="/turbo.js"></script></head><body>
  <h1 id="heading">Actual Nulang WebDevServer</h1>
  <div id="todos"><li id="todo-1">Initial</li></div>
  <form id="create-todo" method="POST" action="/todos"><button id="create" type="submit">Create</button></form>
  <form id="edit-todo" method="POST" action="/todos/2/edit"><button id="edit" type="submit">Edit</button></form>
  <form id="delete-todo" method="POST" action="/todos/2/delete"><button id="delete" type="submit">Delete</button></form>
  <a id="fallback" href="/recent">Recent (no JS)</a>
  <turbo-frame id="recent" src="/recent">Loading...</turbo-frame>
  </body></html>`);

async function main(){
 let stderr='';
 const env={...process.env};
 delete env.NULANG_WEB_PUBLIC_ORIGIN;
 const server=spawn(binary,[root],{env,stdio:['ignore','pipe','pipe']});
 server.stderr.on('data',chunk => stderr+=chunk.toString());
 let browser;
 try{
  const port=await new Promise((resolve,reject)=>{
    let printed='';
    const timer=setTimeout(()=>reject(new Error('server port timed out: '+stderr)),10000);
    server.stdout.on('data',chunk=>{
      printed+=chunk.toString();
      const match=printed.match(/PORT=(\d+)/);
      if(match){clearTimeout(timer);resolve(Number(match[1]));}
    });
    server.on('exit',code=>{clearTimeout(timer);reject(new Error('server exited '+code+' '+stderr));});
  });
  const base='http://127.0.0.1:'+port;
  const rejection=(origin, extra={})=>new Promise((resolve,reject)=>{
    const req=httpRequest(base+'/todos',{method:'POST',headers:{Host:'127.0.0.1:'+port,'Accept':'text/vnd.turbo-stream.html','Content-Length':'0',...extra,...(origin?{Origin:origin}:{})}},res=>{
      let body='';res.on('data',c=>body+=c);res.on('end',()=>resolve({status:res.statusCode,body,contentType:res.headers['content-type']}));
    });
    req.on('error',reject);req.end();
  });
  for (const [origin, headers] of [
    [null,{}],['https://attacker.example',{}],
    [base,{'Sec-Fetch-Site':'cross-site'}],
    [base,{'Cookie':'nulang_session='+'a'.repeat(64)}]
  ]){
   const response=await rejection(origin,headers);
   assert.equal(response.status,403,JSON.stringify(response));
   assert.ok(!response.body.includes('turbo-stream'));
   assert.ok(!String(response.contentType).includes('turbo-stream'));
  }
  console.log('PASS: guarded WebDevServer rejects missing/foreign/cross-site origin and forged session');

  browser=await chromium.launch({headless:true});
  const context=await browser.newContext();
  const page=await context.newPage();
  const errors=[], posts=[];
  page.on('pageerror',e=>errors.push(e.message));
  page.on('response',response=>{
    if(new URL(response.url()).pathname.startsWith('/todos')) posts.push({status:response.status(),type:response.headers()['content-type']});
  });
  await page.goto(base+'/',{waitUntil:'domcontentloaded'});
  await page.locator('turbo-frame#recent').locator('text=Tea & toast').waitFor({timeout:10000});
  console.log('PASS: official Turbo 8 loads real Nulang Frame over HTTP');
  await page.locator('#create').click();
  await page.locator('#todo-2').waitFor({timeout:10000});
  assert.equal(await page.locator('#todo-2').textContent(),'New <script>window.__pwned=true</script>');
  assert.equal(await page.evaluate(()=>Boolean(window.__pwned)),false);
  await page.locator('#edit').click();
  await page.waitForFunction(()=>document.querySelector('#todo-2')?.textContent==='Updated');
  await page.locator('#delete').click();
  await page.locator('#todo-2').waitFor({state:'detached',timeout:10000});
  assert.equal(posts.length,3,'all three form requests should reach actual host');
  posts.forEach(post=>{assert.equal(post.status,200,JSON.stringify(posts));assert.match(post.type,/^text\/vnd\.turbo-stream\.html/);});
  assert.deepEqual(errors,[]);
  console.log('PASS: Turbo POST create/update/delete admitted by exact Host+Origin with true Turbo MIME');
  console.log('PASS: Nulang escaped injected script; no browser JS errors');

  const nojs=await browser.newContext({javaScriptEnabled:false});
  const fallback=await nojs.newPage();
  await fallback.goto(base+'/');
  await fallback.locator('#fallback').click();
  await fallback.waitForURL('**/recent');
  assert.match(await fallback.locator('body').textContent(),/Tea & toast/);
  console.log('PASS: ordinary fallback navigation with JavaScript disabled');
  await nojs.close();await context.close();
 } finally {
  await browser?.close();
  server.kill('SIGTERM');
  fs.rmSync(root,{recursive:true,force:true});
 }
}
main().catch(error=>{console.error(error);process.exitCode=1});
