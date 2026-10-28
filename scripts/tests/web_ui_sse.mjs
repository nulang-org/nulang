// Typed SSE browser host regression, no runtime dependencies.
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"(.*?)"#\s*\.to_string\(\)/s);
assert.ok(match);

const nodes = new Map();
let status = { textContent: 'initial', replaceWith(next) { status = next; nodes.set('#status', next); } };
nodes.set('#status', status);
const document = {
  readyState: 'complete',
  querySelector(selector) { return nodes.get(selector) || null; },
  querySelectorAll() { return []; },
  importNode(next) { return {...next, replaceWith(node) {status = node; nodes.set('#status',node);}}; }
};
class DOMParser {
  parseFromString(html) {
    return {
      querySelector(selector) {
        const value = /<div id="status">([^<]*)<\/div>/.exec(html);
        return selector === '#status' && value ? {textContent: value[1]} : null;
      }
    };
  }
}
const instances = [];
class EventSource {
  constructor(url) { this.url = url; this.handlers = new Map(); this.closed = false; instances.push(this); }
  addEventListener(event, cb) { this.handlers.set(event, cb); }
  emit(name, data) { this.handlers.get(name)?.({data: JSON.stringify(data)}); }
  close() { this.closed = true; }
}
const window = {EventSource, location: {href:'http://localhost/'}};
runInNewContext(match[1], {window, document, DOMParser, URLSearchParams,
  console: {error(){},warn(){}}
});
assert.equal(typeof window.nulang.stream, 'function');
const handle = window.nulang.stream('room-1');
assert.equal(instances[0].url, '/__nulang/ui-sse/room-1');

const protocol = 'nulang-ui-html/1';
function snapshot(revision, value) {
  return {type:'snapshot',protocol,document_id:'room-1',revision,
    elements:[{target:'#status',html:'<div id="status">'+value+'</div>'}]};
}
function patch(base_revision,revision,value) {
  return {type:'patch',protocol,document_id:'room-1',base_revision,revision,
    element:{target:'#status',html:'<div id="status">'+value+'</div>'}};
}
instances[0].emit('nulang-ui-snapshot',snapshot('3','three'));
assert.equal(status.textContent,'three');
instances[0].emit('nulang-ui-patch',patch('3','4','four'));
assert.equal(status.textContent,'four');
instances[0].emit('nulang-ui-patch',patch('3','4','stale'));
assert.equal(status.textContent,'four','duplicate revision ignored');
instances[0].emit('nulang-ui-patch',patch('5','6','gap'));
assert.equal(status.textContent,'four','gap never applies mutation');
assert.equal(instances[0].closed,true,'gap closes stale stream');
assert.equal(instances.length,2,'gap triggers new snapshot subscription');
instances[1].emit('nulang-ui-snapshot',snapshot('6','resynced'));
assert.equal(status.textContent,'resynced');
instances[1].emit('nulang-ui-patch',patch('6','7','seven'));
assert.equal(status.textContent,'seven');
instances[1].emit('nulang-ui-patch',{...patch('7','8','spoof'),document_id:'other-room'});
assert.equal(status.textContent,'seven','reject cross-room event');
handle.close();
assert.equal(instances[1].closed,true);
console.log('Nulang Web: SSE revisions, duplicate handling, gap resnapshot, and cancellation pass');
