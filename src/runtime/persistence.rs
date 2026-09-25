//! Persistence engine for durable actors.
//!
//! v0.7 MVP: in-memory store plus JSON file backend. The store keeps a
//! snapshot of durable actor state and an append-only journal of messages.
//! On recovery the runtime loads the latest snapshot and replays the journal.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::durable_effect::DurableEffectId;
use crate::durable_effect_persistence::DurableEffectPersistenceRecord;
use crate::vm::Value;

use tracing::warn;

/// How a state field is persisted / replicated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StateModel {
    /// Ephemeral, reset on restart.
    Local,
    /// Snapshot + journal, survives restart.
    Durable,
    /// Event journal with deterministic replay.
    EventSourced,
    /// CRDT, merged across the cluster.
    Crdt(crate::ast::CrdtType),
}

impl StateModel {
    pub fn is_persistent(self) -> bool {
        matches!(
            self,
            StateModel::Durable | StateModel::EventSourced | StateModel::Crdt(_)
        )
    }
    pub fn is_crdt(self) -> bool {
        matches!(self, StateModel::Crdt(_))
    }
}

/// A serializable stand-in for `Value`. String values are resolved to their
/// UTF-8 content via `from_value_resolved` when a module constant pool is
/// available. When no module is available, string pointers / pool ids fall
/// back to `Nil` — callers that know they are dealing with strings should use
/// `from_value_resolved` with the actor's bytecode module.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "tag", content = "value")]
pub enum PersistedValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Nil,
    Unit,
    Actor(u64),
}

impl PersistedValue {
    pub fn from_value(v: &Value) -> Self {
        if let Some(i) = v.as_int() {
            PersistedValue::Int(i)
        } else if let Some(f) = v.as_float() {
            PersistedValue::Float(f)
        } else if let Some(b) = v.as_bool() {
            PersistedValue::Bool(b)
        } else if v.is_nil() {
            PersistedValue::Nil
        } else if v.is_unit() {
            PersistedValue::Unit
        } else if let Some(a) = v.as_actor_id() {
            PersistedValue::Actor(a)
        } else {
            // Pointers and string references cannot be safely restored without
            // the owning heap / constant pool, so they normalize to nil.
            // Callers with module access should use from_value_resolved instead.
            PersistedValue::Nil
        }
    }

    /// Like `from_value`, but resolves string pool IDs to their UTF-8 content
    /// when a bytecode module is available. Falls back to `from_value` for
    /// unresolved strings (which normalizes them to `Nil`).
    pub fn from_value_resolved(v: &Value, module: Option<&crate::bytecode::CodeModule>) -> Self {
        if let Some(id) = v.as_string_id() {
            if let Some(content) = module
                .and_then(|m| m.constants.get(id as usize))
                .and_then(|c| match c {
                    crate::bytecode::Constant::String(s) => Some(s.clone()),
                    _ => None,
                })
            {
                return PersistedValue::String(content);
            }
        }
        Self::from_value(v)
    }

    pub fn to_value(&self) -> Value {
        match self {
            PersistedValue::Int(i) => Value::int(*i),
            PersistedValue::Float(f) => Value::float(*f),
            PersistedValue::Bool(b) => Value::bool(*b),
            PersistedValue::String(_) => {
                // Strings must be restored via to_value_on_heap (which
                // allocates on the actor heap) or by callers that inter the
                // content into a module constant pool.  This path is a
                // data-loss sentinel — it should only be reached from
                // callers that lack actor context.
                Value::nil()
            }
            PersistedValue::Nil => Value::nil(),
            PersistedValue::Unit => Value::unit(),
            PersistedValue::Actor(a) => Value::actor_ref(*a),
        }
    }

    /// Convert to a `Value`, allocating string content on the actor heap.
    /// All other variants delegate to `to_value`.
    pub fn to_value_on_heap(&self, actor: &mut crate::runtime::actor::Actor) -> Value {
        match self {
            PersistedValue::String(s) => actor.allocate_string(s),
            other => other.to_value(),
        }
    }
}

/// A serializable snapshot of an actor's durable state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ActorSnapshot {
    pub actor_id: u64,
    pub sequence: u64,
    pub state: HashMap<String, PersistedValue>,
    /// For workflow actors, the name of the signal the current step is
    /// suspended waiting for, if any.  This is part of the snapshot so that
    /// recovery can decide whether the in-flight step must be re-triggered.
    pub waiting_signal: Option<String>,
    /// CRDT state belonging to the runtime's CrdtManager, serialized as
    /// `Vec<(crdt_id, crdt_type_u8, payload_bytes)>`.
    #[serde(default)]
    pub crdt_snapshot: Option<Vec<(u64, u8, Vec<u8>)>>,
    /// Maps CRDT field names (on this actor) to the `CrdtId` stored in
    /// `crdt_snapshot`. Needed so `recover_actor` can rebuild `CrdtManager.field_map`
    /// and `perform Crdt.*` keeps working after a restart.
    #[serde(default)]
    pub crdt_field_map: Option<HashMap<String, u64>>,
    /// Canonical external-authority tokens held by the actor at the time
    /// of the snapshot. Missing on pre-authority snapshots means empty
    /// authority (deny by default). Values are reparsed as a complete
    /// typed manifest before any recovered actor becomes observable.
    #[serde(default)]
    pub authority_tokens: BTreeSet<String>,
}

/// A journal entry records a message delivered to an actor.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JournalEntry {
    pub sequence: u64,
    pub behavior_id: u16,
    pub payload: Vec<PersistedValue>,
}

/// An event-sourced state change. Appended to the event log for each
/// mutation of an EventSourced field. On recovery, events are replayed
/// to reconstruct the field's current value without a full snapshot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventEntry {
    pub sequence: u64,
    /// Name of the EventSourced field being mutated.
    pub field_name: String,
    /// Event name (e.g. "Incremented", "Custom").
    pub event_name: String,
    /// Event arguments.
    pub args: Vec<PersistedValue>,
    /// Computed field value after the apply handler ran.
    /// Stored as a snapshot so recovery can reconstruct the exact
    /// post-apply value without re-executing inlined bytecode.
    #[serde(default = "default_event_value")]
    pub value: PersistedValue,
}

fn default_event_value() -> PersistedValue {
    PersistedValue::Int(1)
}

/// Stable identity of one accepted workflow command activation.
///
/// The command journal sequence is allocated before user code executes and
/// remains stable across suspension, replay, and terminal workflow events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct WorkflowActivationId {
    pub actor_id: u64,
    pub command_sequence: u64,
}

impl WorkflowActivationId {
    pub const fn new(actor_id: u64, command_sequence: u64) -> Self {
        Self {
            actor_id,
            command_sequence,
        }
    }
}

/// A workflow event records a durable, replayable step in a workflow actor.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "tag", content = "value")]
pub enum WorkflowEvent {
    /// Workflow instance started. `state` captures durable fields at creation.
    WorkflowStarted {
        sequence: u64,
        name: String,
        state: Vec<PersistedValue>,
    },
    /// A workflow step completed successfully.
    StepCompleted {
        sequence: u64,
        /// Stable identity of the accepted command this terminal event closes.
        /// Missing on legacy journal records written before activation identity
        /// was persisted.
        #[serde(default)]
        activation: Option<WorkflowActivationId>,
        step_name: String,
    },
    /// A timer was set for a workflow.
    TimerSet {
        sequence: u64,
        name: String,
        duration_ms: u64,
    },
    /// A previously set timer fired.
    TimerFired { sequence: u64, name: String },
    /// An external signal was delivered to the workflow.
    SignalReceived {
        sequence: u64,
        name: String,
        payload: Option<String>,
    },
    /// A saga step was compensated after failure.
    SagaCompensated { sequence: u64, step_name: String },
    /// A branch of a synthetic parallel step completed.
    ParallelBranchCompleted {
        sequence: u64,
        parallel_step_name: String,
        branch_name: String,
    },
    /// A workflow step failed at runtime (the step body returned an
    /// error). Recorded alongside saga compensation so failures are
    /// durable and surfacable (SPEC2 §10 known-issue #5: they used to be
    /// silent — exit 0, no diagnostic).
    StepFailed {
        sequence: u64,
        /// Stable identity of the accepted command this terminal event closes.
        /// Missing on legacy journal records written before activation identity
        /// was persisted.
        #[serde(default)]
        activation: Option<WorkflowActivationId>,
        step_name: String,
        error: String,
    },
    /// Any other event emitted by a workflow handler.
    Custom {
        sequence: u64,
        name: String,
        args: Vec<PersistedValue>,
    },
}

impl WorkflowEvent {
    /// Return the sequence number of this event.
    pub fn sequence(&self) -> u64 {
        match self {
            WorkflowEvent::WorkflowStarted { sequence, .. }
            | WorkflowEvent::StepCompleted { sequence, .. }
            | WorkflowEvent::TimerSet { sequence, .. }
            | WorkflowEvent::TimerFired { sequence, .. }
            | WorkflowEvent::SignalReceived { sequence, .. }
            | WorkflowEvent::SagaCompensated { sequence, .. }
            | WorkflowEvent::ParallelBranchCompleted { sequence, .. }
            | WorkflowEvent::StepFailed { sequence, .. }
            | WorkflowEvent::Custom { sequence, .. } => *sequence,
        }
    }

    /// Return the accepted command activation closed by a terminal event.
    ///
    /// Legacy events return `None` and remain readable for compatibility.
    pub fn activation_id(&self) -> Option<WorkflowActivationId> {
        match self {
            WorkflowEvent::StepCompleted { activation, .. }
            | WorkflowEvent::StepFailed { activation, .. } => *activation,
            _ => None,
        }
    }
}

/// Version of the atomic durable-transition persistence contract.
pub const DURABLE_TRANSITION_VERSION: u16 = 1;

/// One outbound actor message staged inside a durable transition.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DurableOutboxMessage {
    pub destination_actor_id: u64,
    pub ordinal: u32,
    pub behavior_id: u16,
    pub payload: Vec<PersistedValue>,
}

/// One logical durable actor/workflow/entity transition.
#[derive(Debug, Clone)]
pub struct DurableTransition {
    pub version: u16,
    pub actor_id: u64,
    pub activation_epoch: u64,
    pub sequence: u64,
    pub expected_previous_sequence: u64,
    pub command: Option<JournalEntry>,
    pub snapshot: Option<ActorSnapshot>,
    pub workflow_events: Vec<WorkflowEvent>,
    pub domain_events: Vec<EventEntry>,
    pub durable_effects: Vec<DurableEffectPersistenceRecord>,
    pub outbox: Vec<DurableOutboxMessage>,
}

/// Durable tail used for epoch/sequence fencing and idempotent commit retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableTail {
    pub activation_epoch: u64,
    pub sequence: u64,
    pub digest: [u8; 32],
}

/// Result of a successfully committed durable transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableCommit {
    pub actor_id: u64,
    pub activation_epoch: u64,
    pub sequence: u64,
    pub digest: [u8; 32],
}

