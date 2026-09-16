//! Atomic libSQL commit boundary for one durable actor turn.
//!
//! This module deliberately stores the inbox/dedup bundle as an opaque,
//! versioned JSON payload until the delivery stack in PR #303 shares the same
//! baseline. Once `DurableInboxBundle<T>` lands, callers can serialize that
//! exact bundle into `inbox_bundle_json` without changing the transaction
//! contract or SQL schema.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::effect_receipt::{
    EffectInvocationId, EffectReceipt, EffectReceiptError, PersistedEffectState,
};
use crate::effect_receipt_fence::{
    EffectReceiptFence, EffectReceiptFenceDecision, FencedEffectReceiptError,
};

pub const DURABLE_TURN_FORMAT_VERSION: u16 = 1;

/// Latest crash-consistent durable turn committed for one actor namespace.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DurableTurnRecord {
    pub format_version: u16,
    pub actor_id: u64,
    pub actor_sequence: u64,
    /// Versioned actor-state payload owned by the runtime/persistence adapter.
    pub actor_state_json: String,
    /// Opaque serialized inbox+dedup bundle. PR #303's `DurableInboxBundle<T>`
    /// is the intended producer once that stack is merged onto this baseline.
    pub inbox_bundle_json: String,
    /// Logical effect whose terminal observation committed with this turn.
    pub effect_invocation_id: EffectInvocationId,
}

impl DurableTurnRecord {
    pub fn new(
        actor_id: u64,
        actor_sequence: u64,
        actor_state_json: impl Into<String>,
        inbox_bundle_json: impl Into<String>,
        effect_invocation_id: EffectInvocationId,
    ) -> Result<Self, LibsqlDurableTurnError> {
        let record = Self {
            format_version: DURABLE_TURN_FORMAT_VERSION,
            actor_id,
            actor_sequence,
            actor_state_json: actor_state_json.into(),
            inbox_bundle_json: inbox_bundle_json.into(),
            effect_invocation_id,
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
        validate_json("actor state", &self.actor_state_json)?;
        validate_json("inbox bundle", &self.inbox_bundle_json)?;
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
    InvalidJson {
        field: &'static str,
        error: String,
    },
    MissingIntent(EffectInvocationId),
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
    Fence(FencedEffectReceiptError),
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
            Self::InvalidJson { field, error } => {
                write!(f, "invalid {field} JSON: {error}")
            }
            Self::MissingIntent(id) => {
                write!(f, "cannot commit durable turn without effect intent {id}")
            }
            Self::ConflictingReceipt(id) => {
                write!(f, "conflicting terminal receipt for invocation {id}")
            }
            Self::StaleSequence {
                actor_id,
                proposed,
                current,
            } => write!(
                f,
                "stale durable turn for actor {actor_id}: proposed sequence {proposed}, current {current}"
            ),
            Self::ConflictingTurn {
                actor_id,
                actor_sequence,
            } => write!(
                f,
                "conflicting durable turn for actor {actor_id} at sequence {actor_sequence}"
            ),
            Self::Fence(error) => write!(f, "{error}"),
            Self::Receipt(error) => write!(f, "effect receipt rejected: {error}"),
        }
    }
}

impl std::error::Error for LibsqlDurableTurnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fence(error) => Some(error),
            Self::Receipt(error) => Some(error),
            _ => None,
        }
    }
}

