//! General atomic libSQL commit boundary for durable actor turns.
//!
//! A turn has exactly one canonical inbox bundle (sequence + actor state +
//! receiver dedup snapshot) and zero or more terminal effect receipts. All of
//! those records plus activation authority become durable in one IMMEDIATE
//! transaction.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::effect_receipt::{
    EffectInvocationId, EffectReceipt, EffectReceiptError, PersistedEffectState,
};
use crate::effect_receipt_fence::{EffectReceiptFence, EffectReceiptFenceDecision};
use crate::effect_receipt_libsql_v2::{
    evaluate_fence, load_context, load_epoch, load_state, write_epoch, write_state,
    DurableEffectContext, LibsqlDurableEffectError,
};

pub const DURABLE_TURN_FORMAT_VERSION: u16 = 2;

/// Latest authoritative durable turn for one actor namespace.
///
/// `inbox_bundle_json` is the *only* actor-state payload. It is intentionally
/// shaped to accept PR #303's serialized `DurableInboxBundle<T>` directly:
/// `{ actor_sequence, state, dedup }`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DurableTurnRecord {
    pub format_version: u16,
    pub actor_id: u64,
    /// Denormalized for indexed sequence checks. Validation requires this to
    /// equal `inbox_bundle_json.actor_sequence`.
    pub actor_sequence: u64,
    pub inbox_bundle_json: String,
    /// Ordered logical effects whose terminal observations belong to this turn.
    /// Empty is the normal case for a pure actor message.
    pub effect_invocation_ids: Vec<EffectInvocationId>,
}

impl DurableTurnRecord {
    pub fn new(
        actor_id: u64,
        inbox_bundle_json: impl Into<String>,
        effect_invocation_ids: Vec<EffectInvocationId>,
    ) -> Result<Self, LibsqlDurableTurnError> {
        let inbox_bundle_json = inbox_bundle_json.into();
        let actor_sequence = bundle_sequence(&inbox_bundle_json)?;
        let record = Self {
            format_version: DURABLE_TURN_FORMAT_VERSION,
            actor_id,
            actor_sequence,
            inbox_bundle_json,
            effect_invocation_ids,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn validate(&self) -> Result<(), LibsqlDurableTurnError> {
        if self.format_version != DURABLE_TURN_FORMAT_VERSION {
            return Err(LibsqlDurableTurnError::UnsupportedFormat(
                self.format_version,
            ));
        }
        if self.actor_sequence == 0 {
            return Err(LibsqlDurableTurnError::InvalidSequence(0));
        }
        let embedded = bundle_sequence(&self.inbox_bundle_json)?;
        if embedded != self.actor_sequence {
            return Err(LibsqlDurableTurnError::BundleSequenceMismatch {
                record_sequence: self.actor_sequence,
                bundle_sequence: embedded,
            });
        }
        let mut seen = HashSet::new();
        for id in &self.effect_invocation_ids {
            if !seen.insert(*id) {
                return Err(LibsqlDurableTurnError::DuplicateEffect(*id));
            }
        }
        Ok(())
    }
}

pub struct LibsqlDurableTurnStore {
    conn: std::sync::Mutex<libsql::Connection>,
    rt: tokio::runtime::Runtime,
    path: PathBuf,
}

#[derive(Debug)]
pub enum LibsqlDurableTurnError {
    Storage(String),
    CorruptState(String),
    UnsupportedFormat(u16),
    InvalidSequence(u64),
    InvalidInboxBundle(String),
    BundleSequenceMismatch {
        record_sequence: u64,
        bundle_sequence: u64,
    },
    DuplicateEffect(EffectInvocationId),
    ReceiptSetMismatch,
    MissingIntent(EffectInvocationId),
    MissingContext(EffectInvocationId),
    EffectTurnMismatch {
        invocation_id: EffectInvocationId,
        effect_sequence: u64,
        turn_sequence: u64,
    },
    ConflictingReceipt(EffectInvocationId),
    StaleSequence {
        actor_id: u64,
        proposed: u64,
        current: u64,
    },
    ConflictingTurn {
        actor_id: u64,
        actor_sequence: u64,
    },
    EffectStore(LibsqlDurableEffectError),
    Receipt(EffectReceiptError),
}

impl fmt::Display for LibsqlDurableTurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(message) => write!(f, "libSQL durable-turn storage error: {message}"),
            Self::CorruptState(message) => write!(f, "corrupt durable-turn state: {message}"),
            Self::UnsupportedFormat(version) => {
                write!(f, "unsupported durable-turn format version {version}")
            }
            Self::InvalidSequence(sequence) => {
                write!(f, "durable actor sequence must be positive, got {sequence}")
            }
            Self::InvalidInboxBundle(message) => write!(f, "invalid durable inbox bundle: {message}"),
            Self::BundleSequenceMismatch {
                record_sequence,
                bundle_sequence,
            } => write!(
                f,
                "durable turn sequence {record_sequence} does not match inbox bundle sequence {bundle_sequence}"
            ),
            Self::DuplicateEffect(id) => write!(f, "duplicate effect invocation {id} in one turn"),
            Self::ReceiptSetMismatch => {
                f.write_str("durable turn receipt ids do not match declared effect invocation ids")
            }
            Self::MissingIntent(id) => write!(f, "missing effect intent {id} for durable turn"),
            Self::MissingContext(id) => write!(f, "missing durable effect context {id}"),
            Self::EffectTurnMismatch {
                invocation_id,
                effect_sequence,
                turn_sequence,
            } => write!(
                f,
                "effect {invocation_id} belongs to turn {effect_sequence}, not committed turn {turn_sequence}"
            ),
            Self::ConflictingReceipt(id) => write!(f, "conflicting terminal receipt {id}"),
            Self::StaleSequence {
                actor_id,
                proposed,
                current,
            } => write!(
                f,
                "stale durable turn for actor {actor_id}: proposed {proposed}, current {current}"
            ),
            Self::ConflictingTurn {
                actor_id,
                actor_sequence,
            } => write!(
                f,
                "conflicting durable turn for actor {actor_id} at sequence {actor_sequence}"
            ),
            Self::EffectStore(error) => write!(f, "{error}"),
            Self::Receipt(error) => write!(f, "effect receipt rejected: {error}"),
        }
    }
}