impl DurableTransition {
    fn validate_structure(&self) -> io::Result<()> {
        if self.version != DURABLE_TRANSITION_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unsupported durable transition version {}; expected {}",
                    self.version, DURABLE_TRANSITION_VERSION
                ),
            ));
        }
        if self.activation_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable transition activation epoch must be non-zero",
            ));
        }
        if self.sequence == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable transition sequence must be non-zero",
            ));
        }
        let expected_sequence =
            self.expected_previous_sequence
                .checked_add(1)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "durable transition sequence overflow",
                    )
                })?;
        if self.sequence != expected_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "durable transition sequence {} does not follow expected predecessor {}",
                    self.sequence, self.expected_previous_sequence
                ),
            ));
        }

        if let Some(command) = &self.command {
            if command.sequence != self.sequence {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "durable transition command sequence does not match transition sequence",
                ));
            }
        }
        if let Some(snapshot) = &self.snapshot {
            if snapshot.actor_id != self.actor_id {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "durable transition snapshot actor does not match transition actor",
                ));
            }
            if snapshot.sequence != self.sequence {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "durable transition snapshot sequence does not match transition sequence",
                ));
            }
        }
        if self
            .workflow_events
            .iter()
            .any(|event| event.sequence() != self.sequence)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable transition workflow event sequence does not match transition sequence",
            ));
        }
        if self
            .domain_events
            .iter()
            .any(|event| event.sequence != self.sequence)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable transition domain event sequence does not match transition sequence",
            ));
        }

        let mut ordinals = BTreeSet::new();
        for message in &self.outbox {
            if !ordinals.insert(message.ordinal) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "duplicate durable outbox ordinal {} in transition",
                        message.ordinal
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Canonical BLAKE3 identity of this transition.
    pub fn digest(&self) -> io::Result<[u8; 32]> {
        self.validate_structure()?;

        #[derive(serde::Serialize)]
        struct DigestEnvelope<'a> {
            version: u16,
            actor_id: u64,
            activation_epoch: u64,
            sequence: u64,
            expected_previous_sequence: u64,
            command: &'a Option<JournalEntry>,
            snapshot: &'a Option<ActorSnapshot>,
            workflow_events: &'a [WorkflowEvent],
            domain_events: &'a [EventEntry],
            durable_effects: Vec<Vec<u8>>,
            outbox: &'a [DurableOutboxMessage],
        }

        let durable_effects = self
            .durable_effects
            .iter()
            .map(|record| {
                record
                    .to_json()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .collect::<io::Result<Vec<_>>>()?;

        let mut value = serde_json::to_value(DigestEnvelope {
            version: self.version,
            actor_id: self.actor_id,
            activation_epoch: self.activation_epoch,
            sequence: self.sequence,
            expected_previous_sequence: self.expected_previous_sequence,
            command: &self.command,
            snapshot: &self.snapshot,
            workflow_events: &self.workflow_events,
            domain_events: &self.domain_events,
            durable_effects,
            outbox: &self.outbox,
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

        canonicalize_json(&mut value);
        let bytes = serde_json::to_vec(&value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(*blake3::hash(&bytes).as_bytes())
    }
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = std::mem::take(map).into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            for (_, child) in &mut entries {
                canonicalize_json(child);
            }
            for (key, child) in entries {
                map.insert(key, child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                canonicalize_json(item);
            }
        }
        _ => {}
    }
}

/// Persistence backend trait. Implementations may be in-memory or disk-backed.
pub trait PersistenceStore: Send + Sync {
    /// Atomically commit one logical durable transition.
    ///
    /// Backends must return `Unsupported` until they can guarantee that all
    /// Nulang-owned records in the transition commit together.
    fn commit_transition(&mut self, _transition: DurableTransition) -> io::Result<DurableCommit> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic durable transitions are not supported by this persistence backend",
        ))
    }

    /// Load the newest durable record for one logical external effect.
    ///
    /// Backends that cannot recover durable-effect records MUST fail closed
    /// with `Unsupported`; returning `Ok(None)` would let a caller mistake
    /// "cannot inspect history" for "this effect never ran" and redispatch it.
    fn load_durable_effect(
        &self,
        _actor_id: u64,
        _effect_id: DurableEffectId,
    ) -> io::Result<Option<DurableEffectPersistenceRecord>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable effect recovery reads are not supported by this persistence backend",
        ))
    }

    /// Persist a snapshot of durable actor state.
    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()>;

    /// Load the latest snapshot for an actor, if any.
    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot>;

    /// Append a message to the actor's journal.
    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()>;

    /// Read all journal entries for an actor in order.
    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry>;

    /// Append a workflow event to the actor's event journal.
    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()>;

    /// Read all workflow events for an actor in order.
    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent>;

    /// Append a `TimerSet` workflow event.
    fn append_timer_set(
        &mut self,
        actor_id: u64,
        sequence: u64,
        name: String,
        duration_ms: u64,
    ) -> io::Result<()> {
        self.append_workflow_event(
            actor_id,
            WorkflowEvent::TimerSet {
                sequence,
                name,
                duration_ms,
            },
        )
    }

    /// Append a `TimerFired` workflow event.
    fn append_timer_fired(&mut self, actor_id: u64, sequence: u64, name: String) -> io::Result<()> {
        self.append_workflow_event(actor_id, WorkflowEvent::TimerFired { sequence, name })
    }

    /// Append a `SignalReceived` workflow event.
    fn append_signal_received(
        &mut self,
        actor_id: u64,
        sequence: u64,
        name: String,
        payload: Option<String>,
    ) -> io::Result<()> {
        self.append_workflow_event(
            actor_id,
            WorkflowEvent::SignalReceived {
                sequence,
                name,
                payload,
            },
        )
    }

    /// Append a `SagaCompensated` workflow event.
    fn append_saga_compensated(
        &mut self,
        actor_id: u64,
        sequence: u64,
        step_name: String,
    ) -> io::Result<()> {
        self.append_workflow_event(
            actor_id,
            WorkflowEvent::SagaCompensated {
                sequence,
                step_name,
            },
        )
    }

    /// Read timer-related workflow events (`TimerSet` and `TimerFired`).
    fn read_timer_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        self.read_workflow_events(actor_id)
            .into_iter()
            .filter(|e| {
                matches!(
                    e,
                    WorkflowEvent::TimerSet { .. } | WorkflowEvent::TimerFired { .. }
                )
            })
            .collect()
    }

    /// Read `SignalReceived` workflow events.
    fn read_signal_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        self.read_workflow_events(actor_id)
            .into_iter()
            .filter(|e| matches!(e, WorkflowEvent::SignalReceived { .. }))
            .collect()
    }

    /// Read `SagaCompensated` workflow events.
    fn read_saga_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        self.read_workflow_events(actor_id)
            .into_iter()
            .filter(|e| matches!(e, WorkflowEvent::SagaCompensated { .. }))
            .collect()
    }

    /// Append a `ParallelBranchCompleted` workflow event.
    fn append_parallel_branch_completed(
        &mut self,
        actor_id: u64,
        sequence: u64,
        parallel_step_name: String,
        branch_name: String,
    ) -> io::Result<()> {
        self.append_workflow_event(
            actor_id,
            WorkflowEvent::ParallelBranchCompleted {
                sequence,
                parallel_step_name,
                branch_name,
            },
        )
    }

    /// Read `ParallelBranchCompleted` workflow events.
    fn read_parallel_branch_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        self.read_workflow_events(actor_id)
            .into_iter()
            .filter(|e| matches!(e, WorkflowEvent::ParallelBranchCompleted { .. }))
            .collect()
    }

    /// Append an event to the actor's event-sourcing log.
    fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> io::Result<()>;

    /// Read all event-sourcing entries for an actor in order.
    fn read_events(&self, actor_id: u64) -> Vec<EventEntry>;

    /// Highest sequence number known for the actor.
    fn latest_sequence(&self, actor_id: u64) -> u64;

    /// Remove all data for an actor.
    fn clear(&mut self, actor_id: u64) -> io::Result<()>;

    /// Execute an arbitrary SQL query against the store.
    /// Returns rows as JSON arrays of column values. Default: not supported.
    fn query(&self, _sql: &str, _params: &[Value]) -> io::Result<Vec<String>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "DB.query is not supported by this persistence backend",
        ))
    }
}

/// In-memory persistence store. Useful for tests and ephemeral durable actors.
#[derive(Debug, Default, Clone)]
pub struct MemoryStore {
    snapshots: HashMap<u64, ActorSnapshot>,
    journals: HashMap<u64, Vec<JournalEntry>>,
    workflow_events: HashMap<u64, Vec<WorkflowEvent>>,
    events: HashMap<u64, Vec<EventEntry>>,
    durable_tails: HashMap<u64, DurableTail>,
    committed_transitions: HashMap<u64, Vec<DurableTransition>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn durable_tail(&self, actor_id: u64) -> Option<DurableTail> {
        self.durable_tails.get(&actor_id).copied()
    }

    pub fn committed_transitions(&self, actor_id: u64) -> &[DurableTransition] {
        self.committed_transitions
            .get(&actor_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

impl PersistenceStore for MemoryStore {
    fn load_durable_effect(
        &self,
        actor_id: u64,
        effect_id: DurableEffectId,
    ) -> io::Result<Option<DurableEffectPersistenceRecord>> {
        Ok(self
            .committed_transitions(actor_id)
            .iter()
            .rev()
            .flat_map(|transition| transition.durable_effects.iter().rev())
            .find(|record| record.effect().spec().id == effect_id)
            .cloned())
    }

    fn commit_transition(&mut self, transition: DurableTransition) -> io::Result<DurableCommit> {
        transition.validate_structure()?;
        let digest = transition.digest()?;

        if let Some(tail) = self.durable_tails.get(&transition.actor_id).copied() {
            if transition.activation_epoch == tail.activation_epoch
                && transition.sequence == tail.sequence
            {
                if digest == tail.digest {
                    return Ok(DurableCommit {
                        actor_id: transition.actor_id,
                        activation_epoch: tail.activation_epoch,
                        sequence: tail.sequence,
                        digest,
                    });
                }
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "conflicting durable transition already committed at this epoch/sequence",
                ));
            }
            if transition.activation_epoch < tail.activation_epoch {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "stale durable activation epoch {}; committed epoch is {}",
                        transition.activation_epoch, tail.activation_epoch
                    ),
                ));
            }
            if transition.expected_previous_sequence != tail.sequence {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "durable transition predecessor {} does not match committed tail {}",
                        transition.expected_previous_sequence, tail.sequence
                    ),
                ));
            }
        } else {
            // Atomic transitions may begin after legacy snapshot/journal history.
            // The first atomic commit must extend the actually persisted legacy
            // tail rather than pretending that history started at sequence 0.
            let snapshot_seq = self
                .snapshots
                .get(&transition.actor_id)
                .map(|snapshot| snapshot.sequence)
                .unwrap_or(0);
            let journal_seq = self
                .journals
                .get(&transition.actor_id)
                .and_then(|entries| entries.last().map(|entry| entry.sequence))
                .unwrap_or(0);
            let workflow_seq = self
                .workflow_events
                .get(&transition.actor_id)
                .and_then(|events| events.last().map(WorkflowEvent::sequence))
                .unwrap_or(0);
            let domain_seq = self
                .events
                .get(&transition.actor_id)
                .and_then(|events| events.last().map(|event| event.sequence))
                .unwrap_or(0);
            let legacy_tail = snapshot_seq
                .max(journal_seq)
                .max(workflow_seq)
                .max(domain_seq);
            if transition.expected_previous_sequence != legacy_tail {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "first atomic durable transition predecessor {} does not match legacy tail {}",
                        transition.expected_previous_sequence, legacy_tail
                    ),
                ));
            }
        }

        if let Some(snapshot) = &transition.snapshot {
            self.snapshots.insert(transition.actor_id, snapshot.clone());
        }
        if let Some(command) = &transition.command {
            self.journals
                .entry(transition.actor_id)
                .or_default()
                .push(command.clone());
        }
        if !transition.workflow_events.is_empty() {
            self.workflow_events
                .entry(transition.actor_id)
                .or_default()
                .extend(transition.workflow_events.iter().cloned());
        }
        if !transition.domain_events.is_empty() {
            self.events
                .entry(transition.actor_id)
                .or_default()
                .extend(transition.domain_events.iter().cloned());
        }

        let tail = DurableTail {
            activation_epoch: transition.activation_epoch,
            sequence: transition.sequence,
            digest,
        };
        self.durable_tails.insert(transition.actor_id, tail);
        self.committed_transitions
            .entry(transition.actor_id)
            .or_default()
            .push(transition.clone());

        Ok(DurableCommit {
            actor_id: transition.actor_id,
            activation_epoch: transition.activation_epoch,
            sequence: transition.sequence,
            digest,
        })
    }

    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()> {
        self.snapshots.insert(snapshot.actor_id, snapshot);
        Ok(())
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        self.snapshots.get(&actor_id).cloned()
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()> {
        self.journals.entry(actor_id).or_default().push(entry);
        Ok(())
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
        self.journals.get(&actor_id).cloned().unwrap_or_default()
    }

    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()> {
        self.workflow_events
            .entry(actor_id)
            .or_default()
            .push(event);
        Ok(())
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        self.workflow_events
            .get(&actor_id)
            .cloned()
            .unwrap_or_default()
    }

    fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> io::Result<()> {
        self.events.entry(actor_id).or_default().push(entry);
        Ok(())
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        self.events.get(&actor_id).cloned().unwrap_or_default()
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        let snapshot_seq = self
            .snapshots
            .get(&actor_id)
            .map(|s| s.sequence)
            .unwrap_or(0);
        let journal_seq = self
            .journals
            .get(&actor_id)
            .and_then(|j| j.last().map(|e| e.sequence))
            .unwrap_or(0);
        let wf_event_seq = self
            .workflow_events
            .get(&actor_id)
            .and_then(|e| e.last().map(|ev| ev.sequence()))
            .unwrap_or(0);
        let event_seq = self
            .events
            .get(&actor_id)
            .and_then(|e| e.last().map(|ev| ev.sequence))
            .unwrap_or(0);
        let transition_seq = self
            .durable_tails
            .get(&actor_id)
            .map(|tail| tail.sequence)
            .unwrap_or(0);
        snapshot_seq
            .max(journal_seq)
            .max(wf_event_seq)
            .max(event_seq)
            .max(transition_seq)
    }

    fn clear(&mut self, actor_id: u64) -> io::Result<()> {
        self.snapshots.remove(&actor_id);
        self.journals.remove(&actor_id);
        self.workflow_events.remove(&actor_id);
        self.events.remove(&actor_id);
        self.durable_tails.remove(&actor_id);
        self.committed_transitions.remove(&actor_id);
        Ok(())
    }
}

/// File-backed persistence store using JSON.
/// Each actor gets `<base_dir>/<actor_id>/snapshot.json`, `journal.jsonl`,
/// and `workflow_events.jsonl`.
#[derive(Debug, Clone)]
pub struct JsonFileStore {
    base_dir: PathBuf,
}

impl JsonFileStore {
    pub fn new<P: AsRef<Path>>(base_dir: P) -> io::Result<Self> {
        let base_dir = base_dir.as_ref().to_path_buf();
        fs::create_dir_all(&base_dir)?;
        Ok(JsonFileStore { base_dir })
    }

    fn actor_dir(&self, actor_id: u64) -> PathBuf {
        self.base_dir.join(format!("actor_{}", actor_id))
    }

    fn snapshot_path(&self, actor_id: u64) -> PathBuf {
        self.actor_dir(actor_id).join("snapshot.json")
    }

    fn journal_path(&self, actor_id: u64) -> PathBuf {
        self.actor_dir(actor_id).join("journal.jsonl")
    }

    fn workflow_events_path(&self, actor_id: u64) -> PathBuf {
        self.actor_dir(actor_id).join("workflow_events.jsonl")
    }

