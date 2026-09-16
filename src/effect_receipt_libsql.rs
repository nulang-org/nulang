//! libSQL persistence for activation-fenced durable effect receipts.
//!
//! RFC 0030 is still Experimental, so this adapter stays separate from the
//! general `PersistenceStore` trait. The important contract is already real:
//! activation-fence validation and the intent/receipt mutation commit in one
//! IMMEDIATE transaction.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::effect_receipt::{
    EffectIntent, EffectInvocationId, EffectReceipt, EffectReceiptError, EffectReplayDecision,
    InMemoryEffectReceiptStore, PersistedEffectState,
};
use crate::effect_receipt_fence::{
    EffectReceiptFence, EffectReceiptFenceDecision, EffectReceiptMutation, FencedEffectReceiptError,
};

pub struct LibsqlEffectReceiptStore {
    conn: std::sync::Mutex<libsql::Connection>,
    rt: tokio::runtime::Runtime,
    path: PathBuf,
}

#[derive(Debug)]
pub enum LibsqlEffectReceiptError {
    Storage(String),
    CorruptState(String),
    Fence(FencedEffectReceiptError),
}

impl fmt::Display for LibsqlEffectReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(message) => write!(f, "libSQL effect receipt storage error: {message}"),
            Self::CorruptState(message) => {
                write!(f, "corrupt persisted effect receipt state: {message}")
            }
            Self::Fence(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for LibsqlEffectReceiptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fence(error) => Some(error),
            _ => None,
        }
    }
}

impl From<FencedEffectReceiptError> for LibsqlEffectReceiptError {
    fn from(value: FencedEffectReceiptError) -> Self {
        Self::Fence(value)
    }
}

impl From<EffectReceiptError> for LibsqlEffectReceiptError {
    fn from(value: EffectReceiptError) -> Self {
        Self::Fence(FencedEffectReceiptError::Receipt(value))
    }
}

impl From<LibsqlEffectReceiptError> for io::Error {
    fn from(value: LibsqlEffectReceiptError) -> Self {
        io::Error::new(io::ErrorKind::Other, value)
    }
}

impl LibsqlEffectReceiptStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, LibsqlEffectReceiptError> {
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

    pub fn in_memory() -> Result<Self, LibsqlEffectReceiptError> {
        Self::new(":memory:")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, libsql::Connection> {
        self.conn.lock().unwrap()
    }

    fn ensure_tables(&self) -> Result<(), LibsqlEffectReceiptError> {
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
            Ok(())
        })
    }

    pub fn accepted_epoch(&self, actor_id: u64) -> Result<Option<u64>, LibsqlEffectReceiptError> {
        let conn = self.conn();
        self.rt
            .block_on(async { load_epoch(&conn, actor_id).await })
    }

    pub fn load(
        &self,
        actor_id: u64,
        invocation_id: EffectInvocationId,
    ) -> Result<Option<PersistedEffectState>, LibsqlEffectReceiptError> {
        let conn = self.conn();
        self.rt
            .block_on(async { load_state(&conn, actor_id, invocation_id).await })
    }

    pub fn decide(
        &self,
        actor_id: u64,
        proposed: &EffectIntent,
    ) -> Result<EffectReplayDecision, LibsqlEffectReceiptError> {
        let existing = self.load(actor_id, proposed.invocation_id)?;
        let reference = restore_reference_store(existing, proposed.invocation_id)?;
        Ok(reference.decide(proposed)?)
    }

    /// Atomically validates activation authority and commits one intent/receipt
    /// mutation. `BEGIN IMMEDIATE` semantics serialize writers before the fence
    /// is read, so another connection cannot advance authority between the
    /// check and commit.
    pub fn apply_fenced<F: EffectReceiptFence>(
        &self,
        namespace_actor_id: u64,
        fence: &F,
        mutation: EffectReceiptMutation,
    ) -> Result<EffectReceiptFenceDecision, LibsqlEffectReceiptError> {
        let invocation_id = mutation_invocation_id(&mutation);
        let presented_actor_id = fence.actor_id();
        let presented_epoch = fence.epoch();
        let conn = self.conn();

        self.rt.block_on(async {
            let tx = conn
                .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
                .await
                .map_err(storage_error)?;

            let current_epoch = match load_epoch(&tx, namespace_actor_id).await {
                Ok(epoch) => epoch,
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(error);
                }
            };
            let decision = match evaluate_fence(
                namespace_actor_id,
                presented_actor_id,
                presented_epoch,
                current_epoch,
            ) {
                Ok(decision) => decision,
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(error.into());
                }
            };

            let existing = match load_state(&tx, namespace_actor_id, invocation_id).await {
                Ok(state) => state,
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(error);
                }
            };
            let mut reference = match restore_reference_store(existing, invocation_id) {
                Ok(store) => store,
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(error);
                }
            };

            let mutation_result = match mutation {
                EffectReceiptMutation::CreateIntent(intent) => reference.create_intent(intent),
                EffectReceiptMutation::CommitReceipt(receipt) => reference.commit_receipt(receipt),
            };
            if let Err(error) = mutation_result {
                let _ = tx.rollback().await;
                return Err(error.into());
            }

            let Some(next_state) = reference.load(invocation_id).cloned() else {
                let _ = tx.rollback().await;
                return Err(LibsqlEffectReceiptError::CorruptState(format!(
                    "receipt mutation for invocation {invocation_id} produced no state"
                )));
            };
            let state_json = match serde_json::to_string(&next_state) {
                Ok(json) => json,
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(LibsqlEffectReceiptError::CorruptState(error.to_string()));
                }
            };

            if let Err(error) = tx
                .execute(
                    "INSERT INTO effect_receipts (actor_id, invocation_id, state_json)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(actor_id, invocation_id)
                     DO UPDATE SET state_json = excluded.state_json",
                    libsql::params![
                        namespace_actor_id as i64,
                        invocation_id.to_string(),
                        state_json
                    ],
                )
                .await
            {
                let _ = tx.rollback().await;
                return Err(storage_error(error));
            }

            if let Err(error) = tx
                .execute(
                    "INSERT INTO activation_fences (actor_id, epoch) VALUES (?1, ?2)
                     ON CONFLICT(actor_id) DO UPDATE SET epoch = excluded.epoch",
                    libsql::params![namespace_actor_id as i64, presented_epoch as i64],
                )
                .await
            {
                let _ = tx.rollback().await;
                return Err(storage_error(error));
            }

            tx.commit().await.map_err(storage_error)?;
            Ok(decision)
        })
    }

    pub fn create_intent_fenced<F: EffectReceiptFence>(
        &self,
        namespace_actor_id: u64,
        fence: &F,
        intent: EffectIntent,
    ) -> Result<EffectReceiptFenceDecision, LibsqlEffectReceiptError> {
        self.apply_fenced(
            namespace_actor_id,
            fence,
            EffectReceiptMutation::CreateIntent(intent),
        )
    }

    pub fn commit_receipt_fenced<F: EffectReceiptFence>(
        &self,
        namespace_actor_id: u64,
        fence: &F,
        receipt: EffectReceipt,
    ) -> Result<EffectReceiptFenceDecision, LibsqlEffectReceiptError> {
        self.apply_fenced(
            namespace_actor_id,
            fence,
            EffectReceiptMutation::CommitReceipt(receipt),
        )
    }
}

