import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { homepageExamples } from '../../src/lib/homepage-examples.mjs';

const fullPlayground = readFileSync(new URL('../../../playground/web/playground.js', import.meta.url), 'utf8');

test('homepage offers distinct, runnable CoreVM examples', () => {
  assert.deepEqual(homepageExamples.map(({ id }) => id), ['hello', 'recursion', 'closures']);
  for (const { id, label, code } of homepageExamples) {
    assert.ok(label && code, id + ' needs a name and program');
    assert.match(code, /perform IO\.print\(/, id + ' should display an observable result');
    assert.doesNotMatch(code, /\b(?:actor|spawn|ask|send|Http|Inference)\b/, id + ' must not rely on the native runtime');
  }
});

test('homepage snippets match existing full-playground examples byte for byte', () => {
  for (const { id, code } of homepageExamples) {
    assert.ok(fullPlayground.includes(code), id + ' has drifted from a shipped CoreVM example');
  }
});
