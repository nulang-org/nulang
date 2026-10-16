import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
import { generateClient } from '../nulang_ts_client.mjs';

const example = {
  version: 1,
  routes: [
    { method: 'GET', path: '/health', handler: 'health', placement: 'server', bindings: [] },
    { method: 'GET', path: '/items/{item_id: Int}', handler: 'show_item', placement: 'server', params: [{ name: 'item_id', ty: 'Int' }], bindings: [
      { source: 'path', source_name: 'item_id', handler_param: 'id', handler_index: 0, ty: 'Int' },
      { source: 'query', source_name: 'details', handler_param: 'details', handler_index: 1, ty: 'Bool' },
    ] },
    { method: 'POST', path: '/items', handler: 'create_item', placement: 'server', bindings: [
      { source: 'form', source_name: 'title', handler_param: 'name', handler_index: 0, ty: 'String' },
      { source: 'header', source_name: 'X-Request-ID', handler_param: 'trace', handler_index: 1, ty: 'String' },
    ] },
  ],
};

test('generates typed client methods from compiler IR with parameter and body transport', () => {
  const src = generateClient(example);
  assert.match(src, /async show_item\(params: \{ item_id: number; details: boolean \}\): Promise<Response>/);
  assert.match(src, /encodeURIComponent\(String\(params\.item_id\)\)/);
  assert.match(src, /url\.searchParams\.set\("details", String\(params\.details\)\)/);
  assert.match(src, /async create_item\(params: \{ title: string; trace: string \}\): Promise<Response>/);
  assert.match(src, /form\.set\("title", String\(params\.title\)\)/);
  assert.match(src, /headers\.set\("X-Request-ID", String\(params\.trace\)\)/);
  assert.match(src, /throw new NulangApiError\(response\.status, await response\.text\(\)\)/);
  assert.match(src, /async health\(\): Promise<Response>/);
  assert.doesNotMatch(src, /Promise<\s*Item\s*>/);
});

test('encodes path values and avoids inserting unescaped paths and query arguments', () => {
  const src = generateClient(example);
  assert.match(src, /new URL\(`\/items\/\$\{encodeURIComponent\(String\(params\.item_id\)\)\}`, this\.baseUrl\)/);
  assert.doesNotMatch(src, /url\.searchParams\.set\("item_id"/);
});

test('rejects unsupported raw body, cookies and methods without silently omitting operations', () => {
  for (const route of [
    { method: 'POST', path: '/raw', handler: 'raw', bindings: [{ source: 'body', source_name: 'payload', handler_param: 'payload', handler_index: 0, ty: 'String' }] },
    { method: 'GET', path: '/cookie', handler: 'cookies', bindings: [{ source: 'cookie', source_name: 'session', handler_param: 'session', handler_index: 0, ty: 'String' }] },
    { method: 'DELETE', path: '/resource', handler: 'remove', bindings: [] },
  ]) {
    assert.throws(() => generateClient({ version: 1, routes: [route] }), /unsupported/i);
  }
});

test('rejects invalid contracts instead of generating misleading method signatures', () => {
  assert.throws(() => generateClient({ version: 2, routes: [] }), /version/i);
  assert.throws(() => generateClient({ version: 1, routes: [{ method: 'GET', path: '/x/{id}', handler: 'get_x', params: [], bindings: [] }] }), /path/i);
  assert.throws(() => generateClient({ version: 1, routes: [
    { method: 'GET', path: '/a', handler: 'same', bindings: [] },
    { method: 'GET', path: '/b', handler: 'same', bindings: [] },
  ] }), /duplicate/i);
  assert.throws(() => generateClient({ version: 1, routes: [{ method: 'GET', path: '/x', handler: 'bad', bindings: [{ source: 'query', source_name: 'value', handler_param: 'value', handler_index: 0, ty: 'Custom' }] }] }), /unsupported.*type/i);
  assert.throws(() => generateClient({ version: 1, routes: [{ method: 'GET', path: '/x', handler: 'bad', bindings: [{ source: 'query', source_name: 'value', handler_param: 'value', handler_index: 0, ty: 'String' }, { source: 'header', source_name: 'value', handler_param: 'value', handler_index: 1, ty: 'String' }] }] }), /duplicate.*name/i);
});

test('command line tool writes output to explicit file', async () => {
  const folder = await mkdtemp(path.join(tmpdir(), 'nulang-client-'));
  const input = path.join(folder, 'input.json');
  const output = path.join(folder, 'client.ts');
  await writeFile(input, JSON.stringify(example));
  const script = fileURLToPath(new URL('../nulang_ts_client.mjs', import.meta.url));
  execFileSync(process.execPath, [script, input, output], { stdio: 'pipe' });
  const code = await readFile(output, 'utf8');
  assert.match(code, /export class NulangClient/);
});
