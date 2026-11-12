# Experimental typed relational analysis

`src/relational_analysis.rs` is a **compiler-internal research API** for
Prolog-inspired declarative reasoning. It does not change the Nulang source
language, frozen syntax, HM unification, effect checking, capability policy,
bytecode, WASM, actor runtime, or authorization decisions.

## Scope

- Typed, named finite relations with `Symbol`, `Int`, and `Bool` columns.
- Positive Horn clauses, with typed constants, shared variables, and
  range-restricted heads. Input facts are ground and validated against schemas.
- Deterministic, deduplicated semi-naive fixed-point evaluation. Each round
  evaluates joins with at least one newly derived tuple.
- One deterministic derivation tree per result (legacy `explain` API), plus
  a bounded, shared proof DAG via `explain_dag(fact, ProofLimits::default())`.
  Proof DAGs are topologically ordered, with node and depth limits.
- Equality indexes only for columns constrained by constants or earlier atoms;
  indexed buckets use hash lookups but facts and proof choices stay deterministic.
  `statistics()` exposes candidate examinations and index-lookup counts.
- Fail-closed budget errors for maximum facts, join candidate steps, and rounds.
  A failed analysis never returns an incomplete relation as though it were complete.

Example (Rust API rather than proposed Nulang syntax):

```rust
use nulang::relational_analysis::*;

let program = LogicProgram::new(
    vec![
        RelationSchema::new("edge", vec![ColumnType::Symbol, ColumnType::Symbol]),
        RelationSchema::new("reach", vec![ColumnType::Symbol, ColumnType::Symbol]),
    ],
    vec![
        Rule::new(
            "direct",
            Atom::new("reach", vec![Term::Var("a".into()), Term::Var("b".into())]),
            vec![Atom::new("edge", vec![Term::Var("a".into()), Term::Var("b".into())])],
        ),
        Rule::new(
            "transitive",
            Atom::new("reach", vec![Term::Var("a".into()), Term::Var("c".into())]),
            vec![
                Atom::new("reach", vec![Term::Var("a".into()), Term::Var("b".into())]),
                Atom::new("edge", vec![Term::Var("b".into()), Term::Var("c".into())]),
            ],
        ),
    ],
)?;
let result = program.evaluate(
    [
        Fact::new("edge", vec![Value::Symbol("api".into()), Value::Symbol("auth".into())]),
        Fact::new("edge", vec![Value::Symbol("auth".into()), Value::Symbol("db".into())]),
    ],
    Limits::default(),
)?;
let dependency = Fact::new(
    "reach", vec![Value::Symbol("api".into()), Value::Symbol("db".into())]
);
assert!(result.contains(&dependency));
assert!(result.explain_dag(&dependency, ProofLimits::default())?.is_some());
# Ok::<(), LogicError>(())
```

## Non-goals and future gates

This engine is **not integrated into compiler authority decisions**. It is an
optional analysis primitive, not an alternative to the existing fail-closed
effect/capability checker. Do not use it for security decisions until its
data extraction, cross-module symbol identity, and soundness guarantees are
independently verified.

This prototype still uses unoptimized left-to-right rule ordering and copies
tuples into index buckets. It is **not** a production large-scale Datalog engine.
Index creation has a fixed overhead; for small graphs or specialized raw-integer
algorithms it can be slower despite scanning fewer candidates. The legacy
`explain` method recursively expands trees without budgets: prefer
`explain_dag` for arbitrary analysis graphs. The new bounded DAG also does not
impose a byte-level or total-edge memory limit. Do not expose this API directly
to untrusted inputs before resource accounting is strengthened.

No negation, aggregation, general Prolog unification/backtracking, persistent
fact store, or rule DSL is provided.

Before promotion, require the main CI gates (including no-default-features,
default, optional WASM, lint and format), adversarial resource tests, and
larger-graph profiling. The next step is measured join-order planning,
stronger memory budgets, and bounded diagnostic rendering *before* automatic
whole-program analysis or public Nulang syntax.

## Performance smoke benchmark

`benches/relational_analysis_smoke.rs` compares identical graph results from
three approaches: the typed relational engine, a typed unindexed nested join,
and a specialized raw-integer nested loop. Results vary with graph shape and
hardware; this is not a generalized speedup claim. It reports elapsed times,
candidate examinations, and the number of index lookups. The indexed engine
uses additional memory for its lookup buckets. Compile in optimized mode
(`rustc -O`) and compare multiple graph sizes before making performance
decisions. Run this manual bench with `cargo run --release --example` only
after packaging it as a Cargo example; currently it is a standalone Rust
source expecting a linked `nulang` crate.