    fn events_path(&self, actor_id: u64) -> PathBuf {
        self.actor_dir(actor_id).join("events.jsonl")
    }
}

impl PersistenceStore for JsonFileStore {
    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()> {
        let dir = self.actor_dir(snapshot.actor_id);
        fs::create_dir_all(&dir)?;
        let path = self.snapshot_path(snapshot.actor_id);
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // Write to a temp file in the same directory, then atomically rename
        // it into place: a crash mid-write can no longer leave a truncated
        // snapshot.json that recovery would silently treat as "no state".
        let tmp_path = dir.join("snapshot.json.tmp");
        {
            let mut file = fs::File::create(&tmp_path)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        let path = self.snapshot_path(actor_id);
        // A missing file is the normal "no snapshot yet" case — stay silent.
        let data = fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&data) {
            Ok(snapshot) => Some(snapshot),
            Err(e) => {
                // A present-but-unparseable snapshot means corruption (e.g. an
                // older non-atomic write); log it instead of silently resetting
                // the actor's durable state on recovery.
                warn!(
                    "nulang-persist: failed to parse snapshot for actor {} at {}: {}",
                    actor_id,
                    path.display(),
                    e
                );
                None
            }
        }
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()> {
        let dir = self.actor_dir(actor_id);
        fs::create_dir_all(&dir)?;
        let path = self.journal_path(actor_id);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let json = serde_json::to_string(&entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{}", json)?;
        // fsync before returning so the append is durable, not just in
        // the page cache (same discipline as save_snapshot's temp file).
        file.sync_all()?;
        Ok(())
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
        let path = self.journal_path(actor_id);
        let data = match fs::read_to_string(path) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        data.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()> {
        let dir = self.actor_dir(actor_id);
        fs::create_dir_all(&dir)?;
        let path = self.workflow_events_path(actor_id);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let json = serde_json::to_string(&event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{}", json)?;
        // fsync before returning so the append is durable, not just in
        // the page cache (same discipline as save_snapshot's temp file).
        file.sync_all()?;
        Ok(())
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        let path = self.workflow_events_path(actor_id);
        let data = match fs::read_to_string(path) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        data.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> io::Result<()> {
        let dir = self.actor_dir(actor_id);
        fs::create_dir_all(&dir)?;
        let path = self.events_path(actor_id);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let json = serde_json::to_string(&entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{}", json)?;
        // fsync before returning so the event is durable, not just in the
        // page cache (same discipline as append_journal/append_workflow_event
        // and save_snapshot's temp file). EventSourced state reconstructs
        // from this log on recovery, so a lost append is a lost commit.
        file.sync_all()?;
        Ok(())
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        let path = self.events_path(actor_id);
        let data = match fs::read_to_string(path) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        data.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        let snapshot_seq = self
            .load_snapshot(actor_id)
            .map(|s| s.sequence)
            .unwrap_or(0);
        let journal_seq = self
            .read_journal(actor_id)
            .last()
            .map(|e| e.sequence)
            .unwrap_or(0);
        let wf_event_seq = self
            .read_workflow_events(actor_id)
            .last()
            .map(|e| e.sequence())
            .unwrap_or(0);
        let event_seq = self
            .read_events(actor_id)
            .last()
            .map(|e| e.sequence)
            .unwrap_or(0);
        snapshot_seq
            .max(journal_seq)
            .max(wf_event_seq)
            .max(event_seq)
    }

    fn clear(&mut self, actor_id: u64) -> io::Result<()> {
        let dir = self.actor_dir(actor_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// LibsqlStore — libSQL-backed persistence (local, remote Turso, or replica)
// ---------------------------------------------------------------------------

/// libSQL-backed persistence store.
///
/// Each actor gets one row in the `snapshots` table and zero or more rows in
/// the `journal` and `workflow_events` tables, same schema as the old SQLite
/// store.  State and payloads are serialized to JSON and stored as TEXT.
///
/// The same store also serves `perform DB.query(sql, params)` from Nulang
/// code via the `query()` method exposed through `PersistenceStore::query`.
/// Explicit durability/speed tradeoff for the local SQLite journal.
#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteSyncMode {
    /// `synchronous=OFF`: fastest, weakest durability (no fsync before
    /// commit reports success; a power loss can corrupt or lose the
    /// database).  For scratch data only.
    Off,
    /// `synchronous=NORMAL`: durable across application crashes and OS
    /// crashes, but a power loss can lose the most recent commits (WAL is
    /// not fsynced before each commit).
    Normal,
    /// `synchronous=FULL`: every commit is fsynced to stable storage
    /// before returning.  Strongest durability, slower commits.  This is
    /// the SQLite default and the default here.
    Full,
}

#[cfg(feature = "sqlite")]
pub struct LibsqlStore {
    conn: std::sync::Mutex<libsql::Connection>,
    rt: tokio::runtime::Runtime,
    path: PathBuf,
}

#[cfg(feature = "sqlite")]
impl LibsqlStore {
    /// Open (or create) a local file database with the default
    /// `SqliteSyncMode::Full` durability.  Pass `":memory:"` for an
    /// ephemeral in-memory store.
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::with_sync_mode(path, SqliteSyncMode::Full)
    }

    /// Open (or create) a local file database with an explicit journal
    /// durability mode.  Pass `":memory:"` for an ephemeral in-memory
    /// store.
    pub fn with_sync_mode<P: AsRef<Path>>(path: P, sync_mode: SqliteSyncMode) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let db_path = if path == Path::new(":memory:") {
            ":memory:".to_string()
        } else {
            path.to_string_lossy().into_owned()
        };
        let rt =
            tokio::runtime::Runtime::new().map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let db = rt.block_on(async {
            libsql::Builder::new_local(&db_path)
                .build()
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
        })?;
        let conn = db
            .connect()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let store = LibsqlStore {
            conn: std::sync::Mutex::new(conn),
            rt,
            path,
        };
        // Journal pragmas must be applied on the same connection that
        // runs the table DDL, before any writes.
        store.apply_pragmas(sync_mode)?;
        store.ensure_tables()?;
        Ok(store)
    }
    pub fn in_memory() -> io::Result<Self> {
        Self::new(":memory:")
    }

    /// Connect to a remote Turso database.
    ///
    /// Journal pragmas (`journal_mode`, `synchronous`) are deliberately
    /// skipped: a Turso server manages its own journal and durability
    /// policy.
    pub fn new_remote(url: &str, auth_token: &str) -> io::Result<Self> {
        let rt =
            tokio::runtime::Runtime::new().map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let db = rt.block_on(async {
            libsql::Builder::new_remote(url.to_string(), auth_token.to_string())
                .build()
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
        })?;
        let conn = db
            .connect()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let store = LibsqlStore {
            conn: std::sync::Mutex::new(conn),
            rt,
            path: PathBuf::from(url),
        };
        store.ensure_tables()?;
        Ok(store)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Acquire the database connection lock.
    fn conn(&self) -> std::sync::MutexGuard<'_, libsql::Connection> {
        self.conn.lock().unwrap()
    }

    /// Apply the journal durability pragmas on the local connection.
    /// `journal_mode=WAL` gives crash-safe concurrent readers/writers;
    /// `synchronous` controls how much fsync happens per commit.  The
    /// WAL setting persists in the database file, `synchronous` is
    /// per-connection.
    fn apply_pragmas(&self, sync_mode: SqliteSyncMode) -> io::Result<()> {
        let conn = self.conn();
        self.rt.block_on(async {
            // Drain the result rows: `journal_mode` returns one,
            // `synchronous` returns none.  libsql `query` handles both
            // shapes.
            let mut rows = conn
                .query("PRAGMA journal_mode=WAL", ())
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            while rows
                .next()
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?
                .is_some()
            {}
            let level = match sync_mode {
                SqliteSyncMode::Off => "OFF",
                SqliteSyncMode::Normal => "NORMAL",
                SqliteSyncMode::Full => "FULL",
            };
            let sql = format!("PRAGMA synchronous={level}");
            let mut rows = conn
                .query(&sql, ())
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            while rows
                .next()
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?
                .is_some()
            {}
            Ok(())
        })
    }

    fn ensure_tables(&self) -> io::Result<()> {
        let conn = self.conn();
        self.rt.block_on(async {
            conn.execute(
                "CREATE TABLE IF NOT EXISTS snapshots (
                    actor_id INTEGER PRIMARY KEY,
                    sequence INTEGER NOT NULL,
                    state TEXT NOT NULL,
                    waiting_signal TEXT,
                    crdt_snapshot TEXT,
                    crdt_field_map TEXT,
                    authority_tokens TEXT
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            // Migrate databases created before the waiting_signal column existed.
            let _ = conn
                .execute("ALTER TABLE snapshots ADD COLUMN waiting_signal TEXT", ())
                .await;
            // Migrate databases created before the crdt_snapshot column existed.
            let _ = conn
                .execute("ALTER TABLE snapshots ADD COLUMN crdt_snapshot TEXT", ())
                .await;
            // Migrate databases created before the crdt_field_map column existed.
            let _ = conn
                .execute("ALTER TABLE snapshots ADD COLUMN crdt_field_map TEXT", ())
                .await;
            // Authority was added after the original persistence schema. Old rows
            // remain NULL and therefore restore with empty (deny-by-default) authority.
            let _ = conn
                .execute("ALTER TABLE snapshots ADD COLUMN authority_tokens TEXT", ())
                .await;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS journal (
                    actor_id INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    behavior_id INTEGER NOT NULL,
                    payload TEXT NOT NULL,
                    PRIMARY KEY (actor_id, sequence)
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS workflow_events (
                    actor_id INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    event TEXT NOT NULL,
                    PRIMARY KEY (actor_id, sequence)
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS events (
                    actor_id INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    field_name TEXT NOT NULL,
                    event_name TEXT NOT NULL,
                    args TEXT NOT NULL,
                    value TEXT NOT NULL DEFAULT '1',
                    PRIMARY KEY (actor_id, sequence)
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            // Atomic durable-transition storage (RFC 0022). These tables are
            // additive to the legacy snapshot/journal tables so historical
            // stores remain readable while new commits gain one transaction
            // boundary and explicit fencing metadata.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS durable_tails (
                    actor_id INTEGER PRIMARY KEY,
                    activation_epoch INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    digest TEXT NOT NULL
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS durable_transitions (
                    actor_id INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    activation_epoch INTEGER NOT NULL,
                    expected_previous_sequence INTEGER NOT NULL,
                    digest TEXT NOT NULL,
                    PRIMARY KEY (actor_id, sequence)
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS durable_workflow_events (
                    actor_id INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    ordinal INTEGER NOT NULL,
                    event TEXT NOT NULL,
                    PRIMARY KEY (actor_id, sequence, ordinal)
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS durable_domain_events (
                    actor_id INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    ordinal INTEGER NOT NULL,
                    event TEXT NOT NULL,
                    PRIMARY KEY (actor_id, sequence, ordinal)
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS durable_effect_records (
                    actor_id INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    ordinal INTEGER NOT NULL,
                    record TEXT NOT NULL,
                    PRIMARY KEY (actor_id, sequence, ordinal)
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS durable_outbox (
                    actor_id INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    ordinal INTEGER NOT NULL,
                    destination_actor_id INTEGER NOT NULL,
                    behavior_id INTEGER NOT NULL,
                    payload TEXT NOT NULL,
                    PRIMARY KEY (actor_id, sequence, ordinal)
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            Ok(())
        })
    }

    /// Execute a SQL query and return rows as a Vec of JSON strings.
    pub fn query(&self, sql: &str, params: &[Value]) -> io::Result<Vec<String>> {
        let conn = self.conn();
        self.rt.block_on(async {
            let param_values: Vec<String> = params
                .iter()
                .map(|v| {
                    if let Some(i) = v.as_int() {
                        i.to_string()
                    } else if let Some(f) = v.as_float() {
                        f.to_string()
                    } else if let Some(b) = v.as_bool() {
                        b.to_string()
                    } else {
                        v.to_string_repr()
                    }
                })
                .collect();
            let param_refs: Vec<&str> = param_values.iter().map(|s| s.as_str()).collect();
            let mut rows = conn
                .query(sql, libsql::params_from_iter(param_refs))
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            let mut results = Vec::new();
            loop {
                match rows.next().await {
                    Ok(Some(row)) => {
                        let mut cols: Vec<serde_json::Value> = Vec::new();
                        for i in 0..row.column_count() {
                            let val = row
                                .get_value(i)
                                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                            let json_val = match val {
                                libsql::Value::Null => serde_json::Value::Null,
                                libsql::Value::Integer(n) => {
                                    serde_json::value::Number::from_i128(n as i128)
                                        .map(serde_json::Value::Number)
                                        .unwrap_or(serde_json::Value::Null)
                                }
                                libsql::Value::Real(f) => serde_json::value::Number::from_f64(f)
                                    .map(serde_json::Value::Number)
                                    .unwrap_or(serde_json::Value::Null),
                                libsql::Value::Text(s) => serde_json::Value::String(s),
                                libsql::Value::Blob(_) => serde_json::Value::Null,
                            };
                            cols.push(json_val);
                        }
                        let json = serde_json::to_string(&cols)
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                        results.push(json);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
                }
            }
            Ok(results)
        })
    }
}

#[cfg(feature = "sqlite")]
impl PersistenceStore for LibsqlStore {
    fn load_durable_effect(
        &self,
        actor_id: u64,
        effect_id: DurableEffectId,
    ) -> io::Result<Option<DurableEffectPersistenceRecord>> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT record FROM durable_effect_records
                     WHERE actor_id = ?1
                     ORDER BY sequence DESC, ordinal DESC",
                    libsql::params![actor_id as i64],
                )
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;

            while let Some(row) = rows
                .next()
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?
            {
                let json: String = row.get(0).map_err(|error| {
                    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                })?;
                let record = DurableEffectPersistenceRecord::from_json(json.as_bytes())
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                if record.effect().spec().id == effect_id {
                    return Ok(Some(record));
                }
            }
            Ok(None)
        })
    }

    fn commit_transition(&mut self, transition: DurableTransition) -> io::Result<DurableCommit> {
        transition.validate_structure()?;
        let digest = transition.digest()?;
        let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();

        // Serialize everything before opening the transaction. Once the first
        // SQL write happens, only database operations remain fallible and any
        // error rolls the entire transition back.
        let snapshot_data = transition
            .snapshot
            .as_ref()
            .map(|snapshot| {
                Ok::<_, io::Error>((
                    serde_json::to_string(&snapshot.state)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                    serde_json::to_string(&snapshot.crdt_snapshot)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                    serde_json::to_string(&snapshot.crdt_field_map)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                    serde_json::to_string(&snapshot.authority_tokens)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                ))
            })
            .transpose()?;
        let command_payload = transition
            .command
            .as_ref()
            .map(|entry| {
                serde_json::to_string(&entry.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            })
            .transpose()?;
        let workflow_json = transition
            .workflow_events
            .iter()
            .map(|event| {
                serde_json::to_string(event)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let domain_json = transition
            .domain_events
            .iter()
            .map(|event| {
                serde_json::to_string(event)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let effect_json = transition
            .durable_effects
            .iter()
            .map(|record| {
                let bytes = record
                    .to_json()
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let outbox_payload_json = transition
            .outbox
            .iter()
            .map(|message| {
                serde_json::to_string(&message.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            })
            .collect::<io::Result<Vec<_>>>()?;

        let conn = self.conn();
        self.rt.block_on(async {
            let tx = conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

            let current_tail = {
                let mut rows = tx
                    .query(
                        "SELECT activation_epoch, sequence, digest
                         FROM durable_tails WHERE actor_id = ?1",
                        libsql::params![transition.actor_id as i64],
                    )
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                match rows
                    .next()
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?
                {
                    Some(row) => {
                        let epoch: i64 = row
                            .get(0)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                        let sequence: i64 = row
                            .get(1)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                        let stored_digest: String = row
                            .get(2)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                        Some((epoch as u64, sequence as u64, stored_digest))
                    }
                    None => None,
                }
            };

            if let Some((epoch, sequence, stored_digest)) = &current_tail {
                if transition.activation_epoch == *epoch && transition.sequence == *sequence {
                    if stored_digest == &digest_hex {
                        tx.commit()
                            .await
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                        return Ok(DurableCommit {
                            actor_id: transition.actor_id,
                            activation_epoch: transition.activation_epoch,
                            sequence: transition.sequence,
                            digest,
                        });
                    }
                    let _ = tx.rollback().await;
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "conflicting durable transition already committed at this epoch/sequence",
                    ));
                }
                if transition.activation_epoch < *epoch {
                    let _ = tx.rollback().await;
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "stale durable activation epoch {}; committed epoch is {}",
                            transition.activation_epoch, epoch
                        ),
                    ));
                }
                if transition.expected_previous_sequence != *sequence {
                    let _ = tx.rollback().await;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "durable transition predecessor {} does not match committed tail {}",
                            transition.expected_previous_sequence, sequence
                        ),
                    ));
                }
            } else {
                // Continue legacy history on the first atomic commit. This is
                // evaluated inside the same write transaction so no legacy
                // writer can advance the tail between validation and commit.
                let mut rows = tx
                    .query(
                        "SELECT COALESCE(MAX(sequence), 0) FROM (
                            SELECT sequence FROM snapshots WHERE actor_id = ?1
                            UNION ALL
                            SELECT sequence FROM journal WHERE actor_id = ?1
                            UNION ALL
                            SELECT sequence FROM workflow_events WHERE actor_id = ?1
                            UNION ALL
                            SELECT sequence FROM events WHERE actor_id = ?1
                         )",
                        libsql::params![transition.actor_id as i64],
                    )
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                let row = rows
                    .next()
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "failed to read legacy durable tail",
                        )
                    })?;
                let legacy_tail: i64 = row
                    .get(0)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                drop(rows);
                if transition.expected_previous_sequence != legacy_tail as u64 {
                    let _ = tx.rollback().await;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "first atomic durable transition predecessor {} does not match legacy tail {}",
                            transition.expected_previous_sequence, legacy_tail
                        ),
                    ));
                }
            }

            if let (Some(snapshot), Some((state_json, crdt_json, crdt_field_map_json, authority_json))) =
                (&transition.snapshot, &snapshot_data)
            {
                tx.execute(
                    "INSERT INTO snapshots
                     (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(actor_id) DO UPDATE SET
                       sequence=excluded.sequence,
                       state=excluded.state,
                       waiting_signal=excluded.waiting_signal,
                       crdt_snapshot=excluded.crdt_snapshot,
                       crdt_field_map=excluded.crdt_field_map,
                       authority_tokens=excluded.authority_tokens",
                    libsql::params![
                        snapshot.actor_id as i64,
                        snapshot.sequence as i64,
                        state_json.as_str(),
                        snapshot.waiting_signal.as_deref(),
                        crdt_json.as_str(),
                        crdt_field_map_json.as_str(),
                        authority_json.as_str()
                    ],
                )
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            }

            if let (Some(command), Some(payload_json)) = (&transition.command, &command_payload) {
                tx.execute(
                    "INSERT INTO journal (actor_id, sequence, behavior_id, payload)
                     VALUES (?1, ?2, ?3, ?4)",
                    libsql::params![
                        transition.actor_id as i64,
                        command.sequence as i64,
                        command.behavior_id as i64,
                        payload_json.as_str()
                    ],
                )
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            }

            for (ordinal, event_json) in workflow_json.iter().enumerate() {
                tx.execute(
                    "INSERT INTO durable_workflow_events (actor_id, sequence, ordinal, event)
                     VALUES (?1, ?2, ?3, ?4)",
                    libsql::params![
                        transition.actor_id as i64,
                        transition.sequence as i64,
                        ordinal as i64,
                        event_json.as_str()
                    ],
                )
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            }

            for (ordinal, event_json) in domain_json.iter().enumerate() {
                tx.execute(
                    "INSERT INTO durable_domain_events (actor_id, sequence, ordinal, event)
                     VALUES (?1, ?2, ?3, ?4)",
                    libsql::params![
                        transition.actor_id as i64,
                        transition.sequence as i64,
                        ordinal as i64,
                        event_json.as_str()
                    ],
                )
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            }

            for (ordinal, record_json) in effect_json.iter().enumerate() {
                tx.execute(
                    "INSERT INTO durable_effect_records (actor_id, sequence, ordinal, record)
                     VALUES (?1, ?2, ?3, ?4)",
                    libsql::params![
                        transition.actor_id as i64,
                        transition.sequence as i64,
                        ordinal as i64,
                        record_json.as_str()
                    ],
                )
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            }

            for (message, payload_json) in transition.outbox.iter().zip(&outbox_payload_json) {
                tx.execute(
                    "INSERT INTO durable_outbox
                     (actor_id, sequence, ordinal, destination_actor_id, behavior_id, payload)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    libsql::params![
                        transition.actor_id as i64,
                        transition.sequence as i64,
                        message.ordinal as i64,
                        message.destination_actor_id as i64,
                        message.behavior_id as i64,
                        payload_json.as_str()
                    ],
                )
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            }

            tx.execute(
                "INSERT INTO durable_transitions
                 (actor_id, sequence, activation_epoch, expected_previous_sequence, digest)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                libsql::params![
                    transition.actor_id as i64,
                    transition.sequence as i64,
                    transition.activation_epoch as i64,
                    transition.expected_previous_sequence as i64,
                    digest_hex.as_str()
                ],
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

            if let Some((old_epoch, old_sequence, old_digest)) = current_tail {
                let changed = tx
                    .execute(
                        "UPDATE durable_tails
                         SET activation_epoch = ?1, sequence = ?2, digest = ?3
                         WHERE actor_id = ?4
                           AND activation_epoch = ?5
                           AND sequence = ?6
                           AND digest = ?7",
                        libsql::params![
                            transition.activation_epoch as i64,
                            transition.sequence as i64,
                            digest_hex.as_str(),
                            transition.actor_id as i64,
                            old_epoch as i64,
                            old_sequence as i64,
                            old_digest.as_str()
                        ],
                    )
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                if changed != 1 {
                    let _ = tx.rollback().await;
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "durable tail changed while committing transition",
                    ));
                }
            } else {
                tx.execute(
                    "INSERT INTO durable_tails
                     (actor_id, activation_epoch, sequence, digest)
                     VALUES (?1, ?2, ?3, ?4)",
                    libsql::params![
                        transition.actor_id as i64,
                        transition.activation_epoch as i64,
                        transition.sequence as i64,
                        digest_hex.as_str()
                    ],
                )
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            }

            tx.commit()
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

            Ok(DurableCommit {
                actor_id: transition.actor_id,
                activation_epoch: transition.activation_epoch,
                sequence: transition.sequence,
                digest,
            })
        })
    }

    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()> {
        let state_json = serde_json::to_string(&snapshot.state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let crdt_json = serde_json::to_string(&snapshot.crdt_snapshot)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let crdt_field_map_json = serde_json::to_string(&snapshot.crdt_field_map)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let authority_json = serde_json::to_string(&snapshot.authority_tokens)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let conn = self.conn();
        self.rt.block_on(async {
            conn.execute(
                "INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(actor_id) DO UPDATE SET sequence=excluded.sequence, state=excluded.state, waiting_signal=excluded.waiting_signal, crdt_snapshot=excluded.crdt_snapshot, crdt_field_map=excluded.crdt_field_map, authority_tokens=excluded.authority_tokens",
                libsql::params![snapshot.actor_id as i64, snapshot.sequence as i64, state_json, snapshot.waiting_signal.as_deref(), crdt_json.as_str(), crdt_field_map_json.as_str(), authority_json.as_str()],
            ).await.map(|_| ()).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
        })
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens FROM snapshots WHERE actor_id = ?1",
                    libsql::params![actor_id as i64],
                )
                .await
                .ok()?;
            let row = rows.next().await.ok()??;
            let sequence: i64 = row.get(0).ok()?;
            let state_json: String = row.get(1).ok()?;
            let waiting_signal: Option<String> = row.get(2).ok()?;
            let crdt_json: Option<String> = row.get(3).ok()?;
            let crdt_field_map_json: Option<String> = row.get(4).ok()?;
            let authority_json: Option<String> = row.get(5).ok()?;
            let crdt_snapshot: Option<Vec<(u64, u8, Vec<u8>)>> = match crdt_json {
                Some(j) => serde_json::from_str(&j).ok()?,
                None => None,
            };
            let crdt_field_map: Option<HashMap<String, u64>> = match crdt_field_map_json {
                Some(j) => serde_json::from_str(&j).ok()?,
                None => None,
            };
            let authority_tokens: BTreeSet<String> = match authority_json {
                Some(json) => match serde_json::from_str(&json) {
                    Ok(tokens) => tokens,
                    Err(err) => {
                        warn!(
                            "nulang-persist: invalid authority metadata for actor {}: {}",
                            actor_id, err
                        );
                        return None;
                    }
                },
                None => BTreeSet::new(),
            };
            let state: HashMap<String, PersistedValue> = serde_json::from_str(&state_json).ok()?;
            Some(ActorSnapshot {
                actor_id,
                sequence: sequence as u64,
                state,
                waiting_signal,
                crdt_snapshot,
                crdt_field_map,
                authority_tokens,
            })
        })
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()> {
        let payload_json = serde_json::to_string(&entry.payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let conn = self.conn();
        self.rt.block_on(async {
            conn.execute(
                "INSERT INTO journal (actor_id, sequence, behavior_id, payload) VALUES (?1, ?2, ?3, ?4)",
                libsql::params![actor_id as i64, entry.sequence as i64, entry.behavior_id as i64, payload_json],
            ).await.map(|_| ()).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
        })
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = match conn
                .query(
                    "SELECT sequence, behavior_id, payload FROM journal
                 WHERE actor_id = ?1 ORDER BY sequence ASC",
                    libsql::params![actor_id as i64],
                )
                .await
            {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let mut entries = Vec::new();
            loop {
                match rows.next().await {
                    Ok(Some(row)) => {
                        let seq: i64 = match row.get(0) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let bid: i64 = match row.get(1) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let payload_json: String = match row.get(2) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let payload: Vec<PersistedValue> = match serde_json::from_str(&payload_json)
                        {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        entries.push(JournalEntry {
                            sequence: seq as u64,
                            behavior_id: bid as u16,
                            payload,
                        });
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            entries
        })
    }

    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()> {
        let event_json = serde_json::to_string(&event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let conn = self.conn();
        self.rt.block_on(async {
            conn.execute(
                "INSERT INTO workflow_events (actor_id, sequence, event) VALUES (?1, ?2, ?3)",
                libsql::params![actor_id as i64, event.sequence() as i64, event_json],
            )
            .await
            .map(|_| ())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
        })
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut events = Vec::new();

            if let Ok(mut rows) = conn
                .query(
                    "SELECT event FROM workflow_events
                     WHERE actor_id = ?1 ORDER BY sequence ASC",
                    libsql::params![actor_id as i64],
                )
                .await
            {
                loop {
                    match rows.next().await {
                        Ok(Some(row)) => {
                            let event_json: String = match row.get(0) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            if let Ok(event) = serde_json::from_str(&event_json) {
                                events.push(event);
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            }

            if let Ok(mut rows) = conn
                .query(
                    "SELECT event FROM durable_workflow_events
                     WHERE actor_id = ?1 ORDER BY sequence ASC, ordinal ASC",
                    libsql::params![actor_id as i64],
                )
                .await
            {
                loop {
                    match rows.next().await {
                        Ok(Some(row)) => {
                            let event_json: String = match row.get(0) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            if let Ok(event) = serde_json::from_str(&event_json) {
                                events.push(event);
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            }

            events.sort_by_key(WorkflowEvent::sequence);
            events
        })
    }

    fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> io::Result<()> {
        let args_json = serde_json::to_string(&entry.args)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let conn = self.conn();
        self.rt.block_on(async {
            let value_json = serde_json::to_string(&entry.value)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            conn.execute(
                "INSERT INTO events (actor_id, sequence, field_name, event_name, args, value) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                libsql::params![actor_id as i64, entry.sequence as i64, entry.field_name, entry.event_name, args_json, value_json],
            ).await.map(|_| ()).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
        })
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut entries = Vec::new();

            if let Ok(mut rows) = conn
                .query(
                    "SELECT sequence, field_name, event_name, args, value FROM events
                     WHERE actor_id = ?1 ORDER BY sequence ASC",
                    libsql::params![actor_id as i64],
                )
                .await
            {
                loop {
                    match rows.next().await {
                        Ok(Some(row)) => {
                            let sequence: i64 = match row.get(0) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            let field_name: String = match row.get(1) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            let event_name: String = match row.get(2) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            let args_json: String = match row.get(3) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            let value_json: String = match row.get(4) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            let args = match serde_json::from_str(&args_json) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            let value = match serde_json::from_str(&value_json) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            entries.push(EventEntry {
                                sequence: sequence as u64,
                                field_name,
                                event_name,
                                args,
                                value,
                            });
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            }

            if let Ok(mut rows) = conn
                .query(
                    "SELECT event FROM durable_domain_events
                     WHERE actor_id = ?1 ORDER BY sequence ASC, ordinal ASC",
                    libsql::params![actor_id as i64],
                )
                .await
            {
                loop {
                    match rows.next().await {
                        Ok(Some(row)) => {
                            let event_json: String = match row.get(0) {
                                Ok(value) => value,
                                Err(_) => continue,
                            };
                            if let Ok(event) = serde_json::from_str(&event_json) {
                                entries.push(event);
                            }
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            }

            entries.sort_by_key(|entry| entry.sequence);
            entries
        })
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        let conn = self.conn();
        self.rt.block_on(async {
            let snapshot_seq: Option<i64> = async {
                let mut rows = conn
                    .query(
                        "SELECT sequence FROM snapshots WHERE actor_id = ?1",
                        libsql::params![actor_id as i64],
                    )
                    .await
                    .ok()?;
                let row = rows.next().await.ok()??;
                row.get(0).ok()
            }
            .await;
            let journal_seq: Option<i64> = async {
                let mut rows = conn
                    .query(
                        "SELECT sequence FROM journal WHERE actor_id = ?1 ORDER BY sequence DESC LIMIT 1",
                        libsql::params![actor_id as i64],
                    )
                    .await
                    .ok()?;
                let row = rows.next().await.ok()??;
                row.get(0).ok()
            }
            .await;
            let wf_event_seq: Option<i64> = async {
                let mut rows = conn
                    .query(
                        "SELECT sequence FROM workflow_events WHERE actor_id = ?1 ORDER BY sequence DESC LIMIT 1",
                        libsql::params![actor_id as i64],
                    )
                    .await
                    .ok()?;
                let row = rows.next().await.ok()??;
                row.get(0).ok()
            }
            .await;
            let event_seq: Option<i64> = async {
                let mut rows = conn
                    .query(
                        "SELECT sequence FROM events WHERE actor_id = ?1 ORDER BY sequence DESC LIMIT 1",
                        libsql::params![actor_id as i64],
                    )
                    .await
                    .ok()?;
                let row = rows.next().await.ok()??;
                row.get(0).ok()
            }
            .await;
            let atomic_seq: Option<i64> = async {
                let mut rows = conn
                    .query(
                        "SELECT sequence FROM durable_tails WHERE actor_id = ?1",
                        libsql::params![actor_id as i64],
                    )
                    .await
                    .ok()?;
                let row = rows.next().await.ok()??;
                row.get(0).ok()
            }
            .await;

            snapshot_seq
                .unwrap_or(0)
                .max(journal_seq.unwrap_or(0))
                .max(wf_event_seq.unwrap_or(0))
                .max(event_seq.unwrap_or(0))
                .max(atomic_seq.unwrap_or(0)) as u64
        })
    }

    fn clear(&mut self, actor_id: u64) -> io::Result<()> {
        let conn = self.conn();
        self.rt.block_on(async {
            for table in [
                "snapshots",
                "journal",
                "workflow_events",
                "events",
                "durable_workflow_events",
                "durable_domain_events",
                "durable_effect_records",
                "durable_outbox",
                "durable_transitions",
                "durable_tails",
            ] {
                let sql = format!("DELETE FROM {table} WHERE actor_id = ?1");
                conn.execute(&sql, libsql::params![actor_id as i64])
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// RocksDbStore — production LSM-tree persistence backend
// ---------------------------------------------------------------------------

#[cfg(feature = "rocksdb")]
pub struct RocksDbStore {
    db: rocksdb::DB,
}

#[cfg(feature = "rocksdb")]
impl RocksDbStore {
    const CF_SNAPSHOTS: &'static str = "snapshots";
    const CF_JOURNAL: &'static str = "journal";
    const CF_WORKFLOW_EVENTS: &'static str = "workflow_events";
    const CF_EVENTS: &'static str = "events";

    /// Open (or create) a RocksDB-backed store at `path`.
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        // Each actor's data is logically separated by key prefix; column
        // families keep the four streams isolated so iteration and deletion
        // stay cheap.
        let cfs = vec![
            rocksdb::ColumnFamilyDescriptor::new(Self::CF_SNAPSHOTS, rocksdb::Options::default()),
            rocksdb::ColumnFamilyDescriptor::new(Self::CF_JOURNAL, rocksdb::Options::default()),
            rocksdb::ColumnFamilyDescriptor::new(
                Self::CF_WORKFLOW_EVENTS,
                rocksdb::Options::default(),
            ),
            rocksdb::ColumnFamilyDescriptor::new(Self::CF_EVENTS, rocksdb::Options::default()),
        ];
        let db = rocksdb::DB::open_cf_descriptors(&opts, path, cfs)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(RocksDbStore { db })
    }

    fn actor_key(actor_id: u64) -> [u8; 8] {
        actor_id.to_be_bytes()
    }

    fn actor_seq_key(actor_id: u64, sequence: u64) -> [u8; 16] {
        let mut key = [0u8; 16];
        key[..8].copy_from_slice(&actor_id.to_be_bytes());
        key[8..].copy_from_slice(&sequence.to_be_bytes());
        key
    }

    fn cf(&self, name: &str) -> io::Result<&rocksdb::ColumnFamily> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("missing CF {}", name)))
    }
}

#[cfg(feature = "rocksdb")]
impl PersistenceStore for RocksDbStore {
    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()> {
        let cf = self.cf(Self::CF_SNAPSHOTS)?;
        let json = serde_json::to_string(&snapshot)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.db
            .put_cf(cf, Self::actor_key(snapshot.actor_id), json.as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        // RocksDB's `put` is durable once flushed.  Sync the WAL so the
        // snapshot survives a process crash before we report success.
        self.db
            .flush_wal(true)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        let cf = self.cf(Self::CF_SNAPSHOTS).ok()?;
        let bytes = self.db.get_cf(cf, Self::actor_key(actor_id)).ok()??;
        serde_json::from_slice(&bytes).ok()
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()> {
        let cf = self.cf(Self::CF_JOURNAL)?;
        let json = serde_json::to_string(&entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.db
            .put_cf(
                cf,
                Self::actor_seq_key(actor_id, entry.sequence),
                json.as_bytes(),
            )
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        self.db
            .flush_wal(true)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
        let cf = match self.cf(Self::CF_JOURNAL) {
            Ok(cf) => cf,
            Err(_) => return Vec::new(),
        };
        let mut entries = Vec::new();
        let start = Self::actor_seq_key(actor_id, 0);
        let mut iter = self.db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
        );
        while let Some(Ok((key, value))) = iter.next() {
            if key.len() < 8 || key[..8] != Self::actor_key(actor_id) {
                break;
            }
            if let Ok(entry) = serde_json::from_slice::<JournalEntry>(&value) {
                entries.push(entry);
            }
        }
        entries
    }

    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()> {
        let cf = self.cf(Self::CF_WORKFLOW_EVENTS)?;
        let json = serde_json::to_string(&event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.db
            .put_cf(
                cf,
                Self::actor_seq_key(actor_id, event.sequence()),
                json.as_bytes(),
            )
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        self.db
            .flush_wal(true)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        let cf = match self.cf(Self::CF_WORKFLOW_EVENTS) {
            Ok(cf) => cf,
            Err(_) => return Vec::new(),
        };
        let mut events = Vec::new();
        let start = Self::actor_seq_key(actor_id, 0);
        let mut iter = self.db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
        );
        while let Some(Ok((key, value))) = iter.next() {
            if key.len() < 8 || key[..8] != Self::actor_key(actor_id) {
                break;
            }
            if let Ok(event) = serde_json::from_slice::<WorkflowEvent>(&value) {
                events.push(event);
            }
        }
        events
    }

    fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> io::Result<()> {
        let cf = self.cf(Self::CF_EVENTS)?;
        let json = serde_json::to_string(&entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.db
            .put_cf(
                cf,
                Self::actor_seq_key(actor_id, entry.sequence),
                json.as_bytes(),
            )
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        self.db
            .flush_wal(true)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        let cf = match self.cf(Self::CF_EVENTS) {
            Ok(cf) => cf,
            Err(_) => return Vec::new(),
        };
        let mut entries = Vec::new();
        let start = Self::actor_seq_key(actor_id, 0);
        let mut iter = self.db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
        );
        while let Some(Ok((key, value))) = iter.next() {
            if key.len() < 8 || key[..8] != Self::actor_key(actor_id) {
                break;
            }
            if let Ok(entry) = serde_json::from_slice::<EventEntry>(&value) {
                entries.push(entry);
            }
        }
        entries
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        let snapshot_seq = self
            .load_snapshot(actor_id)
            .map(|s| s.sequence)
            .unwrap_or(0);
        let journal_seq = self
            .read_journal(actor_id)
            .last()
            .map(|e| e.sequence)
            .unwrap_or(0);
        let wf_event_seq = self
            .read_workflow_events(actor_id)
            .last()
            .map(|e| e.sequence())
            .unwrap_or(0);
        let event_seq = self
            .read_events(actor_id)
            .last()
            .map(|e| e.sequence)
            .unwrap_or(0);
        snapshot_seq
            .max(journal_seq)
            .max(wf_event_seq)
            .max(event_seq)
    }

    fn clear(&mut self, actor_id: u64) -> io::Result<()> {
        for cf_name in [
            Self::CF_SNAPSHOTS,
            Self::CF_JOURNAL,
            Self::CF_WORKFLOW_EVENTS,
            Self::CF_EVENTS,
        ] {
            let cf = self.cf(cf_name)?;
            // Start from the bare actor prefix.  Snapshot keys are exactly 8
            // bytes; journal/event keys are 16 bytes (actor || sequence).
            // Both layouts sort contiguously under the actor prefix.
            let actor_key = Self::actor_key(actor_id);
            let mut iter = self.db.iterator_cf(
                cf,
                rocksdb::IteratorMode::From(&actor_key, rocksdb::Direction::Forward),
            );
            let mut keys = Vec::new();
            while let Some(Ok((key, _))) = iter.next() {
                if key.len() < 8 || key[..8] != actor_key {
                    break;
                }
                keys.push(key.to_vec());
            }
            for key in keys {
                self.db
                    .delete_cf(cf, &key)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            }
        }
        self.db
            .flush_wal(true)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// PostgresStore — production relational persistence backend
// ---------------------------------------------------------------------------

#[cfg(feature = "postgres")]
pub struct PostgresStore {
    conn: std::sync::Mutex<postgres::Client>,
}

#[cfg(feature = "postgres")]
impl PostgresStore {
    /// Open a PostgreSQL-backed store using a libpq-style connection string.
    ///
    /// Example: `host=localhost user=postgres password=secret dbname=nulang`.
    /// TLS is disabled (`NoTls`); use a connection string with SSL parameters
    /// and a TLS-enabled client for production deployments that require it.
    pub fn new(config: &str) -> io::Result<Self> {
        let client = postgres::Client::connect(config, postgres::NoTls)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let store = PostgresStore {
            conn: std::sync::Mutex::new(client),
        };
        store.ensure_tables()?;
        Ok(store)
    }

    fn ensure_tables(&self) -> io::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS snapshots (
                actor_id BIGINT PRIMARY KEY,
                sequence BIGINT NOT NULL,
                state TEXT NOT NULL,
                waiting_signal TEXT,
                crdt_snapshot TEXT,
                crdt_field_map TEXT,
                authority_tokens TEXT
            )",
            &[],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        conn.execute(
            "ALTER TABLE snapshots ADD COLUMN IF NOT EXISTS authority_tokens TEXT",
            &[],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS journal (
                actor_id BIGINT NOT NULL,
                sequence BIGINT NOT NULL,
                behavior_id INTEGER NOT NULL,
                payload TEXT NOT NULL,
                PRIMARY KEY (actor_id, sequence)
            )",
            &[],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS workflow_events (
                actor_id BIGINT NOT NULL,
                sequence BIGINT NOT NULL,
                event TEXT NOT NULL,
                PRIMARY KEY (actor_id, sequence)
            )",
            &[],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS events (
                actor_id BIGINT NOT NULL,
                sequence BIGINT NOT NULL,
                field_name TEXT NOT NULL,
                event_name TEXT NOT NULL,
                args TEXT NOT NULL,
                value TEXT NOT NULL DEFAULT '1',
                PRIMARY KEY (actor_id, sequence)
            )",
            &[],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }
}

#[cfg(feature = "postgres")]
impl PersistenceStore for PostgresStore {
    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()> {
        let state_json = serde_json::to_string(&snapshot.state)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let crdt_json = serde_json::to_string(&snapshot.crdt_snapshot)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let crdt_field_map_json = serde_json::to_string(&snapshot.crdt_field_map)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let authority_json = serde_json::to_string(&snapshot.authority_tokens)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (actor_id) DO UPDATE SET
               sequence = EXCLUDED.sequence,
               state = EXCLUDED.state,
               waiting_signal = EXCLUDED.waiting_signal,
               crdt_snapshot = EXCLUDED.crdt_snapshot,
               crdt_field_map = EXCLUDED.crdt_field_map,
               authority_tokens = EXCLUDED.authority_tokens",
            &[
                &(snapshot.actor_id as i64),
                &(snapshot.sequence as i64),
                &state_json,
                &snapshot.waiting_signal.as_deref(),
                &crdt_json.as_str(),
                &crdt_field_map_json.as_str(),
                &authority_json.as_str(),
            ],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        let mut conn = self.conn.lock().unwrap();
        let row = conn
            .query_one(
                "SELECT sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens
                 FROM snapshots WHERE actor_id = $1",
                &[&(actor_id as i64)],
            )
            .ok()?;
        let sequence: i64 = row.get(0);
        let state_json: String = row.get(1);
        let waiting_signal: Option<String> = row.get(2);
        let crdt_json: Option<String> = row.get(3);
        let crdt_field_map_json: Option<String> = row.get(4);
        let authority_json: Option<String> = row.get(5);
        let crdt_snapshot: Option<Vec<(u64, u8, Vec<u8>)>> =
            crdt_json.and_then(|j| serde_json::from_str(&j).ok());
        let crdt_field_map: Option<HashMap<String, u64>> =
            crdt_field_map_json.and_then(|j| serde_json::from_str(&j).ok());
        let authority_tokens: BTreeSet<String> = match authority_json {
            Some(json) => match serde_json::from_str(&json) {
                Ok(tokens) => tokens,
                Err(err) => {
                    warn!(
                        "nulang-persist: invalid authority metadata for actor {}: {}",
                        actor_id, err
                    );
                    return None;
                }
            },
            None => BTreeSet::new(),
        };
        let state: HashMap<String, PersistedValue> = serde_json::from_str(&state_json).ok()?;
        Some(ActorSnapshot {
            actor_id,
            sequence: sequence as u64,
            state,
            waiting_signal,
            crdt_snapshot,
            crdt_field_map,
            authority_tokens,
        })
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()> {
        let payload_json = serde_json::to_string(&entry.payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO journal (actor_id, sequence, behavior_id, payload)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (actor_id, sequence) DO UPDATE SET
               behavior_id = EXCLUDED.behavior_id,
               payload = EXCLUDED.payload",
            &[
                &(actor_id as i64),
                &(entry.sequence as i64),
                &(entry.behavior_id as i32),
                &payload_json,
            ],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
        let mut conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let rows = match conn.query(
            "SELECT sequence, behavior_id, payload FROM journal
             WHERE actor_id = $1 ORDER BY sequence ASC",
            &[&(actor_id as i64)],
        ) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.iter()
            .filter_map(|row| {
                let seq: i64 = row.get(0);
                let bid: i32 = row.get(1);
                let payload_json: String = row.get(2);
                let payload: Vec<PersistedValue> = serde_json::from_str(&payload_json).ok()?;
                Some(JournalEntry {
                    sequence: seq as u64,
                    behavior_id: bid as u16,
                    payload,
                })
            })
            .collect()
    }

    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()> {
        let event_json = serde_json::to_string(&event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO workflow_events (actor_id, sequence, event)
             VALUES ($1, $2, $3)
             ON CONFLICT (actor_id, sequence) DO UPDATE SET
               event = EXCLUDED.event",
            &[&(actor_id as i64), &(event.sequence() as i64), &event_json],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        let mut conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let rows = match conn.query(
            "SELECT event FROM workflow_events
             WHERE actor_id = $1 ORDER BY sequence ASC",
            &[&(actor_id as i64)],
        ) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.iter()
            .filter_map(|row| {
                let event_json: String = row.get(0);
                serde_json::from_str(&event_json).ok()
            })
            .collect()
    }

    fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> io::Result<()> {
        let args_json = serde_json::to_string(&entry.args)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let value_json = serde_json::to_string(&entry.value)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events (actor_id, sequence, field_name, event_name, args, value)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (actor_id, sequence) DO UPDATE SET
               field_name = EXCLUDED.field_name,
               event_name = EXCLUDED.event_name,
               args = EXCLUDED.args,
               value = EXCLUDED.value",
            &[
                &(actor_id as i64),
                &(entry.sequence as i64),
                &entry.field_name,
                &entry.event_name,
                &args_json,
                &value_json,
            ],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Ok(())
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        let mut conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let rows = match conn.query(
            "SELECT sequence, field_name, event_name, args, value FROM events
             WHERE actor_id = $1 ORDER BY sequence ASC",
            &[&(actor_id as i64)],
        ) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.iter()
            .filter_map(|row| {
                let seq: i64 = row.get(0);
                let field_name: String = row.get(1);
                let event_name: String = row.get(2);
                let args_json: String = row.get(3);
                let args: Vec<PersistedValue> = serde_json::from_str(&args_json).ok()?;
                let value_json: String = row.get(4);
                let value: PersistedValue = serde_json::from_str(&value_json).ok()?;
                Some(EventEntry {
                    sequence: seq as u64,
                    field_name,
                    event_name,
                    args,
                    value,
                })
            })
            .collect()
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        let mut conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let snapshot_seq: Option<i64> = conn
            .query_opt(
                "SELECT sequence FROM snapshots WHERE actor_id = $1",
                &[&(actor_id as i64)],
            )
            .ok()
            .flatten()
            .map(|row| row.get(0));
        let journal_seq: Option<i64> = conn
            .query_opt(
                "SELECT sequence FROM journal WHERE actor_id = $1 ORDER BY sequence DESC LIMIT 1",
                &[&(actor_id as i64)],
            )
            .ok()
            .flatten()
            .map(|row| row.get(0));
        let wf_event_seq: Option<i64> = conn
            .query_opt(
                "SELECT sequence FROM workflow_events WHERE actor_id = $1 ORDER BY sequence DESC LIMIT 1",
                &[&(actor_id as i64)],
            )
            .ok()
            .flatten()
            .map(|row| row.get(0));
        let event_seq: Option<i64> = conn
            .query_opt(
                "SELECT sequence FROM events WHERE actor_id = $1 ORDER BY sequence DESC LIMIT 1",
                &[&(actor_id as i64)],
            )
            .ok()
            .flatten()
            .map(|row| row.get(0));
        snapshot_seq
            .unwrap_or(0)
            .max(journal_seq.unwrap_or(0))
            .max(wf_event_seq.unwrap_or(0))
            .max(event_seq.unwrap_or(0)) as u64
    }

    fn clear(&mut self, actor_id: u64) -> io::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        for table in ["snapshots", "journal", "workflow_events", "events"] {
            conn.execute(
                &format!("DELETE FROM {} WHERE actor_id = $1", table),
                &[&(actor_id as i64)],
            )
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        }
        Ok(())
    }

    fn query(&self, sql: &str, params: &[Value]) -> io::Result<Vec<String>> {
        let mut conn = self.conn.lock().unwrap();
        // Preserve Nulang Value types when binding Postgres parameters so
        // numeric/boolean comparisons work without explicit SQL casts.
        let typed_params: Vec<Box<dyn postgres::types::ToSql + Sync>> = params
            .iter()
            .map(|v| {
                if let Some(i) = v.as_int() {
                    Box::new(i) as Box<dyn postgres::types::ToSql + Sync>
                } else if let Some(f) = v.as_float() {
                    Box::new(f) as Box<dyn postgres::types::ToSql + Sync>
                } else if let Some(b) = v.as_bool() {
                    Box::new(b) as Box<dyn postgres::types::ToSql + Sync>
                } else {
                    Box::new(v.to_string_repr()) as Box<dyn postgres::types::ToSql + Sync>
                }
            })
            .collect();
        let param_refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
            typed_params.iter().map(|p| p.as_ref()).collect();
        let rows = conn
            .query(sql, &param_refs)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let mut results = Vec::new();
        for row in &rows {
            let mut cols: Vec<serde_json::Value> = Vec::with_capacity(row.len());
            for i in 0..row.len() {
                let json_val = match row.columns().get(i).map(|c| c.type_().name()) {
                    Some("bool") => {
                        let v: bool = row.get(i);
                        serde_json::Value::Bool(v)
                    }
                    Some("int2") | Some("int4") | Some("int8") => {
                        let v: i64 = row.get(i);
                        serde_json::value::Number::from_i128(v as i128)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    }
                    Some("float4") | Some("float8") => {
                        let v: f64 = row.get(i);
                        serde_json::value::Number::from_f64(v)
                            .map(serde_json::Value::Number)
                            .unwrap_or(serde_json::Value::Null)
                    }
                    _ => {
                        let v: String = row.get(i);
                        serde_json::Value::String(v)
                    }
                };
                cols.push(json_val);
            }
            let json = serde_json::to_string(&cols)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            results.push(json);
        }
        Ok(results)
    }
}
#[cfg(test)]
mod json_file_store_tests {
    use super::*;

    /// Unique scratch dir per test (the suite runs tests in parallel, and a
    /// re-run must not see a previous run's leftover files).
    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nulang_json_store_test_{}_{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_json_file_store_save_load_snapshot() {
        let dir = fresh_dir("snapshot");
        let mut store = JsonFileStore::new(&dir).unwrap();
        let mut state = HashMap::new();
        state.insert("count".to_string(), PersistedValue::Int(42));
        store
            .save_snapshot(ActorSnapshot {
                actor_id: 1,
                sequence: 3,
                state,
                waiting_signal: None,
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();

        let loaded = store.load_snapshot(1).unwrap();
        assert_eq!(loaded.actor_id, 1);
        assert_eq!(loaded.sequence, 3);
        assert_eq!(loaded.state.get("count"), Some(&PersistedValue::Int(42)));

        // The atomic (temp + rename) write must not leave its temp file behind.
        assert!(!store
            .snapshot_path(1)
            .with_file_name("snapshot.json.tmp")
            .exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_json_file_store_append_read_journal() {
        let dir = fresh_dir("journal");
        let mut store = JsonFileStore::new(&dir).unwrap();
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: 1,
                    behavior_id: 0,
                    payload: vec![PersistedValue::Int(10)],
                },
            )
            .unwrap();
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: 2,
                    behavior_id: 1,
                    payload: vec![PersistedValue::Int(20)],
                },
            )
            .unwrap();

        let journal = store.read_journal(1);
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].sequence, 1);
        assert_eq!(journal[1].behavior_id, 1);
        assert_eq!(journal[1].payload, vec![PersistedValue::Int(20)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_json_file_store_latest_sequence() {
        let dir = fresh_dir("latest_seq");
        let mut store = JsonFileStore::new(&dir).unwrap();
        store
            .save_snapshot(ActorSnapshot {
                actor_id: 1,
                sequence: 5,
                state: HashMap::new(),
                waiting_signal: None,
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: 7,
                    behavior_id: 0,
                    payload: vec![],
                },
            )
            .unwrap();
        assert_eq!(store.latest_sequence(1), 7);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_json_file_store_clear() {
        let dir = fresh_dir("clear");
        let mut store = JsonFileStore::new(&dir).unwrap();
        store
            .save_snapshot(ActorSnapshot {
                actor_id: 1,
                sequence: 1,
                state: HashMap::new(),
                waiting_signal: None,
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: 2,
                    behavior_id: 0,
                    payload: vec![],
                },
            )
            .unwrap();

        store.clear(1).unwrap();
        assert!(store.load_snapshot(1).is_none());
        assert!(store.read_journal(1).is_empty());
        assert_eq!(store.latest_sequence(1), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_json_file_store_persists_across_instances() {
        let dir = fresh_dir("persist");
        {
            let mut store = JsonFileStore::new(&dir).unwrap();
            let mut state = HashMap::new();
            state.insert("x".to_string(), PersistedValue::Float(1.5));
            store
                .save_snapshot(ActorSnapshot {
                    actor_id: 1,
                    sequence: 1,
                    state,
                    waiting_signal: None,
                    crdt_snapshot: None,
                    crdt_field_map: None,
                    authority_tokens: Default::default(),
                })
                .unwrap();
            store
                .append_journal(
                    1,
                    JournalEntry {
                        sequence: 2,
                        behavior_id: 0,
                        payload: vec![PersistedValue::Bool(true)],
                    },
                )
                .unwrap();
        }

        {
            let store = JsonFileStore::new(&dir).unwrap();
            let snapshot = store.load_snapshot(1).unwrap();
            assert_eq!(snapshot.sequence, 1);
            assert_eq!(snapshot.state.get("x"), Some(&PersistedValue::Float(1.5)));
            let journal = store.read_journal(1);
            assert_eq!(journal.len(), 1);
            assert_eq!(journal[0].payload, vec![PersistedValue::Bool(true)]);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_json_file_store_corrupted_snapshot_loads_none() {
        let dir = fresh_dir("corrupt");
        let mut store = JsonFileStore::new(&dir).unwrap();
        store
            .save_snapshot(ActorSnapshot {
                actor_id: 1,
                sequence: 9,
                state: HashMap::new(),
                waiting_signal: None,
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();

        // Simulate a torn write (the pre-fix failure mode): truncate the
        // snapshot file mid-JSON. Recovery must degrade gracefully to `None`
        // (and log) rather than panic.
        let path = store.snapshot_path(1);
        fs::write(&path, "{\"actor_id\": 1, \"sequ").unwrap();
        assert!(store.load_snapshot(1).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod persisted_value_tests {
    use super::*;
    use crate::bytecode::{CodeModule, Constant};
    use crate::runtime::actor::Actor;

    #[test]
    fn test_from_value_resolved_resolves_string_from_module() {
        let v = Value::int(42);
        assert_eq!(
            PersistedValue::from_value_resolved(&v, None),
            PersistedValue::Int(42)
        );
    }

    #[test]
    fn test_from_value_resolved_resolves_string_pool_id() {
        let mut module = CodeModule::new("test");
        let hello_idx = module.add_constant(Constant::String("hello".to_string()));

        let v = Value::string(hello_idx as u32);
        let pv = PersistedValue::from_value_resolved(&v, Some(&module));
        assert_eq!(pv, PersistedValue::String("hello".to_string()));
    }

    #[test]
    fn test_from_value_resolved_returns_nil_without_module() {
        let mut module = CodeModule::new("test");
        let hello_idx = module.add_constant(Constant::String("hello".to_string()));
        let v = Value::string(hello_idx as u32);

        // Without a module, the string can't be resolved — falls back to Nil.
        let pv = PersistedValue::from_value_resolved(&v, None);
        assert_eq!(pv, PersistedValue::Nil);
    }

    #[test]
    fn test_to_value_on_heap_allocates_string() {
        let pv = PersistedValue::String("world".to_string());
        let mut actor = Actor::new(1, "test".to_string(), 0);

        let v = pv.to_value_on_heap(&mut actor);
        // Allocated string is a TAG_PTR value on the actor heap, not nil.
        assert!(
            !v.is_nil(),
            "string should be allocated, not dropped to nil"
        );
    }

    #[test]
    fn test_string_round_trip_via_persisted_value() {
        // Simulate the checkpoint+restore cycle for a string value.
        let mut module = CodeModule::new("test");
        let idx = module.add_constant(Constant::String("round-trip".to_string()));
        let original = Value::string(idx as u32);

        // Checkpoint: resolve string to content.
        let persisted = PersistedValue::from_value_resolved(&original, Some(&module));
        assert_eq!(persisted, PersistedValue::String("round-trip".to_string()));

        // Serialize → deserialize (JSON).
        let json = serde_json::to_string(&persisted).unwrap();
        let deserialized: PersistedValue = serde_json::from_str(&json).unwrap();
        assert_eq!(
            deserialized,
            PersistedValue::String("round-trip".to_string())
        );

        // Restore: allocate on actor heap. Previously this would return nil.
        let mut actor = Actor::new(1, "test".to_string(), 0);
        let restored = deserialized.to_value_on_heap(&mut actor);
        assert!(
            !restored.is_nil(),
            "string must survive restore, not become nil"
        );
    }
}

#[cfg(test)]
#[cfg(feature = "rocksdb")]
mod rocksdb_store_tests {
    use super::*;

    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nulang_rocksdb_test_{}_{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn test_rocksdb_store_save_load_snapshot() {
        let dir = fresh_dir("snapshot");
        let mut store = RocksDbStore::new(&dir).unwrap();
        let mut state = HashMap::new();
        state.insert("count".to_string(), PersistedValue::Int(42));
        store
            .save_snapshot(ActorSnapshot {
                actor_id: 1,
                sequence: 3,
                state,
                waiting_signal: None,
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();

        let loaded = store.load_snapshot(1).unwrap();
        assert_eq!(loaded.actor_id, 1);
        assert_eq!(loaded.sequence, 3);
        assert_eq!(loaded.state.get("count"), Some(&PersistedValue::Int(42)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rocksdb_store_append_read_journal() {
        let dir = fresh_dir("journal");
        let mut store = RocksDbStore::new(&dir).unwrap();
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: 1,
                    behavior_id: 0,
                    payload: vec![PersistedValue::Int(10)],
                },
            )
            .unwrap();
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: 2,
                    behavior_id: 1,
                    payload: vec![PersistedValue::Int(20)],
                },
            )
            .unwrap();

        let journal = store.read_journal(1);
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].sequence, 1);
        assert_eq!(journal[1].behavior_id, 1);
        assert_eq!(journal[1].payload, vec![PersistedValue::Int(20)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rocksdb_store_latest_sequence() {
        let dir = fresh_dir("latest_seq");
        let mut store = RocksDbStore::new(&dir).unwrap();
        store
            .save_snapshot(ActorSnapshot {
                actor_id: 1,
                sequence: 5,
                state: HashMap::new(),
                waiting_signal: None,
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: 7,
                    behavior_id: 0,
                    payload: vec![],
                },
            )
            .unwrap();
        assert_eq!(store.latest_sequence(1), 7);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rocksdb_store_clear() {
        let dir = fresh_dir("clear");
        let mut store = RocksDbStore::new(&dir).unwrap();
        store
            .save_snapshot(ActorSnapshot {
                actor_id: 1,
                sequence: 1,
                state: HashMap::new(),
                waiting_signal: None,
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: 2,
                    behavior_id: 0,
                    payload: vec![],
                },
            )
            .unwrap();

        store.clear(1).unwrap();
        assert!(store.load_snapshot(1).is_none());
        assert!(store.read_journal(1).is_empty());
        assert_eq!(store.latest_sequence(1), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rocksdb_store_persists_across_instances() {
        let dir = fresh_dir("persist");
        {
            let mut store = RocksDbStore::new(&dir).unwrap();
            let mut state = HashMap::new();
            state.insert("x".to_string(), PersistedValue::Float(1.5));
            store
                .save_snapshot(ActorSnapshot {
                    actor_id: 1,
                    sequence: 1,
                    state,
                    waiting_signal: None,
                    crdt_snapshot: None,
                    crdt_field_map: None,
                    authority_tokens: Default::default(),
                })
                .unwrap();
            store
                .append_journal(
                    1,
                    JournalEntry {
                        sequence: 2,
                        behavior_id: 0,
                        payload: vec![PersistedValue::Bool(true)],
                    },
                )
                .unwrap();
        }

        {
            let store = RocksDbStore::new(&dir).unwrap();
            let snapshot = store.load_snapshot(1).unwrap();
            assert_eq!(snapshot.sequence, 1);
            assert_eq!(snapshot.state.get("x"), Some(&PersistedValue::Float(1.5)));
            let journal = store.read_journal(1);
            assert_eq!(journal.len(), 1);
            assert_eq!(journal[0].payload, vec![PersistedValue::Bool(true)]);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rocksdb_store_query_unsupported() {
        let dir = fresh_dir("query");
        let store = RocksDbStore::new(&dir).unwrap();
        let res = store.query("SELECT 1", &[]);
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::Unsupported);
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
#[cfg(feature = "postgres")]
mod postgres_store_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn pg_url() -> Option<String> {
        std::env::var("NU_TEST_POSTGRES_URL").ok()
    }

    fn fresh_actor_id() -> u64 {
        static COUNTER: AtomicU64 = AtomicU64::new(1000);
        COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    #[test]
    fn test_postgres_store_save_load_snapshot() {
        let url = match pg_url() {
            Some(u) => u,
            None => return,
        };
        let mut store = PostgresStore::new(&url).unwrap();
        let actor_id = fresh_actor_id();
        let mut state = HashMap::new();
        state.insert("count".to_string(), PersistedValue::Int(42));
        store
            .save_snapshot(ActorSnapshot {
                actor_id,
                sequence: 3,
                state,
                waiting_signal: Some("signal".to_string()),
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();

        let loaded = store.load_snapshot(actor_id).unwrap();
        assert_eq!(loaded.actor_id, actor_id);
        assert_eq!(loaded.sequence, 3);
        assert_eq!(loaded.state.get("count"), Some(&PersistedValue::Int(42)));
        assert_eq!(loaded.waiting_signal, Some("signal".to_string()));
        store.clear(actor_id).unwrap();
    }

    #[test]
    fn test_postgres_store_append_read_journal() {
        let url = match pg_url() {
            Some(u) => u,
            None => return,
        };
        let mut store = PostgresStore::new(&url).unwrap();
        let actor_id = fresh_actor_id();
        store
            .append_journal(
                actor_id,
                JournalEntry {
                    sequence: 1,
                    behavior_id: 0,
                    payload: vec![PersistedValue::Int(10)],
                },
            )
            .unwrap();
        store
            .append_journal(
                actor_id,
                JournalEntry {
                    sequence: 2,
                    behavior_id: 1,
                    payload: vec![PersistedValue::Int(20)],
                },
            )
            .unwrap();

        let journal = store.read_journal(actor_id);
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].sequence, 1);
        assert_eq!(journal[1].behavior_id, 1);
        assert_eq!(journal[1].payload, vec![PersistedValue::Int(20)]);
        store.clear(actor_id).unwrap();
    }

    #[test]
    fn test_postgres_store_latest_sequence() {
        let url = match pg_url() {
            Some(u) => u,
            None => return,
        };
        let mut store = PostgresStore::new(&url).unwrap();
        let actor_id = fresh_actor_id();
        store
            .save_snapshot(ActorSnapshot {
                actor_id,
                sequence: 5,
                state: HashMap::new(),
                waiting_signal: None,
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();
        store
            .append_journal(
                actor_id,
                JournalEntry {
                    sequence: 7,
                    behavior_id: 0,
                    payload: vec![],
                },
            )
            .unwrap();
        assert_eq!(store.latest_sequence(actor_id), 7);
        store.clear(actor_id).unwrap();
    }

    #[test]
    fn test_postgres_store_clear() {
        let url = match pg_url() {
            Some(u) => u,
            None => return,
        };
        let mut store = PostgresStore::new(&url).unwrap();
        let actor_id = fresh_actor_id();
        store
            .save_snapshot(ActorSnapshot {
                actor_id,
                sequence: 1,
                state: HashMap::new(),
                waiting_signal: None,
                crdt_snapshot: None,
                crdt_field_map: None,
                authority_tokens: Default::default(),
            })
            .unwrap();
        store
            .append_journal(
                actor_id,
                JournalEntry {
                    sequence: 2,
                    behavior_id: 0,
                    payload: vec![],
                },
            )
            .unwrap();

        store.clear(actor_id).unwrap();
        assert!(store.load_snapshot(actor_id).is_none());
        assert!(store.read_journal(actor_id).is_empty());
        assert_eq!(store.latest_sequence(actor_id), 0);
    }

    #[test]
    fn test_postgres_store_query_typed_params() {
        let url = match pg_url() {
            Some(u) => u,
            None => return,
        };
        let store = PostgresStore::new(&url).unwrap();
        let rows = store
            .query(
                "SELECT $1::int8, $2::float8, $3::bool",
                &[Value::int(42), Value::float(3.14), Value::bool(true)],
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], "[42,3.14,true]");
    }

    #[test]
    fn test_postgres_store_workflow_events_and_events() {
        let url = match pg_url() {
            Some(u) => u,
            None => return,
        };
        let mut store = PostgresStore::new(&url).unwrap();
        let actor_id = fresh_actor_id();
        store
            .append_workflow_event(
                actor_id,
                WorkflowEvent::TimerSet {
                    sequence: 1,
                    name: "t".to_string(),
                    duration_ms: 100,
                },
            )
            .unwrap();
        store
            .append_event(
                actor_id,
                EventEntry {
                    sequence: 2,
                    field_name: "counter".to_string(),
                    event_name: "Inc".to_string(),
                    args: vec![PersistedValue::Int(1)],
                    value: PersistedValue::Int(1),
                },
            )
            .unwrap();

        assert_eq!(store.read_workflow_events(actor_id).len(), 1);
        assert_eq!(store.read_events(actor_id).len(), 1);
        assert_eq!(store.latest_sequence(actor_id), 2);
        store.clear(actor_id).unwrap();
    }
}

#[cfg(test)]
mod durable_transition_tests {
    use super::*;

    fn snapshot(actor_id: u64, sequence: u64, entries: &[(&str, i64)]) -> ActorSnapshot {
        let mut state = HashMap::new();
        for (name, value) in entries {
            state.insert((*name).to_string(), PersistedValue::Int(*value));
        }
        ActorSnapshot {
            actor_id,
            sequence,
            state,
            ..ActorSnapshot::default()
        }
    }

    fn transition(actor_id: u64, epoch: u64, sequence: u64) -> DurableTransition {
        DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: epoch,
            sequence,
            expected_previous_sequence: sequence - 1,
            command: Some(JournalEntry {
                sequence,
                behavior_id: 7,
                payload: vec![PersistedValue::Int(sequence as i64)],
            }),
            snapshot: Some(snapshot(actor_id, sequence, &[("count", sequence as i64)])),
            workflow_events: vec![WorkflowEvent::StepCompleted {
                sequence,
                activation: Some(WorkflowActivationId::new(actor_id, sequence)),
                step_name: format!("step-{sequence}"),
            }],
            domain_events: vec![EventEntry {
                sequence,
                field_name: "count".to_string(),
                event_name: "Incremented".to_string(),
                args: vec![],
                value: PersistedValue::Int(sequence as i64),
            }],
            durable_effects: vec![],
            outbox: vec![DurableOutboxMessage {
                destination_actor_id: actor_id + 1,
                ordinal: 0,
                behavior_id: 9,
                payload: vec![PersistedValue::Int(sequence as i64)],
            }],
        }
    }

    #[test]
    fn legacy_terminal_event_serialization_omits_absent_activation() {
        let event = WorkflowEvent::StepCompleted {
            sequence: 1,
            activation: None,
            step_name: "legacy".to_string(),
        };

        let encoded = serde_json::to_value(event).unwrap();
        let value = encoded
            .get("value")
            .and_then(serde_json::Value::as_object)
            .expect("StepCompleted content object");

        assert!(
            !value.contains_key("activation"),
            "legacy serialization must not add activation:null and change durable digests"
        );
    }

    #[test]
    fn durable_transition_rejects_terminal_activation_for_wrong_actor() {
        let mut candidate = transition(10, 1, 1);
        candidate.workflow_events = vec![WorkflowEvent::StepCompleted {
            sequence: 1,
            activation: Some(WorkflowActivationId::new(11, 1)),
            step_name: "step-1".to_string(),
        }];

        let error = candidate.digest().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn durable_transition_rejects_terminal_activation_for_wrong_command_sequence() {
        let mut candidate = transition(10, 1, 1);
        candidate.workflow_events = vec![WorkflowEvent::StepCompleted {
            sequence: 1,
            activation: Some(WorkflowActivationId::new(10, 99)),
            step_name: "step-1".to_string(),
        }];

        let error = candidate.digest().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn memory_store_commits_transition_as_one_logical_unit() {
        let mut store = MemoryStore::new();
        let committed = store.commit_transition(transition(10, 1, 1)).unwrap();

        assert_eq!(committed.actor_id, 10);
        assert_eq!(committed.activation_epoch, 1);
        assert_eq!(committed.sequence, 1);
        assert_eq!(store.latest_sequence(10), 1);
        assert_eq!(store.read_journal(10).len(), 1);
        assert_eq!(store.read_workflow_events(10).len(), 1);
        assert_eq!(store.read_events(10).len(), 1);
        assert_eq!(store.load_snapshot(10).unwrap().sequence, 1);
        assert_eq!(store.committed_transitions(10).len(), 1);
        assert_eq!(
            store.durable_tail(10),
            Some(DurableTail {
                activation_epoch: 1,
                sequence: 1,
                digest: committed.digest,
            })
        );
    }

    #[test]
    fn memory_store_rejects_sequence_gap_without_partial_mutation() {
        let mut store = MemoryStore::new();
        store.commit_transition(transition(10, 1, 1)).unwrap();

        let mut invalid = transition(10, 1, 3);
        invalid.expected_previous_sequence = 2;
        let error = store.commit_transition(invalid).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(store.latest_sequence(10), 1);
        assert_eq!(store.read_journal(10).len(), 1);
        assert_eq!(store.read_workflow_events(10).len(), 1);
        assert_eq!(store.read_events(10).len(), 1);
        assert_eq!(store.committed_transitions(10).len(), 1);
    }

    #[test]
    fn memory_store_first_atomic_transition_continues_legacy_history() {
        let mut store = MemoryStore::new();
        store
            .save_snapshot(snapshot(10, 5, &[("count", 5)]))
            .unwrap();

        let committed = store.commit_transition(transition(10, 1, 6)).unwrap();

        assert_eq!(committed.sequence, 6);
        assert_eq!(store.latest_sequence(10), 6);
        assert_eq!(store.committed_transitions(10).len(), 1);
    }

    #[test]
    fn memory_store_rejects_stale_activation_epoch() {
        let mut store = MemoryStore::new();
        store.commit_transition(transition(10, 2, 1)).unwrap();

        let mut stale = transition(10, 1, 2);
        stale.expected_previous_sequence = 1;
        let error = store.commit_transition(stale).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(store.latest_sequence(10), 1);
    }

    #[test]
    fn memory_store_exact_retry_is_idempotent_but_conflict_fails_closed() {
        let mut store = MemoryStore::new();
        let original = transition(10, 1, 1);
        let first = store.commit_transition(original.clone()).unwrap();
        let retry = store.commit_transition(original).unwrap();

        assert_eq!(first, retry);
        assert_eq!(store.committed_transitions(10).len(), 1);

        let mut conflict = transition(10, 1, 1);
        conflict.command.as_mut().unwrap().behavior_id = 99;
        let error = store.commit_transition(conflict).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(store.committed_transitions(10).len(), 1);
    }

    #[test]
    fn transition_digest_is_independent_of_hashmap_insertion_order() {
        let mut first = transition(10, 1, 1);
        first.snapshot = Some(snapshot(10, 1, &[("a", 1), ("b", 2), ("c", 3)]));

        let mut second = transition(10, 1, 1);
        second.snapshot = Some(snapshot(10, 1, &[("c", 3), ("a", 1), ("b", 2)]));

        assert_eq!(first.digest().unwrap(), second.digest().unwrap());
    }

    #[test]
    fn transition_rejects_duplicate_outbox_ordinals() {
        let mut value = transition(10, 1, 1);
        value.outbox.push(DurableOutboxMessage {
            destination_actor_id: 12,
            ordinal: 0,
            behavior_id: 10,
            payload: vec![],
        });

        let error = value.digest().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod libsql_atomic_transition_tests {
    use super::*;

    fn snapshot(actor_id: u64, sequence: u64, count: i64) -> ActorSnapshot {
        let mut state = HashMap::new();
        state.insert("count".to_string(), PersistedValue::Int(count));
        ActorSnapshot {
            actor_id,
            sequence,
            state,
            ..ActorSnapshot::default()
        }
    }

    fn transition(actor_id: u64, epoch: u64, sequence: u64) -> DurableTransition {
        DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: epoch,
            sequence,
            expected_previous_sequence: sequence - 1,
            command: Some(JournalEntry {
                sequence,
                behavior_id: 3,
                payload: vec![PersistedValue::Int(sequence as i64)],
            }),
            snapshot: Some(snapshot(actor_id, sequence, sequence as i64)),
            workflow_events: vec![
                WorkflowEvent::StepCompleted {
                    sequence,
                    activation: Some(WorkflowActivationId::new(actor_id, sequence)),
                    step_name: "persist".to_string(),
                },
                WorkflowEvent::Custom {
                    sequence,
                    name: "audit".to_string(),
                    args: vec![PersistedValue::Int(sequence as i64)],
                },
            ],
            domain_events: vec![
                EventEntry {
                    sequence,
                    field_name: "count".to_string(),
                    event_name: "Changed".to_string(),
                    args: vec![],
                    value: PersistedValue::Int(sequence as i64),
                },
                EventEntry {
                    sequence,
                    field_name: "mirror".to_string(),
                    event_name: "Changed".to_string(),
                    args: vec![],
                    value: PersistedValue::Int(sequence as i64),
                },
            ],
            durable_effects: vec![],
            outbox: vec![DurableOutboxMessage {
                destination_actor_id: actor_id + 1,
                ordinal: 0,
                behavior_id: 4,
                payload: vec![PersistedValue::Int(sequence as i64)],
            }],
        }
    }

    #[test]
    fn libsql_atomic_transition_commits_all_components() {
        let mut store = LibsqlStore::in_memory().unwrap();
        let committed = store.commit_transition(transition(42, 1, 1)).unwrap();

        assert_eq!(committed.sequence, 1);
        assert_eq!(store.latest_sequence(42), 1);
        assert_eq!(store.load_snapshot(42).unwrap().sequence, 1);
        assert_eq!(store.read_journal(42).len(), 1);
        assert_eq!(store.read_workflow_events(42).len(), 2);
        assert_eq!(store.read_events(42).len(), 2);

        let outbox = store
            .query(
                "SELECT COUNT(*) FROM durable_outbox WHERE actor_id = 42",
                &[],
            )
            .unwrap();
        assert_eq!(outbox, vec!["[1]".to_string()]);
    }

    #[test]
    fn libsql_atomic_transition_retry_is_idempotent() {
        let mut store = LibsqlStore::in_memory().unwrap();
        let value = transition(42, 1, 1);

        let first = store.commit_transition(value.clone()).unwrap();
        let retry = store.commit_transition(value).unwrap();

        assert_eq!(first, retry);
        let rows = store
            .query(
                "SELECT COUNT(*) FROM durable_transitions WHERE actor_id = 42",
                &[],
            )
            .unwrap();
        assert_eq!(rows, vec!["[1]".to_string()]);
    }

    #[test]
    fn libsql_atomic_transition_rejects_gap_without_partial_writes() {
        let mut store = LibsqlStore::in_memory().unwrap();
        store.commit_transition(transition(42, 1, 1)).unwrap();

        let mut invalid = transition(42, 1, 3);
        invalid.expected_previous_sequence = 2;
        let error = store.commit_transition(invalid).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(store.latest_sequence(42), 1);
        assert_eq!(store.read_journal(42).len(), 1);
        assert_eq!(store.read_workflow_events(42).len(), 2);
        assert_eq!(store.read_events(42).len(), 2);
    }

    #[test]
    fn libsql_first_atomic_transition_continues_legacy_history() {
        let mut store = LibsqlStore::in_memory().unwrap();
        store.save_snapshot(snapshot(42, 5, 5)).unwrap();

        let committed = store.commit_transition(transition(42, 1, 6)).unwrap();

        assert_eq!(committed.sequence, 6);
        assert_eq!(store.latest_sequence(42), 6);
    }

    #[test]
    fn libsql_atomic_transition_rolls_back_after_mid_commit_failure() {
        let mut store = LibsqlStore::in_memory().unwrap();

        // Seed a conflicting outbox row for the sequence being committed. The
        // transaction writes the snapshot, command journal, and staged events
        // before it reaches the outbox insert, so this forces a real
        // mid-transaction database failure rather than an up-front validation
        // error.
        {
            let conn = store.conn();
            store
                .rt
                .block_on(async {
                    conn.execute(
                        "INSERT INTO durable_outbox
                         (actor_id, sequence, ordinal, destination_actor_id, behavior_id, payload)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        libsql::params![42_i64, 1_i64, 0_i64, 999_i64, 1_i64, "[]"],
                    )
                    .await
                })
                .unwrap();
        }

        let error = store.commit_transition(transition(42, 1, 1)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);

        // Every write performed by the failed transition must have rolled
        // back. The deliberately seeded conflicting row remains because it
        // predates the transaction.
        assert!(store.load_snapshot(42).is_none());
        assert!(store.read_journal(42).is_empty());
        assert!(store.read_workflow_events(42).is_empty());
        assert!(store.read_events(42).is_empty());
        assert_eq!(store.latest_sequence(42), 0);

        let transitions = store
            .query(
                "SELECT COUNT(*) FROM durable_transitions WHERE actor_id = 42",
                &[],
            )
            .unwrap();
        let tails = store
            .query(
                "SELECT COUNT(*) FROM durable_tails WHERE actor_id = 42",
                &[],
            )
            .unwrap();
        assert_eq!(transitions, vec!["[0]".to_string()]);
        assert_eq!(tails, vec!["[0]".to_string()]);
    }

    #[test]
    fn libsql_atomic_transition_survives_reopen() {
        let path = std::env::temp_dir().join(format!(
            "nulang_atomic_transition_{}_reopen.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));

        {
            let mut store = LibsqlStore::new(&path).unwrap();
            store.commit_transition(transition(77, 4, 1)).unwrap();
        }

        {
            let store = LibsqlStore::new(&path).unwrap();
            assert_eq!(store.latest_sequence(77), 1);
            assert_eq!(store.load_snapshot(77).unwrap().sequence, 1);
            assert_eq!(store.read_journal(77).len(), 1);
            assert_eq!(store.read_workflow_events(77).len(), 2);
            assert_eq!(store.read_events(77).len(), 2);

            let rows = store
                .query(
                    "SELECT activation_epoch, sequence
                     FROM durable_tails WHERE actor_id = 77",
                    &[],
                )
                .unwrap();
            assert_eq!(rows, vec!["[4,1]".to_string()]);
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn libsql_atomic_transition_rejects_stale_epoch() {
        let mut store = LibsqlStore::in_memory().unwrap();
        store.commit_transition(transition(42, 2, 1)).unwrap();

        let error = store.commit_transition(transition(42, 1, 2)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(store.latest_sequence(42), 1);
    }
}
