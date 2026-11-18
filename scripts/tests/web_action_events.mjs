// Regression: a form must invoke its action on submit, not on every click
// bubbling from an input, while standalone action buttons still use click.
// This executes the JavaScript emitted by the Rust compiler, without packages.
// Run: node scripts/tests/web_action_events.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"(.*?)"#\s*\.to_string\(\)/s);
assert.ok(match, 'expected compiler-generated client runtime');

function element(tagName, action) {
  const listeners = new Map();
  return {
    tagName,
    dataset: { action, actionPlacement: 'client' },
    addEventListener(name, callback) { listeners.set(name, callback); },
    dispatch(name) {
      let prevented = false;
      const listener = listeners.get(name);
      if (listener) listener({ preventDefault() { prevented = true; } });
      return prevented;
    },
    events() { return [...listeners.keys()]; },
  };
}

const form = element('FORM', 'save');
const button = element('BUTTON', 'increment');
const document = {
  readyState: 'complete',
  querySelectorAll(selector) {
    return selector === '[data-action]' ? [form, button] : [];
  },
};
const invoked = [];
const window = {
  nulangActions: {
    save() { invoked.push('save'); },
    increment() { invoked.push('increment'); },
  },
};
runInNewContext(match[1], { document, window }, { timeout: 1000 });

assert.deepEqual(form.events(), ['submit'], 'a form must not listen for bubbling clicks');
assert.equal(form.dispatch('click'), false, 'clicking a form input is not an action');
assert.deepEqual(invoked, []);
assert.equal(form.dispatch('submit'), true, 'submit is intercepted');
assert.deepEqual(invoked, ['save']);
assert.deepEqual(button.events(), ['click'], 'standalone button listens for click only');
assert.equal(button.dispatch('click'), true);
assert.deepEqual(invoked, ['save', 'increment']);
console.log('Nulang Web: form submission and button clicks invoke exactly their intended actions');
