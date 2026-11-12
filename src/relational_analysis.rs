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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColumnType {
    Symbol,
    Int,
    Bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

/// Bound expansion of one explanation, independently of evaluation budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofLimits {
    pub max_nodes: usize,
    pub max_depth: usize,
    /// Maximum number of premise references in the materialized DAG.
    pub max_edges: usize,
    /// Approximate payload budget for emitted nodes, strings, values and edges.
    /// This is not a hard peak allocator limit: temporary traversal and map
    /// structures are excluded.
    pub max_bytes: usize,
}

impl Default for ProofLimits {
    fn default() -> Self {
        Self {
            max_nodes: 2_048,
            max_depth: 256,
            max_edges: 16_384,
            max_bytes: 4 * 1024 * 1024,
        }
    }
}

/// A topologically ordered proof DAG: premise node IDs always precede users.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofDag {
    pub nodes: Vec<ProofNode>,
    pub root: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofNode {
    pub fact: Fact,
    pub evidence: ProofEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofEvidence {
    Input,
    Derived { rule: String, premises: Vec<usize> },
}

/// Statistics for comparing join strategies without timing noise.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EvaluationStats {
    pub candidate_rows: usize,
    pub index_lookups: usize,
    pub rounds: usize,
}

/// Completed result: never contains partial output from a budget failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult {
    relations: BTreeMap<String, BTreeSet<Vec<Value>>>,
    justifications: BTreeMap<Fact, Justification>,
    stats: EvaluationStats,
}

impl QueryResult {
    pub fn statistics(&self) -> EvaluationStats {
        self.stats
    }

    /// Return one deterministic, shared proof graph without recursive tree
    /// expansion. Limits guard total unique nodes and traversal depth.
    pub fn explain_dag(
        &self,
        fact: &Fact,
        limits: ProofLimits,
    ) -> Result<Option<ProofDag>, LogicError> {
        if !self.justifications.contains_key(fact) {
            return Ok(None);
        }
        let mut nodes = Vec::new();
        let mut ids: BTreeMap<Fact, usize> = BTreeMap::new();
        // Depth here means the longest derivation path (not only the first
        // DFS path). Shared subproofs may be reachable at multiple depths.
        let mut heights: Vec<usize> = Vec::new();
        let mut emitted_edges: usize = 0;
        let mut emitted_bytes: usize = 0;
        // Explicit postorder DFS: dependencies are always emitted first.
        let mut stack = vec![(fact.clone(), false, 0usize)];
        while let Some((current, exiting, depth)) = stack.pop() {
            if ids.contains_key(&current) {
                continue;
            }
            let justification = self
                .justifications
                .get(&current)
                .expect("derived premises must have a justification");
            if exiting {
                if nodes.len() >= limits.max_nodes {
                    return Err(LogicError::LimitExceeded("max_proof_nodes"));
                }
                let proof_height = match justification {
                    Justification::Input => 0,
                    Justification::Derived { premises, .. } => {
                        let mut longest = 0usize;
                        for premise in premises {
                            let step = heights[ids[premise]]
                                .checked_add(1)
                                .ok_or(LogicError::LimitExceeded("max_proof_depth"))?;
                            longest = longest.max(step);
                        }
                        longest
                    }
                };
                if proof_height > limits.max_depth {
                    return Err(LogicError::LimitExceeded("max_proof_depth"));
                }
                let (new_edges, rule_bytes) = match justification {
                    Justification::Input => (0usize, 0usize),
                    Justification::Derived { rule, premises } => (premises.len(), rule.len()),
                };
                emitted_edges = emitted_edges
                    .checked_add(new_edges)
                    .ok_or(LogicError::LimitExceeded("max_proof_edges"))?;
                if emitted_edges > limits.max_edges {
                    return Err(LogicError::LimitExceeded("max_proof_edges"));
                }

                // Account for the emitted structure, not for temporary BTreeMap,
                // DFS stack or allocator metadata. This is an approximate output
                // payload cap, deliberately separate from the evaluation budget.
                let mut node_bytes = std::mem::size_of::<ProofNode>()
                    .checked_add(current.relation.len())
                    .and_then(|n| n.checked_add(rule_bytes))
                    .and_then(|n| {
                        n.checked_add(
                            current
                                .values
                                .len()
                                .checked_mul(std::mem::size_of::<Value>())?,
                        )
                    })
                    .and_then(|n| {
                        n.checked_add(new_edges.checked_mul(std::mem::size_of::<usize>())?)
                    })
                    .ok_or(LogicError::LimitExceeded("max_proof_bytes"))?;
                for value in &current.values {
                    if let Value::Symbol(symbol) = value {
                        node_bytes = node_bytes
                            .checked_add(symbol.len())
                            .ok_or(LogicError::LimitExceeded("max_proof_bytes"))?;
                    }
                }
                emitted_bytes = emitted_bytes
                    .checked_add(node_bytes)
                    .ok_or(LogicError::LimitExceeded("max_proof_bytes"))?;
                if emitted_bytes > limits.max_bytes {
                    return Err(LogicError::LimitExceeded("max_proof_bytes"));
                }
                let evidence = match justification {
                    Justification::Input => ProofEvidence::Input,
                    Justification::Derived { rule, premises } => ProofEvidence::Derived {
                        rule: rule.clone(),
                        premises: premises.iter().map(|p| ids[p]).collect(),
                    },
                };
                ids.insert(current.clone(), nodes.len());
                heights.push(proof_height);
                nodes.push(ProofNode {
                    fact: current,
                    evidence,
                });
            } else {
                if depth > limits.max_depth {
                    return Err(LogicError::LimitExceeded("max_proof_depth"));
                }
                stack.push((current, true, depth));
                if let Justification::Derived { premises, .. } = justification {
                    for premise in premises.iter().rev() {
                        if !ids.contains_key(premise) {
                            stack.push((premise.clone(), false, depth + 1));
                        }
                    }
                }
            }
        }
        let root = ids[fact];
        Ok(Some(ProofDag { nodes, root }))
    }

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
    index_columns: BTreeMap<String, BTreeSet<usize>>,
}

