import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

// Guard the homepage's smallest critical text against becoming unreadable.
// The hero workbench has a fixed, dark background so its text contrast can
// be checked deterministically without a browser dependency.
const hero = readFileSync(new URL('../../src/components/Hero.astro', import.meta.url), 'utf8');

function rulesFor(selector) {
  const escaped = selector.replace(/[.*+?^\${}()|[\]\\]/g, '\\$&');
  const result = hero.match(new RegExp(escaped + '\\s*\\{([^}]+)\\}'));
  assert.ok(result, 'missing style rule for ' + selector);
  return result[1];
}
function remSize(rule) {
  const found = rule.match(/font-size:\s*([\d.]+)rem/);
  assert.ok(found, 'font-size must be declared in rem');
  return Number(found[1]);
}
function luminance(hex) {
  const colors = [1, 3, 5].map(i => parseInt(hex.slice(i, i + 2), 16) / 255);
  const channel = x => x <= 0.04045 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4;
  return channel(colors[0]) * 0.2126 + channel(colors[1]) * 0.7152 + channel(colors[2]) * 0.0722;
}
function contrast(a, b) {
  const [light, dark] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (light + 0.05) / (dark + 0.05);
}

test('the keyboard shortcut hint meets WCAG AA normal-text contrast', () => {
  const style = rulesFor('.hero-runtime-note');
  const color = style.match(/color:\s*(#[a-fA-F0-9]{6})/);
  assert.ok(color, 'shortcut hint needs an explicit foreground color');
  assert.ok(contrast(color[1], '#0e141d') >= 4.5,
    'shortcut hint must meet at least 4.5:1 contrast on the workbench background');
});

test('small hero and capability labels remain at least 12px at default zoom', () => {
  for (const selector of [
    '.hero-runtime-note', '.playground-kicker', '.workbench-local',
    '.hero-proof-grid span', '.flow-stage span',
  ]) {
    assert.ok(remSize(rulesFor(selector)) >= 0.75,
      selector + ' must use at least 0.75rem');
  }
});