impl std::error::Error for LibsqlDurableTurnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EffectStore(error) => Some(error),
            Self::Receipt(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LibsqlDurableEffectError> for LibsqlDurableTurnError {
    fn from(value: LibsqlDurableEffectError) -> Self {
        Self::EffectStore(value)
    }
}

impl From<EffectReceiptError> for LibsqlDurableTurnError {
    fn from(value: EffectReceiptError) -> Self {
        Self::Receipt(value)
    }
}

impl LibsqlDurableTurnStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, LibsqlDurableTurnError> {
        let path = path.as_ref().to_path_buf();
        let db_path = if path == Path::new(":memory:") {
            ":memory:".to_string()
        } else {
            path.to_string_lossy().into_owned()
        };
        let rt = tokio::runtime::Runtime::new().map_err(storage_error)?;
        let db = rt.block_on(async {
            libsql::Builder::new_local(&db_path)
                .build()
                .await
                .map_err(storage_error)
        })?;
        let conn = db.connect().map_err(storage_error)?;
        let store = Self {
            conn: std::sync::Mutex::new(conn),
            rt,
            path,
        };
        store.ensure_tables()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, libsql::Connection> {
        self.conn.lock().unwrap()
    }

    fn ensure_tables(&self) -> Result<(), LibsqlDurableTurnError> {
        let conn = self.conn();
        self.rt.block_on(async {
            conn.execute(
                "CREATE TABLE IF NOT EXISTS activation_fences (
                    actor_id INTEGER PRIMARY KEY,
                    epoch INTEGER NOT NULL CHECK(epoch >= 1)
                )",
                (),
            )
            .await
            .map_err(storage_error)?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS effect_receipts (
                    actor_id INTEGER NOT NULL,
                    invocation_id TEXT NOT NULL,
                    state_json TEXT NOT NULL,
                    PRIMARY KEY (actor_id, invocation_id)
                )",
                (),
            )
            .await
            .map_err(storage_error)?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS effect_contexts (
                    actor_id INTEGER NOT NULL,
                    invocation_id TEXT NOT NULL,
                    durable_sequence INTEGER NOT NULL CHECK(durable_sequence >= 1),
                    occurrence_index INTEGER NOT NULL CHECK(occurrence_index >= 0),
                    PRIMARY KEY (actor_id, invocation_id)
                )",
                (),
            )
            .await
            .map_err(storage_error)?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS durable_turns (
                    actor_id INTEGER PRIMARY KEY,
                    actor_sequence INTEGER NOT NULL CHECK(actor_sequence >= 1),
                    turn_json TEXT NOT NULL
                )",
                (),
            )
            .await
            .map_err(storage_error)?;
            Ok(())
        })
    }

    pub fn load_turn(
        &self,
        actor_id: u64,
    ) -> Result<Option<DurableTurnRecord>, LibsqlDurableTurnError> {
        let conn = self.conn();
        self.rt.block_on(async { load_turn(&conn, actor_id).await })
    }

    /// Atomically commit one durable actor turn with zero or more effects.
    pub fn commit_turn<F: EffectReceiptFence>(
        &self,
        namespace_actor_id: u64,
        fence: &F,
        record: DurableTurnRecord,
        receipts: Vec<EffectReceipt>,
    ) -> Result<EffectReceiptFenceDecision, LibsqlDurableTurnError> {
        record.validate()?;
        if record.actor_id != namespace_actor_id {
            return Err(LibsqlDurableTurnError::CorruptState(format!(
                "turn actor {} does not match namespace actor {namespace_actor_id}",
                record.actor_id
            )));
        }
        validate_receipt_set(&record, &receipts)?;

        let presented_actor_id = fence.actor_id();
        let presented_epoch = fence.epoch();
        let conn = self.conn();
        self.rt.block_on(async {
            let tx = conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await
                .map_err(storage_error)?;

            let result = async {
                let current_epoch = load_epoch(&tx, namespace_actor_id).await?;
                let decision = evaluate_fence(
                    namespace_actor_id,
                    presented_actor_id,
                    presented_epoch,
                    current_epoch,
                )?;

                let current_turn = load_turn(&tx, namespace_actor_id).await?;
                validate_turn_sequence(current_turn.as_ref(), &record)?;

                // Validate every effect first. No receipt write becomes visible
                // until all contexts and terminal transitions are known-good.
                let mut completed = Vec::with_capacity(receipts.len());
                for receipt in &receipts {
                    let existing = load_state(&tx, namespace_actor_id, receipt.invocation_id)
                        .await?
                        .ok_or(LibsqlDurableTurnError::MissingIntent(receipt.invocation_id))?;
                    let context = load_context(&tx, namespace_actor_id, receipt.invocation_id)
                        .await?
                        .ok_or(LibsqlDurableTurnError::MissingContext(receipt.invocation_id))?;
                    validate_effect_context(&record, receipt.invocation_id, context)?;
                    completed.push((
                        receipt.invocation_id,
                        complete_effect_state(existing, receipt)?,
                    ));
                }

                for (invocation_id, state) in &completed {
                    write_state(&tx, namespace_actor_id, *invocation_id, state).await?;
                }

                let turn_json = serde_json::to_string(&record)
                    .map_err(|error| LibsqlDurableTurnError::CorruptState(error.to_string()))?;
                tx.execute(
                    "INSERT INTO durable_turns (actor_id, actor_sequence, turn_json)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(actor_id)
                     DO UPDATE SET actor_sequence = excluded.actor_sequence,
                                   turn_json = excluded.turn_json",
                    libsql::params![
                        namespace_actor_id as i64,
                        record.actor_sequence as i64,
                        turn_json
                    ],
                )
                .await
                .map_err(storage_error)?;

                write_epoch(&tx, namespace_actor_id, presented_epoch).await?;
                Ok::<_, LibsqlDurableTurnError>(decision)
            }
            .await;

            match result {
                Ok(decision) => {
                    tx.commit().await.map_err(storage_error)?;
                    Ok(decision)
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    Err(error)
                }
            }
        })
    }
}