fn mutation_invocation_id(mutation: &EffectReceiptMutation) -> EffectInvocationId {
    match mutation {
        EffectReceiptMutation::CreateIntent(intent) => intent.invocation_id,
        EffectReceiptMutation::CommitReceipt(receipt) => receipt.invocation_id,
    }
}

fn persisted_invocation_id(state: &PersistedEffectState) -> EffectInvocationId {
    match state {
        PersistedEffectState::Intent(intent) => intent.invocation_id,
        PersistedEffectState::Completed { intent, .. } => intent.invocation_id,
    }
}

fn restore_reference_store(
    existing: Option<PersistedEffectState>,
    expected_invocation_id: EffectInvocationId,
) -> Result<InMemoryEffectReceiptStore, LibsqlEffectReceiptError> {
    let mut store = InMemoryEffectReceiptStore::default();
    let Some(state) = existing else {
        return Ok(store);
    };
    let persisted_id = persisted_invocation_id(&state);
    if persisted_id != expected_invocation_id {
        return Err(LibsqlEffectReceiptError::CorruptState(format!(
            "row key invocation {expected_invocation_id} contains state for {persisted_id}"
        )));
    }
    match state {
        PersistedEffectState::Intent(intent) => store.create_intent(intent)?,
        PersistedEffectState::Completed { intent, receipt } => {
            store.create_intent(intent)?;
            store.commit_receipt(receipt)?;
        }
    }
    Ok(store)
}

fn evaluate_fence(
    namespace_actor_id: u64,
    presented_actor_id: u64,
    presented_epoch: u64,
    current_epoch: Option<u64>,
) -> Result<EffectReceiptFenceDecision, FencedEffectReceiptError> {
    if presented_epoch == 0 {
        return Err(FencedEffectReceiptError::ZeroEpoch {
            actor_id: presented_actor_id,
        });
    }
    if presented_actor_id != namespace_actor_id {
        return Err(FencedEffectReceiptError::NamespaceMismatch {
            namespace_actor_id,
            presented_actor_id,
        });
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
        });
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
) -> Result<Option<u64>, LibsqlEffectReceiptError> {
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
        return Err(LibsqlEffectReceiptError::CorruptState(format!(
            "activation fence for actor {actor_id} contains epoch {epoch}"
        )));
    }
    Ok(Some(epoch as u64))
}

async fn load_state(
    conn: &libsql::Connection,
    actor_id: u64,
    invocation_id: EffectInvocationId,
) -> Result<Option<PersistedEffectState>, LibsqlEffectReceiptError> {
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
        .map_err(|error| LibsqlEffectReceiptError::CorruptState(error.to_string()))?;
    if persisted_invocation_id(&state) != invocation_id {
        return Err(LibsqlEffectReceiptError::CorruptState(format!(
            "row key invocation {invocation_id} does not match embedded state"
        )));
    }
    Ok(Some(state))
}

