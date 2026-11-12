# Experimental relational Query IR (Rust embedder API)

This is a **small reference implementation**, not a source-level Nulang query
syntax, SQLite/PostgreSQL adapter, NuDB query engine, or distributed query
executor. The library's public module is `nulang::query` and its API is
**Experimental**, so it may evolve without a source-language version change.

## Supported operations

- `Scan`: look up an explicitly registered in-memory table.
- `Filter`: evaluate a pure, typed Boolean expression.
- `Project`: build named output columns from pure expressions.
- `Sort`: stable ascending/descending ordering over a column.
- `Limit`: keep the first N results.

Values are `Int`, `Bool`, or `Text` with no implicit numeric/text
conversions. Missing fields and rows with invalid schemas fail explicitly.
This is **not** SQL NULL semantics; future SQL adapters must preserve their
native nullability and three-valued logic separately rather than silently
mapping missing fields to null.

## Example

```rust
use nulang::query::{Catalog, Column, DataType, Expr, Plan, Row, Scalar, Table, execute, optimize, explain};

let mut row = Row::new();
row.insert("age".into(), Scalar::Int(30));
row.insert("name".into(), Scalar::Text("Grace".into()));

let table = Table::new(
    vec![Column::new("age", DataType::Int), Column::new("name", DataType::Text)],
    vec![row],
).unwrap();
let mut catalog = Catalog::new();
catalog.insert("users", table).unwrap();

let plan = Plan::scan("users")
    .sort("age", false)
    .filter(Expr::gt(Expr::field("age"), Expr::int(18)))
    .project(vec![("name", Expr::field("name"))]);

let optimized = optimize(&plan, &catalog).unwrap();
println!("{}", explain(&optimized));
let results = execute(&optimized, &catalog).unwrap();
assert_eq!(results.len(), 1);
```

`infer_schema`, `execute` and `optimize` reject ill-typed expressions,
unknown fields and unknown data sources. `explain` is a deterministic
logical-plan formatter, **not** a measured-cost profile.

## Optimizer correctness

Only one rule currently rewrites queries: push `Filter` below `Sort`
when the whole plan has typechecked. This is sound for the current pure,
total expression set and stable sorting. No planner rule crosses `Limit`,
`Project`, an effect boundary, or a storage/authorization boundary.

There are no arbitrary closures inside filter expressions, no I/O effects,
and no external authority granted by a query. A future SQL/CRDT/actor
adapter must first validate schema, authorization, data ownership, and
backend semantic compatibility; it must not push arbitrary Nulang closures
onto remote nodes.

## Experimental compiler types and SQL lowering (stacked draft)

A new `query_types::result_collection_type(plan, catalog)` maps validated
Query IR projections into the compiler's existing closed
`Type::Array(Type::Record(...))` representation. This bridges the Rust
host IR to existing compiler **types**, not to Nulang's source parser,
effect checker, or bytecode backends.

`Catalog::register_schema(name, columns)` registers metadata-only sources
for SQL planning without loading records. Calling the local `execute`
reference interpreter on a metadata-only source explicitly fails.

`query_sql::compile_prepared(plan, catalog, dialect)` produces
`PreparedQuery { sql, parameters, output_columns }` for SQLite and
PostgreSQL. Literal values are **always bound**, never interpolated into SQL;
identifiers are quoted and embedded quotes escaped. Parameter order reflects
SQL textual order, including projection expressions before WHERE and LIMIT.

The supported SQL shape is deliberately restricted:
`Scan -> Filter* -> Project?/Sort? -> Limit?`, with projection/sort in
either order subject to alias shadowing restrictions. `Limit` requires
`Sort`. Operations with ambiguous ordering or unsafe collation assumptions
are rejected rather than rewritten.

**Important boundaries:**

- The SQL module emits prepared-statement metadata only; **it does not
  connect to or execute SQLite/PostgreSQL databases**.
- Schema registration is not authentication, authorization, live SQL catalog
  introspection, RLS enforcement, or proof of tenant isolation.
- External SQL sources have no inherent row ordering; even with ORDER BY,
  ties are not stable. Callers requiring deterministic pagination must use
  a guaranteed unique sort key verified by the DB adapter.
- Text ordering and greater-than comparisons reject absent explicit
  collation contracts. The emitted integer/Boolean/text types still require
  backend-specific result decoding and nullability validation.
- The eventual execution adapter must verify driver parameter binding,
  resource/time limits, schema drift, transaction isolation, and backend
  parity against concrete test databases before exposure to applications.

## Explicit limitations

- Rust host-side API only; no new lexer keywords or `.nula` syntax.
- No `Join`, `Aggregate`, database execution, index selection,
  adaptive costing, incremental views, or MVCC query integration yet.
- The local executor clones scans and materializes intermediate results; do
  not use this prototype for unbounded or untrusted data.
- There is no SQL three-valued logic, cross-actor execution, distributed
  transaction, or tenant isolation provided by this module.
- No CLI subcommand; `explain` is an embedder API and does not report
  runtime statistics.

See [RFC 0018](../RFC/0018-query-comprehensions-and-sagas.md) for the
larger accepted experimental query-comprehension proposal. Implementing
that source syntax requires separate parser, compiler and backend work.

## Regression tests

```sh
cargo test --locked --no-default-features --test query_ir
```

The tests pin interpreter behavior, strict schema/type errors, filter/sort
optimizer equivalence, `Limit` optimization barriers and stable explain text.
