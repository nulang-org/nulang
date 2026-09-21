//! Local physical index realization for durable entity snapshots.
//!
//! The compiler owns logical index declarations and query planning. This module
//! provides the first physical executor: a rebuildable in-memory index over
//! durable actor snapshots. It deliberately stores actor ids, not actor state,
//! so the durable persistence store remains the source of truth.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use crate::ast::IndexDecl;
use crate::data_plan::{LogicalAccessPath, LogicalQueryPlan};
use crate::runtime::persistence::{ActorSnapshot, PersistedValue};

/// Totally ordered representation of persisted values suitable for B-tree keys.
///
/// Float values use their IEEE-754 bit pattern transformed into total numeric
/// order. UpperBound is an internal sentinel used only for composite-prefix
/// range scans and can never be produced from application data.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum IndexAtom {
    Nil,
    Unit,
    Bool(bool),
    Int(i64),
    Float(u64),
    String(String),
    Actor(u64),
    UpperBound,
}

impl IndexAtom {
    fn from_persisted(value: &PersistedValue) -> Self {
        match value {
            PersistedValue::Int(value) => Self::Int(*value),
            PersistedValue::Float(value) => {
                let bits = value.to_bits();
                let ordered = if bits >> 63 == 0 {
                    bits | (1 << 63)
                } else {
                    !bits
                };
                Self::Float(ordered)
            }
            PersistedValue::Bool(value) => Self::Bool(*value),
            PersistedValue::String(value) => Self::String(value.clone()),
            PersistedValue::Nil => Self::Nil,
            PersistedValue::Unit => Self::Unit,
            PersistedValue::Actor(value) => Self::Actor(*value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct IndexKey(Vec<IndexAtom>);

#[derive(Debug, Clone)]
struct IndexState {
    decl: IndexDecl,
    entries: BTreeMap<IndexKey, BTreeSet<u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityIndexError {
    UnknownIndex {
        name: String,
    },
    MissingField {
        index: String,
        field: String,
        actor_id: u64,
    },
    MissingQueryValue {
        index: String,
        field: String,
    },
    UniqueViolation {
        index: String,
        actor_id: u64,
        conflicting_actor_id: u64,
    },
}

impl fmt::Display for EntityIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownIndex { name } => write!(f, "unknown entity index '{name}'"),
            Self::MissingField {
                index,
                field,
                actor_id,
            } => write!(
                f,
                "snapshot for actor {actor_id} is missing field '{field}' required by index '{index}'"
            ),
            Self::MissingQueryValue { index, field } => write!(
                f,
                "query using index '{index}' is missing equality value for '{field}'"
            ),
            Self::UniqueViolation {
                index,
                actor_id,
                conflicting_actor_id,
            } => write!(
                f,
                "unique index '{index}' rejects actor {actor_id}; key is already owned by actor {conflicting_actor_id}"
            ),
        }
    }
}

impl std::error::Error for EntityIndexError {}

/// In-memory physical indexes for one durable entity type.
///
/// The structure is intentionally rebuildable. It can be discarded and
/// reconstructed from current durable snapshots without changing application
/// semantics, which keeps the persistence store authoritative.
#[derive(Debug, Clone)]
pub struct MemoryEntityIndexes {
    entity: String,
    indexes: HashMap<String, IndexState>,
    actor_keys: HashMap<u64, HashMap<String, IndexKey>>,
    actor_ids: BTreeSet<u64>,
}

impl MemoryEntityIndexes {
    pub fn new(entity: impl Into<String>, declarations: &[IndexDecl]) -> Self {
        let indexes = declarations
            .iter()
            .cloned()
            .map(|decl| {
                (
                    decl.name.clone(),
                    IndexState {
                        decl,
                        entries: BTreeMap::new(),
                    },
                )
            })
            .collect();

        Self {
            entity: entity.into(),
            indexes,
            actor_keys: HashMap::new(),
            actor_ids: BTreeSet::new(),
        }
    }

    pub fn entity(&self) -> &str {
        &self.entity
    }

    pub fn len(&self) -> usize {
        self.actor_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.actor_ids.is_empty()
    }

    /// Insert or replace one actor snapshot in every declared index.
    ///
    /// Validation happens before mutation, including all unique constraints, so
    /// a failing update leaves the previous index state intact.
    pub fn upsert_snapshot(&mut self, snapshot: &ActorSnapshot) -> Result<(), EntityIndexError> {
        let new_keys = self.keys_for_snapshot(snapshot)?;

        for (name, key) in &new_keys {
            let state = self
                .indexes
                .get(name)
                .expect("key generation only visits known indexes");
            if !state.decl.unique {
                continue;
            }
            if let Some(existing) = state.entries.get(key) {
                if let Some(conflicting_actor_id) =
                    existing.iter().copied().find(|id| *id != snapshot.actor_id)
                {
                    return Err(EntityIndexError::UniqueViolation {
                        index: name.clone(),
                        actor_id: snapshot.actor_id,
                        conflicting_actor_id,
                    });
                }
            }
        }

        if let Some(old_keys) = self.actor_keys.get(&snapshot.actor_id).cloned() {
            for (name, key) in old_keys {
                if let Some(state) = self.indexes.get_mut(&name) {
                    if let Some(ids) = state.entries.get_mut(&key) {
                        ids.remove(&snapshot.actor_id);
                        if ids.is_empty() {
                            state.entries.remove(&key);
                        }
                    }
                }
            }
        }

        for (name, key) in &new_keys {
            let state = self
                .indexes
                .get_mut(name)
                .expect("key generation only visits known indexes");
            state
                .entries
                .entry(key.clone())
                .or_default()
                .insert(snapshot.actor_id);
        }

        self.actor_ids.insert(snapshot.actor_id);
        self.actor_keys.insert(snapshot.actor_id, new_keys);
        Ok(())
    }

    pub fn remove_actor(&mut self, actor_id: u64) {
        let Some(keys) = self.actor_keys.remove(&actor_id) else {
            self.actor_ids.remove(&actor_id);
            return;
        };

        for (name, key) in keys {
            if let Some(state) = self.indexes.get_mut(&name) {
                if let Some(ids) = state.entries.get_mut(&key) {
                    ids.remove(&actor_id);
                    if ids.is_empty() {
                        state.entries.remove(&key);
                    }
                }
            }
        }
        self.actor_ids.remove(&actor_id);
    }

    /// Return actor ids matching the access path portion of a logical plan.
    ///
    /// Residual predicates are intentionally not evaluated here. The caller
    /// loads authoritative actor state and applies them after candidate lookup.
    pub fn candidates(
        &self,
        plan: &LogicalQueryPlan,
        equality_values: &HashMap<String, PersistedValue>,
    ) -> Result<Vec<u64>, EntityIndexError> {
        match &plan.access {
            LogicalAccessPath::EntityScan => Ok(self.actor_ids.iter().copied().collect()),
            LogicalAccessPath::Index {
                name,
                fields,
                matched_prefix,
                ..
            } => {
                let state = self
                    .indexes
                    .get(name)
                    .ok_or_else(|| EntityIndexError::UnknownIndex { name: name.clone() })?;

                let prefix_fields = &fields[..*matched_prefix];
                let mut prefix = Vec::with_capacity(prefix_fields.len());
                for field in prefix_fields {
                    let value = equality_values.get(field).ok_or_else(|| {
                        EntityIndexError::MissingQueryValue {
                            index: name.clone(),
                            field: field.clone(),
                        }
                    })?;
                    prefix.push(IndexAtom::from_persisted(value));
                }

                let prefix_key = IndexKey(prefix.clone());
                if *matched_prefix == state.decl.fields.len() {
                    return Ok(state
                        .entries
                        .get(&prefix_key)
                        .into_iter()
                        .flatten()
                        .copied()
                        .collect());
                }

                let mut upper = prefix;
                upper.push(IndexAtom::UpperBound);
                let upper_key = IndexKey(upper);
                let mut candidates = BTreeSet::new();
                for (_, actor_ids) in state.entries.range(prefix_key..=upper_key) {
                    candidates.extend(actor_ids.iter().copied());
                }
                Ok(candidates.into_iter().collect())
            }
        }
    }

    fn keys_for_snapshot(
        &self,
        snapshot: &ActorSnapshot,
    ) -> Result<HashMap<String, IndexKey>, EntityIndexError> {
        let mut keys = HashMap::with_capacity(self.indexes.len());
        for (name, state) in &self.indexes {
            let mut atoms = Vec::with_capacity(state.decl.fields.len());
            for field in &state.decl.fields {
                let value =
                    snapshot
                        .state
                        .get(field)
                        .ok_or_else(|| EntityIndexError::MissingField {
                            index: name.clone(),
                            field: field.clone(),
                            actor_id: snapshot.actor_id,
                        })?;
                atoms.push(IndexAtom::from_persisted(value));
            }
            keys.insert(name.clone(), IndexKey(atoms));
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::IndexDecl;
    use crate::data_plan::{LogicalAccessPath, LogicalQueryPlan, QueryPredicate};
    use crate::types::Span;

    fn declarations() -> Vec<IndexDecl> {
        vec![
            IndexDecl {
                name: "email".to_string(),
                fields: vec!["email".to_string()],
                unique: true,
                span: Span::default(),
            },
            IndexDecl {
                name: "by_company_status".to_string(),
                fields: vec!["company".to_string(), "status".to_string()],
                unique: false,
                span: Span::default(),
            },
        ]
    }

    fn snapshot(
        actor_id: u64,
        email: &str,
        company: &str,
        status: &str,
    ) -> ActorSnapshot {
        ActorSnapshot {
            actor_id,
            sequence: 1,
            state: HashMap::from([
                (
                    "email".to_string(),
                    PersistedValue::String(email.to_string()),
                ),
                (
                    "company".to_string(),
                    PersistedValue::String(company.to_string()),
                ),
                (
                    "status".to_string(),
                    PersistedValue::String(status.to_string()),
                ),
            ]),
            ..ActorSnapshot::default()
        }
    }

    #[test]
    fn unique_index_rejects_conflict_without_corrupting_existing_state() {
        let mut indexes = MemoryEntityIndexes::new("Customer", &declarations());
        indexes
            .upsert_snapshot(&snapshot(1, "one@example.com", "Acme", "active"))
            .unwrap();

        let err = indexes
            .upsert_snapshot(&snapshot(2, "one@example.com", "Other", "active"))
            .unwrap_err();
        assert!(matches!(
            err,
            EntityIndexError::UniqueViolation {
                actor_id: 2,
                conflicting_actor_id: 1,
                ..
            }
        ));
        assert_eq!(indexes.len(), 1);
    }

    #[test]
    fn actor_update_moves_index_entries_atomically() {
        let mut indexes = MemoryEntityIndexes::new("Customer", &declarations());
        indexes
            .upsert_snapshot(&snapshot(1, "one@example.com", "Acme", "active"))
            .unwrap();
        indexes
            .upsert_snapshot(&snapshot(1, "new@example.com", "Beta", "active"))
            .unwrap();

        let plan = LogicalQueryPlan {
            entity: "Customer".to_string(),
            access: LogicalAccessPath::Index {
                name: "email".to_string(),
                fields: vec!["email".to_string()],
                unique: true,
                matched_prefix: 1,
            },
            residual_predicates: vec![],
        };

        let old = indexes
            .candidates(
                &plan,
                &HashMap::from([(
                    "email".to_string(),
                    PersistedValue::String("one@example.com".to_string()),
                )]),
            )
            .unwrap();
        let new = indexes
            .candidates(
                &plan,
                &HashMap::from([(
                    "email".to_string(),
                    PersistedValue::String("new@example.com".to_string()),
                )]),
            )
            .unwrap();

        assert!(old.is_empty());
        assert_eq!(new, vec![1]);
    }

    #[test]
    fn composite_prefix_lookup_returns_only_matching_candidates() {
        let mut indexes = MemoryEntityIndexes::new("Customer", &declarations());
        indexes
            .upsert_snapshot(&snapshot(1, "one@example.com", "Acme", "active"))
            .unwrap();
        indexes
            .upsert_snapshot(&snapshot(2, "two@example.com", "Acme", "paused"))
            .unwrap();
        indexes
            .upsert_snapshot(&snapshot(3, "three@example.com", "Beta", "active"))
            .unwrap();

        let plan = LogicalQueryPlan {
            entity: "Customer".to_string(),
            access: LogicalAccessPath::Index {
                name: "by_company_status".to_string(),
                fields: vec!["company".to_string(), "status".to_string()],
                unique: false,
                matched_prefix: 1,
            },
            residual_predicates: vec![],
        };

        let candidates = indexes
            .candidates(
                &plan,
                &HashMap::from([(
                    "company".to_string(),
                    PersistedValue::String("Acme".to_string()),
                )]),
            )
            .unwrap();
        assert_eq!(candidates, vec![1, 2]);
    }

    #[test]
    fn entity_scan_uses_known_actor_set() {
        let mut indexes = MemoryEntityIndexes::new("Customer", &declarations());
        indexes
            .upsert_snapshot(&snapshot(7, "seven@example.com", "Acme", "active"))
            .unwrap();
        indexes
            .upsert_snapshot(&snapshot(4, "four@example.com", "Beta", "active"))
            .unwrap();

        let plan = LogicalQueryPlan {
            entity: "Customer".to_string(),
            access: LogicalAccessPath::EntityScan,
            residual_predicates: vec![QueryPredicate::eq("status")],
        };

        assert_eq!(indexes.candidates(&plan, &HashMap::new()).unwrap(), vec![4, 7]);
    }

    #[test]
    fn remove_actor_removes_all_access_paths() {
        let mut indexes = MemoryEntityIndexes::new("Customer", &declarations());
        indexes
            .upsert_snapshot(&snapshot(1, "one@example.com", "Acme", "active"))
            .unwrap();
        indexes.remove_actor(1);

        assert!(indexes.is_empty());
    }
}
