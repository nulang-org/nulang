//! Manual, dependency-free relational-join comparison.
//!
//! Run from the repo root with a build of the nulang crate:
//! rustc --edition=2021 -O benches/relational_analysis_smoke.rs \
//!   --extern nulang=path/to/libnulang.rlib -o /tmp/relational_bench
//! /tmp/relational_bench
//!
//! This is a smoke benchmark, not a calibrated multi-machine performance claim.

use nulang::relational_analysis::{
    Atom, ColumnType, Fact, Limits, LogicProgram, RelationSchema, Rule, Term, Value,
};
use std::time::Instant;

fn v(name: &str) -> Term {
    Term::Var(name.into())
}

fn main() {
    let n: i64 = std::env::args().nth(1).and_then(|v| v.parse().ok()).unwrap_or(3_000);
    assert!((2..=10_000).contains(&n), "expected 2..=10000 edges");
    let program = LogicProgram::new(
        vec![
            RelationSchema::new("edge", vec![ColumnType::Int, ColumnType::Int]),
            RelationSchema::new("two_hop", vec![ColumnType::Int, ColumnType::Int]),
        ],
        vec![Rule::new(
            "two_hop",
            Atom::new("two_hop", vec![v("a"), v("c")]),
            vec![
                Atom::new("edge", vec![v("a"), v("b")]),
                Atom::new("edge", vec![v("b"), v("c")]),
            ],
        )],
    )
    .expect("well-formed graph");
    let edges: Vec<_> = (0..n).map(|i| (i, i + 1)).collect();
    let input: Vec<_> = edges.iter()
        .map(|&(a, b)| Fact::new("edge", vec![Value::Int(a), Value::Int(b)]))
        .collect();

    let indexed_start = Instant::now();
    let result = program.evaluate(input.clone(), Limits::default()).expect("bounded evaluation");
    let indexed_elapsed = indexed_start.elapsed();

    // A deliberately unindexed nested-loop over the same typed input facts.
    // Unlike the raw-integer reference below, this preserves value comparisons.
    let typed_start = Instant::now();
    let mut typed_baseline = Vec::new();
    for left in &input {
        for right in &input {
            if left.values[1] == right.values[0] {
                let (Value::Int(a), Value::Int(c)) = (&left.values[0], &right.values[1]) else {
                    unreachable!("typed input")
                };
                typed_baseline.push((*a, *c));
            }
        }
    }
    let typed_elapsed = typed_start.elapsed();
    typed_baseline.sort_unstable();

    // Special-purpose raw-int loop: lower overhead, not an equivalent engine.
    let unindexed_start = Instant::now();
    let mut baseline = Vec::new();
    let mut examined: usize = 0;
    for &(a, b) in &edges {
        for &(candidate, c) in &edges {
            examined += 1;
            if b == candidate {
                baseline.push((a, c));
            }
        }
    }
    let unindexed_elapsed = unindexed_start.elapsed();
    baseline.sort_unstable();
    let mut actual: Vec<_> = result.facts_for("two_hop").iter().map(|fact| {
        match (&fact.values[0], &fact.values[1]) {
            (Value::Int(a), Value::Int(b)) => (*a, *b),
            _ => unreachable!("typed output"),
        }
    }).collect();
    actual.sort_unstable();
    assert_eq!(actual, baseline);
    assert_eq!(actual, typed_baseline);

    println!("edges: {n}, derived: {}", actual.len());
    println!("indexed engine:    {indexed_elapsed:?}");
    println!("unindexed typed:   {typed_elapsed:?}");
    println!("raw integer loop:  {unindexed_elapsed:?}");
    println!("indexed candidates: {}", result.statistics().candidate_rows);
    println!("naive candidates:   {examined}");
    println!("index lookups:      {}", result.statistics().index_lookups);
}
