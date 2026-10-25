# Two-resource tenant authorization reference (experimental)

This is a **running Nulang policy contract**, not a production login flow or
CRUD server. It complements `nula generate resource` without adding a second
ORM, router, or authorization framework.

The resource modules under `src/resources/` are the exact output of:

```bash
nulang nula generate resource BillboardFace --field name:String --field active:Bool
nulang nula generate resource Campaign --field title:String --field face_id:String
```

The generator's `can_read_*` and `can_write_*` hooks remain **deny-all**.
Application code in `src/lib.nula` shows the explicit policy layer required
before use. Nulang infers the resource record fields, avoiding a duplicate
resource type definition or a parallel schema/loader.

`tests/tenant_isolation.nula` runs two tenants and two roles. The policy
requires an authenticated actor with a non-empty tenant, a matching resource
tenant, and an explicit read/write permission. Campaign checks also require
that the referenced face belongs to the actor's tenant and that
`campaign.face_id == face.id` — preventing a same-tenant campaign from linking
an unrelated tenant's face. Negative tests cover cross-tenant reads and writes,
read-only updates, missing authentication, and mismatched campaign/face joins.

```bash
cd examples/tenant-resources
nulang nula test --verbose
# from repository root, also run:
cargo test --locked --no-default-features --test tenant_resource_reference
```

**Security boundary:** `Actor` here is a *fixture* populated directly by
application tests. It is not proof of identity. In a real HTTP application,
obtain identity, memberships, and permissions **only from a verified server-side
identity/session adapter**. Never deserialize this actor from request input, and
never trust `tenant_id` submitted by a client. Resolve both resource IDs through
tenant-scoped database queries and recheck authorization atomically before any
mutation; check background jobs too. Neither a source-level policy function
nor a generated default-deny hook blocks a route unless that route actually
calls it. Authentication, persistent database isolation/RLS, and real HTTP
handlers are **not implemented** by this example. Do not deploy it as an auth
server or assume this replaces CSRF, origin admission, or server-side RLS.

## Development HTTP boundary regression

`src/http_contract.nula` is a **test-only** pair of GET/POST handlers for both
resources. `src/runtime/tenant_resource_http_tests.rs` compiles these Nulang
handlers and runs them through the *real* `WebDevServer` over TCP. The Rust test
creates high-entropy cookie tokens, seeds the **server-side ephemeral** Web KV
with principal-to-tenant/permission records, and verifies that requests cannot
choose their own tenant by adding body, path, or cookie values. It checks
cross-tenant reads/writes, forged/absent sessions, read-only roles, missing
and duplicate form fields, a missing CSRF token, same-tenant updates, records
with identical IDs in different tenant namespaces, and a campaign whose
referenced face belongs to a foreign tenant. All session and CSRF secrets in
the test are generated using OS randomness.

Run the focused source-equivalent regression:

```bash
cargo test --locked --no-default-features --lib \
  authenticated_http_read_and_mutation_enforce_tenant_and_permissions
```

**Fail-closed storage contract:** examples use
`tenant-demo:face:<trusted-tenant>:<id>` (and corresponding campaign keys),
never a client-provided tenant. Because `Web.kv_get` returns **nil** for absent
keys, the example requires an explicit positive-equality `"active"` marker
for an existing resource and valid session CSRF state. It does **not** use
`value != ""` to decide whether a key exists. POST handlers also count
concrete `Web.form` string pairs and reject missing or duplicate field names.

**Important limits**: this Web KV is **not durable storage**, transactions, or
SQL row-level security. Test sessions are pre-seeded by Rust, not issued by an
OIDC/verified identity integration; the user-facing login flow is absent.
This stacked branch adds request-local `Web.status(code)`: missing or forged
sessions return **401** and authenticated policy failures return **403**.
Non-success responses use `text/plain`, `Cache-Control: no-store`, and do not
inject the client Web runtime. Out-of-range statuses are coerced to 500. This simple test handler looks up an `X-CSRF-Token` but is **not** a
replacement for the fail-closed Host/Origin/Fetch-Metadata and robust token
admission integration in #2186, which is not in this standalone proof's base.
It must not be exposed to untrusted clients or deployed as an API. Introduce
verified identity, request-wide origin/CSRF validation, typed response status,
transactional tenant-scoped SQL and negative RLS tests before generating or
exposing production CRUD routes.