/// Only index columns that can be constrained by a constant or by a variable
/// already bound in an earlier atom. Prevents needless tuple duplication for
/// unrelated columns of wide compiler relations.
#[derive(Debug, Clone, Default)]
struct TupleIndex {
    columns: BTreeMap<String, BTreeSet<usize>>,
    // One reference-counted tuple per insertion, referenced from all indexes.
    // Ordered sets keep derivation order deterministic across input shuffles.
    entries: HashMap<(String, usize, Value), BTreeSet<Arc<[Value]>>>,
}

impl TupleIndex {
    fn new(columns: &BTreeMap<String, BTreeSet<usize>>) -> Self {
        Self {
            columns: columns.clone(),
            entries: HashMap::new(),
        }
    }

    fn insert(&mut self, fact: &Fact) {
        if let Some(columns) = self.columns.get(&fact.relation) {
            if columns.is_empty() {
                return;
            }
            let shared: Arc<[Value]> = Arc::from(fact.values.clone());
            for &column in columns {
                self.entries
                    .entry((fact.relation.clone(), column, fact.values[column].clone()))
                    .or_default()
                    .insert(Arc::clone(&shared));
            }
        }
    }

    fn matching<'a>(
        &'a self,
        atom: &Atom,
        bindings: &BTreeMap<String, Value>,
    ) -> Option<Option<&'a BTreeSet<Arc<[Value]>>>> {
        let mut best: Option<&BTreeSet<Arc<[Value]>>> = None;
        for (column, term) in atom.args.iter().enumerate() {
            if !self
                .columns
                .get(&atom.relation)
                .is_some_and(|cols| cols.contains(&column))
            {
                continue;
            }
            let expected = match term {
                Term::Const(value) => Some(value),
                Term::Var(name) => bindings.get(name),
            };
            let Some(expected) = expected else { continue };
            let key = (atom.relation.clone(), column, expected.clone());
            let Some(bucket) = self.entries.get(&key) else {
                // An indexed equality with no matching values is a proven miss.
                return Some(None);
            };
            if best.is_none_or(|current| bucket.len() < current.len()) {
                best = Some(bucket);
            }
        }
        best.map(Some)
    }
}

