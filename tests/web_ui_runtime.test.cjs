const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const test = require('node:test');
const path = require('node:path');
const runtime = fs.readFileSync(path.join(__dirname, '../src/web/client_runtime.js'), 'utf8');

function boot({ withAction = false, fetchResponse } = {}) {
  const span = { dataset: { signal: 'count' }, textContent: '0' };
  const listeners = new Map();
  const action = {
    dataset: { action: 'save', actionPlacement: 'server' },
    tagName: 'FORM',
    closest: () => null,
    addEventListener(name, fn) { listeners.set(name, fn); }
  };
  const document = {
    readyState: 'complete',
    querySelectorAll(selector) {
      if (selector === '[data-signal]') return [span];
      return selector === '[data-action]' && withAction ? [action] : [];
    }
  };
  let reloaded = 0;
  let posted = null;
  const context = {
    window: { location: { href: '/tasks', reload() { reloaded++; } } },
    document,
    URLSearchParams,
    FormData: class { forEach() {} },
    fetch: async (_, request) => { posted = request; return fetchResponse; },
    console,
    crypto: { randomUUID: () => 'test-id' },
  };
  vm.runInNewContext(runtime, context);
  return { span, app: context.window.nulang, listeners, reloaded: () => reloaded, posted: () => posted };
}

const wire = (value) => ({ type: 'string', value });
const patch = (base, next, value, extra = []) => ({
  protocol: 'nulang-ui-msg/1',
  type: 'patch',
  patch: { protocol: 'nulang-ui/1', document_id: 'app', base_revision: base, revision: next,
    operations: [{ op: 'set_property', node_id: 'signal:0:count', name: 'value', value: wire(value) }, ...extra] }
});

test('applies a canonical patch and advances the revision', () => {
  const { app, span } = boot();
  assert.equal(app.receiveUiMessage(patch('0', '1', '5')), true);
  assert.equal(span.textContent, '5');
  assert.equal(app.uiRevision(), '1');
});

test('rejects stale, foreign, and unsupported patches without partial mutation', () => {
  const { app, span } = boot();
  const unsupported = patch('0', '1', '5', [{ op: 'remove_node', node_id: 'read:0:count' }]);
  assert.equal(app.receiveUiMessage(unsupported), false);
  assert.equal(span.textContent, '0');
  assert.equal(app.uiRevision(), '0');
  const foreign = patch('0', '1', '5');
  foreign.patch.document_id = 'someone-else';
  assert.equal(app.receiveUiMessage(foreign), false);
  assert.equal(app.receiveUiMessage(patch('1', '2', '5')), false);
  assert.equal(span.textContent, '0');
});

test('uses canonical snapshot to resynchronize the current document', () => {
  const { app, span } = boot();
  assert.equal(app.receiveUiMessage({ type: 'snapshot', protocol: 'nulang-ui-msg/1', document: {
    protocol: 'nulang-ui/1', document_id: 'app', revision: '7', root: 'root',
    nodes: [{ kind: 'signal', properties: { name: wire('count'), value: wire('9') } }]
  }}), true);
  assert.equal(span.textContent, '9');
  assert.equal(app.uiRevision(), '7');
  assert.equal(app.receiveUiMessage(patch('7', '8', '10')), true);
  assert.equal(span.textContent, '10');
});

test('POST preserves canonical payload and consumes patch without reload', async () => {
  const response = { ok: true, headers: { get: () => 'application/vnd.nulang-ui+json' }, json: async () => patch('0', '1', '42') };
  const ctx = boot({ withAction: true, fetchResponse: response });
  assert.equal(ctx.listeners.has('click'), false, 'form must not submit twice');
  ctx.listeners.get('submit')({ preventDefault() {} });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(ctx.reloaded(), 0);
  assert.equal(ctx.span.textContent, '42');
  assert.equal(ctx.posted().method, 'POST');
  const message = JSON.parse(ctx.posted().body.get('__nulang_ui_message'));
  assert.equal(message.request.revision, '0');
  assert.equal(message.request.document_id, 'app');
});

test('legacy HTML server action response falls back to reload', async () => {
  const response = { ok: true, headers: { get: () => 'text/html; charset=utf-8' } };
  const ctx = boot({ withAction: true, fetchResponse: response });
  ctx.listeners.get('submit')({ preventDefault() {} });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(ctx.reloaded(), 1);
});
