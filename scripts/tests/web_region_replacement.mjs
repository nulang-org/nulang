// DOM region replacement is the lifecycle boundary used by future conditional
// branches and server patches. No HTML string parsing is performed here.
// Run: node scripts/tests/web_region_replacement.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const rust = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = rust.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"(.*?)"#\s*\.to_string\(\)/s);
assert.ok(match, 'generated client runtime is extractable');

function element(attributes = {}, tagName = 'DIV', content = '') {
  const values = new Map(Object.entries(attributes));
  const listeners = new Map();
  return {
    tagName, textContent: content, parentNode: null, children: [],
    get dataset() {
      return { signal: values.get('data-signal'), action: values.get('data-action'),
        actionPlacement: values.get('data-action-placement') };
    },
    get attributes() { return [...values].map(([name, value]) => ({ name, value })); },
    getAttribute(name) { return values.get(name) ?? null; },
    hasAttribute(name) { return values.has(name); },
    setAttribute(name, value) { values.set(name, String(value)); },
    removeAttribute(name) { values.delete(name); },
    addEventListener(name, fn) {
      if (!listeners.has(name)) listeners.set(name, new Set());
      listeners.get(name).add(fn);
    },
    removeEventListener(name, fn) { listeners.get(name)?.delete(fn); },
    listenerCount(name) { return listeners.get(name)?.size ?? 0; },
    dispatch(name) { listeners.get(name)?.forEach(fn => fn({ preventDefault() {} })); },
    matches(selector) { return selector.startsWith('[') && values.has(selector.slice(1, -1)); },
    querySelectorAll(selector) {
      return this.children.flatMap(child => [
        ...(child.matches(selector) ? [child] : []),
        ...child.querySelectorAll(selector),
      ]);
    },
  };
}

function append(parent, ...children) {
  for (const child of children) {
    assert.equal(child.parentNode, null);
    parent.children.push(child);
    child.parentNode = parent;
  }
  return parent;
}
function attachReplace(parent) {
  parent.replaceChild = function (next, prior) {
    const index = parent.children.indexOf(prior);
    if (index < 0 || next.parentNode) throw new Error('invalid node replacement');
    parent.children[index] = next;
    prior.parentNode = null;
    next.parentNode = parent;
  };
  return parent;
}

const root = attachReplace(element());
const previous = append(element(), element({ 'data-signal': 'status' }, 'SPAN', 'old'),
  element({ 'data-action': 'tap', 'data-action-placement': 'client' }, 'BUTTON'));
const after = element({}, 'FOOTER');
append(root, element({}, 'HEADER'), previous, after);
const document = { readyState: 'complete', querySelectorAll: q => root.querySelectorAll(q) };
let clicks = 0;
const window = { nulangActions: { tap() { clicks++; } } };
runInNewContext(match[1], { document, window }, { timeout: 1000 });
const oldLabel = previous.children[0], oldButton = previous.children[1];
assert.equal(oldButton.listenerCount('click'), 1);
assert.equal(window.nulang.signal('status').get(), 'old');
assert.equal(typeof window.nulang.replaceRegion, 'function', 'replacement API must exist');

const replacement = append(element(), element({ 'data-signal': 'status' }, 'SPAN', 'stale'),
  element({ 'data-action': 'tap', 'data-action-placement': 'client' }, 'BUTTON'));
const nextLabel = replacement.children[0], nextButton = replacement.children[1];
const installed = window.nulang.replaceRegion(previous, replacement);
assert.equal(installed, replacement);
assert.deepEqual(root.children, [root.children[0], replacement, after], 'sibling order preserved');
assert.equal(previous.parentNode, null, 'old subtree is detached');
assert.equal(nextLabel.textContent, 'old', 'new subtree reconciles with existing signal');
assert.equal(oldButton.listenerCount('click'), 0, 'old action listeners are cleaned');
assert.equal(nextButton.listenerCount('click'), 1, 'new action listeners are installed once');
oldButton.dispatch('click');
nextButton.dispatch('click');
assert.equal(clicks, 1, 'detached action is inactive');
window.nulang.signal('status').set('current');
assert.equal(oldLabel.textContent, 'old', 'old bindings no longer receive updates');
assert.equal(nextLabel.textContent, 'current', 'new bindings receive updates');

assert.throws(() => window.nulang.replaceRegion(previous, element()), /parent|attached/,
  'detached current node is rejected');
assert.throws(() => window.nulang.replaceRegion(replacement, after), /detached|already/,
  'connected next node is rejected');
assert.equal(nextButton.listenerCount('click'), 1, 'invalid replace does not dispose live actions');
assert.equal(root.children[1], replacement, 'invalid replace preserves tree');

const bad = append(element(), element({ 'data-action': 'tap',
  'data-action-placement': 'client' }, 'BUTTON'));
const brokenParent = root.replaceChild;
root.replaceChild = () => { throw new Error('synthetic replacement failure'); };
assert.throws(() => window.nulang.replaceRegion(replacement, bad), /synthetic replacement/);
root.replaceChild = brokenParent;
assert.equal(root.children[1], replacement, 'failed replace preserves current root');
assert.equal(nextButton.listenerCount('click'), 1, 'failed replace rehydrates previous root');
nextButton.dispatch('click');
assert.equal(clicks, 2, 'rollback keeps original action usable');

console.log('Nulang DOM region replacement smoke passed');
