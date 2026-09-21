//! Persistence engine for durable actors.
//!
//! v0.7 MVP: in-memory store plus JSON file backend. The store keeps a
//! snapshot of durable actor state and an append-only journal of messages.
//! On recovery the runtime loads the latest snapshot and replays the journal.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

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
    StepCompleted { sequence: u64, step_name: String },
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
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WorkflowCommitRecord {
    event: WorkflowEvent,
    snapshot: ActorSnapshot,
}

/// Persistence backend trait. Implementations may be in-memory or disk-backed.
pub trait PersistenceStore: Send + Sync {
    /// Persist a snapshot of durable actor state.
    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()>;

    /// Load the latest snapshot for an actor, if any.
    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot>;

    /// Append a message to the actor's journal.
    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()>;

    /// Read all journal entries for an actor in order.
    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry>;

    /// Read at most `limit` journal entries whose sequence is greater than or
    /// equal to `start_sequence`. Backends should override this with a native
    /// range scan; the default preserves compatibility for custom stores.
    fn scan_journal_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<JournalEntry> {
        if limit == 0 {
            return Vec::new();
        }
        self.read_journal(actor_id)
            .into_iter()
            .filter(|entry| entry.sequence >= start_sequence)
            .take(limit)
            .collect()
    }

    /// Append a workflow event to the actor's event journal.
    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()>;

    /// Atomically publish one workflow event and the checkpoint produced by
    /// that same logical transition. The event and snapshot must share a
    /// sequence number.
    ///
    /// Built-in durable backends override this with a native transaction,
    /// write batch, or single-record commit log. The default keeps third-party
    /// stores source-compatible but cannot guarantee atomicity.
    fn commit_workflow_event_and_snapshot(
        &mut self,
        actor_id: u64,
        event: WorkflowEvent,
        snapshot: ActorSnapshot,
    ) -> io::Result<()> {
        if event.sequence() != snapshot.sequence || snapshot.actor_id != actor_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow event and snapshot must describe the same actor sequence",
            ));
        }
        self.append_workflow_event(actor_id, event)?;
        self.save_snapshot(snapshot)
    }

    /// Read all workflow events for an actor in order.
    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent>;

    /// Read at most `limit` workflow events starting at an inclusive actor
    /// sequence. See `scan_journal_from` for the compatibility contract.
    fn scan_workflow_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<WorkflowEvent> {
        if limit == 0 {
            return Vec::new();
        }
        self.read_workflow_events(actor_id)
            .into_iter()
            .filter(|event| event.sequence() >= start_sequence)
            .take(limit)
            .collect()
    }

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

    /// Append one logical group of event-sourced field mutations.
    ///
    /// Backends with transactional/batch primitives should override this so
    /// either the full group becomes durable or none of it does. The default
    /// preserves compatibility for custom stores but only provides sequential
    /// append semantics.
    fn append_events(&mut self, actor_id: u64, entries: &[EventEntry]) -> io::Result<()> {
        for entry in entries {
            self.append_event(actor_id, entry.clone())?;
        }
        Ok(())
    }

    /// Read all event-sourcing entries for an actor in order.
    fn read_events(&self, actor_id: u64) -> Vec<EventEntry>;

    /// Read at most `limit` event-sourced mutations starting at an inclusive
    /// actor sequence. Multiple fields may legally share one sequence and must
    /// remain in deterministic append order.
    fn scan_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<EventEntry> {
        if limit == 0 {
            return Vec::new();
        }
        self.read_events(actor_id)
            .into_iter()
            .filter(|entry| entry.sequence >= start_sequence)
            .take(limit)
            .collect()
    }

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
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PersistenceStore for MemoryStore {
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

    fn scan_journal_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<JournalEntry> {
        self.journals
            .get(&actor_id)
            .into_iter()
            .flatten()
            .filter(|entry| entry.sequence >= start_sequence)
            .take(limit)
            .cloned()
            .collect()
    }

    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()> {
        self.workflow_events
            .entry(actor_id)
            .or_default()
            .push(event);
        Ok(())
    }

    fn commit_workflow_event_and_snapshot(
        &mut self,
        actor_id: u64,
        event: WorkflowEvent,
        snapshot: ActorSnapshot,
    ) -> io::Result<()> {
        if event.sequence() != snapshot.sequence || snapshot.actor_id != actor_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow event and snapshot must describe the same actor sequence",
            ));
        }
        self.workflow_events
            .entry(actor_id)
            .or_default()
            .push(event);
        self.snapshots.insert(actor_id, snapshot);
        Ok(())
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        self.workflow_events
            .get(&actor_id)
            .cloned()
            .unwrap_or_default()
    }

    fn scan_workflow_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<WorkflowEvent> {
        self.workflow_events
            .get(&actor_id)
            .into_iter()
            .flatten()
            .filter(|event| event.sequence() >= start_sequence)
            .take(limit)
            .cloned()
            .collect()
    }

    fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> io::Result<()> {
        self.events.entry(actor_id).or_default().push(entry);
        Ok(())
    }

    fn append_events(&mut self, actor_id: u64, entries: &[EventEntry]) -> io::Result<()> {
        self.events
            .entry(actor_id)
            .or_default()
            .extend(entries.iter().cloned());
        Ok(())
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        self.events.get(&actor_id).cloned().unwrap_or_default()
    }

    fn scan_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<EventEntry> {
        self.events
            .get(&actor_id)
            .into_iter()
            .flatten()
            .filter(|entry| entry.sequence >= start_sequence)
            .take(limit)
            .cloned()
            .collect()
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
        snapshot_seq
            .max(journal_seq)
            .max(wf_event_seq)
            .max(event_seq)
    }

    fn clear(&mut self, actor_id: u64) -> io::Result<()> {
        self.snapshots.remove(&actor_id);
        self.journals.remove(&actor_id);
        self.workflow_events.remove(&actor_id);
        self.events.remove(&actor_id);
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

    fn workflow_commits_path(&self, actor_id: u64) -> PathBuf {
        self.actor_dir(actor_id).join("workflow_commits.jsonl")
    }

    fn read_workflow_commits(&self, actor_id: u64) -> Vec<WorkflowCommitRecord> {
        let file = match fs::File::open(self.workflow_commits_path(actor_id)) {
            Ok(file) => file,
            Err(_) => return Vec::new(),
        };
        BufReader::new(file)
            .lines()
            .filter_map(Result::ok)
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }

    fn repair_workflow_commit_tail(&self, actor_id: u64) -> io::Result<()> {
        let path = self.workflow_commits_path(actor_id);
        let mut file = match fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let bytes = fs::read(&path)?;
        if bytes.is_empty() || bytes.last() == Some(&b'\n') {
            return Ok(());
        }

        let valid_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|position| position + 1)
            .unwrap_or(0);
        file.set_len(valid_len as u64)?;
        file.sync_all()
    }

    fn events_path(&self, actor_id: u64) -> PathBuf {
        self.actor_dir(actor_id).join("events.jsonl")
    }
}

