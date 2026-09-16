//! Race-safe libSQL preparation for durable actor effects.
//!
//! This module is the v2 convergence layer for RFC 0030. It keeps the existing
//! receipt state machine intact while adding two invariants needed by durable
//! actor turns:
//!
//! - the durable actor/turn/occurrence context is persisted and verifiable;
//! - replay decision + intent creation/re-observation + activation fencing happen
//!   in one IMMEDIATE transaction before provider execution.
//!
//! The existing `effect_receipt_libsql` adapter remains available for standalone
//! effects while this path hardens. Durable actor integrations should use this
//! module.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::effect_receipt::{
    EffectIntent, EffectInvocationId, EffectReceipt, EffectReceiptError, EffectReplayDecision,
    InMemoryEffectReceiptStore, PersistedEffectState,
};
use crate::effect_receipt_fence::{
    EffectReceiptFence, EffectReceiptFenceDecision, FencedEffectReceiptError,
};

/// Explicit semantic context for one durable actor effect occurrence.
///
/// `EffectInvocationId` remains the compact canonical identity. Persisting the
/// context lets storage verify that the invocation being committed actually
/// belongs to the actor turn that is becoming authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DurableEffectContext {
    pub actor_id: u64,
    pub durable_sequence: u64,
    pub occurrence_index: u32,
}

impl DurableEffectContext {
    pub const fn new(actor_id: u64, durable_sequence: u64, occurrence_index: u32) -> Self {
        Self {
            actor_id,
            durable_sequence,
            occurrence_index,
        }
    }

    pub fn validate_against(
        self,
        namespace_actor_id: u64,
        intent: &EffectIntent,
    ) -> Result<(), LibsqlDurableEffectError> {
        if self.actor_id != namespace_actor_id {
            return Err(LibsqlDurableEffectError::ContextMismatch {
                invocation_id: intent.invocation_id,
                detail: format!(
                    "context actor {} does not match namespace actor {namespace_actor_id}",
                    self.actor_id
                ),
            });
        }
        if self.durable_sequence == 0 {
            return Err(LibsqlDurableEffectError::ContextMismatch {
                invocation_id: intent.invocation_id,
                detail: "durable sequence must be positive".to_string(),
            });
        }

        // Durable actor effects currently use the stable actor owner encoding
        // already used throughout the receipt tests and adapters. When nominal
        // durable owner ids land, this derivation should consume that canonical
        // owner identity instead of formatting it here.
        let owner = format!("actor:{}", self.actor_id);
        let expected = EffectInvocationId::derive(
            owner.as_bytes(),
            self.durable_sequence,
            intent.site_id,
            self.occurrence_index,
        );
        if expected != intent.invocation_id {
            return Err(LibsqlDurableEffectError::ContextMismatch {
                invocation_id: intent.invocation_id,
                detail: format!(
                    "invocation does not derive from actor {}, turn {}, occurrence {}",
                    self.actor_id, self.durable_sequence, self.occurrence_index
                ),
            });
        }
        Ok(())
    }
}

pub struct LibsqlDurableEffectStore {
    conn: std::sync::Mutex<libsql::Connection>,
    rt: tokio::runtime::Runtime,
    path: PathBuf,
}

#[derive(Debug)]
pub enum LibsqlDurableEffectError {
    Storage(String),
    CorruptState(String),
    ContextMismatch {
        invocation_id: EffectInvocationId,
        detail: String,
    },
    MissingContext(EffectInvocationId),
    ConflictingContext(EffectInvocationId),
    Fence(FencedEffectReceiptError),
    Receipt(EffectReceiptError),
}

impl fmt::Display for LibsqlDurableEffectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(message) => write!(f, "libSQL durable effect storage error: {message}"),
            Self::CorruptState(message) => write!(f, "corrupt durable effect state: {message}"),
            Self::ContextMismatch {
                invocation_id,
                detail,
            } => write!(
                f,
                "durable effect context mismatch for {invocation_id}: {detail}"
            ),
            Self::MissingContext(id) => {
                write!(f, "missing durable effect context for invocation {id}")
            }
            Self::ConflictingContext(id) => {
                write!(f, "conflicting durable effect context for invocation {id}")
            }
            Self::Fence(error) => write!(f, "{error}"),
            Self::Receipt(error) => write!(f, "effect receipt rejected: {error}"),
        }
    }
}

impl std::error::Error for LibsqlDurableEffectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fence(error) => Some(error),
            Self::Receipt(error) => Some(error),
            _ => None,
        }
    }
}

impl From<FencedEffectReceiptError> for LibsqlDurableEffectError {
    fn from(value: FencedEffectReceiptError) -> Self {
        Self::Fence(value)
    }
}

impl From<EffectReceiptError> for LibsqlDurableEffectError {
    fn from(value: EffectReceiptError) -> Self {
        Self::Receipt(value)
    }
}

