# Web interoperability: JavaScript, TypeScript and Python

**Status:** Experimental. These are HTTP clients for existing Nulang Web routes,
not SDKs for NUL0 actor messaging, workflow replay, or SQL queries.

## Produce the contract

Run from the Nulang repository:

```sh
cargo run --locked --no-default-features --example export_web_openapi -- ./my-app/src > openapi.json
```

The example calls the existing compiler-owned
`compile_validated_contracts_from_tree` API and fails on invalid bindings;
it does not emit partially validated schemas. Run a Nulang Web server
separately. The OpenAPI 3.1 output currently describes methods, routes,
path/query/header/form parameters, and Nulang response-type extensions.
**Actual response JSON media types are not yet part of the web response
contract.** Do not assume all HTTP responses are JSON.

## JavaScript / TypeScript

No npm dependencies. Requires a Fetch-compatible environment (modern browser,
Node.js 18+, or an edge runtime).

```js
import {readFile} from 'node:fs/promises';
import {NulangWebClient} from './interop/web/javascript/client.mjs';

const contract = JSON.parse(await readFile('./openapi.json', 'utf8'));
const api = new NulangWebClient({
  baseUrl: 'http://localhost:8080',
  contract,
  token: process.env.NULANG_TOKEN,
  timeoutMs: 10000,
});
const response = await api.call('get_user', {path: {id: '42'}});
console.log(await response.text());
```

Use `client.d.mts` for TypeScript ESM/NodeNext typechecking.
Response bodies remain raw because typed JSON media contracts are not guaranteed.

## Python

The Python client uses the standard library. It does not embed PyO3
or link to a version-specific libpython shared library.

```python
import json
from interop.web.python.client import NulangWebClient, ApiError

with open('openapi.json', encoding='utf8') as f:
    contract = json.load(f)
api = NulangWebClient('http://localhost:8080', contract, timeout=10)
try:
    response = api.call('get_user', path={'id': '42'})
    print(response.text())
except ApiError as error:
    print('HTTP status:', error.status)
```

When running scripts outside the repository root set `PYTHONPATH` accordingly.

## Protocol behavior and limitations

- Call by OpenAPI `operationId` with explicitly declared `path`, `query` and
  `headers` arguments. UTF-8 paths and repeated query values are encoded correctly.
- A `token` sends `Authorization: Bearer ...`. Do not hardcode credentials. A conflicting
  authorization header is rejected rather than overwritten.
- The optional `traceparent` argument forwards a validated W3C version-00 header.
  **It does not establish automatic OpenTelemetry HTTP span extraction.**
  The host tracer supplies that header, and the Nulang server still needs an
  inbound propagator to attach HTTP and actor spans.
- Form and JSON request bodies are serialized only when their media type
  is explicitly declared in OpenAPI. Responses are left as bytes (Python) or
  the native Fetch Response (JS). On non-2xx status the clients raise
  `ApiError` and retain the status/body. Network failures remain native errors.
- Redirects are not followed, preventing credentials from being forwarded to another origin.
- Timeouts are bounded. There are **no automatic retries**: retrying a POST
  after a timeout may duplicate an operation that already committed.
- Undeclared fields, duplicate operation IDs, unknown media types, unsupported
  cookies and invalid paths are rejected, not silently dropped.
- This is deliberately **not a full OpenAPI generator**: it does not handle
  multipart, discriminator models, OAuth flows, streaming, or cookie-based
  browser sessions. A validated contract is trusted build configuration;
  do not load schemas from an untrusted URL.

## Tests

```sh
python3 -m unittest discover -s interop/web/python -p 'test_*.py'
node --test interop/web/javascript/client.test.mjs
tsc --noEmit --strict --module nodenext --moduleResolution nodenext \
  --target ES2022 --lib ES2022,DOM interop/web/javascript/typecheck.mts
```

With a local native CLI built with TCP support, also run:

```sh
cargo build --locked --no-default-features --features tcp --bin nulang
python3 scripts/smoke_web_client_interop.py --nulang-bin ./target/debug/nulang
```

The Python and JavaScript suites both exercise a real local HTTP server.
JavaScript also uses injected Fetch implementations to cover error and redirect
edge cases. The additional native smoke launches Nulang's real `Http.serve`
implementation on an ephemeral localhost port and uses both language clients
to request its response. **This proves a minimal HTTP wire round-trip, not
compiler-generated OpenAPI route/export conformance:** the native server has
one generic handler and the smoke uses a deliberately hand-authored contract.
Typed response schemas, route/query binding, and trace-context extraction by
the Nulang server still require separate executable integration tests.

## Remaining interoperability priorities

| Surface | Existing foundation | Most important remaining gap |
| --- | --- | --- |
| Python embedded | PyO3 bridge | ABI-independent out-of-process tool adapters and lifetime/error contracts |
| C/native | C FFI (`src/ffi/`) | Versioned ABI conformance, ownership semantics, error APIs |
| WASM components | WIT generator + Wasmtime host | Host-component interop conformance suites |
| Databases | `PersistenceStore` with libSQL and optional PostgreSQL/RocksDB | General-purpose parameterized SQL, pools, migrations, transactions; persistence stores are not SQL SDKs |
| Outbound HTTP | Basic `Http.get/post` effects | Headers, all methods, cancellation, streaming, status/errors, request timeouts |
| OpenTelemetry | OTLP exporters and actor W3C trace context | Incoming HTTP context extraction and *real* OTEL parent span context |
| Infrastructure | Compiler Dockerfile, web Docker adapter, Cloud adapter | OCI release image, health/readiness, SIGTERM drain, env config, deployment smoke CI |

**Known correctness issue:** `packages/http-client/src/lib.nula` accepts headers
with `with_header` but `send_request` calls basic `Http.get/post` and does not send
them. Do not depend on it for authenticated requests until an outbound request
effect with real header support and tests is implemented.

Preserve the frozen language core. Add interoperability as versioned adapters,
WIT components and generated contract consumers rather than a second actor
transport or special-purpose syntax for each external service.