fn storage_error(error: impl fmt::Display) -> LibsqlEffectReceiptError {
    LibsqlEffectReceiptError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect_receipt::{EffectIdentity, EffectSiteId, RequestFingerprint};

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

    fn fixture(actor_id: u64) -> EffectIntent {
        let site = EffectSiteId::from_semantic_bytes(b"orders.Charge.capture#0");
        let owner = format!("actor:{actor_id}");
        let invocation = EffectInvocationId::derive(owner.as_bytes(), 7, site, 0);
        let identity = EffectIdentity::new("Payments", "charge").unwrap();
        let fingerprint = RequestFingerprint::from_canonical_bytes(b"order=42&amount=1000");
        let provider_key = Some(invocation.provider_idempotency_key("test-payments-v1"));
        EffectIntent::new(invocation, site, identity, fingerprint, provider_key)
    }

    #[test]
    fn first_write_persists_intent_and_epoch() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let fence = TestFence { actor_id, epoch: 1 };
        let store = LibsqlEffectReceiptStore::in_memory().unwrap();

        assert_eq!(
            store
                .create_intent_fenced(actor_id, &fence, intent.clone())
                .unwrap(),
            EffectReceiptFenceDecision::Initialize { actor_id, epoch: 1 }
        );
        assert_eq!(store.accepted_epoch(actor_id).unwrap(), Some(1));
        assert_eq!(
            store.decide(actor_id, &intent).unwrap(),
            EffectReplayDecision::RecoverIndeterminate(intent)
        );
    }

    #[test]
    fn terminal_receipt_round_trips() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let receipt = EffectReceipt::success(&intent, 1, b"provider-ok".to_vec());
        let fence = TestFence { actor_id, epoch: 3 };
        let store = LibsqlEffectReceiptStore::in_memory().unwrap();

        store
            .create_intent_fenced(actor_id, &fence, intent.clone())
            .unwrap();
        store
            .commit_receipt_fenced(actor_id, &fence, receipt.clone())
            .unwrap();
        assert_eq!(
            store.decide(actor_id, &intent).unwrap(),
            EffectReplayDecision::ReturnReceipt(receipt)
        );
    }

    #[test]
    fn stale_writer_is_rejected_after_failover() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let old = TestFence { actor_id, epoch: 1 };
        let current = TestFence { actor_id, epoch: 2 };
        let store = LibsqlEffectReceiptStore::in_memory().unwrap();

        store
            .create_intent_fenced(actor_id, &old, intent.clone())
            .unwrap();
        store
            .create_intent_fenced(actor_id, &current, intent.clone())
            .unwrap();
        let stale_receipt = EffectReceipt::success(&intent, 1, b"late-old".to_vec());

        assert!(matches!(
            store.commit_receipt_fenced(actor_id, &old, stale_receipt),
            Err(LibsqlEffectReceiptError::Fence(
                FencedEffectReceiptError::StaleActivation {
                    actor_id: 42,
                    presented_epoch: 1,
                    current_epoch: 2,
                }
            ))
        ));
        assert_eq!(store.accepted_epoch(actor_id).unwrap(), Some(2));
    }

    #[test]
    fn failed_higher_epoch_mutation_does_not_advance_fence() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let epoch_one = TestFence { actor_id, epoch: 1 };
        let epoch_two = TestFence { actor_id, epoch: 2 };
        let store = LibsqlEffectReceiptStore::in_memory().unwrap();

        store
            .create_intent_fenced(actor_id, &epoch_one, intent.clone())
            .unwrap();
        let other = fixture(99);
        let invalid_receipt = EffectReceipt::success(&other, 1, b"wrong".to_vec());
        assert!(store
            .commit_receipt_fenced(actor_id, &epoch_two, invalid_receipt)
            .is_err());
        assert_eq!(store.accepted_epoch(actor_id).unwrap(), Some(1));
        assert_eq!(
            store.decide(actor_id, &intent).unwrap(),
            EffectReplayDecision::RecoverIndeterminate(intent)
        );
    }

    #[test]
    fn file_store_reopens_with_receipt_and_epoch() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let receipt = EffectReceipt::success(&intent, 1, b"provider-ok".to_vec());
        let fence = TestFence { actor_id, epoch: 4 };
        let path = std::env::temp_dir().join(format!(
            "nulang-effect-receipts-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        {
            let store = LibsqlEffectReceiptStore::new(&path).unwrap();
            store
                .create_intent_fenced(actor_id, &fence, intent.clone())
                .unwrap();
            store
                .commit_receipt_fenced(actor_id, &fence, receipt.clone())
                .unwrap();
        }
        {
            let reopened = LibsqlEffectReceiptStore::new(&path).unwrap();
            assert_eq!(reopened.accepted_epoch(actor_id).unwrap(), Some(4));
            assert_eq!(
                reopened.decide(actor_id, &intent).unwrap(),
                EffectReplayDecision::ReturnReceipt(receipt)
            );
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }
}