fn bundle_sequence(json: &str) -> Result<u64, LibsqlDurableTurnError> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| LibsqlDurableTurnError::InvalidInboxBundle(error.to_string()))?;
    let object = value.as_object().ok_or_else(|| {
        LibsqlDurableTurnError::InvalidInboxBundle("bundle must be a JSON object".to_string())
    })?;
    let sequence = object
        .get("actor_sequence")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            LibsqlDurableTurnError::InvalidInboxBundle(
                "bundle.actor_sequence must be a positive integer".to_string(),
            )
        })?;
    if sequence == 0 {
        return Err(LibsqlDurableTurnError::InvalidSequence(0));
    }
    if !object.contains_key("state") {
        return Err(LibsqlDurableTurnError::InvalidInboxBundle(
            "bundle.state is required".to_string(),
        ));
    }
    if !object.contains_key("dedup") {
        return Err(LibsqlDurableTurnError::InvalidInboxBundle(
            "bundle.dedup is required".to_string(),
        ));
    }
    Ok(sequence)
}

fn validate_receipt_set(
    record: &DurableTurnRecord,
    receipts: &[EffectReceipt],
) -> Result<(), LibsqlDurableTurnError> {
    let declared: HashSet<_> = record.effect_invocation_ids.iter().copied().collect();
    let actual: HashSet<_> = receipts.iter().map(|r| r.invocation_id).collect();
    if actual.len() != receipts.len() || declared != actual {
        return Err(LibsqlDurableTurnError::ReceiptSetMismatch);
    }
    Ok(())
}

