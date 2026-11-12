# Read-only administrative views (experimental)

Nulang Web provides a small, HTML-first presentation layer inspired by
Filament's resource tables and infolists. It is additive to Nulang's compiler-
owned Web Contract IR. It does not provide an ORM, a second router, implicit
CRUD endpoints, session auth, tenant discovery, or background data access.

## Opt-in typed model views

```sh
nulang nula new catalog --template web
cd catalog
nulang nula generate model BillboardFace --field name:String \
    --field active:Bool --field impressions:Int --field daily_rate:Float \
    --read-only-views
```

This generates a model with the existing deny-all `can_read_billboard_face`
and `can_write_billboard_face` hooks plus two read-only helpers:

- `billboard_face_readonly_table(records)`
- `billboard_face_readonly_detail(record)`

The helpers use the **same validated field declarations** as the model, convert
primitive values (`String`, `Bool`, `Int`, `Float`) to escaped HTML via
`stdlib::web::admin`, and enforce a nonempty actor tenant ID, exact
`record.tenant_id == actor_tenant_id`, **and** the model's read policy before
rendering each record. The mandatory tenant guard still applies if an
application mistakenly implements `can_read_*` to always return `true`. When all policies still deny, the table has no rows and the
detail helper renders `Access denied`. Nothing is automatically registered as
an HTTP route. There are no generated database queries or writes.

The `--read-only-views` flag is opt-in; the original `nula generate model`
command produces unchanged model-only source. Existing output is never
overwritten. The generated Nulang source remains application-owned.

**Security:** Generated helpers no longer accept a user-supplied tenant ID.
They read `Web.verified_tenant_id()` from the active protected request.
Outside an authenticated request this returns empty, and the generated views
deny every record even if an application read policy is permissive. A trusted
session resolver must verify identity, expiry, revocation and tenant membership;
matching tenant strings alone are insufficient authorization.

Database queries must also be tenant-scoped, and rows must not be fetched or
sent to untrusted clients before authorization. Rendering is not a substitute
for a server-side access-control boundary. A denied detail is a rendered HTML fragment,
not automatically an HTTP 401/403 response; applications must reject unauthorized
HTTP requests at their actual ingress/handler boundary before data is fetched.

## Reusable HTML helpers

`import stdlib::web::admin` provides:

```nulang
readonly_table(["Name", "State"], [["West board", "Active"]])
readonly_detail(["Name", "State"], ["West board", "Active"])
denied_view()
```

These are **pure presentation helpers**. They escape all dynamic values through
`Web.text`, produce accessible `<th scope="col">` and `<dl>` markup, and render
only an error message if row widths or field counts are inconsistent. They do
not validate authorization; applications must perform that before calling them.

## Deliberate omissions

- No automatic mutations, action links, or write permissions. Authorization
  checks at an HTTP handler and transaction boundary remain mandatory.
- No search, sorting, pagination, relationships, uploads, or schema migrations.
  Those should be implemented separately, with bounded server-side queries.
- No additional JavaScript client runtime. The views are ordinary HTML and can
  be embedded in Hotwire Frames/Streams once the audited integration is merged.

## Qualification

The read-only view helpers have real HTTP tests for escaping and invalid
shapes, and model-generated helpers have parser, import-resolution, and
TypeChecker tests. The separate audited security/Hotwire stack remains draft.
Do not treat this module as production CRUD or proof of end-to-end tenant
isolation. Real-HTTP negative tests now cover cross-tenant records and blank
identity values even when the app's custom policy is permissive; these are
not authentication tests, and do not exercise a database's RLS policy.
Exact-head CI, authenticated-session integration, tenant-RLS negative tests,
and full release security review are mandatory before sensitive production use.

### Current integration limitation

On the #2538-based review stack, local `cargo audit --no-fetch` still exits
nonzero with `RUSTSEC-2026-0459` in the inherited `libsqlite3-sys 0.30.1`
lockfile dependency. Do not waive the advisory or merge this branch independently
of the patched SQLite/security integration. The generator and rendering tests
are local evidence, not production authorization or a release qualification.

### Audited stack integration

When stacked on the patched SQLite/Hotwire runtime in draft #2589, the
local offline RustSec audit passes with three allowed advisory warnings.
The original #2538/#2592 generator-only base still has
`RUSTSEC-2026-0459` in its inherited libsqlite3-sys dependency graph. Do not
merge that older base without the audited stack or an equivalent fix.
