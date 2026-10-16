// Dependency-free regression tests for Nulang's compiler-emitted browser micro-runtime.
// Run: node --test tests/web_reactivity_runtime.test.cjs
// This verifies the emitted JS, not native Nulang compilation or real browser behavior.
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

function emittedRuntime() {
  const rust = fs.readFileSync(
    path.resolve(__dirname, '../src/web/reactivity.rs'),
    'utf8'
  );
  const fn = rust.indexOf('pub fn generate_client_runtime() -> String {');
  assert.ok(fn >= 0, 'client runtime generator must exist');
  const begin = rust.indexOf('r#"', fn);
  const end = rust.indexOf('"#', begin + 3);
  assert.ok(begin >= 0 && end > begin, 'client runtime must be a raw string');
  return rust.slice(begin + 3, end);
}

function element(initial) {
  const attributes = Object.assign(Object.create(null), initial.attributes || {});
  const calls = [];
  return {
    attributes,
    calls,
    textContent: initial.textContent || '',
    dataset: { signal: attributes['data-signal'] },
    tagName: initial.tagName || 'SPAN',
    getAttribute(name) {
      return Object.hasOwn(attributes, name) ? attributes[name] : null;
    },
    setAttribute(name, value) {
      calls.push([name, value]);
      attributes[name] = value;
    },
    addEventListener() {
      throw new Error('no action listeners expected in this fixture');
    }
  };
}

function hydrate(elements, actions) {
  const window = { nulangActions: actions };
  const document = {
    readyState: 'complete',
    querySelectorAll(selector) {
      const match = /^\[([a-z-]+)\]$/.exec(selector);
      assert.ok(match, 'unexpected selector: ' + selector);
      const key = match[1];
      return elements.filter((el) => el.getAttribute(key) !== null);
    },
    addEventListener() {
      throw new Error('ready document must hydrate synchronously');
    }
  };
  vm.runInNewContext(emittedRuntime(), { window, document, console });
  return window.nulang;
}

test('attribute-only binding initializes from SSR and updates inert attrs', () => {
  const button = element({
    tagName: 'BUTTON',
    textContent: 'Unchanged text',
    attributes: {
      title: 'Original',
      'aria-label': 'Original',
      'data-nulang-bind-title': 'label',
      'data-nulang-bind-aria-label': 'label'
    }
  });
  const api = hydrate([button]);
  assert.equal(api.signal('label').get(), 'Original');
  api.signal('label').set('Updated');
  assert.equal(button.getAttribute('title'), 'Updated');
  assert.equal(button.getAttribute('aria-label'), 'Updated');
  assert.equal(button.textContent, 'Unchanged text');
  assert.deepEqual(
    button.calls.map(([name]) => name).sort(),
    ['aria-label', 'title']
  );
});

test('text and attribute bindings update without touching URL attributes', () => {
  const text = element({
    textContent: 'Pending',
    attributes: { 'data-signal': 'status' }
  });
  const link = element({
    textContent: 'Do not replace',
    attributes: {
      href: 'https://example.com/unchanged',
      title: 'Pending',
      'data-nulang-bind-title': 'status'
    }
  });
  const api = hydrate([text, link]);
  assert.equal(api.signal('status').get(), 'Pending');
  api.signal('status').set('Ready');
  assert.equal(text.textContent, 'Ready');
  assert.equal(link.getAttribute('title'), 'Ready');
  assert.equal(link.getAttribute('href'), 'https://example.com/unchanged');
  assert.equal(link.textContent, 'Do not replace');
  assert.equal(link.calls.filter(([name]) => name === 'href').length, 0);
});

test('bound attribute values stay data and never use innerHTML', () => {
  const button = element({
    textContent: 'Original text',
    attributes: { title: 'Before', 'data-nulang-bind-title': 'caption' }
  });
  const api = hydrate([button]);
  const payload = '<img src=x onerror=alert(1)>';
  api.signal('caption').set(payload);
  assert.equal(button.getAttribute('title'), payload);
  assert.equal(button.textContent, 'Original text');
  assert.ok(!emittedRuntime().includes('innerHTML'));
});

test('signal keys do not collide with inherited Object properties', () => {
  const button = element({
    attributes: { title: 'A', 'data-nulang-bind-title': '__proto__' }
  });
  const api = hydrate([button]);
  assert.equal(api.signal('__proto__').get(), 'A');
  api.signal('__proto__').set('B');
  assert.equal(button.getAttribute('title'), 'B');
});

test('unchanged signal values do not write to DOM and ordinary updates remain synchronous', () => {
  const button = element({
    attributes: { title: 'Ready', 'data-nulang-bind-title': 'status' }
  });
  const api = hydrate([button]);
  api.signal('status').set('Ready');
  assert.deepEqual(button.calls, []);
  api.signal('status').set('Updated');
  assert.equal(button.getAttribute('title'), 'Updated');
  assert.deepEqual(button.calls, [['title', 'Updated']]);
});

test('batch coalesces repeated writes while signal reads remain immediate', () => {
  const text = element({ textContent: 'Initial', attributes: { 'data-signal': 'status' } });
  const button = element({
    attributes: { title: 'Initial', 'data-nulang-bind-title': 'status' }
  });
  const api = hydrate([text, button]);
  const result = api.batch(() => {
    api.signal('status').set('First');
    api.signal('status').set('Second');
    assert.equal(api.signal('status').get(), 'Second');
    assert.equal(text.textContent, 'Initial');
    assert.equal(button.getAttribute('title'), 'Initial');
    assert.deepEqual(button.calls, []);
    return 42;
  });
  assert.equal(result, 42);
  assert.equal(text.textContent, 'Second');
  assert.equal(button.getAttribute('title'), 'Second');
  assert.deepEqual(button.calls, [['title', 'Second']]);
});

