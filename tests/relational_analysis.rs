//! Regression tests for the compiler-internal, finite typed Horn-rule evaluator.
use nulang::relational_analysis::{
    Atom, ColumnType, Evidence, Fact, Limits, LogicError, LogicProgram, RelationSchema, Rule, Term,
    Value,
};

fn symbol(value: &str) -> Value {
    Value::Symbol(value.to_owned())
}

fn var(name: &str) -> Term {
    Term::Var(name.to_owned())
}

fn relation(name: &str, columns: Vec<ColumnType>) -> RelationSchema {
    RelationSchema::new(name, columns)
}

fn atom(name: &str, args: Vec<Term>) -> Atom {
    Atom::new(name, args)
}

fn fact(name: &str, a: &str, b: &str) -> Fact {
    Fact::new(name, vec![symbol(a), symbol(b)])
}

fn dependency_program() -> LogicProgram {
    LogicProgram::new(
        vec![
            relation("edge", vec![ColumnType::Symbol, ColumnType::Symbol]),
            relation("depends", vec![ColumnType::Symbol, ColumnType::Symbol]),
        ],
        vec![
            Rule::new(
                "direct_dependency",
                atom("depends", vec![var("a"), var("b")]),
                vec![atom("edge", vec![var("a"), var("b")])],
            ),
            Rule::new(
                "transitive_dependency",
                atom("depends", vec![var("a"), var("c")]),
                vec![
                    atom("depends", vec![var("a"), var("b")]),
                    atom("edge", vec![var("b"), var("c")]),
                ],
            ),
        ],
    )
    .expect("well-typed dependency program")
}

#[test]
fn recursive_reachability_is_complete_and_explainable() {
    let result = dependency_program()
        .evaluate(
            [
                fact("edge", "API", "Auth"),
                fact("edge", "Auth", "Database"),
                fact("edge", "Other", "Cache"),
            ],
            Limits::default(),
        )
        .unwrap();
    let target = fact("depends", "API", "Database");
    assert!(result.contains(&target));
    assert!(!result.contains(&fact("depends", "API", "Cache")));
    let proof = result.explain(&target).expect("derived fact has proof");
    match proof.evidence {
        Evidence::Derived { rule, premises } => {
            assert_eq!(rule, "transitive_dependency");
            assert_eq!(premises.len(), 2);
            assert_eq!(premises[0].fact, fact("depends", "API", "Auth"));
            assert_eq!(premises[1].fact, fact("edge", "Auth", "Database"));
            assert!(matches!(premises[1].evidence, Evidence::Input));
        }
        Evidence::Input => panic!("target must be derived"),
    }
}

#[test]
fn cycles_terminate_and_duplicate_facts_are_removed() {
    let result = dependency_program()
        .evaluate(
            [
                fact("edge", "A", "B"),
                fact("edge", "B", "A"),
                fact("edge", "A", "B"),
            ],
            Limits::default(),
        )
        .unwrap();
    let reached = result.facts_for("depends");
    assert_eq!(reached.len(), 4);
    assert!(reached.contains(&fact("depends", "A", "A")));
    assert!(reached.contains(&fact("depends", "B", "B")));
    assert!(result.explain(&fact("depends", "A", "A")).is_some());
}

#[test]
fn input_insertion_order_does_not_change_facts_or_explanations() {
    let edges = vec![
        fact("edge", "A", "B"),
        fact("edge", "B", "D"),
        fact("edge", "A", "C"),
        fact("edge", "C", "D"),
    ];
    let forward = dependency_program()
        .evaluate(edges.clone(), Limits::default())
        .unwrap();
    let backward = dependency_program()
        .evaluate(edges.into_iter().rev(), Limits::default())
        .unwrap();
    assert_eq!(forward.facts_for("depends"), backward.facts_for("depends"));
    assert_eq!(
        forward.explain(&fact("depends", "A", "D")),
        backward.explain(&fact("depends", "A", "D"))
    );
}

#[test]
fn rejects_unsafe_or_ambiguous_rules_at_construction() {
    let schemas = vec![relation(
        "pair",
        vec![ColumnType::Symbol, ColumnType::Symbol],
    )];
    let unsafe_rule = Rule::new(
        "unsafe",
        atom("pair", vec![var("a"), var("unbound")]),
        vec![atom("pair", vec![var("a"), var("a")])],
    );
    assert!(matches!(
        LogicProgram::new(schemas.clone(), vec![unsafe_rule]),
        Err(LogicError::InvalidRule(_))
    ));
    let mismatched_variable_type = Rule::new(
        "type_mismatch",
        atom("pair", vec![var("x"), var("x")]),
        vec![atom("counter", vec![var("x")])],
    );
    assert!(matches!(
        LogicProgram::new(
            [schemas, vec![relation("counter", vec![ColumnType::Int])]].concat(),
            vec![mismatched_variable_type]
        ),
        Err(LogicError::InvalidRule(_))
    ));
    assert!(matches!(
        LogicProgram::new(
            vec![relation("pair", vec![ColumnType::Symbol])],
            vec![Rule::new(
                "unknown",
                atom("pair", vec![var("x")]),
                vec![atom("missing", vec![var("x")])],
            )]
        ),
        Err(LogicError::InvalidRule(_))
    ));
}

