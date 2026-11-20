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
