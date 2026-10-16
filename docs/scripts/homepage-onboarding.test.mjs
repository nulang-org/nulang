import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const steps = readFileSync(new URL('../src/components/Steps.astro', import.meta.url), 'utf8');
const hero = readFileSync(new URL('../src/components/Hero.astro', import.meta.url), 'utf8');
const homepage = readFileSync(new URL('../src/content/docs/index.mdx', import.meta.url), 'utf8');

test('start paths distinguish browser CoreVM from the native runtime', () => {
  assert.match(steps, /href: '\/playground\/'/);
  assert.match(steps, /href: '\/getting-started\/installation\/'/);
  assert.match(steps, /Actors, networking, and durable state are not available in the browser/);
  assert.match(steps, /native runtime/);
});

test('first-run choices are not presented as a mandatory three-step sequence', () => {
  assert.match(steps, /start-path-grid/);
  assert.match(steps, /Already installed\?/);
  assert.doesNotMatch(steps, /Three ways to begin/);
  assert.match(homepage, /Choose how to try Nulang/);
});

test('hero states browser boundary at the point of interaction', () => {
  assert.match(hero, /Actors and durable state need the native runtime/);
  assert.match(homepage, /Try the language core in the browser/);
});
