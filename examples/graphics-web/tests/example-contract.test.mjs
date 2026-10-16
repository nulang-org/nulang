import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

// Web.html deliberately rejects <script> via el(). A fixed, trusted static
// script reference must use Web.raw without interpolating application data.
test('Nulang example uses trusted static module script markup', () => {
  const source = readFileSync(new URL('../src/main.nula', import.meta.url), 'utf8');
  assert.doesNotMatch(source, /el\("script"/);
  assert.match(source, /raw\("<script type=\\"module\\" src=\\"\/demo\.js\\"><\/script>"\)/);
});
