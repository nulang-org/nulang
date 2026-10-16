// Regression tests against the browser runtime embedded in src/web/reactivity.rs.
// Run: node --test tests/web_action_payload.test.cjs
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

function emittedRuntime() {
  const rust = fs.readFileSync(path.join(__dirname, '../src/web/reactivity.rs'), 'utf8');
  const generator = rust.indexOf('pub fn generate_client_runtime() -> String {');
  assert.notEqual(generator, -1, 'expected compiler browser-runtime generator');
  const first = rust.indexOf('r#"', generator);
  const last = rust.indexOf('"#', first + 3);
  assert.ok(first >= 0 && last > first, 'expected raw JS runtime source');
  return rust.slice(first + 3, last);
}

function serializeFromEmittedRuntime(entries) {
  const runtime = emittedRuntime();
  const anchor = '  function createActionMessage(handler, placement, body) {';
  assert.ok(runtime.includes(anchor), 'expected action-message serializer');
  const instrumented = runtime.replace(
    anchor,
    '  window.__testSerializeForm = wireFormPayload;\n' + anchor
  );
  const window = {};
  vm.runInNewContext(instrumented, {
    window,
    document: { readyState: 'loading', addEventListener() {} },
    console: { warn() {}, error() {} }
  });
  // Simulates URLSearchParams' ordered entries without requiring a browser.
  const body = { forEach(callback) {
    for (const [name, value] of entries) callback(value, name);
  }};
  return window.__testSerializeForm(body);
}

test('action payload preserves form fields named after Object prototype properties', () => {
  const input = [
    ['title', 'ordinary field'],
    ['__proto__', 'literal user input'],
    ['constructor', 'another field'],
    ['toString', 'a third field']
  ];
  const payload = serializeFromEmittedRuntime(input);
  const decoded = JSON.parse(JSON.stringify(payload));

  assert.equal(decoded.type, 'object');
  assert.deepEqual(Object.keys(decoded.value), input.map(([name]) => name));
  for (const [name, value] of input) {
    assert.deepEqual(decoded.value[name], { type: 'string', value });
  }
});

test('repeated ordinary fields keep the existing last-value behavior', () => {
  const payload = serializeFromEmittedRuntime([
    ['message', 'first'],
    ['message', 'second']
  ]);
  assert.deepEqual(JSON.parse(JSON.stringify(payload)), {
    type: 'object',
    value: { message: { type: 'string', value: 'second' } }
  });
});


test('server form action serializes prototype-named fields into the actual POST wire message', async () => {
  const received = [];
  const listeners = Object.create(null);
  let reloads = 0;
  let prevented = false;
  const entries = [
    ['__proto__', 'literal user input'],
    ['title', 'Example'],
    ['constructor', 'not an inherited method']
  ];
  const form = {
    dataset: { action: 'save', actionPlacement: 'server' },
    tagName: 'FORM',
    closest(selector) {
      assert.equal(selector, 'form');
      return this;
    },
    addEventListener(event, listener) {
      listeners[event] = listener;
    }
  };
  const window = {
    location: {
      href: 'https://example.test/items/new',
      reload() { reloads++; }
    }
  };
  class FakeFormData {
    constructor(el) { assert.equal(el, form); }
    forEach(callback) {
      for (const [name, value] of entries) callback(value, name);
    }
  }

  vm.runInNewContext(emittedRuntime(), {
    window,
    URLSearchParams,
    FormData: FakeFormData,
    fetch: async (url, options) => {
      received.push({ url, options });
      return { ok: true };
    },
    document: {
      readyState: 'complete',
      querySelectorAll(selector) {
        return selector === '[data-action]' ? [form] : [];
      }
    },
    console: {
      warn() {},
      error(err) { throw err; }
    }
  });

  assert.equal(typeof listeners.submit, 'function');
  listeners.submit({ preventDefault() { prevented = true; } });
  // The event listener starts an asynchronous server action internally.
  for (let i = 0; i < 4; i++) await Promise.resolve();

  assert.equal(prevented, true);
  assert.equal(received.length, 1, 'must submit a single server action');
  assert.equal(received[0].url, window.location.href);
  assert.equal(received[0].options.method, 'POST');
  const body = received[0].options.body;
  assert.equal(body.get('__proto__'), 'literal user input');
  const message = JSON.parse(body.get('__nulang_ui_message'));
  assert.equal(message.request.action_id, 'save');
  assert.deepEqual(Object.keys(message.request.payload.value), entries.map(([name]) => name));
  for (const [name, value] of entries) {
    assert.deepEqual(message.request.payload.value[name], { type: 'string', value });
  }
  assert.equal(reloads, 1);
});