/// Iterate over either indexed shared tuples or the unindexed relation.
enum Candidates<'a> {
    Indexed(std::collections::btree_set::Iter<'a, Arc<[Value]>>),
    Unindexed(std::collections::btree_set::Iter<'a, Vec<Value>>),
}

impl<'a> Iterator for Candidates<'a> {
    type Item = &'a [Value];

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Indexed(rows) => rows.next().map(AsRef::as_ref),
            Self::Unindexed(rows) => rows.next().map(Vec::as_slice),
        }
    }
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

        let mut index_columns: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
        for rule in &rules {
            let mut bound: BTreeSet<&str> = BTreeSet::new();
            for atom in &rule.body {
                for (column, term) in atom.args.iter().enumerate() {
                    let indexed = match term {
                        Term::Const(_) => true,
                        Term::Var(name) => bound.contains(name.as_str()),
                    };
                    if indexed {
                        index_columns
                            .entry(atom.relation.clone())
                            .or_default()
                            .insert(column);
                    }
                }
                for term in &atom.args {
                    if let Term::Var(name) = term {
                        bound.insert(name);
                    }
                }
            }
        }
        Ok(Self {
            schemas: by_name,
            rules,
            index_columns,
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

        let mut known_index = TupleIndex::new(&self.index_columns);
        for fact in proofs.keys() {
            known_index.insert(fact);
        }
        let mut delta = known.clone();
        let mut delta_index = known_index.clone();
        let mut rounds = 0usize;
        let mut stats = EvaluationStats::default();
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
                        &known_index,
                        &delta_index,
                        &mut BTreeMap::new(),
                        &mut Vec::new(),
                        &mut pending,
                        &mut stats,
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
                known_index.insert(&fact);
                proofs.insert(fact, justification);
            }
            delta = next;
            delta_index = TupleIndex::new(&self.index_columns);
            for (relation, tuples) in &delta {
                for tuple in tuples {
                    delta_index.insert(&Fact::new(relation, tuple.clone()));
                }
            }
            rounds += 1;
        }
        stats.rounds = rounds;

        Ok(QueryResult {
            relations: known,
            justifications: proofs,
            stats,
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
    known_index: &TupleIndex,
    delta_index: &TupleIndex,
    bindings: &mut BTreeMap<String, Value>,
    premises: &mut Vec<Fact>,
    pending: &mut BTreeMap<Fact, Justification>,
    stats: &mut EvaluationStats,
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
    let (source, index) = if position == pivot {
        (delta, delta_index)
    } else {
        (known, known_index)
    };
    let indexed = index.matching(atom, bindings);
    if matches!(indexed, Some(None)) {
        return Ok(());
    }
    let candidates = if let Some(Some(bucket)) = indexed {
        stats.index_lookups = stats.index_lookups.saturating_add(1);
        Candidates::Indexed(bucket.iter())
    } else {
        Candidates::Unindexed(source[&atom.relation].iter())
    };
    for tuple in candidates {
        stats.candidate_rows = stats
            .candidate_rows
            .checked_add(1)
            .ok_or(LogicError::LimitExceeded("max_steps"))?;
        if stats.candidate_rows > max_steps {
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
            premises.push(Fact::new(&atom.relation, tuple.to_vec()));
            let result = join(
                rule,
                position + 1,
                pivot,
                known,
                delta,
                known_index,
                delta_index,
                bindings,
                premises,
                pending,
                stats,
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

#[cfg(test)]
mod index_allocation_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn multiple_indexes_share_one_backing_tuple_allocation() {
        let mut columns = BTreeMap::new();
        columns.insert("multi".to_string(), [0, 1, 2].into_iter().collect());
        let mut index = TupleIndex::new(&columns);
        let values = vec![
            Value::Symbol("very long value".repeat(200)),
            Value::Int(17),
            Value::Bool(true),
        ];
        index.insert(&Fact::new("multi", values.clone()));
        let entry = |column| {
            index
                .entries
                .get(&("multi".to_string(), column, values[column].clone()))
                .unwrap()
                .iter()
                .next()
                .unwrap()
        };
        assert!(Arc::ptr_eq(entry(0), entry(1)));
        assert!(Arc::ptr_eq(entry(1), entry(2)));
    }
}
