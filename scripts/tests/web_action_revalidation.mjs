// Opt-in selective GET revalidation after a successful progressive POST.
// Run: node scripts/tests/web_action_revalidation.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'generated browser runtime exists');

const htmlResponse = (body, status = 200, contentType = 'text/html; charset=utf-8') => ({
  ok: status >= 200 && status < 300, status, redirected: false,
  headers: { get(name) { return name.toLowerCase() === 'content-type' ? contentType : null; } },
  text: async () => body
});
const flush = async () => {
  for (let i = 0; i < 5; i++) await new Promise(resolve => setImmediate(resolve));
};

function harness({ revalidate = '#summary, #notifications', get = null, post = null,
    overlap = false, compiledData = null } = {}) {
  const nodes = new Map();
  const updates = [];
  const listeners = new Map();
  const fetches = [];
  const errors = [];
  let reloads = 0;
  function node(id, text) {
    return {
      id, textContent: text,
      replaceWith(incoming) {
        updates.push(id);
        nodes.set(id, node(incoming.id, incoming.textContent));
      }
    };
  }
  for (const [id, text] of [['detail', 'Old detail'], ['summary', 'Old summary'],
      ['notifications', 'Old notifications'], ['untouched', 'Untouched']]) {
    nodes.set(id, node(id, text));
  }
  if (overlap) nodes.get('summary').contains = candidate => candidate.id === 'notifications';
  const form = {
    tagName: 'FORM',
    dataset: { action: 'save', actionPlacement: 'server', nulangTarget: '#detail',
      ...(revalidate === null ? {} : { nulangRevalidate: revalidate }) },
    closest(name) { return name === 'form' ? form : null; },
    addEventListener(name, fn) { listeners.set(name, fn); }
  };
  const document = {
    readyState: 'complete',
    querySelectorAll(selector) { return selector === '[data-action]' ? [form] : []; },
    querySelector(selector) { return nodes.get(selector.slice(1)) || null; },
    importNode(incoming) { return node(incoming.id, incoming.textContent); }
  };
  class DOMParser {
    parseFromString(html, type) {
      assert.equal(type, 'text/html');
      return {
        querySelector(selector) {
          const id = selector.slice(1);
          const element = html.match(new RegExp('<div id="' + id + '">([^<]*)</div>'));
          return element ? node(id, element[1]) : null;
        }
      };
    }
  }
  class FormData { forEach(fn) { fn('A', 'value'); } }
  const fetch = (url, options) => {
    fetches.push({ url, options });
    if (options.method === 'POST') {
      return typeof post === 'function'
        ? post(url, options)
        : Promise.resolve(post || htmlResponse('<div id="detail">Saved</div>'));
    }
    if (options.method === 'GET') {
      return typeof get === 'function'
        ? get(url, options)
        : Promise.resolve(get || htmlResponse(
          '<div id="detail">Canonical detail</div>' +
          '<div id="summary">Fresh summary</div>' +
          '<div id="notifications">Fresh notifications</div>'
        ));
    }
    throw new Error('unexpected HTTP method');
  };
  const window = {
    location: { href: 'https://example.test/tasks', reload() { reloads++; } },
    ...(compiledData ? { nulangCompiledData: compiledData } : {})
  };
  runInNewContext(match[1], {
    document, window, DOMParser, FormData, URLSearchParams, URL, fetch, AbortController,
    console: { error(...a) { errors.push(a.map(String).join(' ')); }, warn() {} }
  });
  return {
    submit() { listeners.get('submit')({ preventDefault() {} }); },
    navigateTo(url) { window.location.href = url; },
    text(id) { return nodes.get(id)?.textContent; },
    updates, fetches, errors,
    reloads() { return reloads; }
  };
}

