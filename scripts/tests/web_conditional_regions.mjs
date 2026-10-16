// Conditional regions are compiler-marked SSR branch roots. Browser callers
// must supply an already-created, detached alternate branch. No HTML strings
// or client-side evaluation of Nulang server effects is performed here.
// Run: node scripts/tests/web_conditional_regions.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"(.*?)"#\s*\.to_string\(\)/s);
assert.ok(match, 'generated runtime not found');

function element(attrs = {}, tagName = 'SECTION', content = '') {
  const values = new Map(Object.entries(attrs));
  const listeners = new Map();
  return {
    tagName, textContent: content, parentNode: null, children: [],
    get dataset() { return {
      signal: values.get('data-signal'), action: values.get('data-action'),
      actionPlacement: values.get('data-action-placement'),
    }; },
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
    matches(query) { return query.startsWith('[') && values.has(query.slice(1, -1)); },
    querySelectorAll(query) {
      return this.children.flatMap(child => [
        ...(child.matches(query) ? [child] : []), ...child.querySelectorAll(query),
      ]);
    },
    replaceChild(next, old) {
      const idx = this.children.indexOf(old);
      if (idx < 0 || next.parentNode) throw new Error('cannot replace');
      this.children[idx] = next; old.parentNode = null; next.parentNode = this;
    },
  };
}
function append(parent, ...nodes) {
  for (const node of nodes) {
    assert.equal(node.parentNode, null);
    parent.children.push(node); node.parentNode = parent;
  }
  return parent;
}
const on = append(element({ 'data-nulang-if': 'visible', 'data-nulang-branch': 'then' }),
  element({ 'data-action': 'tap', 'data-action-placement': 'client' }, 'BUTTON'));
const root = append(element(), element({}, 'HEADER'), on, element({}, 'FOOTER'));
const document = { readyState: 'complete', querySelectorAll: query => root.querySelectorAll(query) };
const invoked = [];
const window = { nulangActions: { tap() { invoked.push('tap'); } } };
runInNewContext(match[1], { document, window }, { timeout: 1000 });

assert.equal(window.nulang.signal('visible').get(), true, 'SSR active branch initializes Bool');
assert.equal(typeof window.nulang.replaceConditional, 'function');
assert.equal(on.children[0].listenerCount('click'), 1);
const off = append(element({ 'data-nulang-if': 'visible', 'data-nulang-branch': 'else' }),
  element({ 'data-signal': 'label' }, 'SPAN', 'off'),
  element({ 'data-action': 'tap', 'data-action-placement': 'client' }, 'BUTTON'));
assert.equal(window.nulang.replaceConditional(on, off), off);
assert.equal(root.children[1], off);
assert.equal(window.nulang.signal('visible').get(), false, 'successful switch updates Bool');
assert.equal(on.children[0].listenerCount('click'), 0, 'inactive branch action disposed');
assert.equal(off.children[1].listenerCount('click'), 1);
on.children[0].dispatch('click');
off.children[1].dispatch('click');
assert.deepEqual(invoked, ['tap']);

const foreign = element({ 'data-nulang-if': 'other', 'data-nulang-branch': 'then' });
assert.throws(() => window.nulang.replaceConditional(off, foreign), /signal|region/i);
assert.equal(root.children[1], off, 'wrong identity has no DOM effect');

const sameBranch = element({ 'data-nulang-if': 'visible', 'data-nulang-branch': 'else' });
assert.throws(() => window.nulang.replaceConditional(off, sameBranch), /branch|alternate/i);
assert.equal(root.children[1], off, 'same branch has no DOM effect');
const missingMetadata = element();
assert.throws(() => window.nulang.replaceConditional(off, missingMetadata), /region|branch/i);
assert.equal(root.children[1], off);

const back = element({ 'data-nulang-if': 'visible', 'data-nulang-branch': 'then' });
assert.equal(window.nulang.replaceConditional(off, back), back);
assert.equal(window.nulang.signal('visible').get(), true);
assert.equal(off.children[1].listenerCount('click'), 0);
assert.equal(root.children[1], back);

const fail = element({ 'data-nulang-if': 'visible', 'data-nulang-branch': 'else' });
const replace = root.replaceChild;
root.replaceChild = () => { throw new Error('synthetic DOM failure'); };
assert.throws(() => window.nulang.replaceConditional(back, fail), /synthetic DOM failure/);
root.replaceChild = replace;
assert.equal(root.children[1], back, 'old branch retained after failure');
assert.equal(window.nulang.signal('visible').get(), true, 'failed switch retains signal state');
console.log('Nulang compiler conditional-region contract smoke passed');
