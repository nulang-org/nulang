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

function hydrate(elements) {
  const window = {};
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
