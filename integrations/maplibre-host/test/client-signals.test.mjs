import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

// Test the JS emitted by the Rust compiler, not a second implementation.
test('generated Nulang browser runtime notifies and disposes signal subscribers', () => {
  const source = readFileSync(new URL('../../../src/web/reactivity.rs', import.meta.url), 'utf8');
  const entry = source.indexOf('pub fn generate_client_runtime() -> String {');
  assert.ok(entry > -1, 'missing compiler generator');
  const script = source.slice(entry).match(/r#"([\s\S]*?)"#\s*\.to_string\(\)/)?.[1];
  assert.ok(script, 'missing generated JavaScript');

  const badge = { dataset: { signal: 'showSites' }, textContent: 'false' };
  const window = {};
  runInNewContext(script, {
    window,
    document: {
      readyState: 'complete',
      querySelectorAll: selector => selector === '[data-signal]' ? [badge] : [],
    },
  });

  const signal = window.nulang.signal('showSites');
  assert.equal(signal.get(), 'false');
  const updates = [];
  const unsubscribe = signal.subscribe(value => updates.push(value));
  signal.set('true');
  signal.set('true'); // identical value must not notify twice
  unsubscribe();
  signal.set('false');
  assert.deepEqual(updates, ['true']);
  assert.equal(badge.textContent, 'false');
});
