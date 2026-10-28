// Regression tests for progressive HTML updates from Nulang server actions.
// Run: node scripts/tests/web_action_patch.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('../../src/web/reactivity.rs', import.meta.url), 'utf8');
const match = source.match(/pub fn generate_client_runtime\(\) -> String \{\s*r#"(.*?)"#\s*\.to_string\(\)/s);
assert.ok(match, 'expected generated Nulang browser runtime');

async function runAction({ target, body, ok = true, contentType = 'text/html; charset=utf-8' }) {
  const events = new Map();
  const errors = [];
  let reloads = 0;
  let replacements = 0;
  let registrations = 0;
  let clientInvocations = 0;
  let requestedUrl;
  let submittedBody;
  let current = { id: 'status', textContent: 'Before', replaceWith(node) { current = node; replacements++; } };
  const form = {
    tagName: 'FORM',
    dataset: { action: 'update', actionPlacement: 'server', ...(target ? { nulangTarget: target } : {}) },
    closest(selector) { return selector === 'form' ? form : null; },
    addEventListener(name, fn) { events.set(name, fn); registrations++; }
  };
  const document = {
    readyState: 'complete',
    querySelectorAll(selector) {
      return selector === '[data-action]'
        ? [form, ...(current.childAction ? [current.childAction] : [])]
        : [];
    },
    querySelector(selector) { return selector === '#status' ? current : null; },
    importNode(node) { return { ...node }; }
  };
  class DOMParser {
    parseFromString(html, type) {
      assert.equal(type, 'text/html');
      return {
        querySelector(selector) {
          if (selector !== '#status') return null;
          const fragment = html.match(/<div id="status">([\s\S]*?)<\/div>/);
          if (!fragment) return null;
          const node = { id: 'status', textContent: fragment[1].replace(/<[^>]*>/g, '') };
          if (fragment[1].includes('data-action="ping"')) {
            const listeners = new Map();
            node.childAction = {
              tagName: 'BUTTON',
              dataset: { action: 'ping', actionPlacement: 'client' },
              addEventListener(name, fn) { listeners.set(name, fn); },
              closest() { return null; },
              click() { listeners.get('click')?.({ preventDefault() {} }); }
            };
          }
          return node;
        }
      };
    }
  }
  class FormData {
    forEach(fn) { fn('A title', 'title'); }
  }
  const fetch = async (url, options) => {
    requestedUrl = url;
    submittedBody = options.body;
    return {
      ok,
      status: ok ? 200 : 500,
      headers: { get(name) { return name.toLowerCase() === 'content-type' ? contentType : null; } },
      text: async () => body
    };
  };
  const window = {
    location: { href: 'http://localhost/todos', reload() { reloads++; } },
    nulangActions: { ping() { clientInvocations++; } }
  };
  runInNewContext(match[1], {
    document, window, DOMParser, FormData, URLSearchParams, fetch,
    console: { error(...args) { errors.push(args.map(String).join(' ')); }, warn() {} }
  }, { timeout: 1000 });
  assert.ok(events.has('submit'));
  events.get('submit')({ preventDefault() {} });
  await new Promise(resolve => setImmediate(resolve));
  current.childAction?.click();
  return { text: current.textContent, reloads, replacements, registrations, clientInvocations, errors, requestedUrl, submittedBody };
}

const fragment = await runAction({ target: '#status', body: '<div id="status">Updated</div>' });
assert.equal(fragment.text, 'Updated', 'server action patches matching HTML fragment');
assert.equal(fragment.replacements, 1, 'updates only the declared DOM target');
assert.equal(fragment.reloads, 0, 'successful patch does not reload');
assert.equal(fragment.registrations, 1, 'rehydration cannot duplicate action listeners');
assert.equal(fragment.requestedUrl, 'http://localhost/todos');
assert.equal(fragment.submittedBody.get('title'), 'A title');
assert.equal(fragment.submittedBody.get('__nulang_action'), 'update');
assert.ok(fragment.submittedBody.get('__nulang_ui_message'), 'canonical action envelope retained');

const page = await runAction({ target: '#status', body: '<!doctype html><html><body><div id="status">From page</div></body></html>' });
assert.equal(page.text, 'From page', 'can select a target from a full HTML response');
assert.equal(page.reloads, 0);

const rehydrated = await runAction({
  target: '#status',
  body: '<div id="status"><button data-action="ping">Ping</button></div>'
});
assert.equal(rehydrated.clientInvocations, 1, 'new interactive elements are hydrated after a patch');
assert.equal(rehydrated.registrations, 1, 'original action is not wired twice');

const legacy = await runAction({ body: '<div id="status">Legacy</div>' });
assert.equal(legacy.reloads, 1, 'actions without opt-in preserve reload behavior');
assert.equal(legacy.replacements, 0);

const missing = await runAction({ target: '#status', body: '<div id="wrong">Oops</div>' });
assert.equal(missing.replacements, 0, 'unmatched response cannot replace unrelated DOM');
assert.equal(missing.reloads, 0, 'unmatched response fails closed');
assert.ok(missing.errors.length > 0, 'missing target surfaces an error');

const invalid = await runAction({ target: 'body > div', body: '<div id="status">Invalid</div>' });
assert.equal(invalid.replacements, 0, 'arbitrary CSS selectors cannot target unrelated DOM');
assert.ok(invalid.errors.length > 0);

const failure = await runAction({ target: '#status', body: '<div id="status">Bad</div>', ok: false });
assert.equal(failure.replacements, 0, 'failed HTTP response cannot patch');
assert.equal(failure.reloads, 0);
assert.ok(failure.errors.length > 0);

const unexpected = await runAction({ target: '#status', body: '<div id="status">Bad</div>', contentType: 'application/json' });
assert.equal(unexpected.replacements, 0, 'non-HTML responses are rejected');
assert.ok(unexpected.errors.length > 0);
console.log('Nulang Web: targeted server action HTML updates preserve legacy behavior and fail closed');
