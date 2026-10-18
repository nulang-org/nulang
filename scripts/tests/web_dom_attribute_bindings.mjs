// Exercise the exact generated browser runtime with attribute and text parts.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const rust = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const start = rust.indexOf('pub fn generate_client_runtime() -> String {');
assert.notEqual(start, -1);
const match = rust.slice(start).match(/r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'generated JavaScript should be extractable');

function element(initial) {
  const attributes = new Map(Object.entries(initial));
  return {
    getAttribute(name) { return attributes.has(name) ? attributes.get(name) : null; },
    setAttribute(name, value) { attributes.set(name, String(value)); },
    attributes,
  };
}

const textNode = { dataset: { signal: 'count' }, textContent: '2' };
const label = element({
  'title': '2',
  'aria-label': '2',
  'data-nulang-attrs': 'title:count,aria-label:count',
});
const hostile = element({
  'href': 'safe',
  'onclick': 'safe',
  'data-nulang-attrs': 'href:count,onclick:count,title:constructor',
  'title': 'original',
});
const document = {
  readyState: 'complete',
  querySelectorAll(selector) {
    if (selector === '[data-signal]') return [textNode];
    if (selector === '[data-action]') return [];
    if (selector === '[data-nulang-attrs]') return [label, hostile];
    throw new Error('unknown selector: ' + selector);
  },
};
const window = {};
runInNewContext(match[1], { window, document, console, URLSearchParams });

assert.equal(window.nulang.signal('count').get(), '2');
window.nulang.signal('count').set('3');
assert.equal(textNode.textContent, '3', 'text binding updated');
assert.equal(label.getAttribute('title'), '3', 'title binding updated');
assert.equal(label.getAttribute('aria-label'), '3', 'ARIA binding updated');
assert.equal(hostile.getAttribute('href'), 'safe', 'URL attributes are not writable');
assert.equal(hostile.getAttribute('onclick'), 'safe', 'event attributes are not writable');
window.nulang.signal('constructor').set('changed');
assert.equal(hostile.getAttribute('title'), 'changed', 'prototype-like signal names remain safe');
console.log('web DOM attribute binding regressions passed');
