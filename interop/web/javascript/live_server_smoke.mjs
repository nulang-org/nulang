// Real Nulang HTTP server transport test (not an OpenAPI route-export test).
import assert from 'node:assert/strict';
import { NulangWebClient } from './client.mjs';

const baseUrl = process.argv[2];
if (!baseUrl) throw new Error('usage: node live_server_smoke.mjs <base-url>');

const contract = {
  openapi: '3.1.0',
  paths: {
    '/probe/{id}': {
      get: {
        operationId: 'read_probe',
        parameters: [
          {name: 'id', in: 'path', required: true},
          {name: 'tag', in: 'query'},
          {name: 'x-tenant-id', in: 'header', required: true},
        ],
      },
    },
  },
};
const client = new NulangWebClient({
  baseUrl, contract, token: 'local-smoke-token', timeoutMs: 4000,
});
const response = await client.call('read_probe', {
  path: {id: 'a/b'},
  query: {tag: ['one two', 'é']},
  headers: {'x-tenant-id': 'local-smoke'},
  traceparent: '00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01',
});
assert.equal(response.status, 200);
assert.equal(await response.text(), 'nulang-web-live');
console.log('PASS Node Fetch client -> native Nulang HTTP server');
