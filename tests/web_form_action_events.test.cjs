// Execute the real compiler-emitted JavaScript; no browser dependencies required.
// Run: node --test tests/web_form_action_events.test.cjs
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

function emittedRuntime() {
  const rust = fs.readFileSync(path.join(__dirname, '../src/web/reactivity.rs'), 'utf8');
  const fn = rust.indexOf('pub fn generate_client_runtime() -> String {');
  assert.ok(fn >= 0, 'runtime generator must exist');
  const start = rust.indexOf('r#"', fn);
  const end = rust.indexOf('"#', start + 3);
  assert.ok(start >= 0 && end > start, 'runtime must use a raw string literal');
  return rust.slice(start + 3, end);
}

function actionElement(tagName, action) {
  return {
    tagName,
    dataset: { action, actionPlacement: 'server' },
    listeners: Object.create(null),
    fields: [['sku', 'item-1']],
    getAttribute(name) {
      return name === 'data-action' ? action : null;
    },
    addEventListener(type, fn) {
      assert.ok(!this.listeners[type], 'duplicate listener: ' + type);
      this.listeners[type] = fn;
    },
    closest() {
      return this.form || this;
    }
  };
}

function setup(elements) {
  const calls = [];
  const window = { location: { href: 'http://localhost:8080/inventory', reload() {} } };
  const document = {
    readyState: 'complete',
    querySelectorAll(selector) {
      return selector === '[data-action]' ? elements : [];
    }
  };
  class TestFormData {
    constructor(form) {
      this.fields = form.fields;
    }
    forEach(fn) {
      for (const [key, value] of this.fields) fn(value, key);
    }
  }
  const fetch = (url, options) => {
    calls.push({ url, options });
    return Promise.resolve({ ok: true });
  };
  vm.runInNewContext(emittedRuntime(), {
    window, document, FormData: TestFormData, URLSearchParams, fetch,
    console: { warn() {}, error() {} }
  });
  return calls;
}

test('form data-action uses submit only, not bubbling click plus submit', () => {
  const form = actionElement('FORM', 'reserve_inventory');
  const calls = setup([form]);
  assert.equal(form.listeners.click, undefined, 'a form click must not dispatch');
  assert.equal(typeof form.listeners.submit, 'function');

  // A submit-button click can bubble to FORM before the submit event fires.
  form.listeners.click?.({ preventDefault() {} });
  form.listeners.submit({ preventDefault() {} });
  assert.equal(calls.length, 1, 'one user submission must emit one action request');
  assert.equal(calls[0].options.method, 'POST');
});

test('non-form action buttons still dispatch on click only', () => {
  const form = actionElement('FORM', '');
  const button = actionElement('BUTTON', 'approve_inventory');
  button.form = form;
  const calls = setup([button]);
  assert.equal(typeof button.listeners.click, 'function');
  assert.equal(button.listeners.submit, undefined);

  button.listeners.click({ preventDefault() {} });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].options.method, 'POST');
});
