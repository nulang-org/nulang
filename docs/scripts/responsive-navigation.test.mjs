import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const navigation = readFileSync(
  new URL('../src/components/MobileNav.astro', import.meta.url),
  'utf8',
);
const stylesheet = readFileSync(
  new URL('../src/styles/custom.css', import.meta.url),
  'utf8',
);

test('mobile menu closes only after its responsive layout ends', () => {
  const tabletLayout = stylesheet.match(
    /@media \(max-width: ([\d.]+)rem\) and \(min-width: ([\d.]+)rem\) \{[\s\S]*?\.mobile-nav \{ display: block; \}/,
  );
  assert.ok(tabletLayout, 'responsive mobile menu range must be declared');

  const closeMedia = navigation.match(
    /matchMedia\('\(min-width: ([\d.]+)rem\)'\)/,
  );
  assert.ok(closeMedia, 'resize listener must close the mobile menu');

  // One CSS pixel beyond the last width at which the mobile menu is visible,
  // using the browser's standard 16px root font-size.
  assert.equal(Number(closeMedia[1]), Number(tabletLayout[1]) + 1 / 16);
});

test('narrow-screen header keeps GitHub available in the navigation menu', () => {
  const homepageStyles = stylesheet.slice(
    stylesheet.indexOf('/* ── 2026 homepage discovery and responsive polish ── */'),
  );
  assert.match(
    homepageStyles,
    /@media \(max-width: 40rem\) \{\s*header \.social-icons \{\s*display: none;/,
  );
  assert.match(navigation, /label: 'GitHub'/);
});