test('nested batches flush only after the outermost scope exits', () => {
  const button = element({
    attributes: { title: 'Zero', 'data-nulang-bind-title': 'count' }
  });
  const api = hydrate([button]);
  api.batch(() => {
    api.signal('count').set('One');
    api.batch(() => {
      api.signal('count').set('Two');
      assert.deepEqual(button.calls, []);
    });
    assert.deepEqual(button.calls, []);
  });
  assert.equal(button.getAttribute('title'), 'Two');
  assert.deepEqual(button.calls, [['title', 'Two']]);
});

test('a throwing batch flushes the final state and propagates the error', () => {
  const button = element({
    attributes: { title: 'Before', 'data-nulang-bind-title': 'status' }
  });
  const api = hydrate([button]);
  assert.throws(() => api.batch(() => {
    api.signal('status').set('After');
    throw new Error('callback failed');
  }), /callback failed/);
  assert.equal(button.getAttribute('title'), 'After');
  api.signal('status').set('Later');
  assert.equal(button.getAttribute('title'), 'Later');
});

test('batch rejects non-function callbacks', () => {
  const api = hydrate([]);
  assert.throws(() => api.batch(null), /batch requires a function/);
});


function subtree(elements) {
  return {
    querySelectorAll(selector) {
      const match = /^\[([a-z-]+)\]$/.exec(selector);
      assert.ok(match, 'unexpected selector: ' + selector);
      return elements.filter((el) => el.getAttribute(match[1]) !== null);
    }
  };
}

test('mount synchronizes new DOM bindings and dispose prevents stale writes', () => {
  const api = hydrate([]);
  api.signal('status').set('Live');
  const text = element({ textContent: 'Stale', attributes: { 'data-signal': 'status' } });
  const button = element({
    attributes: { title: 'Stale', 'data-nulang-bind-title': 'status' }
  });
  const root = subtree([text, button]);
  const owner = api.mount(root);
  assert.equal(text.textContent, 'Live');
  assert.equal(button.getAttribute('title'), 'Live');
  api.signal('status').set('First');
  assert.equal(text.textContent, 'First');
  assert.equal(button.getAttribute('title'), 'First');

  owner.dispose();
  owner.dispose(); // Disposal is idempotent.
  const prior = button.calls.length;
  api.signal('status').set('Second');
  assert.equal(text.textContent, 'First');
  assert.equal(button.getAttribute('title'), 'First');
  assert.equal(button.calls.length, prior);

  api.mount(root);
  assert.equal(text.textContent, 'Second');
  assert.equal(button.getAttribute('title'), 'Second');
  api.signal('status').set('Third');
  assert.equal(button.calls.filter(([name, value]) =>
    name === 'title' && value === 'Third').length, 1);
});

test('repeated or overlapping mounts never double-subscribe an element', () => {
  const button = element({
    attributes: { title: 'Initial', 'data-nulang-bind-title': 'status' }
  });
  const api = hydrate([button]); // Document hydration already owns button.
  const root = subtree([button]);
  const mounted = api.mount(root);
  assert.equal(api.mount(root), mounted);
  api.signal('status').set('Next');
  assert.deepEqual(button.calls, [['title', 'Next']]);
  mounted.dispose(); // Must not dispose the document-owned binding.
  api.signal('status').set('Last');
  assert.deepEqual(button.calls, [['title', 'Next'], ['title', 'Last']]);
});

test('disposing during a batch suppresses pending writes to the detached scope', () => {
  const button = element({
    attributes: { title: 'Before', 'data-nulang-bind-title': 'status' }
  });
  const api = hydrate([]);
  const root = subtree([button]);
  const owner = api.mount(root);
  api.batch(() => {
    api.signal('status').set('During');
    owner.dispose();
  });
  assert.equal(button.getAttribute('title'), 'Before');
  assert.equal(button.calls.length, 0);
  api.mount(root);
  assert.equal(button.getAttribute('title'), 'During');
});

test('disposed scope removes action listeners and can be mounted again', () => {
  let clicked = 0;
  const api = hydrate([], { increment() { clicked += 1; } });
  const action = element({
    tagName: 'BUTTON',
    attributes: { 'data-action': 'increment', 'data-action-placement': 'client' }
  });
  action.dataset.action = 'increment';
  action.dataset.actionPlacement = 'client';
  const listeners = new Map();
  action.addEventListener = (type, fn) => {
    if (!listeners.has(type)) listeners.set(type, new Set());
    listeners.get(type).add(fn);
  };
  action.removeEventListener = (type, fn) => listeners.get(type)?.delete(fn);
  function click() {
    for (const fn of listeners.get('click') || []) fn({ preventDefault() {} });
  }
  const root = subtree([action]);
  const owner = api.mount(root);
  click();
  assert.equal(clicked, 1);
  owner.dispose();
  assert.equal(listeners.get('click').size, 0);
  click();
  assert.equal(clicked, 1);

  api.mount(root);
  click();
  assert.equal(clicked, 2);
  assert.equal(listeners.get('click').size, 1);
});

test('mount rejects invalid roots without changing reactive state', () => {
  const api = hydrate([]);
  assert.throws(() => api.mount(null), /mount requires a root/);
  assert.throws(() => api.mount({}), /mount requires a root/);
  api.signal('value').set('Still usable');
  assert.equal(api.signal('value').get(), 'Still usable');
});
