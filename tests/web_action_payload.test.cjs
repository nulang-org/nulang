// Regression tests against the browser runtime embedded in src/web/reactivity.rs.
// Run: node --test tests/web_action_payload.test.cjs
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

function serializeFromEmittedRuntime(entries) {
  const rust = fs.readFileSync(path.join(__dirname, '../src/web/reactivity.rs'), 'utf8');
  const generator = rust.indexOf('pub fn generate_client_runtime() -> String {');
  assert.notEqual(generator, -1, 'expected compiler browser-runtime generator');
  const first = rust.indexOf('r#"', generator);
  const last = rust.indexOf('"#', first + 3);
  assert.ok(first >= 0 && last > first, 'expected raw JS runtime source');
  const runtime = rust.slice(first + 3, last);
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
