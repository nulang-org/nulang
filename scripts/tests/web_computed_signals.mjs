// Exercise the JavaScript emitted by the Rust Web compiler, without browser dependencies.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const rust = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const start = rust.indexOf('pub fn generate_client_runtime() -> String {');
assert.notEqual(start, -1);
const match = rust.slice(start).match(/r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'find the compiler-emitted browser runtime');

function mount(values) {
  const elements = values.map(([signal, content]) => ({
    dataset: { signal }, textContent: content,
  }));
  const document = {
    readyState: 'complete',
    querySelectorAll(selector) {
      if (selector === '[data-signal]') return elements;
      if (selector === '[data-action]') return [];
      throw new Error('unexpected selector: ' + selector);
    },
  };
  const window = {};
  runInNewContext(match[1], { window, document, console, URLSearchParams });
  return { api: window.nulang, elements };
}

{
  const { api, elements } = mount([['count', '2']]);
  const seen = [];
  const stop = api.signal('count').subscribe(value => seen.push(value));
  api.signal('count').set('2'); // no semantic change
  api.signal('count').set('3');
  assert.deepEqual(seen, ['3'], 'subscriptions only observe changes');
  assert.equal(elements[0].textContent, '3', 'existing DOM binding still updates');
  stop();
  stop(); // idempotent
  api.signal('count').set('4');
  assert.deepEqual(seen, ['3'], 'unsubscribe detaches listener');
  assert.equal(elements[0].textContent, '4');
}

{
  const { api } = mount([['price', '3'], ['count', '2']]);
  let evaluations = 0;
  const total = api.computed(['price', 'count'], (price, count) => {
    evaluations++;
    return Number(price) * Number(count);
  });
  assert.equal(total.get(), 6);
  assert.equal(evaluations, 1);
  const seen = [];
  const stop = total.subscribe(value => seen.push(value));
  api.signal('count').set('3');
  assert.equal(total.get(), 9);
  assert.deepEqual(seen, [9]);
  api.signal('price').set('3');
  assert.equal(evaluations, 2, 'no recalculation on unchanged input');
  api.signal('price').set('4');
  assert.equal(total.get(), 12);
  assert.deepEqual(seen, [9, 12]);
  stop();
  api.signal('count').set('4');
  assert.deepEqual(seen, [9, 12], 'derived subscription can be removed');
  assert.equal(total.get(), 16);
  total.dispose();
  total.dispose();
  api.signal('price').set('5');
  assert.equal(total.get(), 16, 'disposed computed value is no longer updated');
  assert.throws(() => total.subscribe(() => {}), /disposed/);
}

{
  const { api } = mount([['__proto__', '2'], ['constructor', '3']]);
  const combined = api.computed(['__proto__', 'constructor'],
    (a, b) => Number(a) + Number(b));
  assert.equal(combined.get(), 5, 'prototype-named signals are ordinary keys');
  api.signal('__proto__').set('4');
  assert.equal(combined.get(), 7);
  combined.dispose();
}

{
  const { api } = mount([['first', '1'], ['second', '10']]);
  const sources = ['first'];
  const computed = api.computed(sources, first => Number(first) + 1);
  sources[0] = 'second';
  api.signal('first').set('3');
  assert.equal(computed.get(), 4, 'dependency list is snapshotted at registration');
  api.signal('second').set('20');
  assert.equal(computed.get(), 4, 'mutating caller array cannot retarget dependencies');
  computed.dispose();
}

{
  const { api } = mount([['count', '1']]);
  assert.throws(() => api.computed([], x => x), /dependencies/);
  assert.throws(() => api.computed(['count', 'count'], x => x), /duplicate/);
  assert.throws(() => api.computed(['count'], null), /function/);
  assert.throws(() => api.signal('count').subscribe(null), /function/);
}

console.log('web computed signal regressions passed');
