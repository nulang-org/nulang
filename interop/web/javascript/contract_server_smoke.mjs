// End-to-end Nulang Web Contract IR -> OpenAPI -> Fetch -> WebDevServer.
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {NulangWebClient, ApiError} from './client.mjs';

const [baseUrl, contractPath] = process.argv.slice(2);
if (!baseUrl || !contractPath) {
  throw new Error('usage: contract_server_smoke.mjs <base-url> <openapi-json>');
}
const contract = JSON.parse(await readFile(contractPath, 'utf8'));
const operation = contract.paths?.['/probe/{id}']?.get;
assert.equal(operation?.operationId, 'read_probe');
assert.equal(operation?.parameters?.find(p => p.name === 'id')?.in, 'path');
assert.equal(operation?.parameters?.find(p => p.name === 'limit')?.in, 'query');

const client = new NulangWebClient({baseUrl, contract, timeoutMs: 5000});
const response = await client.call('read_probe', {
  path: {id: 7},
  query: {limit: 35},
});
assert.equal(response.status, 200);
assert.equal((await response.text()).trim(), '42');

await assert.rejects(
  client.call('read_probe', {path: {id: 7}, query: {limit: 'invalid'}}),
  error => {
    assert.ok(error instanceof ApiError);
    assert.equal(error.status, 400);
    assert.equal(JSON.parse(error.body).code, 'invalid_request_input');
    return true;
  },
);
console.log('PASS exported Web contract -> Nulang typed route -> JS Fetch client');