fn validate_effect_context(
    record: &DurableTurnRecord,
    invocation_id: EffectInvocationId,
    context: DurableEffectContext,
) -> Result<(), LibsqlDurableTurnError> {
    if context.actor_id != record.actor_id || context.durable_sequence != record.actor_sequence {
        return Err(LibsqlDurableTurnError::EffectTurnMismatch {
            invocation_id,
            effect_sequence: context.durable_sequence,
            turn_sequence: record.actor_sequence,
        });
    }
    Ok(())
}

fn validate_turn_sequence(
    current: Option<&DurableTurnRecord>,
    proposed: &DurableTurnRecord,
) -> Result<(), LibsqlDurableTurnError> {
    let Some(current) = current else {
        return Ok(());
    };
    if proposed.actor_sequence < current.actor_sequence {
        return Err(LibsqlDurableTurnError::StaleSequence {
            actor_id: proposed.actor_id,
            proposed: proposed.actor_sequence,
            current: current.actor_sequence,
        });
    }
    if proposed.actor_sequence == current.actor_sequence && proposed != current {
        return Err(LibsqlDurableTurnError::ConflictingTurn {
            actor_id: proposed.actor_id,
            actor_sequence: proposed.actor_sequence,
        });
    }
    Ok(())
}

fn complete_effect_state(
    existing: PersistedEffectState,
    receipt: &EffectReceipt,
) -> Result<PersistedEffectState, LibsqlDurableTurnError> {
    match existing {
        PersistedEffectState::Intent(intent) => {
            receipt.validate_against(&intent)?;
            Ok(PersistedEffectState::Completed {
                intent,
                receipt: receipt.clone(),
            })
        }
        PersistedEffectState::Completed {
            intent,
            receipt: existing_receipt,
        } => {
            receipt.validate_against(&intent)?;
            if existing_receipt != *receipt {
                return Err(LibsqlDurableTurnError::ConflictingReceipt(
                    receipt.invocation_id,
                ));
            }
            Ok(PersistedEffectState::Completed {
                intent,
                receipt: existing_receipt,
            })
        }
    }
}

async fn load_turn(
    conn: &libsql::Connection,
    actor_id: u64,
) -> Result<Option<DurableTurnRecord>, LibsqlDurableTurnError> {
    let mut rows = conn
        .query(
            "SELECT actor_sequence, turn_json FROM durable_turns WHERE actor_id = ?1",
            libsql::params![actor_id as i64],
        )
        .await
        .map_err(storage_error)?;
    let Some(row) = rows.next().await.map_err(storage_error)? else {
        return Ok(None);
    };
    let sequence: i64 = row.get(0).map_err(storage_error)?;
    if sequence <= 0 {
        return Err(LibsqlDurableTurnError::CorruptState(format!(
            "durable turn for actor {actor_id} contains sequence {sequence}"
        )));
    }
    let turn_json: String = row.get(1).map_err(storage_error)?;
    let record: DurableTurnRecord = serde_json::from_str(&turn_json)
        .map_err(|error| LibsqlDurableTurnError::CorruptState(error.to_string()))?;
    record.validate()?;
    if record.actor_id != actor_id || record.actor_sequence != sequence as u64 {
        return Err(LibsqlDurableTurnError::CorruptState(format!(
            "durable turn row key/sequence does not match embedded record for actor {actor_id}"
        )));
    }
    Ok(Some(record))
}

