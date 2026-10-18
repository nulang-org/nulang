// Test explicit nested signal batches without changing immediate set() semantics.
// Runs the exact Rust-emitted browser runtime using a minimal DOM.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const rust = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = rust.slice(rust.indexOf('pub fn generate_client_runtime() -> String {'))
  .match(/r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'generated runtime exists');

let writes = 0;
let value = '0';
const text = {
  dataset: { signal: 'count' },
  get textContent() { return value; },
  set textContent(next) { value = String(next); writes++; },
};
const document = {
  readyState: 'complete',
  querySelectorAll(selector) {
    if (selector === '[data-signal]') return [text];
    if (selector === '[data-action]' || selector === '[data-nulang-attrs]') return [];
    throw new Error('unknown selector ' + selector);
  },
};
const window = {};
runInNewContext(match[1], { window, document, console, URLSearchParams });
const api = window.nulang;

assert.equal(api.signal('count').get(), '0');
api.signal('count').set('1');
assert.equal(value, '1', 'outside batch writes immediately');
assert.equal(writes, 1);
api.signal('count').set('1');
assert.equal(writes, 1, 'same-value writes must not touch DOM');
const result = api.batch(() => {
  api.signal('count').set('2');
  api.signal('count').set('3');
  api.batch(() => {
    api.signal('count').set('4');
    assert.equal(value, '1', 'nested batch does not flush early');
  });
  assert.equal(writes, 1, 'no intermediate DOM writes inside batch');
  return 'result';
});
assert.equal(result, 'result');
assert.equal(value, '4');
assert.equal(writes, 2, 'multiple changes collapse into one DOM write');

assert.throws(() => api.batch(() => {
  api.signal('count').set('5');
  throw new Error('expected');
}), /expected/);
assert.equal(value, '5', 'finalizer flushes even on exception');
assert.equal(writes, 3);
console.log('explicit signal batching regressions passed');
