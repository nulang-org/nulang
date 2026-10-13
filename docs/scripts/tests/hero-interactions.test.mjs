import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import test from 'node:test';
import { homepageExamples } from '../../src/lib/homepage-examples.mjs';

const component = readFileSync(new URL('../../src/components/Hero.astro', import.meta.url), 'utf8');
const script = component.match(/<script is:inline>([\s\S]*?)<\/script>/)?.[1];
assert.ok(script, 'homepage hero must provide its executable browser controller');

function element() {
  const listeners = new Map();
  const attributes = new Map();
  const classes = new Set();
  return {
    listeners,
    attributes,
    classList: {
      add: (name) => classes.add(name),
      remove: (name) => classes.delete(name),
      contains: (name) => classes.has(name),
    },
    textContent: '',
    value: '',
    disabled: false,
    addEventListener: (type, fn) => listeners.set(type, fn),
    getAttribute: (name) => attributes.get(name) ?? null,
    setAttribute: (name, value) => attributes.set(name, String(value)),
    trigger(type, event) { return listeners.get(type)?.(event); },
  };
}

function mountHero({ fetchCompiler } = {}) {
  const source = element();
  const run = element();
  const label = element();
  const output = element();
  source.value = homepageExamples[0].code;

  const buttons = homepageExamples.map(({ code }, index) => {
    const button = element();
    button.setAttribute('data-hero-example-code', code);
    button.setAttribute('aria-pressed', String(index === 0));
    return button;
  });
  const controls = {
    '.hero-source': source,
    '.hero-run': run,
    '.run-label': label,
    '.hero-output': output,
  };
  const root = {
    querySelector(selector) { return controls[selector] ?? null; },
    querySelectorAll(selector) {
      return selector === '[data-hero-example]' ? buttons : [];
    },
  };

  const memory = { buffer: new ArrayBuffer(1024 * 64) };
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();
  const invocations = [];
  const exports = {
    memory,
    nulang_alloc: () => 256,
    nulang_free: () => {},
    nulang_run: (ptr, size) => {
      invocations.push(decoder.decode(new Uint8Array(memory.buffer, ptr, size)));
      const result = encoder.encode(JSON.stringify({ ok: true, output: 'Example ran\n' }));
      new DataView(memory.buffer).setUint32(1024, result.length, true);
      new Uint8Array(memory.buffer, 1028, result.length).set(result);
      return 1024;
    },
  };
  const WebAssembly = { instantiate: async () => ({ instance: { exports } }) };
  const defaultFetch = async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(0) });

  runInNewContext(script, {
    document: { querySelector: () => root },
    fetch: fetchCompiler ?? defaultFetch,
    WebAssembly,
    TextEncoder,
    TextDecoder,
    Uint8Array,
    DataView,
  });
  return { buttons, source, run, label, output, invocations };
}

test('example selection updates editor and pressed state without running code', () => {
  const { buttons, source, output, invocations } = mountHero();
  buttons[2].trigger('click');
  assert.equal(source.value, homepageExamples[2].code);
  assert.deepEqual(buttons.map((button) => button.getAttribute('aria-pressed')), [
    'false', 'false', 'true',
  ]);
  assert.match(output.textContent, /Example loaded/);
  assert.equal(invocations.length, 0);
});

test('click and Ctrl+Enter execute the selected example', async () => {
  const { buttons, source, run, output, invocations } = mountHero();
  buttons[1].trigger('click');
  await run.trigger('click');
  assert.equal(invocations[0], homepageExamples[1].code);
  assert.equal(output.textContent, 'Example ran\n');

  let prevented = false;
  source.trigger('keydown', {
    key: 'Enter',
    ctrlKey: true,
    metaKey: false,
    preventDefault() { prevented = true; },
  });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(prevented, true);
  assert.equal(invocations.length, 2);
});

test('Ctrl+Enter during WASM loading never queues a duplicate execution', async () => {
  let finishFetch;
  const fetchCompiler = () => new Promise((resolve) => { finishFetch = resolve; });
  const { source, run, invocations } = mountHero({ fetchCompiler });

  const firstRun = run.trigger('click');
  assert.equal(run.disabled, true, 'loading should disable the Run button');

  let prevented = false;
  source.trigger('keydown', {
    key: 'Enter',
    ctrlKey: true,
    metaKey: false,
    preventDefault() { prevented = true; },
  });
  assert.equal(prevented, true);
  finishFetch({ ok: true, arrayBuffer: async () => new ArrayBuffer(0) });
  await firstRun;
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(invocations.length, 1, 'shortcut must not re-enter execution during loading');
});
