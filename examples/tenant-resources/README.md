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
