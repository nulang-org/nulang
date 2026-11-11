# Convention-based Web resource scaffolding (experimental)

The Web starter contains managed insertion points for additive, GET-only resource routes. This adopts a small part of Rails/Bullet Train scaffolding without an ORM or second web contract.

## Usage

Create the existing web starter and generate a resource:

    nulang nula new catalog --template web
    cd catalog
    nulang nula generate resource BlogPost
    nulang nula build --web

The command generates two ordinary handlers and declarative routes:

- GET /blog_posts: index placeholder
- GET /blog_posts/new: creation-form placeholder (no mutation)

Both are visible to the compiler-owned Web Contract IR and work with the current static build because neither has dynamic path captures.

## Guarantees and limitations

- PascalCase ASCII names only, with a 64-character limit; conventionally derived snake-case plural paths. Irregular nouns may need manual routes.
- Existing routes are never overwritten. Duplicate, missing, or out-of-order extension markers fail without changing source.
- It modifies only src/main.nula. The resulting code is application-owned and remains editable.
- Generated pages are READ-ONLY scaffolds, NOT CRUD. No persistence, tenant isolation, CSRF, authentication, policy, database migration, or durable writes are provided.
- Security-sensitive mutation endpoints must separately validate input, authorize access, and commit with appropriate transaction/outbox guarantees.

This is the first additive step toward model-backed typed scaffolding. Future generators should consume the existing validated compiler contracts rather than inventing parallel runtime metadata.

## Source-checkout standard-library resolution

Generated Web applications use the same source-aware standard-library resolver as compilation and Web Contract IR extraction. A binary built from source can build a generated application **outside the compiler checkout** without a manually configured `NULANG_STDLIB`. An explicit `NULANG_STDLIB` override takes precedence and fails closed if its path is invalid.

The external-directory regression covers both `nula generate resource` and `nula generate model`, then checks generated static pages and routes in the Web deployment IR. This requires the separate standard-library resolution fix (#2175), which is included in this integration branch and must be promoted along with it.


## Typed records are a separate, explicit command

The GET-only route scaffold above does **not** generate a database model,
persistence, mutations, or authorization. To create a typed data record and
a separate input type instead, run:

    nulang nula generate model BlogPost --field title:String --field published:Bool

This writes `src/resources/blog_post.nula` containing `BlogPost`,
`BlogPostInput`, and `can_read_blog_post`/`can_write_blog_post` policy
hooks that **return false by default**. It never registers HTTP routes.
Generated fields are canonicalized and validated with the Nulang lexer.
Both commands reject overwriting previously generated output. A model
and GET-only routes can be generated for the same name independently,
without implicitly connecting the model or policy to those routes.

**Do not treat generated tenant_id equality, deny-all hook declarations,
or read-only pages as authorization.** A real service still needs
authenticated identity, tenant membership, resource permission checks,
storage transactions, CSRF and input validation, with negative tests.
Neither generator supplies production CRUD.

## Optional PostgreSQL migration scaffold

To generate a reviewable migration from the **same field declaration** as the typed model, add `--migration`:

    nulang nula generate model BillboardFace --field name:String --field active:Bool --field impressions:Int --migration

This additionally creates `migrations/create_billboard_face.sql` (not applied automatically).
The generator refuses existing files and symlinked output directories, uses deterministic
field ordering and quoted SQL identifiers, and never overwrites application migrations.

The SQL scaffold declares a `public.billboard_face` table with a composite
`(tenant_id, id)` UUID primary key, enables and forces PostgreSQL row-level
security, and creates a tenant equality policy using `app.tenant_id`.
It deliberately grants no database privileges. Fields are required by default:
`String` maps to `TEXT`, `Int` to `BIGINT`, `Bool` to `BOOLEAN`,
and `Float` to `DOUBLE PRECISION` (which **must not** be used for money).

**This is not production-ready CRUD or an access-control system.**
Review and apply the SQL only with a separate trusted migration identity.
Application code must obtain tenant identity from verified auth/membership,
run tenant-local transactions, and use a restricted non-`BYPASSRLS` database
role. PostgreSQL custom settings can be set by the connected role, so
`app.tenant_id` is not a defense against arbitrary SQL execution or SQL
injection. Add tenant foreign keys and explicit least-privilege grants only
when the application's real schema and operational roles are known. Verify
cross-tenant read/write denial in a disposable PostgreSQL integration test
before opening any mutation endpoint.

The generator does not inspect subsequent edits to the model; migrations are
historical application-owned snapshots. Generating both files is best-effort
rollback on errors, not an atomic filesystem transaction across two paths.
