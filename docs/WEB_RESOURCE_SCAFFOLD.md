# Resource scaffolding (experimental)

Nulang's resource generator borrows Rails/Laravel's convention-over-configuration
approach while preserving explicit, compiler-checkable application boundaries.

## Generate a typed resource

From an initialized Nulang package:

```bash
nulang nula generate resource BillboardFace \
  --field name:String \
  --field daily_rate:Float \
  --field active:Bool
```

The command writes exactly one source file:
`src/resources/billboard_face.nula`. It defines:

- `BillboardFace`: a typed record with generated `id: String` and
  `tenant_id: String` fields plus the requested domain fields.
- `BillboardFaceInput`: an input record with only the domain fields.
- `can_read_billboard_face` and `can_write_billboard_face`:
  explicit **deny-all** policy hooks that an application must implement.

Only `String`, `Int`, `Float`, and `Bool` field types are supported
in this first increment. The resource identifier must be a PascalCase ASCII
identifier; field identifiers must be snake_case-compatible ASCII names. Names
and types are validated before creating any directories or files. Fields are
emitted alphabetically, so reordering command flags does not change the output.

Generation **never overwrites** an existing resource file. There is no
`--force` option. It also rejects symlinked `src/` or `src/resources/` output
parents and verifies the resolved output directory remains inside the package.
These safeguards are not intended as a sandbox against concurrent filesystem
mutation. Review and edit the generated source explicitly.

## Security and integration boundary

This is a **type and policy scaffold**, not an automatically deployed CRUD API.
It does not add HTTP routes, database tables/migrations, persistence, form
validation, authentication, audit events, or runtime authorization middleware.
An unused policy hook is not an access-control boundary.

Before exposing any resource operation:

1. Bind the request to an authenticated principal; do not trust tenant IDs
   supplied in the request body or path as proof of membership.
2. Resolve resources through a tenant-scoped query and check the actor's
   explicit permission for the operation. Tenant equality alone is insufficient.
3. Connect policy checks to each read/mutation handler and enforce them on
   every execution path, including durable jobs and background handlers.
4. Use the existing Nulang web route/request-binding contracts for typed inputs;
   reject malformed requests and deny access when policy context is missing.
5. Add integration tests for cross-tenant access, unauthorized updates, and
   malformed inputs before registering write routes.

The web framework already owns route contract extraction and request-binding
validation under `src/web/`. Future generators should consume those APIs
rather than invent an alternative router, ORM, or authentication system.

## Executable two-resource tenant policy example

See [`examples/tenant-resources`](../examples/tenant-resources/README.md) for a
checked-in pair of generated resources and executable authorization assertions:
positive same-tenant reads/writes and negative cross-tenant, no-permission,
unauthenticated, and invalid relationship cases. The example is a **policy
contract**, not a production identity provider, HTTP server, database/RLS
implementation, or authorization middleware.

## Verification

```bash
cargo test package::resource_generator::tests
cargo test
```

The focused tests cover generated source parsing/typechecking, default-deny
policies, canonical output, invalid/duplicate inputs, manifest requirements,
non-destructive reruns, and symlinked output rejection on Unix. An additional
CLI dispatch regression test verifies end-to-end command wiring.

### HTTP contract exercise (test-only)

The two-resource reference also contains source-level GET/POST Web handlers,
validated through the native `WebDevServer` real TCP integration test. This
shows policy-denied requests cannot observe or mutate test-owned Web KV keys.
The reference stores test records under server-derived tenant-qualified keys
and checks positive existence markers rather than treating `nil` as empty text.
The Web KV backend is **ephemeral**. With the new request-local
`Web.status` effect, denied handlers use 401 for missing sessions and 403 for
authenticated authorization failure, with no-store text responses. The regression **does not prove durable tenant
persistence, verified external authentication, or SQL-level RLS**. Do not
connect these example handlers to public routes before those independent
boundaries and the strict origin/CSRF admission stack are completed.


### Database RLS proof (disposable-only)

The reference under
[`examples/tenant-resources/sql/001_tenant_rls.sql`](../examples/tenant-resources/sql/001_tenant_rls.sql)
adds a **separate PostgreSQL fixture** with role-bound tenants, forced RLS,
composite keys, tenant-preserving campaign-to-face foreign keys, and a
transactional negative-access regression. To execute the same **fail-closed**
acceptance harness used by the `Web Tenant PostgreSQL RLS (disposable)` GitHub
check, run `bash tests/run_tenant_resource_postgres_rls_ci.sh` from the repository
root with `pg_virtualenv` and PostgreSQL installed. The harness first proves
unguarded execution, forged cluster provenance, and default-port substitution
are rejected; it then creates a throwaway cluster and runs cross-tenant and
transaction assertions. It also uses distinct, randomly generated passwords
to test independent PostgreSQL LOGIN sessions (including rejection of tenant A
credentials for tenant B). These disposable PostgreSQL credentials do not
establish external application identity. The cluster is destroyed on completion.

This is a **targeted supplemental check** for changes to the tenant-resource
example, SQL, Web runtime, and relevant generators; it is not evidence of
production authentication or a real HTTP → PostgreSQL integration. The Web
dev server still uses in-memory KV in the example. No production database,
OIDC identities, tenant-to-login provisioning, trusted connection pool,
or durable application handler is configured by this test.
