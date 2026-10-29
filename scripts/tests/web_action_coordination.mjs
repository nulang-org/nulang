// Regression: per-target request policies protect mutations from accidental
// cancellation, and queue actions in submit order with isolated payloads.
// Run: node scripts/tests/web_action_coordination.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'browser runtime found');

function setup(policy) {
  const listeners = new Map();
  const requests = [];
  const errors = [];
  let text = 'Initial';
  let field = 'first';
  let replaceCount = 0;
  let current = {
    id: 'status',
    textContent: text,
    replaceWith(node) { current = node; replaceCount++; text = node.textContent; }
  };
  const form = {
    tagName: 'FORM',
    dataset: { action: 'save', actionPlacement: 'server', nulangTarget: '#status',
      ...(policy ? { nulangCoordination: policy } : {}) },
    closest(tag) { return tag === 'form' ? this : null; },
    addEventListener(name, cb) { listeners.set(name, cb); }
  };
  const document = {
    readyState: 'complete',
    querySelectorAll(q) { return q === '[data-action]' ? [form] : []; },
    querySelector(q) { return q === '#status' ? current : null; },
    importNode(node) { return { ...node, replaceWith: current.replaceWith }; }
  };
  class DOMParser {
    parseFromString(html) {
      const m = html.match(/<div id="status">([^<]*)<\/div>/);
      return { querySelector(q) { return q === '#status' && m ? { id: 'status', textContent: m[1] } : null; } };
    }
  }
  class FormData {
    forEach(callback) { callback(field, 'name'); }
  }
  const window = { location: { href: 'https://example.test/', reload() { throw Error('unexpected reload'); } } };
  const fetch = (_url, options) => new Promise((resolve, reject) => {
    requests.push({ resolve, reject, options });
  });
  runInNewContext(match[1], {
    document, DOMParser, FormData, URLSearchParams, fetch, AbortController,
    window,
    console: { error(...args) { errors.push(args.join(' ')); }, warn() {} }
  }, { timeout: 1000 });
  const submit = () => listeners.get('submit')({ preventDefault() {} });
  return {
    requests, errors, form, submit,
    setField(v) { field = v; },
    navigateTo(url) { window.location.href = url; },
    getText() { return text; },
    getReplacements() { return replaceCount; }
  };
}
const html = value => ({
  ok: true,
  status: 200,
  headers: { get() { return 'text/html'; } },
  text: async () => `<div id="status">${value}</div>`
});
const flush = async () => {
  await new Promise(resolve => setImmediate(resolve));
  await new Promise(resolve => setImmediate(resolve));
};

{
  const h = setup('drop');
  h.submit();
  h.setField('second');
  h.submit();
  assert.equal(h.requests.length, 1, 'drop prevents a second POST while first pending');
  assert.equal(h.requests[0].options.signal?.aborted, false, 'drop never aborts a running mutation');
  h.requests[0].resolve(html('first result'));
  await flush();
  assert.equal(h.getText(), 'first result');
  h.submit();
  assert.equal(h.requests.length, 2, 'drop unlocks after completion');
}

{
  const h = setup('queue');
  h.submit();
  h.setField('second');
  h.submit();
  h.setField('third');
  h.submit();
  assert.equal(h.requests.length, 1, 'queue sends only first POST immediately');
  assert.equal(h.requests[0].options.body.get('name'), 'first');
  h.requests[0].resolve(html('first result'));
  await flush();
  assert.equal(h.requests.length, 2, 'second POST begins after first patch');
  assert.equal(h.requests[1].options.body.get('name'), 'second', 'queue snapshots field values at submit time');
  h.requests[1].resolve(html('second result'));
  await flush();
  assert.equal(h.requests.length, 3);
  assert.equal(h.requests[2].options.body.get('name'), 'third');
  h.requests[2].resolve(html('third result'));
  await flush();
  assert.equal(h.getText(), 'third result');
  assert.equal(h.getReplacements(), 3);
  assert.deepEqual(h.errors, []);
}

{
  const h = setup('queue');
  h.submit();
  h.submit();
  h.requests[0].resolve({ ok: false, status: 409 });
  await flush();
  assert.equal(h.requests.length, 2, 'failed queued action must not deadlock subsequent actions');
  assert.equal(h.errors.length, 1, 'errors remain visible');
  h.requests[1].resolve(html('after failure'));
  await flush();
  assert.equal(h.getText(), 'after failure');
}

{
  const h = setup('invalid');
  h.submit();
  assert.equal(h.requests.length, 0, 'unknown coordination policy must fail closed');
  await flush();
  assert.equal(h.errors.length, 1);
}

{
  const h = setup('queue');
  h.submit();
  for (let i = 0; i < 32; i++) h.submit();
  h.submit();
  await flush();
  assert.equal(h.requests.length, 1, 'queue must not execute concurrent POSTs');
  assert.equal(h.errors.length, 1, 'an overflowing queue must reject the additional request');
  assert.match(h.errors[0], /queue.*full/i);
}

{
  const h = setup('queue');
  h.submit();
  h.form.dataset.nulangCoordination = 'replace';
  h.submit();
  await flush();
  assert.equal(h.requests.length, 1, 'mixing policies in active target fails closed');
  assert.equal(h.errors.length, 1);
  h.requests[0].resolve(html('original'));
  await flush();
  assert.equal(h.getText(), 'original');
}
{
  const h = setup('queue');
  h.submit();
  h.submit();
  h.navigateTo('https://example.test/other-route');
  h.requests[0].resolve(html('obsolete'));
  await flush();
  assert.equal(h.requests.length, 1,
    'queued mutations must not post to a previous route after navigation');
  assert.equal(h.getText(), 'Initial');
}

console.log('Nulang Web: drop/queue/replace action coordination verified');
