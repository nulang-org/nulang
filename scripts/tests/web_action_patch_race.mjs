// Regression: the latest submitted patch for a target must win even when
// an earlier fetch or response body resolves later.
// Run: node scripts/tests/web_action_patch_race.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'expected generated Nulang browser runtime');

const listeners = new Map();
const requests = [];
const errors = [];
let updates = 0;
let current = {
  id: 'status',
  textContent: 'Initial',
  replaceWith(incoming) { current = incoming; updates++; }
};
const form = {
  tagName: 'FORM',
  dataset: { action: 'update', actionPlacement: 'server', nulangTarget: '#status' },
  closest(tag) { return tag === 'form' ? this : null; },
  addEventListener(name, fn) { listeners.set(name, fn); }
};
const document = {
  readyState: 'complete',
  querySelectorAll(selector) { return selector === '[data-action]' ? [form] : []; },
  querySelector(selector) { return selector === '#status' ? current : null; },
  importNode(node) {
    return { ...node, replaceWith(incoming) { current = document.importNode(incoming); updates++; } };
  }
};
class DOMParser {
  parseFromString(body) {
    const match = body.match(/<div id="status">([^<]*)<\/div>/);
    return {
      querySelector(selector) {
        return selector === '#status' && match
          ? { id: 'status', textContent: match[1] }
          : null;
      }
    };
  }
}
class FormData {
  forEach(callback) { callback('example', 'title'); }
}
const fetch = (_url, options) => new Promise((resolve, reject) => {
  requests.push({ resolve, reject, options });
});
const response = (value, read = async () => `<div id="status">${value}</div>`) => ({
  ok: true,
  status: 200,
  headers: { get: () => 'text/html' },
  text: read
});
const flush = async () => {
  await new Promise(resolve => setImmediate(resolve));
  await new Promise(resolve => setImmediate(resolve));
};
runInNewContext(match[1], {
  document, window: { location: { href: 'https://example.test/tasks', reload() { throw Error('unexpected reload'); } } },
  DOMParser, FormData, URLSearchParams, fetch, AbortController,
  console: { error(...args) { errors.push(args.join(' ')); }, warn() {} }
});
const submit = listeners.get('submit');
assert.ok(submit, 'form submission wired');
const event = { preventDefault() {} };

// Two overlapping mutations of the same region. Later submission wins even
// if the first request ignores the abort signal.
submit(event);
submit(event);
assert.equal(requests.length, 2);
requests[1].resolve(response('Newer'));
await flush();
requests[0].resolve(response('Older'));
await flush();
assert.equal(current.textContent, 'Newer', 'late response must not overwrite latest patch');
assert.equal(updates, 1, 'stale response must not cause additional DOM changes');
assert.equal(errors.length, 0, JSON.stringify(errors));
assert.equal(requests[0].options.signal?.aborted, true, 'superseded request should be aborted');

// Race after headers arrive but before the first response body resolves.
let resolveBody;
submit(event);
requests[2].resolve(response('unused', () => new Promise(resolve => { resolveBody = resolve; })));
await flush();
assert.equal(typeof resolveBody, 'function');
submit(event);
requests[3].resolve(response('Newest'));
await flush();
resolveBody('<div id="status">Stale body</div>');
await flush();
assert.equal(current.textContent, 'Newest', 'stale body resolution cannot overwrite latest patch');
assert.equal(updates, 2);
assert.equal(errors.length, 0);

// Real fetch rejects an aborted request instead of resolving it. The
// superseded request's AbortError must not reach the application's logger.
submit(event);
submit(event);
requests[4].reject(Object.assign(new Error('aborted'), { name: 'AbortError' }));
requests[5].resolve(response('After abort'));
await flush();
assert.equal(current.textContent, 'After abort');
assert.equal(errors.length, 0, 'abort of superseded fetch is not reported');

// The current submission must still surface a genuine HTTP error.
submit(event);
requests[6].resolve({ ok: false, status: 409 });
await flush();
assert.equal(current.textContent, 'After abort', 'failed action cannot patch the DOM');
assert.equal(errors.length, 1, 'current HTTP errors remain visible');
assert.match(errors[0], /409/);
console.log('Nulang Web: superseded action responses cannot roll back targeted DOM');
