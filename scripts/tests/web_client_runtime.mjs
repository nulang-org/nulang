// Browser-free smoke test of the actual JavaScript emitted by Nulang Web.
// Run: node scripts/tests/web_client_runtime.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"(.*?)"#\s*\.to_string\(\)/s);
assert.ok(match, 'compiler-generated client runtime must remain extractable');
const clientRuntime = match[1];

function element(attributes, text = '') {
  const values = new Map(Object.entries(attributes));
  return {
    textContent: text,
    // Mirror HTMLElement.dataset, which generated hydration reads directly.
    get dataset() {
      return { signal: values.get('data-signal'),
        action: values.get('data-action'),
        actionPlacement: values.get('data-action-placement') };
    },
    get attributes() {
      return [...values].map(([name, value]) => ({ name, value }));
    },
    hasAttribute(name) { return values.has(name); },
    getAttribute(name) { return values.get(name) ?? null; },
    setAttribute(name, value) { values.set(name, String(value)); },
    removeAttribute(name) { values.delete(name); },
    addEventListener() {},
  };
}

const label = element({ 'data-signal': 'status' }, 'active');
const attributes = element({
  'class': 'active',
  'title': 'active',
  'aria-label': 'active',
  'data-state': 'active',
  'data-nulang-bind': 'true',
  'data-nulang-bind-class': 'status',
  'data-nulang-bind-title': 'status',
  'data-nulang-bind-aria-label': 'status',
  'data-nulang-bind-data-state': 'status',
  // A forged binding cannot enable scriptable/URL-bearing attributes.
  'data-nulang-bind-href': 'status',
  'href': '/safe',
});
const tooltip = element({
  'title': 'before',
  'data-nulang-bind': 'true',
  'data-nulang-bind-title': 'tooltip',
});
const all = [label, attributes, tooltip];
const document = {
  readyState: 'complete',
  querySelectorAll(selector) {
    const key = selector.slice(1, -1);
    return all.filter((el) => el.hasAttribute(key));
  },
};
const window = {};
runInNewContext(clientRuntime, { document, window }, { timeout: 1000 });

const status = window.nulang.signal('status');
assert.equal(status.get(), 'active', 'SSR text initializes the signal');
status.set('inactive');
assert.equal(label.textContent, 'inactive');
for (const name of ['class', 'title', 'aria-label', 'data-state']) {
  assert.equal(attributes.getAttribute(name), 'inactive', name);
}
assert.equal(attributes.getAttribute('href'), '/safe', 'URL attribute remains unchanged');

const title = window.nulang.signal('tooltip');
assert.equal(title.get(), 'before', 'attribute-only signal hydrates');
title.set('after');
assert.equal(tooltip.getAttribute('title'), 'after');
status.set(null);
for (const name of ['class', 'title', 'aria-label', 'data-state']) {
  assert.equal(attributes.hasAttribute(name), false, name + ' removed on null');
}
assert.equal(attributes.getAttribute('href'), '/safe');

console.log('Nulang client runtime: text, safe attributes, hydration and removal passed');
