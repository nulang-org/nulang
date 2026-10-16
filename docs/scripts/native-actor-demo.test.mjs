import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const component = readFileSync(new URL('../src/components/NativeActorDemo.astro', import.meta.url), 'utf8');
const homepage = readFileSync(new URL('../src/content/docs/index.mdx', import.meta.url), 'utf8');
const architecture = readFileSync(new URL('../src/components/ArchitectureMap.astro', import.meta.url), 'utf8');
const hero = readFileSync(new URL('../src/components/Hero.astro', import.meta.url), 'utf8');
const canonical = readFileSync(new URL('../../../examples/worker_pool.nula', import.meta.url), 'utf8');

test('native demo contains the checked-in executable worker-pool program', () => {
  const displayed = component.match(/const demoSource = String\.raw\x60([\s\S]*?)\x60;/);
  assert.ok(displayed, 'demo should expose literal code, not a fabricated output');
  const normalize = (source) => source.split('\n')
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith('//'))
    .join('\n');
  assert.equal(normalize(displayed[1]), normalize(canonical));
  assert.match(component, /https:\/\/github\.com\/nulang-org\/nulang\/blob\/main\/examples\/worker_pool\.nula/);
});

test('native sample is discoverable and not mislabeled as browser runnable', () => {
  assert.match(homepage, /import NativeActorDemo from/);
  assert.match(homepage, /<NativeActorDemo \/>/);
  assert.match(component, /not the browser CoreVM/);
  assert.match(component, /new actor identity and default state/);
  assert.match(component, /durable recovery is separate/);
});

test('hero and architecture avoid implying all supervised restarts hydrate durable state', () => {
  assert.match(architecture, /transient actors restart from their template/);
  assert.match(hero, /Opt-in persistence/);
  assert.doesNotMatch(architecture, /supervisor restart\s*[\s\S]{0,100}hydrate durable state/);
});