impl From<FencedEffectReceiptError> for LibsqlDurableTurnError {
    fn from(value: FencedEffectReceiptError) -> Self {
        Self::Fence(value)
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
            // These two tables intentionally match the RFC 0030 libSQL adapter.
            // `CREATE IF NOT EXISTS` makes this store composable with
            // `LibsqlEffectReceiptStore` against the same database file.
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

    /// Commit one receiver turn as one physical SQLite transaction.
    ///
    /// Preconditions:
    /// - the effect intent already exists from the pre-provider phase;
    /// - `receipt` is the terminal observation for that same invocation;
    /// - `record` contains the actor state + inbox/dedup state that become
    ///   authoritative only together with the receipt.
    ///
    /// The transaction serializes writers before reading activation authority,
    /// validates the receipt transition and actor sequence, writes the completed
    /// receipt, writes the durable turn, advances the fence, then commits.
    pub fn commit_turn<F: EffectReceiptFence>(
        &self,
        namespace_actor_id: u64,
        fence: &F,
        record: DurableTurnRecord,
        receipt: EffectReceipt,
    ) -> Result<EffectReceiptFenceDecision, LibsqlDurableTurnError> {
        record.validate()?;
        if record.actor_id != namespace_actor_id {
            return Err(LibsqlDurableTurnError::CorruptState(format!(
                "turn actor {} does not match namespace actor {namespace_actor_id}",
                record.actor_id
            )));
        }
        if record.effect_invocation_id != receipt.invocation_id {
            return Err(LibsqlDurableTurnError::CorruptState(format!(
                "turn invocation {} does not match receipt {}",
                record.effect_invocation_id, receipt.invocation_id
            )));
        }

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

                let existing_effect =
                    load_effect_state(&tx, namespace_actor_id, receipt.invocation_id)
                        .await?
                        .ok_or(LibsqlDurableTurnError::MissingIntent(receipt.invocation_id))?;

                let next_effect = complete_effect_state(existing_effect, &receipt)?;
                let effect_json = serde_json::to_string(&next_effect)
                    .map_err(|error| LibsqlDurableTurnError::CorruptState(error.to_string()))?;
                let turn_json = serde_json::to_string(&record)
                    .map_err(|error| LibsqlDurableTurnError::CorruptState(error.to_string()))?;

                tx.execute(
                    "INSERT INTO effect_receipts (actor_id, invocation_id, state_json)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(actor_id, invocation_id)
                     DO UPDATE SET state_json = excluded.state_json",
                    libsql::params![
                        namespace_actor_id as i64,
                        receipt.invocation_id.to_string(),
                        effect_json
                    ],
                )
                .await
                .map_err(storage_error)?;

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

                tx.execute(
                    "INSERT INTO activation_fences (actor_id, epoch) VALUES (?1, ?2)
                     ON CONFLICT(actor_id) DO UPDATE SET epoch = excluded.epoch",
                    libsql::params![namespace_actor_id as i64, presented_epoch as i64],
                )
                .await
                .map_err(storage_error)?;

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

fn validate_json(field: &'static str, json: &str) -> Result<(), LibsqlDurableTurnError> {
    serde_json::from_str::<serde_json::Value>(json).map_err(|error| {
        LibsqlDurableTurnError::InvalidJson {
            field,
            error: error.to_string(),
        }
    })?;
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

fn persisted_invocation_id(state: &PersistedEffectState) -> EffectInvocationId {
    match state {
        PersistedEffectState::Intent(intent) => intent.invocation_id,
        PersistedEffectState::Completed { intent, .. } => intent.invocation_id,
    }
}

fn evaluate_fence(
    namespace_actor_id: u64,
    presented_actor_id: u64,
    presented_epoch: u64,
    current_epoch: Option<u64>,
) -> Result<EffectReceiptFenceDecision, LibsqlDurableTurnError> {
    if presented_epoch == 0 {
        return Err(FencedEffectReceiptError::ZeroEpoch {
            actor_id: presented_actor_id,
        }
        .into());
    }
    if presented_actor_id != namespace_actor_id {
        return Err(FencedEffectReceiptError::NamespaceMismatch {
            namespace_actor_id,
            presented_actor_id,
        }
        .into());
    }
    let Some(current_epoch) = current_epoch else {
        return Ok(EffectReceiptFenceDecision::Initialize {
            actor_id: namespace_actor_id,
            epoch: presented_epoch,
        });
    };
    if presented_epoch < current_epoch {
        return Err(FencedEffectReceiptError::StaleActivation {
            actor_id: namespace_actor_id,
            presented_epoch,
            current_epoch,
        }
        .into());
    }
    if presented_epoch == current_epoch {
        return Ok(EffectReceiptFenceDecision::Current {
            actor_id: namespace_actor_id,
            epoch: presented_epoch,
        });
    }
    Ok(EffectReceiptFenceDecision::Advance {
        actor_id: namespace_actor_id,
        previous_epoch: current_epoch,
        epoch: presented_epoch,
    })
}

async fn load_epoch(
    conn: &libsql::Connection,
    actor_id: u64,
) -> Result<Option<u64>, LibsqlDurableTurnError> {
    let mut rows = conn
        .query(
            "SELECT epoch FROM activation_fences WHERE actor_id = ?1",
            libsql::params![actor_id as i64],
        )
        .await
        .map_err(storage_error)?;
    let Some(row) = rows.next().await.map_err(storage_error)? else {
        return Ok(None);
    };
    let epoch: i64 = row.get(0).map_err(storage_error)?;
    if epoch <= 0 {
        return Err(LibsqlDurableTurnError::CorruptState(format!(
            "activation fence for actor {actor_id} contains epoch {epoch}"
        )));
    }
    Ok(Some(epoch as u64))
}

async fn load_effect_state(
    conn: &libsql::Connection,
    actor_id: u64,
    invocation_id: EffectInvocationId,
) -> Result<Option<PersistedEffectState>, LibsqlDurableTurnError> {
    let mut rows = conn
        .query(
            "SELECT state_json FROM effect_receipts
             WHERE actor_id = ?1 AND invocation_id = ?2",
            libsql::params![actor_id as i64, invocation_id.to_string()],
        )
        .await
        .map_err(storage_error)?;
    let Some(row) = rows.next().await.map_err(storage_error)? else {
        return Ok(None);
    };
    let state_json: String = row.get(0).map_err(storage_error)?;
    let state: PersistedEffectState = serde_json::from_str(&state_json)
        .map_err(|error| LibsqlDurableTurnError::CorruptState(error.to_string()))?;
    if persisted_invocation_id(&state) != invocation_id {
        return Err(LibsqlDurableTurnError::CorruptState(format!(
            "effect row key {invocation_id} does not match embedded state"
        )));
    }
    Ok(Some(state))
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
    use crate::effect_receipt::{EffectIdentity, EffectIntent, EffectSiteId, RequestFingerprint};
    use crate::effect_receipt_libsql::LibsqlEffectReceiptStore;

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

    fn fixture(actor_id: u64, slot: u32) -> EffectIntent {
        let site = EffectSiteId::from_semantic_bytes(b"tests.durable-turn.effect");
        let owner = format!("actor:{actor_id}");
        let invocation = EffectInvocationId::derive(owner.as_bytes(), 11, site, slot);
        EffectIntent::new(
            invocation,
            site,
            EffectIdentity::new("Http", "post").unwrap(),
            RequestFingerprint::from_canonical_bytes(format!("request-{slot}").as_bytes()),
            Some(invocation.provider_idempotency_key("test-http-v1")),
        )
    }

    fn turn(
        actor_id: u64,
        sequence: u64,
        invocation_id: EffectInvocationId,
        count: u64,
    ) -> DurableTurnRecord {
        DurableTurnRecord::new(
            actor_id,
            sequence,
            format!(r#"{{"count":{count}}}"#),
            format!(r#"{{"capacity":8,"committed":["message-{sequence}"]}}"#),
            invocation_id,
        )
        .unwrap()
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
    fn receipt_actor_state_inbox_and_fence_commit_together() {
        let actor_id = 42;
        let path = temp_db("durable-turn-atomic");
        let intent = fixture(actor_id, 0);
        let receipt = EffectReceipt::success(&intent, 1, b"provider-ok".to_vec());
        let fence = TestFence { actor_id, epoch: 1 };

        {
            let effects = LibsqlEffectReceiptStore::new(&path).unwrap();
            effects
                .create_intent_fenced(actor_id, &fence, intent.clone())
                .unwrap();
        }
        {
            let turns = LibsqlDurableTurnStore::new(&path).unwrap();
            let record = turn(actor_id, 12, intent.invocation_id, 1);
            assert_eq!(
                turns
                    .commit_turn(actor_id, &fence, record.clone(), receipt.clone())
                    .unwrap(),
                EffectReceiptFenceDecision::Current { actor_id, epoch: 1 }
            );
            assert_eq!(turns.load_turn(actor_id).unwrap(), Some(record));
        }
        {
            let effects = LibsqlEffectReceiptStore::new(&path).unwrap();
            assert_eq!(
                effects.decide(actor_id, &intent).unwrap(),
                crate::effect_receipt::EffectReplayDecision::ReturnReceipt(receipt)
            );
            assert_eq!(effects.accepted_epoch(actor_id).unwrap(), Some(1));
        }

        cleanup(&path);
    }

    #[test]
    fn stale_sequence_rolls_back_receipt_and_turn() {
        let actor_id = 42;
        let path = temp_db("durable-turn-sequence-rollback");
        let fence = TestFence { actor_id, epoch: 1 };
        let first = fixture(actor_id, 0);
        let second = fixture(actor_id, 1);

        {
            let effects = LibsqlEffectReceiptStore::new(&path).unwrap();
            effects
                .create_intent_fenced(actor_id, &fence, first.clone())
                .unwrap();
            effects
                .create_intent_fenced(actor_id, &fence, second.clone())
                .unwrap();
        }
        {
            let turns = LibsqlDurableTurnStore::new(&path).unwrap();
            turns
                .commit_turn(
                    actor_id,
                    &fence,
                    turn(actor_id, 20, first.invocation_id, 1),
                    EffectReceipt::success(&first, 1, b"first".to_vec()),
                )
                .unwrap();
            let error = turns
                .commit_turn(
                    actor_id,
                    &fence,
                    turn(actor_id, 19, second.invocation_id, 2),
                    EffectReceipt::success(&second, 1, b"second".to_vec()),
                )
                .unwrap_err();
            assert!(matches!(
                error,
                LibsqlDurableTurnError::StaleSequence { .. }
            ));
            assert_eq!(
                turns.load_turn(actor_id).unwrap().unwrap().actor_sequence,
                20
            );
        }
        {
            let effects = LibsqlEffectReceiptStore::new(&path).unwrap();
            assert!(matches!(
                effects.decide(actor_id, &second).unwrap(),
                crate::effect_receipt::EffectReplayDecision::RecoverIndeterminate(_)
            ));
        }

        cleanup(&path);
    }

    #[test]
    fn conflicting_same_sequence_rolls_back_receipt() {
        let actor_id = 42;
        let path = temp_db("durable-turn-conflict");
        let fence = TestFence { actor_id, epoch: 1 };
        let first = fixture(actor_id, 0);
        let second = fixture(actor_id, 1);

        {
            let effects = LibsqlEffectReceiptStore::new(&path).unwrap();
            effects
                .create_intent_fenced(actor_id, &fence, first.clone())
                .unwrap();
            effects
                .create_intent_fenced(actor_id, &fence, second.clone())
                .unwrap();
        }
        {
            let turns = LibsqlDurableTurnStore::new(&path).unwrap();
            turns
                .commit_turn(
                    actor_id,
                    &fence,
                    turn(actor_id, 30, first.invocation_id, 1),
                    EffectReceipt::success(&first, 1, b"first".to_vec()),
                )
                .unwrap();
            let error = turns
                .commit_turn(
                    actor_id,
                    &fence,
                    turn(actor_id, 30, second.invocation_id, 99),
                    EffectReceipt::success(&second, 1, b"second".to_vec()),
                )
                .unwrap_err();
            assert!(matches!(
                error,
                LibsqlDurableTurnError::ConflictingTurn { .. }
            ));
        }
        {
            let effects = LibsqlEffectReceiptStore::new(&path).unwrap();
            assert!(matches!(
                effects.decide(actor_id, &second).unwrap(),
                crate::effect_receipt::EffectReplayDecision::RecoverIndeterminate(_)
            ));
        }

        cleanup(&path);
    }

    #[test]
    fn stale_activation_cannot_publish_a_turn() {
        let actor_id = 42;
        let path = temp_db("durable-turn-fence");
        let old = TestFence { actor_id, epoch: 1 };
        let current = TestFence { actor_id, epoch: 2 };
        let intent = fixture(actor_id, 0);

        {
            let effects = LibsqlEffectReceiptStore::new(&path).unwrap();
            effects
                .create_intent_fenced(actor_id, &old, intent.clone())
                .unwrap();
            effects
                .create_intent_fenced(actor_id, &current, intent.clone())
                .unwrap();
        }
        {
            let turns = LibsqlDurableTurnStore::new(&path).unwrap();
            let error = turns
                .commit_turn(
                    actor_id,
                    &old,
                    turn(actor_id, 1, intent.invocation_id, 1),
                    EffectReceipt::success(&intent, 1, b"late".to_vec()),
                )
                .unwrap_err();
            assert!(matches!(
                error,
                LibsqlDurableTurnError::Fence(FencedEffectReceiptError::StaleActivation { .. })
            ));
            assert!(turns.load_turn(actor_id).unwrap().is_none());
        }

        cleanup(&path);
    }

    #[test]
    fn malformed_inbox_bundle_fails_before_storage() {
        let actor_id = 42;
        let invocation = fixture(actor_id, 0).invocation_id;
        let error = DurableTurnRecord::new(actor_id, 1, r#"{"count":1}"#, "not-json", invocation)
            .unwrap_err();
        assert!(matches!(
            error,
            LibsqlDurableTurnError::InvalidJson {
                field: "inbox bundle",
                ..
            }
        ));
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }
}