#[test]
fn validates_ground_input_facts_and_schema_uniqueness() {
    assert!(matches!(
        LogicProgram::new(
            vec![
                relation("edge", vec![ColumnType::Symbol]),
                relation("edge", vec![ColumnType::Bool]),
            ],
            vec![],
        ),
        Err(LogicError::InvalidSchema(_))
    ));
    let program = dependency_program();
    assert!(matches!(
        program.evaluate([Fact::new("edge", vec![symbol("A")])], Limits::default()),
        Err(LogicError::InvalidFact(_))
    ));
    assert!(matches!(
        program.evaluate(
            [Fact::new("edge", vec![Value::Int(1), symbol("B")])],
            Limits::default()
        ),
        Err(LogicError::InvalidFact(_))
    ));
    assert!(matches!(
        program.evaluate([fact("missing", "A", "B")], Limits::default()),
        Err(LogicError::InvalidFact(_))
    ));
}

#[test]
fn fails_closed_when_fact_or_step_budget_is_exhausted() {
    let program = dependency_program();
    let input = [fact("edge", "A", "B"), fact("edge", "B", "C")];
    assert_eq!(
        program.evaluate(
            input.clone(),
            Limits {
                max_facts: 2,
                ..Limits::default()
            }
        ),
        Err(LogicError::LimitExceeded("max_facts"))
    );
    assert_eq!(
        program.evaluate(
            input,
            Limits {
                max_steps: 1,
                ..Limits::default()
            }
        ),
        Err(LogicError::LimitExceeded("max_steps"))
    );
    assert_eq!(
        program.evaluate(
            [fact("edge", "A", "B")],
            Limits {
                max_rounds: 1,
                ..Limits::default()
            }
        ),
        Err(LogicError::LimitExceeded("max_rounds"))
    );
}

#[test]
fn repeated_body_atoms_only_join_on_consistent_variable_bindings() {
    let program = LogicProgram::new(
        vec![
            relation("edge", vec![ColumnType::Symbol, ColumnType::Symbol]),
            relation("two_step", vec![ColumnType::Symbol, ColumnType::Symbol]),
        ],
        vec![Rule::new(
            "join",
            atom("two_step", vec![var("a"), var("c")]),
            vec![
                atom("edge", vec![var("a"), var("b")]),
                atom("edge", vec![var("b"), var("c")]),
            ],
        )],
    )
    .unwrap();
    let result = program
        .evaluate(
            [fact("edge", "A", "B"), fact("edge", "B", "C")],
            Limits::default(),
        )
        .unwrap();
    assert_eq!(
        result.facts_for("two_step"),
        vec![fact("two_step", "A", "C")]
    );
}

#[test]
fn typed_constant_predicates_filter_facts_without_coercion() {
    let program = LogicProgram::new(
        vec![
            relation(
                "actor",
                vec![ColumnType::Symbol, ColumnType::Int, ColumnType::Bool],
            ),
            relation("eligible", vec![ColumnType::Symbol]),
        ],
        vec![Rule::new(
            "eligible_actors",
            atom("eligible", vec![var("name")]),
            vec![atom(
                "actor",
                vec![
                    var("name"),
                    Term::Const(Value::Int(5)),
                    Term::Const(Value::Bool(true)),
                ],
            )],
        )],
    )
    .unwrap();
    let result = program
        .evaluate(
            [
                Fact::new(
                    "actor",
                    vec![symbol("match"), Value::Int(5), Value::Bool(true)],
                ),
                Fact::new(
                    "actor",
                    vec![symbol("no_rank"), Value::Int(4), Value::Bool(true)],
                ),
                Fact::new(
                    "actor",
                    vec![symbol("inactive"), Value::Int(5), Value::Bool(false)],
                ),
            ],
            Limits::default(),
        )
        .unwrap();
    assert_eq!(
        result.facts_for("eligible"),
        vec![Fact::new("eligible", vec![symbol("match")])]
    );
}