test('form actions run on submit, never on ordinary clicks inside the form', async () => {
  const listeners = Object.create(null);
  const requests = [];
  let reloadCount = 0;
  const form = {
    dataset: { action: 'update', actionPlacement: 'server' },
    tagName: 'FORM',
    closest(selector) {
      assert.equal(selector, 'form');
      return this;
    },
    addEventListener(event, callback) {
      listeners[event] = callback;
    }
  };
  class FakeFormData {
    constructor(element) { assert.equal(element, form); }
    forEach(callback) { callback('update', 'operation'); }
  }
  const win = {
    location: {
      href: 'https://example.test/items',
      reload() { reloadCount++; }
    }
  };
  vm.runInNewContext(emittedRuntime(), {
    window: win,
    URLSearchParams,
    FormData: FakeFormData,
    document: {
      readyState: 'complete',
      querySelectorAll(selector) {
        return selector === '[data-action]' ? [form] : [];
      }
    },
    fetch: async (url, options) => {
      requests.push({ url, options });
      return { ok: true };
    },
    console: { warn() {}, error(error) { throw error; } }
  });

  // A click on an input, label or other child bubbles to the form.
  // The form must not listen for clicks, or merely focusing an input posts data.
  assert.equal(listeners.click, undefined, 'form clicks must not trigger server actions');
  assert.equal(requests.length, 0);
  assert.equal(typeof listeners.submit, 'function');
  let prevented = false;
  listeners.submit({ preventDefault() { prevented = true; } });
  for (let i = 0; i < 4; i++) await Promise.resolve();
  assert.equal(prevented, true);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].options.method, 'POST');
  assert.equal(reloadCount, 1);
});

test('non-form action elements retain click handling', () => {
  const listeners = Object.create(null);
  let calls = 0;
  const button = {
    dataset: { action: 'open', actionPlacement: 'client' },
    tagName: 'BUTTON',
    addEventListener(event, callback) {
      listeners[event] = callback;
    }
  };
  vm.runInNewContext(emittedRuntime(), {
    window: {
      nulangActions: { open() { calls++; } }
    },
    document: {
      readyState: 'complete',
      querySelectorAll(selector) {
        return selector === '[data-action]' ? [button] : [];
      }
    },
    console: { warn() {}, error(error) { throw error; } }
  });
  assert.equal(typeof listeners.click, 'function');
  assert.equal(listeners.submit, undefined);
  listeners.click({ preventDefault() {} });
  assert.equal(calls, 1);
});

test('rapid duplicate submits share one in-flight action and failure permits retry', async () => {
  const listeners = Object.create(null);
  const pending = [];
  const errors = [];
  let reloads = 0;
  const form = {
    tagName: 'FORM',
    dataset: { action: 'create', actionPlacement: 'server' },
    closest(selector) {
      assert.equal(selector, 'form');
      return this;
    },
    addEventListener(event, listener) {
      listeners[event] = listener;
    }
  };
  class FakeFormData {
    constructor(element) { assert.equal(element, form); }
    forEach(callback) { callback('draft', 'title'); }
  }

  vm.runInNewContext(emittedRuntime(), {
    window: {
      location: {
        href: 'https://example.test/records',
        reload() { reloads++; }
      }
    },
    URLSearchParams,
    FormData: FakeFormData,
    document: {
      readyState: 'complete',
      querySelectorAll(selector) {
        return selector === '[data-action]' ? [form] : [];
      }
    },
    fetch: (_url, options) => new Promise((resolve) => {
      pending.push({ resolve, options });
    }),
    console: { warn() {}, error(error) { errors.push(error); } }
  });

  const event = { preventDefault() {} };
  listeners.submit(event);
  listeners.submit(event);
  assert.equal(pending.length, 1,
    'a second submit while the first POST is unresolved must not post again');

  // A server rejection must unblock the form so that the user can retry.
  pending[0].resolve({ ok: false, status: 409 });
  for (let i = 0; i < 6; i++) await Promise.resolve();
  assert.equal(errors.length, 1);
  assert.equal(reloads, 0);
  listeners.submit(event);
  assert.equal(pending.length, 2, 'form must be retryable after a failed POST');
  pending[1].resolve({ ok: true });
  for (let i = 0; i < 6; i++) await Promise.resolve();
  assert.equal(reloads, 1);
});
