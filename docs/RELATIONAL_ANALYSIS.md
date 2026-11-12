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
- One deterministic derivation tree per result for proof-oriented diagnostics.
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
assert!(result.explain(&dependency).is_some());
# Ok::<(), LogicError>(())
```

## Non-goals and future gates

This engine is **not integrated into compiler authority decisions**. It is an
optional analysis primitive, not an alternative to the existing fail-closed
effect/capability checker. Do not use it for security decisions until its
data extraction, cross-module symbol identity, and soundness guarantees are
independently verified.

The initial evaluator uses ordered sets and unindexed joins. It is appropriate
for small analysis graphs but **not** a production large-scale Datalog engine.
Proof explanations are recursively expanded trees and may duplicate shared
subproofs; use caution with large/deep graphs. No negation, aggregation,
general Prolog unification/backtracking, persistent fact store, or rule DSL
is provided.

Before promotion, require the main CI gates (including no-default-features,
default, optional WASM, lint and format), comparison against reference graph
fixtures, adversarial resource tests, and profiling. The next iteration should
add indexed join planning and bounded proof DAG reporting *before* any
automatic whole-program analysis or public Nulang syntax.