fn scan_jsonl<T, F>(path: PathBuf, start_sequence: u64, limit: usize, sequence: F) -> Vec<T>
where
    T: serde::de::DeserializeOwned,
    F: Fn(&T) -> u64,
{
    if limit == 0 {
        return Vec::new();
    }
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::with_capacity(limit.min(1024));
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        let value: T = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if sequence(&value) < start_sequence {
            continue;
        }
        out.push(value);
        if out.len() >= limit {
            break;
        }
    }
    out
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
        let standalone = {
            let path = self.snapshot_path(actor_id);
            match fs::read_to_string(&path) {
                Ok(data) => match serde_json::from_str(&data) {
                    Ok(snapshot) => Some(snapshot),
                    Err(error) => {
                        warn!(
                            "nulang-persist: failed to parse snapshot for actor {} at {}: {}",
                            actor_id,
                            path.display(),
                            error
                        );
                        None
                    }
                },
                Err(_) => None,
            }
        };
        let committed = self
            .read_workflow_commits(actor_id)
            .into_iter()
            .last()
            .map(|record| record.snapshot);

        match (standalone, committed) {
            (Some(standalone), Some(committed)) => {
                if committed.sequence >= standalone.sequence {
                    Some(committed)
                } else {
                    Some(standalone)
                }
            }
            (Some(snapshot), None) | (None, Some(snapshot)) => Some(snapshot),
            (None, None) => None,
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

    fn scan_journal_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<JournalEntry> {
        scan_jsonl(
            self.journal_path(actor_id),
            start_sequence,
            limit,
            |entry: &JournalEntry| entry.sequence,
        )
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

    fn commit_workflow_event_and_snapshot(
        &mut self,
        actor_id: u64,
        event: WorkflowEvent,
        snapshot: ActorSnapshot,
    ) -> io::Result<()> {
        if event.sequence() != snapshot.sequence || snapshot.actor_id != actor_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow event and snapshot must describe the same actor sequence",
            ));
        }
        let dir = self.actor_dir(actor_id);
        fs::create_dir_all(&dir)?;
        self.repair_workflow_commit_tail(actor_id)?;
        let path = self.workflow_commits_path(actor_id);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let record = WorkflowCommitRecord { event, snapshot };
        let json = serde_json::to_string(&record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        writeln!(file, "{}", json)?;
        file.sync_all()
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        let mut by_sequence = BTreeMap::new();
        let path = self.workflow_events_path(actor_id);
        if let Ok(data) = fs::read_to_string(path) {
            for event in data
                .lines()
                .filter_map(|line| serde_json::from_str::<WorkflowEvent>(line).ok())
            {
                by_sequence.insert(event.sequence(), event);
            }
        }
        for record in self.read_workflow_commits(actor_id) {
            by_sequence.insert(record.event.sequence(), record.event);
        }
        by_sequence.into_values().collect()
    }

    fn scan_workflow_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<WorkflowEvent> {
        if limit == 0 {
            return Vec::new();
        }
        let legacy = scan_jsonl(
            self.workflow_events_path(actor_id),
            start_sequence,
            limit,
            WorkflowEvent::sequence,
        );
        let committed: Vec<WorkflowCommitRecord> = scan_jsonl(
            self.workflow_commits_path(actor_id),
            start_sequence,
            limit,
            |record: &WorkflowCommitRecord| record.event.sequence(),
        );
        let mut by_sequence = BTreeMap::new();
        for event in legacy {
            by_sequence.insert(event.sequence(), event);
        }
        for record in committed {
            by_sequence.insert(record.event.sequence(), record.event);
        }
        by_sequence.into_values().take(limit).collect()
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

    fn scan_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<EventEntry> {
        scan_jsonl(
            self.events_path(actor_id),
            start_sequence,
            limit,
            |entry: &EventEntry| entry.sequence,
        )
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
            // v2 permits one event-sourced mutation per field at the same actor
            // sequence. The legacy table keyed only by (actor_id, sequence),
            // which silently made multi-field event sourcing backend-dependent.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS events_v2 (
                    actor_id INTEGER NOT NULL,
                    sequence INTEGER NOT NULL,
                    ordinal INTEGER NOT NULL,
                    field_name TEXT NOT NULL,
                    event_name TEXT NOT NULL,
                    args TEXT NOT NULL,
                    value TEXT NOT NULL DEFAULT '1',
                    PRIMARY KEY (actor_id, sequence, ordinal),
                    UNIQUE (actor_id, sequence, field_name)
                )",
                (),
            )
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "INSERT OR IGNORE INTO events_v2
                    (actor_id, sequence, ordinal, field_name, event_name, args, value)
                 SELECT actor_id, sequence, 0, field_name, event_name, args, value
                 FROM events",
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

    fn scan_journal_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<JournalEntry> {
        if limit == 0 {
            return Vec::new();
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = match conn
                .query(
                    "SELECT sequence, behavior_id, payload FROM journal
                     WHERE actor_id = ?1 AND sequence >= ?2
                     ORDER BY sequence ASC LIMIT ?3",
                    libsql::params![actor_id as i64, start_sequence as i64, limit],
                )
                .await
            {
                Ok(rows) => rows,
                Err(_) => return Vec::new(),
            };
            let mut entries = Vec::new();
            while let Ok(Some(row)) = rows.next().await {
                let seq: i64 = match row.get(0) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let behavior_id: i64 = match row.get(1) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let payload_json: String = match row.get(2) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let payload = match serde_json::from_str(&payload_json) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                entries.push(JournalEntry {
                    sequence: seq as u64,
                    behavior_id: behavior_id as u16,
                    payload,
                });
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

    fn commit_workflow_event_and_snapshot(
        &mut self,
        actor_id: u64,
        event: WorkflowEvent,
        snapshot: ActorSnapshot,
    ) -> io::Result<()> {
        if event.sequence() != snapshot.sequence || snapshot.actor_id != actor_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow event and snapshot must describe the same actor sequence",
            ));
        }

        let event_json = serde_json::to_string(&event)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let state_json = serde_json::to_string(&snapshot.state)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let crdt_json = serde_json::to_string(&snapshot.crdt_snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let crdt_field_map_json = serde_json::to_string(&snapshot.crdt_field_map)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let authority_json = serde_json::to_string(&snapshot.authority_tokens)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let conn = self.conn();

        self.rt.block_on(async {
            conn.execute("BEGIN IMMEDIATE", ())
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;

            let result: io::Result<()> = async {
                conn.execute(
                    "INSERT INTO workflow_events (actor_id, sequence, event)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(actor_id, sequence) DO UPDATE SET event=excluded.event",
                    libsql::params![actor_id as i64, event.sequence() as i64, event_json],
                )
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;

                conn.execute(
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
                        actor_id as i64,
                        snapshot.sequence as i64,
                        state_json,
                        snapshot.waiting_signal.as_deref(),
                        crdt_json.as_str(),
                        crdt_field_map_json.as_str(),
                        authority_json.as_str()
                    ],
                )
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
                Ok(())
            }
            .await;

            match result {
                Ok(()) => conn
                    .execute("COMMIT", ())
                    .await
                    .map(|_| ())
                    .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string())),
                Err(error) => {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    Err(error)
                }
            }
        })
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = match conn
                .query(
                    "SELECT event FROM workflow_events
                 WHERE actor_id = ?1 ORDER BY sequence ASC",
                    libsql::params![actor_id as i64],
                )
                .await
            {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let mut events = Vec::new();
            loop {
                match rows.next().await {
                    Ok(Some(row)) => {
                        let event_json: String = match row.get(0) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if let Ok(event) = serde_json::from_str(&event_json) {
                            events.push(event);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            events
        })
    }

    fn scan_workflow_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<WorkflowEvent> {
        if limit == 0 {
            return Vec::new();
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = match conn
                .query(
                    "SELECT event FROM workflow_events
                     WHERE actor_id = ?1 AND sequence >= ?2
                     ORDER BY sequence ASC LIMIT ?3",
                    libsql::params![actor_id as i64, start_sequence as i64, limit],
                )
                .await
            {
                Ok(rows) => rows,
                Err(_) => return Vec::new(),
            };
            let mut events = Vec::new();
            while let Ok(Some(row)) = rows.next().await {
                let event_json: String = match row.get(0) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                if let Ok(event) = serde_json::from_str(&event_json) {
                    events.push(event);
                }
            }
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
                "INSERT INTO events_v2
                    (actor_id, sequence, ordinal, field_name, event_name, args, value)
                 SELECT ?1, ?2, COALESCE(MAX(ordinal) + 1, 0), ?3, ?4, ?5, ?6
                 FROM events_v2 WHERE actor_id = ?1 AND sequence = ?2
                 ON CONFLICT(actor_id, sequence, field_name) DO UPDATE SET
                    event_name = excluded.event_name,
                    args = excluded.args,
                    value = excluded.value",
                libsql::params![
                    actor_id as i64,
                    entry.sequence as i64,
                    entry.field_name,
                    entry.event_name,
                    args_json,
                    value_json
                ],
            )
            .await
            .map(|_| ())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
        })
    }

    fn append_events(&mut self, actor_id: u64, entries: &[EventEntry]) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let conn = self.conn();
        self.rt.block_on(async {
            conn.execute("BEGIN IMMEDIATE", ())
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

            let result: io::Result<()> = async {
                for entry in entries {
                    let args_json = serde_json::to_string(&entry.args)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    let value_json = serde_json::to_string(&entry.value)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                    conn.execute(
                        "INSERT INTO events_v2
                            (actor_id, sequence, ordinal, field_name, event_name, args, value)
                         SELECT ?1, ?2, COALESCE(MAX(ordinal) + 1, 0), ?3, ?4, ?5, ?6
                         FROM events_v2 WHERE actor_id = ?1 AND sequence = ?2
                         ON CONFLICT(actor_id, sequence, field_name) DO UPDATE SET
                            event_name = excluded.event_name,
                            args = excluded.args,
                            value = excluded.value",
                        libsql::params![
                            actor_id as i64,
                            entry.sequence as i64,
                            entry.field_name.as_str(),
                            entry.event_name.as_str(),
                            args_json,
                            value_json
                        ],
                    )
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                }
                Ok(())
            }
            .await;

            match result {
                Ok(()) => conn
                    .execute("COMMIT", ())
                    .await
                    .map(|_| ())
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string())),
                Err(err) => {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    Err(err)
                }
            }
        })
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = match conn
                .query(
                    "SELECT sequence, field_name, event_name, args, value FROM events_v2
                 WHERE actor_id = ?1 ORDER BY sequence ASC, ordinal ASC",
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
                        let field_name: String = match row.get(1) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let event_name: String = match row.get(2) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let args_json: String = match row.get(3) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let args: Vec<PersistedValue> = match serde_json::from_str(&args_json) {
                            Ok(p) => p,
                            Err(_) => continue,
                        };
                        let value_json: String = match row.get(4) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let value: PersistedValue = match serde_json::from_str(&value_json) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        entries.push(EventEntry {
                            sequence: seq as u64,
                            field_name,
                            event_name,
                            args,
                            value,
                        });
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            entries
        })
    }

    fn scan_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<EventEntry> {
        if limit == 0 {
            return Vec::new();
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.conn();
        self.rt.block_on(async {
            let mut rows = match conn
                .query(
                    "SELECT sequence, field_name, event_name, args, value FROM events_v2
                     WHERE actor_id = ?1 AND sequence >= ?2
                     ORDER BY sequence ASC, ordinal ASC LIMIT ?3",
                    libsql::params![actor_id as i64, start_sequence as i64, limit],
                )
                .await
            {
                Ok(rows) => rows,
                Err(_) => return Vec::new(),
            };
            let mut entries = Vec::new();
            while let Ok(Some(row)) = rows.next().await {
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
            entries
        })
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        let conn = self.conn();
        self.rt.block_on(async {
            let snapshot_seq: Option<i64> = async {
                let mut rows = conn.query(
                    "SELECT sequence FROM snapshots WHERE actor_id = ?1",
                    libsql::params![actor_id as i64],
                ).await.ok()?;
                let row = rows.next().await.ok()??;
                row.get(0).ok()
            }.await;
            let journal_seq: Option<i64> = async {
                let mut rows = conn.query(
                    "SELECT sequence FROM journal WHERE actor_id = ?1 ORDER BY sequence DESC LIMIT 1",
                    libsql::params![actor_id as i64],
                ).await.ok()?;
                let row = rows.next().await.ok()??;
                row.get(0).ok()
            }.await;
            let wf_event_seq: Option<i64> = async {
                let mut rows = conn.query(
                    "SELECT sequence FROM workflow_events WHERE actor_id = ?1 ORDER BY sequence DESC LIMIT 1",
                    libsql::params![actor_id as i64],
                ).await.ok()?;
                let row = rows.next().await.ok()??;
                row.get(0).ok()
            }.await;
            let event_seq: Option<i64> = async {
                let mut rows = conn.query(
                    "SELECT sequence FROM events_v2 WHERE actor_id = ?1 ORDER BY sequence DESC LIMIT 1",
                    libsql::params![actor_id as i64],
                ).await.ok()?;
                let row = rows.next().await.ok()??;
                row.get(0).ok()
            }.await;
            snapshot_seq.unwrap_or(0)
                .max(journal_seq.unwrap_or(0))
                .max(wf_event_seq.unwrap_or(0))
                .max(event_seq.unwrap_or(0)) as u64
        })
    }

    fn clear(&mut self, actor_id: u64) -> io::Result<()> {
        let conn = self.conn();
        self.rt.block_on(async {
            conn.execute(
                "DELETE FROM snapshots WHERE actor_id = ?1",
                libsql::params![actor_id as i64],
            )
            .await
            .map(|_| ())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "DELETE FROM journal WHERE actor_id = ?1",
                libsql::params![actor_id as i64],
            )
            .await
            .map(|_| ())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "DELETE FROM workflow_events WHERE actor_id = ?1",
                libsql::params![actor_id as i64],
            )
            .await
            .map(|_| ())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "DELETE FROM events WHERE actor_id = ?1",
                libsql::params![actor_id as i64],
            )
            .await
            .map(|_| ())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            conn.execute(
                "DELETE FROM events_v2 WHERE actor_id = ?1",
                libsql::params![actor_id as i64],
            )
            .await
            .map(|_| ())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
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

    fn actor_event_key(actor_id: u64, sequence: u64, field_name: &str) -> Vec<u8> {
        let mut key = Vec::with_capacity(16 + field_name.len());
        key.extend_from_slice(&actor_id.to_be_bytes());
        key.extend_from_slice(&sequence.to_be_bytes());
        key.extend_from_slice(field_name.as_bytes());
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

    fn scan_journal_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<JournalEntry> {
        if limit == 0 {
            return Vec::new();
        }
        let cf = match self.cf(Self::CF_JOURNAL) {
            Ok(cf) => cf,
            Err(_) => return Vec::new(),
        };
        let start = Self::actor_seq_key(actor_id, start_sequence);
        let mut iter = self.db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
        );
        let mut entries = Vec::with_capacity(limit.min(1024));
        while entries.len() < limit {
            let Some(Ok((key, value))) = iter.next() else {
                break;
            };
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

    fn commit_workflow_event_and_snapshot(
        &mut self,
        actor_id: u64,
        event: WorkflowEvent,
        snapshot: ActorSnapshot,
    ) -> io::Result<()> {
        if event.sequence() != snapshot.sequence || snapshot.actor_id != actor_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow event and snapshot must describe the same actor sequence",
            ));
        }

        let workflow_cf = self.cf(Self::CF_WORKFLOW_EVENTS)?;
        let snapshot_cf = self.cf(Self::CF_SNAPSHOTS)?;
        let event_json = serde_json::to_vec(&event)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let snapshot_json = serde_json::to_vec(&snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut batch = rocksdb::WriteBatch::default();
        batch.put_cf(
            workflow_cf,
            Self::actor_seq_key(actor_id, event.sequence()),
            event_json,
        );
        batch.put_cf(snapshot_cf, Self::actor_key(actor_id), snapshot_json);
        self.db
            .write(batch)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
        self.db
            .flush_wal(true)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))
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

    fn scan_workflow_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<WorkflowEvent> {
        if limit == 0 {
            return Vec::new();
        }
        let cf = match self.cf(Self::CF_WORKFLOW_EVENTS) {
            Ok(cf) => cf,
            Err(_) => return Vec::new(),
        };
        let start = Self::actor_seq_key(actor_id, start_sequence);
        let mut iter = self.db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
        );
        let mut events = Vec::with_capacity(limit.min(1024));
        while events.len() < limit {
            let Some(Ok((key, value))) = iter.next() else {
                break;
            };
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
                Self::actor_event_key(actor_id, entry.sequence, &entry.field_name),
                json.as_bytes(),
            )
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        self.db
            .flush_wal(true)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn append_events(&mut self, actor_id: u64, entries: &[EventEntry]) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let cf = self.cf(Self::CF_EVENTS)?;
        let mut batch = rocksdb::WriteBatch::default();
        for entry in entries {
            let json = serde_json::to_vec(entry)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            batch.put_cf(
                cf,
                Self::actor_event_key(actor_id, entry.sequence, &entry.field_name),
                json,
            );
        }
        self.db
            .write(batch)
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

    fn scan_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<EventEntry> {
        if limit == 0 {
            return Vec::new();
        }
        let cf = match self.cf(Self::CF_EVENTS) {
            Ok(cf) => cf,
            Err(_) => return Vec::new(),
        };
        let start = Self::actor_seq_key(actor_id, start_sequence);
        let mut iter = self.db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
        );
        let mut entries = Vec::with_capacity(limit.min(1024));
        while entries.len() < limit {
            let Some(Ok((key, value))) = iter.next() else {
                break;
            };
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
            // bytes; journal/workflow keys start with actor || sequence and event keys
            // additionally carry the field name. All layouts remain contiguous
            // under the actor prefix.
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
        conn.execute(
            "CREATE TABLE IF NOT EXISTS events_v2 (
                actor_id BIGINT NOT NULL,
                sequence BIGINT NOT NULL,
                ordinal BIGINT NOT NULL,
                field_name TEXT NOT NULL,
                event_name TEXT NOT NULL,
                args TEXT NOT NULL,
                value TEXT NOT NULL DEFAULT '1',
                PRIMARY KEY (actor_id, sequence, ordinal),
                UNIQUE (actor_id, sequence, field_name)
            )",
            &[],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        conn.execute(
            "INSERT INTO events_v2
                (actor_id, sequence, ordinal, field_name, event_name, args, value)
             SELECT actor_id, sequence, 0, field_name, event_name, args, value
             FROM events
             ON CONFLICT (actor_id, sequence, field_name) DO NOTHING",
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

    fn scan_journal_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<JournalEntry> {
        if limit == 0 {
            return Vec::new();
        }
        let mut conn = match self.conn.lock() {
            Ok(conn) => conn,
            Err(_) => return Vec::new(),
        };
        let start_sequence = i64::try_from(start_sequence).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = match conn.query(
            "SELECT sequence, behavior_id, payload FROM journal
             WHERE actor_id = $1 AND sequence >= $2
             ORDER BY sequence ASC LIMIT $3",
            &[&(actor_id as i64), &start_sequence, &limit],
        ) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows.iter()
            .filter_map(|row| {
                let sequence: i64 = row.get(0);
                let behavior_id: i32 = row.get(1);
                let payload_json: String = row.get(2);
                let payload = serde_json::from_str(&payload_json).ok()?;
                Some(JournalEntry {
                    sequence: sequence as u64,
                    behavior_id: behavior_id as u16,
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

    fn commit_workflow_event_and_snapshot(
        &mut self,
        actor_id: u64,
        event: WorkflowEvent,
        snapshot: ActorSnapshot,
    ) -> io::Result<()> {
        if event.sequence() != snapshot.sequence || snapshot.actor_id != actor_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow event and snapshot must describe the same actor sequence",
            ));
        }

        let event_json = serde_json::to_string(&event)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let state_json = serde_json::to_string(&snapshot.state)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let crdt_json = serde_json::to_string(&snapshot.crdt_snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let crdt_field_map_json = serde_json::to_string(&snapshot.crdt_field_map)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let authority_json = serde_json::to_string(&snapshot.authority_tokens)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut conn = self.conn.lock().unwrap();
        let mut tx = conn
            .transaction()
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;

        tx.execute(
            "INSERT INTO workflow_events (actor_id, sequence, event)
             VALUES ($1, $2, $3)
             ON CONFLICT (actor_id, sequence) DO UPDATE SET event = EXCLUDED.event",
            &[&(actor_id as i64), &(event.sequence() as i64), &event_json],
        )
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;

        tx.execute(
            "INSERT INTO snapshots
                (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (actor_id) DO UPDATE SET
                sequence = EXCLUDED.sequence,
                state = EXCLUDED.state,
                waiting_signal = EXCLUDED.waiting_signal,
                crdt_snapshot = EXCLUDED.crdt_snapshot,
                crdt_field_map = EXCLUDED.crdt_field_map,
                authority_tokens = EXCLUDED.authority_tokens",
            &[
                &(actor_id as i64),
                &(snapshot.sequence as i64),
                &state_json,
                &snapshot.waiting_signal.as_deref(),
                &crdt_json.as_str(),
                &crdt_field_map_json.as_str(),
                &authority_json.as_str(),
            ],
        )
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;

        tx.commit()
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))
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

    fn scan_workflow_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<WorkflowEvent> {
        if limit == 0 {
            return Vec::new();
        }
        let mut conn = match self.conn.lock() {
            Ok(conn) => conn,
            Err(_) => return Vec::new(),
        };
        let start_sequence = i64::try_from(start_sequence).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = match conn.query(
            "SELECT event FROM workflow_events
             WHERE actor_id = $1 AND sequence >= $2
             ORDER BY sequence ASC LIMIT $3",
            &[&(actor_id as i64), &start_sequence, &limit],
        ) {
            Ok(rows) => rows,
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
            "INSERT INTO events_v2
                (actor_id, sequence, ordinal, field_name, event_name, args, value)
             SELECT $1, $2, COALESCE(MAX(ordinal) + 1, 0), $3, $4, $5, $6
             FROM events_v2 WHERE actor_id = $1 AND sequence = $2
             ON CONFLICT (actor_id, sequence, field_name) DO UPDATE SET
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

    fn append_events(&mut self, actor_id: u64, entries: &[EventEntry]) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let mut tx = conn
            .transaction()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        for entry in entries {
            let args_json = serde_json::to_string(&entry.args)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let value_json = serde_json::to_string(&entry.value)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            tx.execute(
                "INSERT INTO events_v2
                    (actor_id, sequence, ordinal, field_name, event_name, args, value)
                 SELECT $1, $2, COALESCE(MAX(ordinal) + 1, 0), $3, $4, $5, $6
                 FROM events_v2 WHERE actor_id = $1 AND sequence = $2
                 ON CONFLICT (actor_id, sequence, field_name) DO UPDATE SET
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
        }
        tx.commit()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        let mut conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let rows = match conn.query(
            "SELECT sequence, field_name, event_name, args, value FROM events_v2
             WHERE actor_id = $1 ORDER BY sequence ASC, ordinal ASC",
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

    fn scan_events_from(
        &self,
        actor_id: u64,
        start_sequence: u64,
        limit: usize,
    ) -> Vec<EventEntry> {
        if limit == 0 {
            return Vec::new();
        }
        let mut conn = match self.conn.lock() {
            Ok(conn) => conn,
            Err(_) => return Vec::new(),
        };
        let start_sequence = i64::try_from(start_sequence).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = match conn.query(
            "SELECT sequence, field_name, event_name, args, value FROM events_v2
             WHERE actor_id = $1 AND sequence >= $2
             ORDER BY sequence ASC, ordinal ASC LIMIT $3",
            &[&(actor_id as i64), &start_sequence, &limit],
        ) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows.iter()
            .filter_map(|row| {
                let sequence: i64 = row.get(0);
                let field_name: String = row.get(1);
                let event_name: String = row.get(2);
                let args_json: String = row.get(3);
                let value_json: String = row.get(4);
                let args = serde_json::from_str(&args_json).ok()?;
                let value = serde_json::from_str(&value_json).ok()?;
                Some(EventEntry {
                    sequence: sequence as u64,
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
                "SELECT sequence FROM events_v2 WHERE actor_id = $1 ORDER BY sequence DESC LIMIT 1",
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
        for table in [
            "snapshots",
            "journal",
            "workflow_events",
            "events",
            "events_v2",
        ] {
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
        assert!(
            !store
                .snapshot_path(1)
                .with_file_name("snapshot.json.tmp")
                .exists()
        );
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
mod workflow_atomic_commit_tests {
    use super::*;

    fn snapshot(actor_id: u64, sequence: u64) -> ActorSnapshot {
        let mut state = HashMap::new();
        state.insert("step_index".to_string(), PersistedValue::Int(2));
        ActorSnapshot {
            actor_id,
            sequence,
            state,
            waiting_signal: None,
            crdt_snapshot: None,
            crdt_field_map: None,
            authority_tokens: BTreeSet::new(),
        }
    }

    fn completed(sequence: u64) -> WorkflowEvent {
        WorkflowEvent::StepCompleted {
            sequence,
            step_name: "settle".to_string(),
        }
    }

    #[test]
    fn memory_workflow_commit_publishes_event_and_snapshot_at_one_sequence() {
        let mut store = MemoryStore::new();
        store
            .commit_workflow_event_and_snapshot(41, completed(7), snapshot(41, 7))
            .unwrap();

        let events = store.read_workflow_events(41);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence(), 7);
        assert_eq!(store.load_snapshot(41).unwrap().sequence, 7);
        assert_eq!(store.latest_sequence(41), 7);
    }

    #[test]
    fn workflow_commit_rejects_mismatched_sequence_without_partial_memory_write() {
        let mut store = MemoryStore::new();
        let error = store
            .commit_workflow_event_and_snapshot(42, completed(8), snapshot(42, 9))
            .expect_err("mismatched workflow commit must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(store.read_workflow_events(42).is_empty());
        assert!(store.load_snapshot(42).is_none());
    }

    #[test]
    fn json_workflow_commit_recovers_event_and_snapshot_from_one_commit_log() {
        let dir = std::env::temp_dir().join(format!(
            "nulang_workflow_commit_test_{}_{}",
            std::process::id(),
            43
        ));
        let _ = fs::remove_dir_all(&dir);

        {
            let mut store = JsonFileStore::new(&dir).unwrap();
            store
                .commit_workflow_event_and_snapshot(43, completed(11), snapshot(43, 11))
                .unwrap();
            assert!(store.workflow_commits_path(43).exists());
            assert!(!store.workflow_events_path(43).exists());
        }

        let reopened = JsonFileStore::new(&dir).unwrap();
        let events = reopened.read_workflow_events(43);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence(), 11);
        let recovered = reopened.load_snapshot(43).unwrap();
        assert_eq!(recovered.sequence, 11);
        assert_eq!(
            recovered.state.get("step_index"),
            Some(&PersistedValue::Int(2))
        );
        assert_eq!(reopened.latest_sequence(43), 11);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_workflow_commit_repairs_partial_trailing_record_before_append() {
        let dir = std::env::temp_dir().join(format!(
            "nulang_workflow_commit_tail_test_{}_{}",
            std::process::id(),
            45
        ));
        let _ = fs::remove_dir_all(&dir);

        let mut store = JsonFileStore::new(&dir).unwrap();
        store
            .commit_workflow_event_and_snapshot(45, completed(1), snapshot(45, 1))
            .unwrap();

        let path = store.workflow_commits_path(45);
        {
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(br#"{"event":"#).unwrap();
            file.sync_all().unwrap();
        }

        store
            .commit_workflow_event_and_snapshot(45, completed(2), snapshot(45, 2))
            .unwrap();

        let events = store.read_workflow_events(45);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence(), 1);
        assert_eq!(events[1].sequence(), 2);
        assert_eq!(store.load_snapshot(45).unwrap().sequence, 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn libsql_workflow_commit_is_visible_as_one_sequence() {
        let mut store = LibsqlStore::in_memory().unwrap();
        store
            .commit_workflow_event_and_snapshot(44, completed(13), snapshot(44, 13))
            .unwrap();

        let events = store.read_workflow_events(44);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence(), 13);
        assert_eq!(store.load_snapshot(44).unwrap().sequence, 13);
        assert_eq!(store.latest_sequence(44), 13);
    }
}
