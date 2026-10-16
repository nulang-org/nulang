// Regression for repeatable scoped hydration and explicit lifecycle disposal.
// Run: node scripts/tests/web_scoped_lifecycle.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const rust = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = rust.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"(.*?)"#\s*\.to_string\(\)/s);
assert.ok(match, 'generated Nulang client runtime must be extractable');

function element(attributes = {}, tagName = 'DIV', textContent = '') {
  const values = new Map(Object.entries(attributes));
  const children = [];
  const listeners = new Map();
  const writes = [];
  const node = {
    tagName, textContent, children, writes,
    get dataset() {
      return { signal: values.get('data-signal'), action: values.get('data-action'),
        actionPlacement: values.get('data-action-placement') };
    },
    get attributes() { return [...values].map(([name, value]) => ({ name, value })); },
    getAttribute(name) { return values.get(name) ?? null; },
    hasAttribute(name) { return values.has(name); },
    setAttribute(name, value) { writes.push([name, String(value)]); values.set(name, String(value)); },
    removeAttribute(name) { writes.push([name, null]); values.delete(name); },
    addEventListener(name, listener) {
      if (!listeners.has(name)) listeners.set(name, new Set());
      listeners.get(name).add(listener);
    },
    removeEventListener(name, listener) { listeners.get(name)?.delete(listener); },
    listeners(name) { return listeners.get(name)?.size ?? 0; },
    dispatch(name) { listeners.get(name)?.forEach(fn => fn({ preventDefault() {} })); },
    matches(selector) { return selector.startsWith('[') && values.has(selector.slice(1, -1)); },
    querySelectorAll(selector) {
      return children.flatMap(child => [ ...(child.matches(selector) ? [child] : []), ...child.querySelectorAll(selector) ]);
    },
  };
  return node;
}

const initial = element();
const label = element({ 'data-signal': 'state' }, 'SPAN', 'initial');
const bound = element({ title: 'initial', 'data-nulang-bind': 'true',
  'data-nulang-bind-title': 'state' });
const button = element({ 'data-action': 'activate', 'data-action-placement': 'client' }, 'BUTTON');
initial.children.push(label, bound, button);
const document = { readyState: 'complete', querySelectorAll: selector => initial.querySelectorAll(selector) };
let calls = 0;
const window = { nulangActions: { activate() { calls++; } } };
runInNewContext(match[1], { document, window }, { timeout: 1000 });

const state = window.nulang.signal('state');
assert.equal(state.get(), 'initial');
assert.equal(button.listeners('click'), 1, 'one action listener after initial hydration');
assert.equal(typeof window.nulang.hydrate, 'function', 'scoped hydrate API');
assert.equal(typeof window.nulang.dispose, 'function', 'scoped dispose API');
window.nulang.hydrate(initial);
window.nulang.hydrate(initial);
assert.equal(button.listeners('click'), 1, 'repeat hydration must not duplicate actions');

state.set('updated');
assert.equal(label.textContent, 'updated');
assert.equal(bound.getAttribute('title'), 'updated');
assert.equal(bound.writes.filter(([name]) => name === 'title').length, 1,
  'repeat hydration must not duplicate attribute updates');

window.nulang.dispose(initial);
assert.equal(button.listeners('click'), 0, 'disposal detaches action listeners');
state.set('after disposal');
assert.equal(label.textContent, 'updated', 'disposed text nodes are not written');
assert.equal(bound.getAttribute('title'), 'updated', 'disposed attrs are not written');
button.dispatch('click');
assert.equal(calls, 0, 'disposed action must not fire');

const replacement = element();
const newLabel = element({ 'data-signal': 'state' }, 'SPAN', 'stale SSR');
const newBound = element({ title: 'stale SSR', 'data-nulang-bind': 'true',
  'data-nulang-bind-title': 'state' });
const newButton = element({ 'data-action': 'activate', 'data-action-placement': 'client' }, 'BUTTON');
replacement.children.push(newLabel, newBound, newButton);
window.nulang.hydrate(replacement);
assert.equal(newLabel.textContent, 'after disposal', 'reused signals reconcile freshly mounted text');
assert.equal(newBound.getAttribute('title'), 'after disposal', 'reused signals reconcile freshly mounted attr');
newButton.dispatch('click');
assert.equal(calls, 1);
window.nulang.dispose(replacement);
window.nulang.hydrate(replacement);
newButton.dispatch('click');
assert.equal(calls, 2, 'remount attaches exactly one action');

console.log('Nulang scoped DOM hydration/disposal regression passed');
