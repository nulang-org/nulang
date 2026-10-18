// Regression for wrapper-free, comment-anchored reactive text updates.
// Runs the exact client runtime emitted by src/web/reactivity.rs.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const rust = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = rust.slice(rust.indexOf('pub fn generate_client_runtime() -> String {'))
  .match(/r#"([\s\S]*?)"#\s*\.to_string\(\)/);
assert.ok(match, 'generated JavaScript should be extractable');

function hexUtf16(name) {
  return [...name].flatMap(ch => {
    const result = [];
    for (let i = 0; i < ch.length; i++) {
      result.push(ch.charCodeAt(i).toString(16).padStart(4, '0'));
    }
    return result;
  }).join('');
}

function makeRegion(signal, content) {
  const encoded = hexUtf16(signal);
  const start = { nodeType: 8, data: 'nulang-signal:' + encoded + ':start' };
  const initial = { nodeType: 3, textContent: content };
  const end = { nodeType: 8, data: 'nulang-signal:' + encoded + ':end' };
  const parent = {
    childNodes: [start, initial, end],
    insertBefore(node, before) {
      const index = this.childNodes.indexOf(before);
      assert.ok(index >= 0, 'anchor remains attached');
      this.childNodes.splice(index, 0, node);
      node.parentNode = this;
    },
  };
  for (const node of parent.childNodes) {
    node.parentNode = parent;
    Object.defineProperty(node, 'nextSibling', {
      get() { return this.parentNode?.childNodes[this.parentNode.childNodes.indexOf(this) + 1] ?? null; },
    });
    node.remove = function() {
      const index = this.parentNode.childNodes.indexOf(this);
      this.parentNode.childNodes.splice(index, 1);
      this.parentNode = null;
    };
  }
  return { start, end, parent };
}

const a = makeRegion('count', '2');
const b = makeRegion('count', '2');
const tricky = makeRegion('constructor', 'initial');
const comments = [a.start, a.end, b.start, b.end, tricky.start, tricky.end];
const document = {
  readyState: 'complete',
  createTreeWalker(root, type) {
    assert.equal(root, document);
    assert.equal(type, 128, 'comment-node walker only');
    let position = 0;
    return { nextNode: () => comments[position++] ?? null };
  },
  createTextNode(value) {
    const node = { nodeType: 3, textContent: String(value) };
    Object.defineProperty(node, 'nextSibling', {
      get() { return this.parentNode?.childNodes[this.parentNode.childNodes.indexOf(this) + 1] ?? null; },
    });
    node.remove = function() {
      this.parentNode.childNodes.splice(this.parentNode.childNodes.indexOf(this), 1);
      this.parentNode = null;
    };
    return node;
  },
  querySelectorAll(selector) {
    if (['[data-signal]', '[data-nulang-attrs]', '[data-action]'].includes(selector)) return [];
    throw new Error('unexpected selector: ' + selector);
  },
};
const window = {};
runInNewContext(match[1], { window, document, console, URLSearchParams });

assert.equal(window.nulang.signal('count').get(), '2', 'hydrate initial text from DOM');
window.nulang.signal('count').set('3');
for (const region of [a, b]) {
  assert.deepEqual(region.parent.childNodes.map(n => n.nodeType), [8, 3, 8], 'no wrapper element');
  assert.equal(region.parent.childNodes[1].textContent, '3', 'each binding updates');
  assert.equal(region.parent.childNodes[0], region.start, 'start anchor retained');
  assert.equal(region.parent.childNodes[2], region.end, 'end anchor retained');
}
window.nulang.signal('constructor').set('<img src=x>');
assert.equal(tricky.parent.childNodes[1].textContent, '<img src=x>', 'untrusted text stays text');
console.log('comment-anchored signal hydration regressions passed');