const valid = harness();
valid.submit();
await flush();
assert.equal(valid.text('detail'), 'Saved', 'POST response patches primary target');
assert.equal(valid.text('summary'), 'Fresh summary', 'GET revalidates declared region');
assert.equal(valid.text('notifications'), 'Fresh notifications', 'one GET updates second region');
assert.equal(valid.text('untouched'), 'Untouched', 'unrelated UI is untouched');
assert.deepEqual(valid.updates, ['detail', 'summary', 'notifications']);
assert.deepEqual(valid.fetches.map(f => f.options.method), ['POST', 'GET']);
assert.equal(valid.fetches[1].url, 'https://example.test/tasks');
assert.equal(valid.fetches[1].options.cache, 'no-store');
assert.equal(valid.reloads(), 0);
assert.deepEqual(valid.errors, []);

const legacy = harness({ revalidate: null });
legacy.submit();
await flush();
assert.deepEqual(legacy.fetches.map(f => f.options.method), ['POST']);
assert.equal(legacy.text('detail'), 'Saved');
assert.equal(legacy.text('summary'), 'Old summary');

for (const config of ['body > section', '#summary #summary', '#detail', '', '#summary, #detail',
  '#summary #notifications #a #b #c #d #e #f #g']) {
  const bad = harness({ revalidate: config });
  bad.submit();
  await flush();
  assert.deepEqual(bad.fetches.map(f => f.options.method), [],
    'invalid config cannot submit a mutation: ' + config);
  assert.equal(bad.text('detail'), 'Old detail', 'invalid config fails closed: ' + config);
  assert.ok(bad.errors.length, 'invalid config reports error: ' + config);
}

for (const [name, get] of [
  ['missing revalidation region', htmlResponse('<div id="summary">New</div>')],
  ['GET HTTP error', htmlResponse('error', 503)],
  ['non HTML GET', htmlResponse('{"ok":true}', 200, 'application/json')],
  ['unsafe redirect', { ...htmlResponse('<div id="summary">New</div>'), redirected: true }]
]) {
  const fail = harness({ get });
  fail.submit();
  await flush();
  assert.equal(fail.text('detail'), 'Old detail', name + ' must prevent partial POST patch');
  assert.equal(fail.text('summary'), 'Old summary', name + ' must prevent partial GET patch');
  assert.equal(fail.text('notifications'), 'Old notifications');
  assert.deepEqual(fail.updates, []);
  assert.equal(fail.errors.length, 1, name + ' reports failure');
}

const overlapping = harness({ overlap: true });
overlapping.submit();
await flush();
assert.deepEqual(overlapping.updates, [], 'nested targets cannot be partially patched');
assert.equal(overlapping.errors.length, 1, 'nested target collision is reported');

// A pending POST must not revalidate the new page after client navigation.
let finishPagePost;
const leftPage = harness({ post() {
  return new Promise(resolve => { finishPagePost = resolve; });
} });
leftPage.submit();
leftPage.navigateTo('https://example.test/other-page');
finishPagePost(htmlResponse('<div id="detail">Saved after navigation</div>'));
await flush();
assert.deepEqual(leftPage.fetches.map(f => f.options.method), ['POST'],
  'old page cannot trigger a GET against a newer route');
assert.equal(leftPage.text('detail'), 'Old detail', 'old mutation cannot patch a newer page');
assert.deepEqual(leftPage.errors, [], 'abandoned navigation cannot log an action failure');

// The POST is complete, but an earlier revalidation GET resolves after a
// newer submission has already patched the same primary target.
let resolveOldGet;
let getCalls = 0;
const racing = harness({ get() {
  getCalls++;
  if (getCalls === 1) return new Promise(resolve => { resolveOldGet = resolve; });
  return htmlResponse('<div id="summary">New summary</div><div id="notifications">New notifications</div>');
}});
racing.submit();
await flush();
assert.equal(typeof resolveOldGet, 'function');
racing.submit();
await flush();
resolveOldGet(htmlResponse('<div id="summary">Stale</div><div id="notifications">Stale</div>'));
await flush();
assert.equal(racing.text('detail'), 'Saved');
assert.equal(racing.text('summary'), 'New summary', 'stale revalidation cannot overwrite recent data');
assert.equal(racing.text('notifications'), 'New notifications');
assert.equal(racing.errors.length, 0, 'superseded revalidation does not log errors');



