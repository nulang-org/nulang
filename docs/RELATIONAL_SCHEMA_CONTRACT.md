# Relational schema contract (experimental v0alpha1)

This is a portable, versioned **metadata subset** shared by eventual Nulang typed
SQL tools and Nulang Cloud deployment/admission tooling. It is deliberately
separate from the compiler's actor-state semantic identity and from NuDB's
tablet/WAL format.

Implementation: `src/database/schema_contract.rs`.

## Implemented

- Rust/Serde contract types for PostgreSQL and SQLite catalog metadata.
- Tables, columns, nullability, verbatim SQL type names and column defaults.
- Ordered primary keys, column indexes (including uniqueness), and foreign keys.
- Validation for unknown versions/JSON fields, empty or colliding identifiers,
  missing columns and foreign-key targets, invalid reference cardinality and
  duplicate constraints.
- Deterministic JSON output and domain-separated `blake3:` fingerprints.
  Tables, columns and named constraint declaration *order* are normalized,
  while composite key/index column order remains semantic.
- Field metadata lookup for future typed query checking.

No new language keywords, mandatory code-generation process, database connection
or runtime service dependency is introduced.

## Example

```json
{
  "version": "nulang.relational-schema/v0alpha1",
  "dialect": "postgres",
  "tables": [
    {
      "name": "public.users",
      "columns": [
        {
          "name": "id",
          "sql_type": "uuid",
          "nullable": false
        },
        {
          "name": "email",
          "sql_type": "text",
          "nullable": false
        }
      ],
      "primary_key": ["id"],
      "indexes": [
        {
          "name": "users_email_unique",
          "columns": ["email"],
          "unique": true
        }
      ]
    }
  ]
}
```

Use `RelationalSchema::from_json` to deserialize and validate untrusted input,
`to_json` for normalized interchange, and `fingerprint` for a deterministic
contract identity. The fingerprint is a versioned format identity, **not** a
runtime certificate that a real database matches this schema.

## Strict boundaries

This initial format does **not** cover schemas/catalog namespaces as separate
objects, check/exclusion constraints, expression/partial/covering indexes,
operator classes, collation, generated/identity columns, domains/enums,
partitioning, triggers, views, stored routines, grants, policies/RLS or
database-specific DDL. PostgreSQL types and default expressions are retained
verbatim; they are **not parsed** into trusted SQL ASTs. A consumer must not
interpolate them into executable statements.

Consequently:

1. **Do not** treat this v0alpha1 fingerprint as complete production drift
   detection. An introspector must reject unsupported catalog objects or
   report explicitly that the snapshot is partial.
2. **Do not** generate or execute migrations from these strings yet.
3. **Do not** mistake structural validation for real database catalog
   validation. The database decides whether type families and constraints are
   legal.
4. Future versions must change the schema identifier and fingerprint domain
   when contract semantics or canonicalization change.

## Follow-on integration work (not implemented here)

1. Implement catalog introspection with coverage accounting, starting with
   PostgreSQL and SQLite. Reject incomplete snapshots for deployment gating.
2. Add a typed query expression IR with dialect-specific parameter binding,
   resolved column types/nullability, and explicit unsafe-SQL escape boundaries.
   Reuse Nulang's existing RFC 0018 query-comprehension direction.
3. Add a migration diff/planner with explicit destructive-change classification,
   reviewable SQL and serialization/lock checks.
4. Implement durable migration ledgers, database-schema drift preflight and
   deployment admission in Nulang Cloud; keep self-hosted equivalents available.
5. Add staged migration and recovery tests against actual database instances.

Focused test command:

```sh
cargo test --no-default-features --test relational_schema_contract
```

Do not confuse the contract fingerprint with
`semantic_schema::canonical_actor_state_schema_bytes` or the existing
RFC 0020 behavior manifest; those track different semantics.


## Experimental typed SQL read planning

The Rust API in `database::typed_sql` accepts a validated relational schema
contract plus an explicit `SelectSpec`. It returns SQL with **bound placeholders**
and an ordered list of `SqlParameter` values; it **does not execute** any SQL.

Supported in v0alpha1: one table, selected named columns, a single typed
equality or null predicate, and a compulsory `LIMIT` between 1 and 1,000.
Unknown tables/columns, invalid schema contracts, duplicate projections,
unsupported SQL types, mismatched parameter types and unbounded reads fail
closed. PostgreSQL uses positional `$1` parameters; SQLite uses `?1` and
both dialects safely quote identifiers.

Output projection metadata includes declared SQL type names and nullability.
This is **not** proof of Nulang language compile-time type checking, and is
**not** a general SQL dialect implementation. There are no joins, writes,
transactions, permission checks, expression indexes, prepared-statement
execution, nullable bind values, user-controlled ORDER BY, or migration DDL.

The caller must enforce authorization/tenant isolation, use a driver capable of
binding `SqlParameter` correctly, and independently ensure the live database
matches the versioned contract. In particular, SQLite's declared affinities
can differ from values actually stored at runtime.

Focused validation:

```sh
cargo test --no-default-features --test typed_sql_read_plan
```