#[test]
fn rules_must_have_positive_bodies_and_unique_names() {
    let schemas = vec![relation("node", vec![ColumnType::Symbol])];
    let empty = Rule::new(
        "unconditional",
        atom("node", vec![Term::Const(symbol("A"))]),
        vec![],
    );
    assert!(matches!(
        LogicProgram::new(schemas.clone(), vec![empty]),
        Err(LogicError::InvalidRule(_))
    ));

    let valid = Rule::new(
        "same_name",
        atom("node", vec![var("n")]),
        vec![atom("node", vec![var("n")])],
    );
    assert!(matches!(
        LogicProgram::new(schemas, vec![valid.clone(), valid]),
        Err(LogicError::InvalidRule(_))
    ));
}

#[test]
fn transitive_closure_matches_reference_for_every_three_node_graph() {
    // Exhaust all 2^9 directed graphs, including self-edges and cycles.
    // The reference Floyd-Warshall closure starts non-reflexive; a node can
    // reach itself only through an actual non-empty path.
    let names = ["A", "B", "C"];
    let program = dependency_program();
    for mask in 0_u16..(1 << 9) {
        let mut edges = Vec::new();
        let mut reachable = [[false; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                if mask & (1 << (i * 3 + j)) != 0 {
                    edges.push(fact("edge", names[i], names[j]));
                    reachable[i][j] = true;
                }
            }
        }
        for k in 0..3 {
            for i in 0..3 {
                for j in 0..3 {
                    reachable[i][j] |= reachable[i][k] && reachable[k][j];
                }
            }
        }
        let result = program.evaluate(edges, Limits::default()).unwrap();
        for i in 0..3 {
            for j in 0..3 {
                assert_eq!(
                    result.contains(&fact("depends", names[i], names[j])),
                    reachable[i][j],
                    "graph mask={mask}, from={}, to={}",
                    names[i],
                    names[j]
                );
            }
        }
    }
}

#[test]
fn proof_dag_deduplicates_shared_premises_and_limits_expansion() {
    use nulang::relational_analysis::{ProofEvidence, ProofLimits};

    let program = LogicProgram::new(
        vec![
            relation("leaf", vec![ColumnType::Symbol]),
            relation("derived", vec![ColumnType::Symbol]),
            relation("root", vec![ColumnType::Symbol]),
        ],
        vec![
            Rule::new(
                "derive",
                atom("derived", vec![var("n")]),
                vec![atom("leaf", vec![var("n")])],
            ),
            Rule::new(
                "join_twice",
                atom("root", vec![var("n")]),
                vec![
                    atom("derived", vec![var("n")]),
                    atom("derived", vec![var("n")]),
                ],
            ),
        ],
    )
    .unwrap();
    let target = Fact::new("root", vec![symbol("A")]);
    let result = program
        .evaluate([Fact::new("leaf", vec![symbol("A")])], Limits::default())
        .unwrap();
    let dag = result
        .explain_dag(&target, ProofLimits::default())
        .unwrap()
        .unwrap();
    assert_eq!(dag.nodes.len(), 3, "shared premises must not be copied");
    assert_eq!(dag.nodes[dag.root].fact, target);
    assert!(matches!(
        &dag.nodes[dag.root].evidence,
        ProofEvidence::Derived { premises, .. }
        if premises.len() == 2 && premises[0] == premises[1] && premises[0] < dag.root
    ));
    assert_eq!(
        result.explain_dag(
            &target,
            ProofLimits {
                max_nodes: 2,
                max_depth: 12
            },
        ),
        Err(LogicError::LimitExceeded("max_proof_nodes"))
    );
    assert_eq!(
        result.explain_dag(
            &target,
            ProofLimits {
                max_nodes: 12,
                max_depth: 1
            },
        ),
        Err(LogicError::LimitExceeded("max_proof_depth"))
    );
    assert_eq!(
        result.explain_dag(
            &Fact::new("root", vec![symbol("absent")]),
            ProofLimits::default()
        ),
        Ok(None)
    );
}

#[test]
fn indexed_join_avoids_quadratic_candidate_scanning() {
    let program = LogicProgram::new(
        vec![
            relation("edge", vec![ColumnType::Int, ColumnType::Int]),
            relation("two_step", vec![ColumnType::Int, ColumnType::Int]),
        ],
        vec![Rule::new(
            "join",
            atom("two_step", vec![var("a"), var("c")]),
            vec![
                atom("edge", vec![var("a"), var("b")]),
                atom("edge", vec![var("b"), var("c")]),
            ],
        )],
    )
    .unwrap();
    let input = (0..400).map(|i| Fact::new("edge", vec![Value::Int(i), Value::Int(i + 1)]));
    let result = program.evaluate(input, Limits::default()).unwrap();
    assert_eq!(result.facts_for("two_step").len(), 399);
    let statistics = result.statistics();
    assert!(statistics.index_lookups > 0, "{statistics:?}");
    assert!(
        statistics.candidate_rows < 4_000,
        "indexed join should avoid scanning all 400 edges per join: {statistics:?}"
    );
}