// Distinct primary mutations may invalidate the same shared summary. The last
// action that claims the summary wins, even when the first GET completes late.
{
  const actions = new Map();
  const nodes = new Map();
  const errors = [];
  const requests = [];
  const waiting = [];
  const makeNode = (id, text) => ({
    id, textContent: text,
    replaceWith(incoming) { nodes.set(id, makeNode(id, incoming.textContent)); }
  });
  for (const id of ['left', 'right', 'summary']) nodes.set(id, makeNode(id, 'Old ' + id));
  const makeForm = (handler, target) => ({
    tagName: 'FORM',
    dataset: { action: handler, actionPlacement: 'server',
      nulangTarget: target, nulangRevalidate: '#summary' },
    closest(selector) { return selector === 'form' ? this : null; },
    addEventListener(_type, fn) { actions.set(handler, fn); }
  });
  const forms = [makeForm('save_left', '#left'), makeForm('save_right', '#right')];
  const document = {
    readyState: 'complete',
    querySelectorAll(selector) { return selector === '[data-action]' ? forms : []; },
    querySelector(selector) { return nodes.get(selector.slice(1)) || null; },
    importNode(n) { return makeNode(n.id, n.textContent); }
  };
  class DOMParser {
    parseFromString(html) {
      return { querySelector(selector) {
        const id = selector.slice(1);
        const m = html.match(new RegExp('<div id="' + id + '">([^<]*)</div>'));
        return m ? makeNode(id, m[1]) : null;
      } };
    }
  }
  class FormData { forEach() {} }
  const fetch = (_url, opts) => {
    requests.push(opts);
    if (opts.method === 'POST') {
      const body = opts.body.get('__nulang_action');
      const id = body === 'save_left' ? 'left' : 'right';
      return Promise.resolve(htmlResponse('<div id="' + id + '">New ' + id + '</div>'));
    }
    return new Promise(resolve => waiting.push(resolve));
  };
  runInNewContext(match[1], {
    document, window: { location: { href: 'https://example.test/items', reload() {} } },
    DOMParser, FormData, URLSearchParams, fetch, AbortController,
    console: { error(...args) { errors.push(args.map(String).join(' ')); }, warn() {} }
  });
  const event = { preventDefault() {} };
  actions.get('save_left')(event);
  await flush();
  assert.equal(waiting.length, 1, 'left mutation begins canonical GET');
  actions.get('save_right')(event);
  await flush();
  assert.equal(waiting.length, 2, 'right mutation begins canonical GET');
  waiting[1](htmlResponse('<div id="summary">Right summary</div>'));
  await flush();
  waiting[0](htmlResponse('<div id="summary">Stale left summary</div>'));
  await flush();
  assert.equal(nodes.get('summary').textContent, 'Right summary',
    'shared target cannot be rolled back by another primary action');
  assert.equal(nodes.get('right').textContent, 'New right');
  assert.equal(nodes.get('left').textContent, 'Old left',
    'superseded action does not apply a stale primary fragment');
  assert.equal(errors.length, 0);
  assert.equal(requests[1].signal?.aborted, true, 'shared-target supersession aborts old GET');
}

const compiledData = {
  mutations: [{ path: '/tasks', invalidates: ['tasks'],
    targets: ['#summary', '#notifications'] }]
};
const automatic = harness({ revalidate: null, compiledData });
automatic.submit();
await flush();
assert.equal(automatic.text('detail'), 'Saved');
assert.equal(automatic.text('summary'), 'Fresh summary',
  'compiler-validated mutation metadata triggers sibling refresh');
assert.equal(automatic.text('notifications'), 'Fresh notifications');
assert.deepEqual(automatic.fetches.map(f => f.options.method), ['POST', 'GET']);

const undeclared = harness({ revalidate: '#untouched', compiledData });
undeclared.submit();
await flush();
assert.equal(undeclared.fetches.length, 0,
  'explicit selectors outside compiled dependency mapping reject before POST');
assert.equal(undeclared.text('detail'), 'Old detail');
assert.equal(undeclared.errors.length, 1);

console.log('Nulang Web: explicit and compiler-validated revalidation is race-safe');