impl LibsqlDurableEffectStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, LibsqlDurableEffectError> {
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

    pub fn in_memory() -> Result<Self, LibsqlDurableEffectError> {
        Self::new(":memory:")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, libsql::Connection> {
        self.conn.lock().unwrap()
    }

    fn ensure_tables(&self) -> Result<(), LibsqlDurableEffectError> {
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
            Ok(())
        })
    }

    pub fn accepted_epoch(&self, actor_id: u64) -> Result<Option<u64>, LibsqlDurableEffectError> {
        let conn = self.conn();
        self.rt
            .block_on(async { load_epoch(&conn, actor_id).await })
    }

    pub fn load(
        &self,
        actor_id: u64,
        invocation_id: EffectInvocationId,
    ) -> Result<Option<PersistedEffectState>, LibsqlDurableEffectError> {
        let conn = self.conn();
        self.rt
            .block_on(async { load_state(&conn, actor_id, invocation_id).await })
    }

    pub fn load_context(
        &self,
        actor_id: u64,
        invocation_id: EffectInvocationId,
    ) -> Result<Option<DurableEffectContext>, LibsqlDurableEffectError> {
        let conn = self.conn();
        self.rt
            .block_on(async { load_context(&conn, actor_id, invocation_id).await })
    }

    /// Atomically decide what to do at a durable effect site and establish the
    /// presented activation as authoritative before provider execution.
    ///
    /// This replaces the unsafe `decide(); create_intent_fenced()` split for
    /// durable actor effects. The receipt state, context, and activation fence
    /// are observed/updated under one IMMEDIATE transaction.
    pub fn prepare_fenced<F: EffectReceiptFence>(
        &self,
        namespace_actor_id: u64,
        fence: &F,
        proposed: &EffectIntent,
        context: DurableEffectContext,
    ) -> Result<EffectReplayDecision, LibsqlDurableEffectError> {
        proposed.validate()?;
        context.validate_against(namespace_actor_id, proposed)?;

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
                let _decision = evaluate_fence(
                    namespace_actor_id,
                    presented_actor_id,
                    presented_epoch,
                    current_epoch,
                )?;

                let existing = load_state(&tx, namespace_actor_id, proposed.invocation_id).await?;
                let mut reference = restore_reference_store(existing, proposed.invocation_id)?;
                let replay = reference.decide(proposed)?;

                if matches!(replay, EffectReplayDecision::ExecuteNew) {
                    reference.create_intent(proposed.clone())?;
                    let next =
                        reference
                            .load(proposed.invocation_id)
                            .cloned()
                            .ok_or_else(|| {
                                LibsqlDurableEffectError::CorruptState(format!(
                                    "preparing invocation {} produced no receipt state",
                                    proposed.invocation_id
                                ))
                            })?;
                    write_state(&tx, namespace_actor_id, proposed.invocation_id, &next).await?;
                }

                ensure_context(&tx, namespace_actor_id, proposed.invocation_id, context).await?;
                write_epoch(&tx, namespace_actor_id, presented_epoch).await?;
                Ok::<_, LibsqlDurableEffectError>(replay)
            }
            .await;

            match result {
                Ok(replay) => {
                    tx.commit().await.map_err(storage_error)?;
                    Ok(replay)
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    Err(error)
                }
            }
        })
    }

    /// Commit one terminal receipt independently of actor state.
    ///
    /// Durable actor turns should normally commit receipts through
    /// `durable_turn_libsql_v2` instead so state + dedup + all receipts share one
    /// transaction. This method remains useful for standalone durable effects.
    pub fn commit_receipt_fenced<F: EffectReceiptFence>(
        &self,
        namespace_actor_id: u64,
        fence: &F,
        receipt: EffectReceipt,
    ) -> Result<EffectReceiptFenceDecision, LibsqlDurableEffectError> {
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
                let existing = load_state(&tx, namespace_actor_id, receipt.invocation_id)
                    .await?
                    .ok_or(EffectReceiptError::MissingIntent(receipt.invocation_id))?;
                let mut reference = restore_reference_store(Some(existing), receipt.invocation_id)?;
                reference.commit_receipt(receipt.clone())?;
                let next = reference
                    .load(receipt.invocation_id)
                    .cloned()
                    .ok_or_else(|| {
                        LibsqlDurableEffectError::CorruptState(format!(
                            "committing invocation {} produced no state",
                            receipt.invocation_id
                        ))
                    })?;
                write_state(&tx, namespace_actor_id, receipt.invocation_id, &next).await?;
                write_epoch(&tx, namespace_actor_id, presented_epoch).await?;
                Ok::<_, LibsqlDurableEffectError>(decision)
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

pub(crate) async fn load_epoch(
    conn: &libsql::Connection,
    actor_id: u64,
) -> Result<Option<u64>, LibsqlDurableEffectError> {
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
        return Err(LibsqlDurableEffectError::CorruptState(format!(
            "activation fence for actor {actor_id} contains epoch {epoch}"
        )));
    }
    Ok(Some(epoch as u64))
}

