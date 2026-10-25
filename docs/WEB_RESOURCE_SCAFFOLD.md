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
