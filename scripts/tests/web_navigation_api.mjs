// No-dependency regression for Navigation API progressive link enhancement.
// Run: node scripts/tests/web_navigation_api.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const js = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(js, 'generated browser runtime exists');
assert.match(source, /html\.contains\("data-nulang-nav-target="\)/, 'navigation-only HTML loads client runtime');

function harness({ navigationSupported = true, target = '#status', fetchImpl = null } = {}) {
  let current = { id: 'status', textContent: 'Old', replaceWith(incoming) { current = incoming; } };
  const actions = new Map();
  const fetchRequests = [];
  const fallbacks = [];
  const errors = [];
  const sourceLink = {
    href: 'https://example.test/new',
    tagName: 'A',
    dataset: { nulangNavTarget: target },
    target: '',
    download: '',
    closest(selector) { return selector === 'a[data-nulang-nav-target]' ? this : null; },
    hasAttribute(name) { return name === 'download' ? false : false; },
    getAttribute(name) { return name === 'rel' ? '' : null; }
  };
  const document = {
    title: 'Previous',
    readyState: 'complete',
    querySelectorAll(selector) { return selector === '[data-action]' ? [] : []; },
    querySelector(selector) { return selector === '#status' ? current : null; },
    importNode(node) {
      return { ...node, replaceWith(incoming) { current = document.importNode(incoming); } };
    }
  };
  class DOMParser {
    parseFromString(html, type) {
      assert.equal(type, 'text/html');
      const m = html.match(/<div id="status">([^<]*)<\/div>/);
      const title = html.match(/<title>([^<]*)<\/title>/);
      return { title: title?.[1] || '', querySelector(sel) {
        return sel === '#status' && m ? { id: 'status', textContent: m[1] } : null;
      } };
    }
  }
  const navigation = navigationSupported ? {
    addEventListener(type, listener) { actions.set(type, listener); }
  } : undefined;
  const window = {
    location: {
      href: 'https://example.test/old',
      origin: 'https://example.test',
      replace(url) { fallbacks.push(url); }
    },
    ...(navigationSupported ? { navigation } : {})
  };
  function fetch(url, options) {
    fetchRequests.push({ url, options });
    if (fetchImpl) return fetchImpl(url, options);
    return Promise.resolve({
      ok: true,
      status: 200,
      headers: { get: () => 'text/html; charset=utf-8' },
      text: async () => '<!doctype html><html><head><title>New page</title></head><body><div id="status">Updated</div></body></html>'
    });
  }
  runInNewContext(js[1], {
    window, document, DOMParser, fetch, URL,
    URLSearchParams, AbortController,
    console: { warn() {}, error(...args) { errors.push(args.join(' ')); } }
  });
  const makeEvent = (overrides = {}) => {
    const controller = new AbortController();
    let callback;
    const event = {
      canIntercept: true, hashChange: false, downloadRequest: null,
      formData: null, navigationType: 'push', sourceElement: sourceLink,
      destination: { url: 'https://example.test/new' },
      signal: controller.signal,
      intercept(opts) { callback = opts.handler; },
      ...overrides
    };
    return { event, controller, intercepted: () => !!callback, run: () => callback?.() };
  };
  return {
    makeEvent, sourceLink,
    emit(e) { actions.get('navigate')?.(e); },
    listenerRegistered: () => actions.has('navigate'),
    text: () => current.textContent,
    title: () => document.title,
    requests: fetchRequests, fallbacks, errors,
    setCurrent(value) { current = value; }
  };
}

const normal = harness();
assert.ok(normal.listenerRegistered(), 'Navigation API event listener registered');
const navigation = normal.makeEvent();
normal.emit(navigation.event);
assert.ok(navigation.intercepted(), 'opt-in same-origin link intercepted');
await navigation.run();
assert.equal(normal.text(), 'Updated', 'morphs only the chosen target');
assert.equal(normal.title(), 'New page', 'updates page title after valid full-page response');
assert.equal(normal.requests.length, 1);
assert.equal(normal.requests[0].url, 'https://example.test/new');
assert.equal(normal.requests[0].options.method, 'GET');
assert.equal(normal.requests[0].options.signal, navigation.controller.signal);
assert.deepEqual(normal.fallbacks, []);

