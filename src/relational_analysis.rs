//! Bounded typed relational analysis for compiler-owned facts.
//!
//! This is deliberately *not* Prolog's execution model. Rules are positive,
//! range-restricted Horn clauses over a finite set of typed ground tuples.
//! Semi-naive fixed-point evaluation prevents a recursive rule from revisiting
//! the same complete derivations on every round. Evaluation is deterministic,
//! side-effect-free, and fails closed on resource exhaustion.
//!
//! Intended consumers include call-graph reachability, effect provenance and
//! capability analysis. This module is not wired into the language parser,
//! typechecker, runtime, or authorization decisions.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColumnType {
    Symbol,
    Int,
    Bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    Symbol(String),
    Int(i64),
    Bool(bool),
}

impl Value {
    fn column_type(&self) -> ColumnType {
        match self {
            Self::Symbol(_) => ColumnType::Symbol,
            Self::Int(_) => ColumnType::Int,
            Self::Bool(_) => ColumnType::Bool,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationSchema {
    pub name: String,
    pub columns: Vec<ColumnType>,
}

impl RelationSchema {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnType>) -> Self {
        Self {
            name: name.into(),
            columns,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fact {
    pub relation: String,
    pub values: Vec<Value>,
}

impl Fact {
    pub fn new(relation: impl Into<String>, values: Vec<Value>) -> Self {
        Self {
            relation: relation.into(),
            values,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Var(String),
    Const(Value),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Atom {
    pub relation: String,
    pub args: Vec<Term>,
}

impl Atom {
    pub fn new(relation: impl Into<String>, args: Vec<Term>) -> Self {
        Self {
            relation: relation.into(),
            args,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub name: String,
    pub head: Atom,
    pub body: Vec<Atom>,
}

impl Rule {
    pub fn new(name: impl Into<String>, head: Atom, body: Vec<Atom>) -> Self {
        Self {
            name: name.into(),
            head,
            body,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum ground facts, including the input corpus.
    pub max_facts: usize,
    /// Maximum tuple candidates examined across all joins and rounds.
    pub max_steps: usize,
    /// Maximum rounds, including the last saturation check.
    pub max_rounds: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_facts: 20_000,
            max_steps: 1_000_000,
            max_rounds: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicError {
    InvalidSchema(String),
    InvalidRule(String),
    InvalidFact(String),
    LimitExceeded(&'static str),
}

impl fmt::Display for LogicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema(msg) => write!(f, "invalid relation schema: {msg}"),
            Self::InvalidRule(msg) => write!(f, "invalid logic rule: {msg}"),
            Self::InvalidFact(msg) => write!(f, "invalid input fact: {msg}"),
            Self::LimitExceeded(resource) => {
                write!(
                    f,
                    "relational analysis exceeded {resource}; no partial result returned"
                )
            }
        }
    }
}

impl Error for LogicError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Justification {
    Input,
    Derived { rule: String, premises: Vec<Fact> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    pub fact: Fact,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    Input,
    Derived {
        rule: String,
        premises: Vec<Explanation>,
    },
}

/// Completed result: never contains partial output from a budget failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult {
    relations: BTreeMap<String, BTreeSet<Vec<Value>>>,
    justifications: BTreeMap<Fact, Justification>,
}

impl QueryResult {
    pub fn contains(&self, fact: &Fact) -> bool {
        self.relations
            .get(&fact.relation)
            .is_some_and(|tuples| tuples.contains(&fact.values))
    }

    pub fn facts_for(&self, relation: &str) -> Vec<Fact> {
        self.relations
            .get(relation)
            .into_iter()
            .flat_map(|tuples| tuples.iter())
            .map(|tuple| Fact::new(relation, tuple.clone()))
            .collect()
    }

    /// Returns one deterministic derivation tree, not every possible proof.
    /// Each derived fact depends only on facts from an earlier fixed-point round,
    /// making this tree acyclic even when the underlying relation has cycles.
    pub fn explain(&self, fact: &Fact) -> Option<Explanation> {
        let justification = self.justifications.get(fact)?;
        let evidence = match justification {
            Justification::Input => Evidence::Input,
            Justification::Derived { rule, premises } => Evidence::Derived {
                rule: rule.clone(),
                premises: premises
                    .iter()
                    .map(|premise| {
                        self.explain(premise)
                            .expect("every derived premise was present in an earlier round")
                    })
                    .collect(),
            },
        };
        Some(Explanation {
            fact: fact.clone(),
            evidence,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LogicProgram {
    schemas: BTreeMap<String, Vec<ColumnType>>,
    rules: Vec<Rule>,
}

impl LogicProgram {
    pub fn new(
        schemas: impl IntoIterator<Item = RelationSchema>,
        rules: Vec<Rule>,
    ) -> Result<Self, LogicError> {
        let mut by_name = BTreeMap::new();
        for schema in schemas {
            if schema.name.trim().is_empty() || schema.columns.is_empty() {
                return Err(LogicError::InvalidSchema(
                    "relation names and column lists must not be empty".into(),
                ));
            }
            if by_name
                .insert(schema.name.clone(), schema.columns)
                .is_some()
            {
                return Err(LogicError::InvalidSchema(format!(
                    "duplicate relation name: {}",
                    schema.name
                )));
            }
        }

        let mut names = BTreeSet::new();
        for rule in &rules {
            if rule.name.trim().is_empty() || !names.insert(rule.name.clone()) {
                return Err(LogicError::InvalidRule(format!(
                    "missing or duplicate rule name: {}",
                    rule.name
                )));
            }
            if rule.body.is_empty() {
                return Err(LogicError::InvalidRule(format!(
                    "{}: rules require a positive body; provide ground facts as input",
                    rule.name
                )));
            }

            let mut variables = BTreeMap::new();
            for predicate in &rule.body {
                validate_atom(&by_name, predicate, &mut variables)
                    .map_err(|msg| LogicError::InvalidRule(format!("{}: {msg}", rule.name)))?;
            }
            validate_atom(&by_name, &rule.head, &mut variables)
                .map_err(|msg| LogicError::InvalidRule(format!("{}: {msg}", rule.name)))?;

            // Positive, range-restricted rules only: every head variable must
            // appear in the body. No negation, cuts, free existential heads,
            // or implicit search domains.
            let bound: BTreeSet<_> = rule
                .body
                .iter()
                .flat_map(|atom| atom.args.iter())
                .filter_map(|term| match term {
                    Term::Var(v) => Some(v.as_str()),
                    Term::Const(_) => None,
                })
                .collect();
            for term in &rule.head.args {
                if let Term::Var(name) = term {
                    if !bound.contains(name.as_str()) {
                        return Err(LogicError::InvalidRule(format!(
                            "{}: unbound head variable {name}",
                            rule.name
                        )));
                    }
                }
            }
        }

        Ok(Self {
            schemas: by_name,
            rules,
        })
    }

    /// Evaluate using semi-naive joins: each rule evaluation must consume at
    /// least one fact newly added in the previous round.
    pub fn evaluate(
        &self,
        facts: impl IntoIterator<Item = Fact>,
        limits: Limits,
    ) -> Result<QueryResult, LogicError> {
        let mut known: BTreeMap<String, BTreeSet<Vec<Value>>> = self
            .schemas
            .keys()
            .map(|name| (name.clone(), BTreeSet::new()))
            .collect();
        let mut proofs = BTreeMap::new();
        let mut count = 0usize;

        for fact in facts {
            let Some(columns) = self.schemas.get(&fact.relation) else {
                return Err(LogicError::InvalidFact(format!(
                    "unknown relation {}",
                    fact.relation
                )));
            };
            if columns.len() != fact.values.len()
                || columns
                    .iter()
                    .zip(&fact.values)
                    .any(|(expected, actual)| *expected != actual.column_type())
            {
                return Err(LogicError::InvalidFact(format!(
                    "wrong arity or types for relation {}",
                    fact.relation
                )));
            }
            if known
                .get_mut(&fact.relation)
                .expect("schema exists")
                .insert(fact.values.clone())
            {
                count += 1;
                if count > limits.max_facts {
                    return Err(LogicError::LimitExceeded("max_facts"));
                }
                proofs.insert(fact, Justification::Input);
            }
        }

        let mut delta = known.clone();
        let mut rounds = 0usize;
        let mut steps = 0usize;
        while delta.values().any(|tuples| !tuples.is_empty()) {
            if rounds >= limits.max_rounds {
                return Err(LogicError::LimitExceeded("max_rounds"));
            }
            let mut pending = BTreeMap::<Fact, Justification>::new();
            for rule in &self.rules {
                for pivot in 0..rule.body.len() {
                    // Only run join paths that have a new tuple at the pivot.
                    if delta[&rule.body[pivot].relation].is_empty() {
                        continue;
                    }
                    join(
                        rule,
                        0,
                        pivot,
                        &known,
                        &delta,
                        &mut BTreeMap::new(),
                        &mut Vec::new(),
                        &mut pending,
                        &mut steps,
                        limits.max_steps,
                        limits.max_facts.saturating_sub(count),
                    )?;
                }
            }

            let mut next: BTreeMap<String, BTreeSet<Vec<Value>>> = self
                .schemas
                .keys()
                .map(|name| (name.clone(), BTreeSet::new()))
                .collect();
            for (fact, justification) in pending {
                if known[&fact.relation].contains(&fact.values) {
                    continue;
                }
                count += 1;
                if count > limits.max_facts {
                    return Err(LogicError::LimitExceeded("max_facts"));
                }
                next.get_mut(&fact.relation)
                    .expect("validated rule head")
                    .insert(fact.values.clone());
                known
                    .get_mut(&fact.relation)
                    .expect("validated rule head")
                    .insert(fact.values.clone());
                proofs.insert(fact, justification);
            }
            delta = next;
            rounds += 1;
        }

        Ok(QueryResult {
            relations: known,
            justifications: proofs,
        })
    }
}

fn validate_atom(
    schemas: &BTreeMap<String, Vec<ColumnType>>,
    atom: &Atom,
    variables: &mut BTreeMap<String, ColumnType>,
) -> Result<(), String> {
    let Some(columns) = schemas.get(&atom.relation) else {
        return Err(format!("unknown relation {}", atom.relation));
    };
    if columns.len() != atom.args.len() {
        return Err(format!(
            "{} expects {} arguments, got {}",
            atom.relation,
            columns.len(),
            atom.args.len()
        ));
    }
    for (term, expected) in atom.args.iter().zip(columns) {
        match term {
            Term::Var(name) => {
                if name.trim().is_empty() {
                    return Err("variable names must not be empty".into());
                }
                if let Some(previous) = variables.insert(name.clone(), *expected) {
                    if previous != *expected {
                        return Err(format!("variable {name} has conflicting types"));
                    }
                }
            }
            Term::Const(value) if value.column_type() != *expected => {
                return Err(format!("constant has wrong type in {}", atom.relation));
            }
            Term::Const(_) => {}
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn join(
    rule: &Rule,
    position: usize,
    pivot: usize,
    known: &BTreeMap<String, BTreeSet<Vec<Value>>>,
    delta: &BTreeMap<String, BTreeSet<Vec<Value>>>,
    bindings: &mut BTreeMap<String, Value>,
    premises: &mut Vec<Fact>,
    pending: &mut BTreeMap<Fact, Justification>,
    steps: &mut usize,
    max_steps: usize,
    max_pending: usize,
) -> Result<(), LogicError> {
    if position == rule.body.len() {
        let tuple = rule
            .head
            .args
            .iter()
            .map(|term| match term {
                Term::Const(value) => value.clone(),
                Term::Var(name) => bindings
                    .get(name)
                    .expect("safe rule guarantees a bound head")
                    .clone(),
            })
            .collect();
        let fact = Fact::new(&rule.head.relation, tuple);
        if known[&fact.relation].contains(&fact.values) {
            return Ok(());
        }
        if !pending.contains_key(&fact) && pending.len() >= max_pending {
            return Err(LogicError::LimitExceeded("max_facts"));
        }
        pending
            .entry(fact)
            .or_insert_with(|| Justification::Derived {
                rule: rule.name.clone(),
                premises: premises.clone(),
            });
        return Ok(());
    }

    let atom = &rule.body[position];
    let source = if position == pivot { delta } else { known };
    for tuple in &source[&atom.relation] {
        *steps = steps
            .checked_add(1)
            .ok_or(LogicError::LimitExceeded("max_steps"))?;
        if *steps > max_steps {
            return Err(LogicError::LimitExceeded("max_steps"));
        }
        let mut inserted = Vec::new();
        let mut compatible = true;
        for (term, value) in atom.args.iter().zip(tuple) {
            match term {
                Term::Const(expected) if expected != value => {
                    compatible = false;
                    break;
                }
                Term::Const(_) => {}
                Term::Var(name) => match bindings.get(name) {
                    Some(bound) if bound != value => {
                        compatible = false;
                        break;
                    }
                    Some(_) => {}
                    None => {
                        bindings.insert(name.clone(), value.clone());
                        inserted.push(name.clone());
                    }
                },
            }
        }
        if compatible {
            premises.push(Fact::new(&atom.relation, tuple.clone()));
            let result = join(
                rule,
                position + 1,
                pivot,
                known,
                delta,
                bindings,
                premises,
                pending,
                steps,
                max_steps,
                max_pending,
            );
            premises.pop();
            // Restore the bindings even when a nested join exceeds its budget.
            for name in inserted {
                bindings.remove(&name);
            }
            result?;
        } else {
            for name in inserted {
                bindings.remove(&name);
            }
        }
    }
    Ok(())
}