pub(crate) async fn load_state(
    conn: &libsql::Connection,
    actor_id: u64,
    invocation_id: EffectInvocationId,
) -> Result<Option<PersistedEffectState>, LibsqlDurableEffectError> {
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
        .map_err(|error| LibsqlDurableEffectError::CorruptState(error.to_string()))?;
    if persisted_invocation_id(&state) != invocation_id {
        return Err(LibsqlDurableEffectError::CorruptState(format!(
            "row key invocation {invocation_id} does not match embedded state"
        )));
    }
    Ok(Some(state))
}

pub(crate) async fn load_context(
    conn: &libsql::Connection,
    actor_id: u64,
    invocation_id: EffectInvocationId,
) -> Result<Option<DurableEffectContext>, LibsqlDurableEffectError> {
    let mut rows = conn
        .query(
            "SELECT durable_sequence, occurrence_index FROM effect_contexts
             WHERE actor_id = ?1 AND invocation_id = ?2",
            libsql::params![actor_id as i64, invocation_id.to_string()],
        )
        .await
        .map_err(storage_error)?;
    let Some(row) = rows.next().await.map_err(storage_error)? else {
        return Ok(None);
    };
    let durable_sequence: i64 = row.get(0).map_err(storage_error)?;
    let occurrence_index: i64 = row.get(1).map_err(storage_error)?;
    if durable_sequence <= 0 || occurrence_index < 0 || occurrence_index > u32::MAX as i64 {
        return Err(LibsqlDurableEffectError::CorruptState(format!(
            "invalid durable effect context for actor {actor_id}, invocation {invocation_id}"
        )));
    }
    Ok(Some(DurableEffectContext {
        actor_id,
        durable_sequence: durable_sequence as u64,
        occurrence_index: occurrence_index as u32,
    }))
}

pub(crate) async fn write_state(
    conn: &libsql::Connection,
    actor_id: u64,
    invocation_id: EffectInvocationId,
    state: &PersistedEffectState,
) -> Result<(), LibsqlDurableEffectError> {
    let state_json = serde_json::to_string(state)
        .map_err(|error| LibsqlDurableEffectError::CorruptState(error.to_string()))?;
    conn.execute(
        "INSERT INTO effect_receipts (actor_id, invocation_id, state_json)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(actor_id, invocation_id)
         DO UPDATE SET state_json = excluded.state_json",
        libsql::params![actor_id as i64, invocation_id.to_string(), state_json],
    )
    .await
    .map_err(storage_error)?;
    Ok(())
}

pub(crate) async fn write_epoch(
    conn: &libsql::Connection,
    actor_id: u64,
    epoch: u64,
) -> Result<(), LibsqlDurableEffectError> {
    conn.execute(
        "INSERT INTO activation_fences (actor_id, epoch) VALUES (?1, ?2)
         ON CONFLICT(actor_id) DO UPDATE SET epoch = excluded.epoch",
        libsql::params![actor_id as i64, epoch as i64],
    )
    .await
    .map_err(storage_error)?;
    Ok(())
}