fn storage_error(error: impl fmt::Display) -> LibsqlDurableTurnError {
    LibsqlDurableTurnError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect_receipt::{
        EffectIdentity, EffectIntent, EffectSiteId, RequestFingerprint,
    };
    use crate::effect_receipt_libsql_v2::LibsqlDurableEffectStore;

    #[derive(Clone, Copy, Debug)]
    struct TestFence {
        actor_id: u64,
        epoch: u64,
    }

    impl EffectReceiptFence for TestFence {
        fn actor_id(&self) -> u64 {
            self.actor_id
        }

        fn epoch(&self) -> u64 {
            self.epoch
        }
    }

    fn fixture(
        actor_id: u64,
        sequence: u64,
        occurrence: u32,
    ) -> (EffectIntent, DurableEffectContext) {
        let site = EffectSiteId::from_semantic_bytes(b"tests.durable-turn.effect");
        let owner = format!("actor:{actor_id}");
        let invocation = EffectInvocationId::derive(owner.as_bytes(), sequence, site, occurrence);
        let intent = EffectIntent::new(
            invocation,
            site,
            EffectIdentity::new("Http", "post").unwrap(),
            RequestFingerprint::from_canonical_bytes(
                format!("request-{sequence}-{occurrence}").as_bytes(),
            ),
            Some(invocation.provider_idempotency_key("test-http-v2")),
        );
        (
            intent,
            DurableEffectContext::new(actor_id, sequence, occurrence),
        )
    }

    fn bundle(sequence: u64, state: u64) -> String {
        format!(
            r#"{{"actor_sequence":{sequence},"state":{{"count":{state}}},"dedup":{{"capacity":8,"committed":[]}}}}"#
        )
    }

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang-{name}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn pure_turn_commits_without_any_effects() {
        let actor_id = 42;
        let path = temp_db("pure-turn-v2");
        let fence = TestFence { actor_id, epoch: 1 };
        let turns = LibsqlDurableTurnStore::new(&path).unwrap();
        let record = DurableTurnRecord::new(actor_id, bundle(1, 1), vec![]).unwrap();

        turns
            .commit_turn(actor_id, &fence, record.clone(), vec![])
            .unwrap();
        assert_eq!(turns.load_turn(actor_id).unwrap(), Some(record));
        cleanup(&path);
    }

    #[test]
    fn multiple_effects_commit_with_one_bundle_and_fence() {
        let actor_id = 42;
        let sequence = 7;
        let path = temp_db("multi-effect-turn-v2");
        let fence = TestFence { actor_id, epoch: 1 };
        let effects = LibsqlDurableEffectStore::new(&path).unwrap();
        let (one, one_ctx) = fixture(actor_id, sequence, 0);
        let (two, two_ctx) = fixture(actor_id, sequence, 1);
        effects
            .prepare_fenced(actor_id, &fence, &one, one_ctx)
            .unwrap();
        effects
            .prepare_fenced(actor_id, &fence, &two, two_ctx)
            .unwrap();

        let receipts = vec![
            EffectReceipt::success(&one, 1, b"one".to_vec()),
            EffectReceipt::success(&two, 1, b"two".to_vec()),
        ];
        let record = DurableTurnRecord::new(
            actor_id,
            bundle(sequence, 2),
            vec![one.invocation_id, two.invocation_id],
        )
        .unwrap();
        let turns = LibsqlDurableTurnStore::new(&path).unwrap();
        turns
            .commit_turn(actor_id, &fence, record.clone(), receipts.clone())
            .unwrap();

        assert_eq!(turns.load_turn(actor_id).unwrap(), Some(record));
        for intent in [&one, &two] {
            assert!(matches!(
                effects.load(actor_id, intent.invocation_id).unwrap(),
                Some(PersistedEffectState::Completed { .. })
            ));
        }
        cleanup(&path);
    }

    #[test]
    fn effect_from_different_turn_is_rejected_and_rolled_back() {
        let actor_id = 42;
        let path = temp_db("turn-context-mismatch-v2");
        let fence = TestFence { actor_id, epoch: 1 };
        let effects = LibsqlDurableEffectStore::new(&path).unwrap();
        let (intent, context) = fixture(actor_id, 7, 0);
        effects
            .prepare_fenced(actor_id, &fence, &intent, context)
            .unwrap();
        let receipt = EffectReceipt::success(&intent, 1, b"ok".to_vec());
        let record = DurableTurnRecord::new(actor_id, bundle(8, 1), vec![intent.invocation_id])
            .unwrap();
        let turns = LibsqlDurableTurnStore::new(&path).unwrap();

        assert!(matches!(
            turns.commit_turn(actor_id, &fence, record, vec![receipt]),
            Err(LibsqlDurableTurnError::EffectTurnMismatch { .. })
        ));
        assert!(turns.load_turn(actor_id).unwrap().is_none());
        assert!(matches!(
            effects.load(actor_id, intent.invocation_id).unwrap(),
            Some(PersistedEffectState::Intent(_))
        ));
        cleanup(&path);
    }

    #[test]
    fn bundle_sequence_is_verified_against_denormalized_sequence() {
        let actor_id = 42;
        let mut record = DurableTurnRecord::new(actor_id, bundle(9, 1), vec![]).unwrap();
        record.actor_sequence = 10;
        assert!(matches!(
            record.validate(),
            Err(LibsqlDurableTurnError::BundleSequenceMismatch { .. })
        ));
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }
}
