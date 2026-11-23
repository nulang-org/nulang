# Typed HTTP API contract example

A small Nulang Web package demonstrating typed path/query/form request bindings,
which are included in compiler-generated Web deployment IR. This is an API
**contract example**, not a database-backed CRUD system or a secure service:
`create_item` only echoes its input, and no authentication or persistence is
configured.

From this directory with the native `nulang` CLI available:

```sh
nulang nula dev
# In a second terminal:
curl http://127.0.0.1:3000/health
curl 'http://127.0.0.1:3000/items/42?details=true'
curl -X POST -d 'title=Example' http://127.0.0.1:3000/items
```

Build the Web deployment IR and produce a typed TypeScript request client:

```sh
nulang nula build --web
node ../../scripts/nulang_ts_client.mjs dist/nulang-app.ir.json client.ts
```

Generated methods return `Promise<Response>` rather than pretending to decode
JSON or typed Nulang return values. Nulang's Web IR v1 does not yet define
response media types. HTTP failures throw `NulangApiError` with status and body.
The client supports GET and POST routes with typed path/query/header and
URL-encoded form inputs. Raw bodies, explicit cookie parameters, and unknown
argument types fail closed until their transport semantics are specified.

The generated client only handles request construction; it does not create a
database, session system, security policy or Cloud deployment. Nulang Cloud's
WASM-hosted web execution and hard-crash recovery remain separately gated.
