import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

function hydrateTestRuntime() {
  const rust = readFileSync(new URL('../../../src/web/reactivity.rs', import.meta.url), 'utf8');
  const match = rust.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"([\s\S]*?)"#/);
  assert.ok(match, 'Nulang client runtime string must remain extractable');
  const span = { dataset: { signal: 'pointCount' }, textContent: '10000' };
  const document = {
    readyState: 'complete',
    querySelectorAll: (selector) => selector === '[data-signal]' ? [span] : []
  };
  const window = {};
  runInNewContext(match[1], { window, document, console, globalThis: {}, URLSearchParams, FormData });
  return { signal: window.nulang.signal('pointCount'), span };
}

test('client signal subscribers receive changes and can unsubscribe', () => {
  const { signal, span } = hydrateTestRuntime();
  assert.equal(signal.get(), '10000');
  const received = [];
  const unsubscribe = signal.subscribe(value => received.push(value));
  signal.set(5000);
  assert.equal(span.textContent, 5000);
  assert.deepEqual(received, [5000]);
  unsubscribe();
  signal.set(2500);
  assert.deepEqual(received, [5000]);
  assert.equal(signal.get(), 2500);
});

test('unsubscribe is safe to call twice, subscriptions stay isolated', () => {
  const { signal } = hydrateTestRuntime();
  const first = [];
  const second = [];
  const unsubscribe = signal.subscribe(v => first.push(v));
  signal.subscribe(v => second.push(v));
  unsubscribe();
  unsubscribe();
  signal.set(1250);
  assert.deepEqual(first, []);
  assert.deepEqual(second, [1250]);
});