const missingAPI = harness({ navigationSupported: false });
assert.equal(missingAPI.listenerRegistered(), false, 'unsupported browsers use native navigation');

for (const [name, props] of [
  ['cross-origin', { destination: { url: 'https://other.test/path' } }],
  ['not interceptable', { canIntercept: false }],
  ['hash navigation', { hashChange: true }],
  ['file download', { downloadRequest: 'file.txt' }],
  ['history traversal', { navigationType: 'traverse' }],
  ['page reload', { navigationType: 'reload' }],
  ['form navigation', { formData: {} }],
  ['no initiating link', { sourceElement: null }],
  ['external link relation', { sourceElement: {
    ...harness().sourceLink,
    getAttribute(name) { return name === 'rel' ? 'nofollow external' : null; }
  } }],
  ['link explicitly opens new tab', { sourceElement: {
    ...harness().sourceLink,
    target: '_blank'
  } }]
]) {
  const h = harness();
  const e = h.makeEvent(props);
  h.emit(e.event);
  assert.equal(e.intercepted(), false, name + ' must remain browser-managed');
  assert.equal(h.requests.length, 0);
}
const invalid = harness({ target: 'body > main' });
const invalidEvent = invalid.makeEvent();
invalid.emit(invalidEvent.event);
assert.equal(invalidEvent.intercepted(), false, 'invalid selectors fall back to native navigation');
const missingTarget = harness();
missingTarget.setCurrent(null);
const missingTargetEvent = missingTarget.makeEvent();
missingTarget.emit(missingTargetEvent.event);
assert.equal(missingTargetEvent.intercepted(), false, 'missing target falls back to native navigation');

// Intercepted but unsuccessful/unsafe responses must trigger a full document
// navigation without modifying the currently displayed DOM.
for (const [name, response] of [
  ['server failure', { ok: false, status: 503 }],
  ['redirect', { ok: true, status: 200, redirected: true }],
  ['unexpected content', { ok: true, status: 200,
    headers: { get: () => 'application/json' } }],
  ['missing fragment', { ok: true, status: 200,
    headers: { get: () => 'text/html' }, text: async () => '<div id="other">Missing</div>' }]
]) {
  const h = harness({ fetchImpl: async () => response });
  const e = h.makeEvent();
  h.emit(e.event);
  assert.ok(e.intercepted(), name + ' initially intercepts');
  await e.run();
  assert.equal(h.text(), 'Old', name + ' cannot patch unrelated DOM');
  assert.equal(h.title(), 'Previous');
  assert.deepEqual(h.fallbacks, ['https://example.test/new'], name + ' resumes full navigation');
  assert.equal(h.errors.length, 1, name + ' reports its failure');
}

const canceled = harness();
const canceledEvent = canceled.makeEvent();
canceled.emit(canceledEvent.event);
canceledEvent.controller.abort();
await canceledEvent.run();
assert.equal(canceled.text(), 'Old');
assert.deepEqual(canceled.fallbacks, [], 'canceled navigation never reloads');
assert.deepEqual(canceled.errors, [], 'canceled navigation does not log an error');
assert.equal(canceled.requests.length, 0, 'already canceled navigation avoids fetching');

// A later navigation can finish before an older, aborted fetch. Neither its
// content nor its title may be overwritten by the stale response.
const pending = [];
const racing = harness({ fetchImpl: () => new Promise(resolve => pending.push(resolve)) });
const first = racing.makeEvent();
racing.emit(first.event);
const firstFinish = first.run();
const second = racing.makeEvent();
racing.emit(second.event);
const secondFinish = second.run();
first.controller.abort();
const responseFor = value => ({ ok: true, status: 200,
  headers: { get: () => 'text/html' },
  text: async () => `<title>${value}</title><div id="status">${value}</div>` });
pending[1](responseFor('Latest'));
await secondFinish;
pending[0](responseFor('Stale'));
await firstFinish;
assert.equal(racing.text(), 'Latest');
assert.equal(racing.title(), 'Latest');
assert.deepEqual(racing.fallbacks, []);

console.log('Nulang Web: Navigation API enhancement and safe native fallback passed');
