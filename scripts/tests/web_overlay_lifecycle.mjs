// Tests browser overlay lifecycle: isolated fragment, typed result, cancellation.
// Run: node scripts/tests/web_overlay_lifecycle.mjs
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';
const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'client runtime exists');
const listeners = new Map();
const formActions = new Map();
const errors = [];
const requests = [];
const opener = {isConnected: true, focus() {document.activeElement = opener;}};
const dialog = {
  tagName: 'DIALOG', id: 'editor', textContent: 'Empty', isConnected: true,
  open: false,
  replaceWith(next) {this.textContent = next.textContent;},
  showModal() {this.open = true; document.activeElement = this;},
  close() {this.open = false;},
  addEventListener(name, callback) {listeners.set(name, callback);},
  removeEventListener(name, callback) {if (listeners.get(name) === callback) listeners.delete(name);},
  contains(other) {return other === this;}
};
const form = {
  tagName:'FORM',
  dataset:{action:'save', actionPlacement:'server', nulangTarget:'#outside'},
  closest(kind) {return kind === 'form' ? this : (kind === 'dialog' ? dialog : null);},
  addEventListener(name, callback) {formActions.set(name, callback);}
};
const outside = {id:'outside'};
const document = {
  readyState: 'complete', activeElement: opener,
  querySelector(selector) {return selector === '#editor' ? dialog :
    (selector === '#outside' ? outside : null);},
  querySelectorAll(selector) {return selector === '[data-action]' ? [form] : [];},
  importNode(node) {return {textContent: node.textContent};}
};
class DOMParser {
  parseFromString(html) {
    return {querySelector(selector) {
      const match = /<dialog id="editor">([^<]*)<\/dialog>/.exec(html);
      return selector === '#editor' && match ? {tagName:'DIALOG',textContent:match[1]} : null;
    }};
  }
}
const response = text => ({
  ok:true, redirected:false, status:200,
  headers:{get(){return 'text/html'}},
  text:async()=>'<dialog id="editor">'+text+'</dialog>'
});
const fetch = (url, options) => new Promise(resolve => requests.push({url,options,resolve}));
class FormData { forEach() {} }
const window = {location:{href:'https://local.test/tasks', origin:'https://local.test'}};
runInNewContext(match[1], {window, document, DOMParser, URL, URLSearchParams, FormData, fetch, AbortController,
  console:{error(...parts){errors.push(parts.join(' '));}, warn(){}}});
assert.equal(typeof window.nulang.overlay?.open, 'function', 'overlay API is installed');
assert.equal(typeof window.nulang.overlay?.complete, 'function');
assert.equal(typeof window.nulang.overlay?.cancel, 'function');
assert.throws(() => window.nulang.overlay.open('body > div','/edit'), /target/i);
assert.throws(() => window.nulang.overlay.open('#editor','https://evil.test/edit'), /origin/i);
assert.equal(requests.length,0, 'invalid URLs cannot fetch');

const completed = window.nulang.overlay.open('#editor','/edit');
assert.equal(requests.length,1);
assert.equal(requests[0].url,'https://local.test/edit');
assert.equal(requests[0].options.method,'GET');
assert.equal(requests[0].options.cache,'no-store');
assert.throws(()=>window.nulang.overlay.open('#editor','/again'),/active/i,'duplicate open rejected');
requests[0].resolve(response('Loaded editor'));
await new Promise(resolve=>setImmediate(resolve));
assert.equal(dialog.open,true);
assert.equal(dialog.textContent,'Loaded editor');
formActions.get('submit')({preventDefault(){}});
await new Promise(resolve=>setImmediate(resolve));
assert.equal(requests.length,1,'overlay action cannot reach a sibling DOM target');
assert.equal(errors.length,1,'cross-scope action rejected');
assert.match(errors[0],/overlay.*scope/i);
form.dataset.nulangTarget = '#editor';
form.dataset.nulangRevalidate = '#outside';
formActions.get('submit')({preventDefault(){}});
await new Promise(resolve=>setImmediate(resolve));
assert.equal(requests.length,1,'overlay revalidation cannot reach a sibling DOM target');
assert.equal(errors.length,2);
assert.match(errors[1],/overlay.*scope/i);
delete form.dataset.nulangRevalidate;
assert.throws(()=>window.nulang.overlay.complete('#editor',{type:'string',value:99}),/invalid/i);
assert.equal(dialog.open,true,'invalid result cannot close overlay');
assert.equal(window.nulang.overlay.complete('#editor',{type:'string',value:'saved'}),true);
const outcome = await completed;
assert.equal(outcome.protocol,'nulang-ui-msg/1');
assert.equal(outcome.overlay_id,'#editor');
assert.equal(outcome.status,'completed');
assert.equal(outcome.value.type,'string');
assert.equal(outcome.value.value,'saved');
assert.equal(dialog.open,false);
assert.equal(document.activeElement,opener,'focus restored after result');
assert.equal(listeners.size,0,'close handlers removed');
assert.equal(window.nulang.overlay.complete('#editor',{type:'null'}),false,'closed scope unavailable');

// Cancel before a GET completes: no stale fragment or focus changes.
const pending = window.nulang.overlay.open('#editor','/slow');
pending.catch(()=>{});
assert.equal(requests.length,2);
assert.equal(window.nulang.overlay.cancel('#editor'),true);
assert.equal(requests[1].options.signal.aborted,true);
await assert.rejects(pending,/cancel/i);
requests[1].resolve(response('Late response'));
await new Promise(resolve=>setImmediate(resolve));
assert.equal(dialog.open,false);
assert.equal(dialog.textContent,'Loaded editor','canceled load cannot mutate DOM');

const navigated = window.nulang.overlay.open('#editor','/navigated');
navigated.catch(()=>{});
window.location.href = 'https://local.test/other';
requests[2].resolve(response('Navigation stale'));
const navigationOutcome = await Promise.race([
  navigated.then(()=>'completed',()=> 'rejected'),
  new Promise(resolve=>setTimeout(()=>resolve('pending'),40))
]);
assert.equal(navigationOutcome, 'rejected','navigation must settle the pending overlay');
window.location.href = 'https://local.test/tasks';
assert.equal(dialog.open,false);

const native = window.nulang.overlay.open('#editor','/native');
native.catch(()=>{});
requests[3].resolve(response('Native close'));
await new Promise(resolve=>setImmediate(resolve));
assert.equal(dialog.open,true);
listeners.get('cancel')?.({preventDefault(){}});
await assert.rejects(native,/cancel/i);
assert.equal(dialog.open,false);
assert.equal(listeners.size,0);
assert.equal(document.activeElement,opener);

// A native method="dialog" form fires close without cancel; it must also
// settle and release ownership rather than retaining an unresolved promise.
const nativeClose = window.nulang.overlay.open('#editor','/method-dialog');
nativeClose.catch(()=>{});
requests[4].resolve(response('Method dialog'));
await new Promise(resolve=>setImmediate(resolve));
assert.equal(dialog.open,true);
dialog.close();
listeners.get('close')?.({});
const closeStatus = await Promise.race([
  nativeClose.then(()=>'completed',()=> 'rejected'),
  new Promise(resolve=>setTimeout(()=>resolve('pending'),40))
]);
assert.equal(closeStatus,'rejected','native close must cancel and settle overlay');
assert.equal(listeners.size,0);
assert.equal(document.activeElement,opener);
console.log('Nulang Web: scoped overlay load, typed result, cancellation and cleanup passed');
