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

## Source-checkout stdlib path workaround

On current main, a Nulang binary built from source may fail to resolve standard-library imports during Web Contract IR extraction when run from a generated app outside the compiler checkout. Until issue #2171 is fixed, set NULANG_STDLIB to the absolute path of the compiler checkout's src/stdlib directory before running the web build:

    export NULANG_STDLIB=/absolute/path/to/nulang/src/stdlib
    nulang nula build --web

This issue also reproduces on an unmodified web starter and is not introduced by the generator.


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