async fn ensure_context(
    conn: &libsql::Connection,
    actor_id: u64,
    invocation_id: EffectInvocationId,
    proposed: DurableEffectContext,
) -> Result<(), LibsqlDurableEffectError> {
    if let Some(existing) = load_context(conn, actor_id, invocation_id).await? {
        if existing != proposed {
            return Err(LibsqlDurableEffectError::ConflictingContext(invocation_id));
        }
        return Ok(());
    }
    conn.execute(
        "INSERT INTO effect_contexts
         (actor_id, invocation_id, durable_sequence, occurrence_index)
         VALUES (?1, ?2, ?3, ?4)",
        libsql::params![
            actor_id as i64,
            invocation_id.to_string(),
            proposed.durable_sequence as i64,
            proposed.occurrence_index as i64
        ],
    )
    .await
    .map_err(storage_error)?;
    Ok(())
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
) -> Result<InMemoryEffectReceiptStore, LibsqlDurableEffectError> {
    let mut store = InMemoryEffectReceiptStore::default();
    let Some(state) = existing else {
        return Ok(store);
    };
    let persisted_id = persisted_invocation_id(&state);
    if persisted_id != expected_invocation_id {
        return Err(LibsqlDurableEffectError::CorruptState(format!(
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

pub(crate) fn evaluate_fence(
    namespace_actor_id: u64,
    presented_actor_id: u64,
    presented_epoch: u64,
    current_epoch: Option<u64>,
) -> Result<EffectReceiptFenceDecision, LibsqlDurableEffectError> {
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

fn storage_error(error: impl fmt::Display) -> LibsqlDurableEffectError {
    LibsqlDurableEffectError::Storage(error.to_string())
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

    fn fixture(
        actor_id: u64,
        sequence: u64,
        occurrence: u32,
    ) -> (EffectIntent, DurableEffectContext) {
        let site = EffectSiteId::from_semantic_bytes(b"orders.Charge.capture#0");
        let owner = format!("actor:{actor_id}");
        let invocation = EffectInvocationId::derive(owner.as_bytes(), sequence, site, occurrence);
        let intent = EffectIntent::new(
            invocation,
            site,
            EffectIdentity::new("Payments", "charge").unwrap(),
            RequestFingerprint::from_canonical_bytes(b"order=42&amount=1000"),
            Some(invocation.provider_idempotency_key("test-payments-v1")),
        );
        (
            intent,
            DurableEffectContext::new(actor_id, sequence, occurrence),
        )
    }

    #[test]
    fn prepare_persists_intent_context_and_epoch_atomically() {
        let actor_id = 42;
        let (intent, context) = fixture(actor_id, 7, 0);
        let fence = TestFence { actor_id, epoch: 1 };
        let store = LibsqlDurableEffectStore::in_memory().unwrap();

        assert_eq!(
            store
                .prepare_fenced(actor_id, &fence, &intent, context)
                .unwrap(),
            EffectReplayDecision::ExecuteNew
        );
        assert_eq!(store.accepted_epoch(actor_id).unwrap(), Some(1));
        assert_eq!(
            store.load_context(actor_id, intent.invocation_id).unwrap(),
            Some(context)
        );
        assert!(matches!(
            store.load(actor_id, intent.invocation_id).unwrap(),
            Some(PersistedEffectState::Intent(_))
        ));
    }

    #[test]
    fn repeated_prepare_observes_indeterminate_without_reallocating_identity() {
        let actor_id = 42;
        let (intent, context) = fixture(actor_id, 7, 0);
        let fence = TestFence { actor_id, epoch: 1 };
        let store = LibsqlDurableEffectStore::in_memory().unwrap();
        store
            .prepare_fenced(actor_id, &fence, &intent, context)
            .unwrap();

        assert!(matches!(
            store
                .prepare_fenced(actor_id, &fence, &intent, context)
                .unwrap(),
            EffectReplayDecision::RecoverIndeterminate(_)
        ));
    }

    #[test]
    fn completed_receipt_is_returned_by_atomic_prepare() {
        let actor_id = 42;
        let (intent, context) = fixture(actor_id, 7, 0);
        let fence = TestFence { actor_id, epoch: 1 };
        let store = LibsqlDurableEffectStore::in_memory().unwrap();
        store
            .prepare_fenced(actor_id, &fence, &intent, context)
            .unwrap();
        let receipt = EffectReceipt::success(&intent, 1, b"ok".to_vec());
        store
            .commit_receipt_fenced(actor_id, &fence, receipt.clone())
            .unwrap();

        assert_eq!(
            store
                .prepare_fenced(actor_id, &fence, &intent, context)
                .unwrap(),
            EffectReplayDecision::ReturnReceipt(receipt)
        );
    }

    #[test]
    fn mismatched_context_fails_before_provider_eligibility() {
        let actor_id = 42;
        let (intent, _) = fixture(actor_id, 7, 0);
        let fence = TestFence { actor_id, epoch: 1 };
        let store = LibsqlDurableEffectStore::in_memory().unwrap();
        let wrong = DurableEffectContext::new(actor_id, 8, 0);

        assert!(matches!(
            store.prepare_fenced(actor_id, &fence, &intent, wrong),
            Err(LibsqlDurableEffectError::ContextMismatch { .. })
        ));
        assert_eq!(store.accepted_epoch(actor_id).unwrap(), None);
    }

    #[test]
    fn higher_epoch_prepare_fences_out_old_activation() {
        let actor_id = 42;
        let (intent, context) = fixture(actor_id, 7, 0);
        let old = TestFence { actor_id, epoch: 1 };
        let current = TestFence { actor_id, epoch: 2 };
        let store = LibsqlDurableEffectStore::in_memory().unwrap();
        store
            .prepare_fenced(actor_id, &old, &intent, context)
            .unwrap();
        store
            .prepare_fenced(actor_id, &current, &intent, context)
            .unwrap();

        let stale_receipt = EffectReceipt::success(&intent, 1, b"late".to_vec());
        assert!(matches!(
            store.commit_receipt_fenced(actor_id, &old, stale_receipt),
            Err(LibsqlDurableEffectError::Fence(
                FencedEffectReceiptError::StaleActivation { .. }
            ))
        ));
        assert_eq!(store.accepted_epoch(actor_id).unwrap(), Some(2));
    }
}
