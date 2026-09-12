//! Durable SQLite task reservations for crash-safe, multi-scheduler assignment.
//!
//! Reservations are acquired under `BEGIN IMMEDIATE`, so competing scheduler
//! processes serialize the capacity check and claim. Each lease carries an
//! opaque token plus a monotonically increasing version for CAS-style renewals
//! and releases.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskReservation {
    pub task_id: Uuid,
    pub agent_id: String,
    pub lease_token: Uuid,
    pub lease_expires_at_ms: i64,
    pub version: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ReservationError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("task {task_id} is already reserved by {agent_id} until {lease_expires_at_ms}")]
    TaskReserved {
        task_id: Uuid,
        agent_id: String,
        lease_expires_at_ms: i64,
    },
    #[error("worker {agent_id} has no remaining reservation capacity")]
    WorkerCapacityExhausted { agent_id: String },
    #[error("reservation lease was lost for task {task_id}")]
    ReservationLost { task_id: Uuid },
}

pub struct TaskReservationStore {
    path: PathBuf,
}

impl TaskReservationStore {
    pub fn open(path: &Path) -> Result<Self, ReservationError> {
        let store = Self {
            path: path.to_path_buf(),
        };
        let conn = Connection::open(&store.path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS task_reservations (
                task_id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                lease_token TEXT NOT NULL,
                lease_expires_at_ms INTEGER NOT NULL,
                version INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_task_reservations_agent_lease
                ON task_reservations(agent_id, lease_expires_at_ms);
            "#,
        )?;
        Ok(store)
    }

    pub fn reserve_with_capacity(
        &self,
        task_id: Uuid,
        agent_id: &str,
        max_concurrency: usize,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<TaskReservation, ReservationError> {
        let mut conn = Connection::open(&self.path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM task_reservations WHERE lease_expires_at_ms <= ?1",
            params![now_ms],
        )?;

        if let Some((owner, expires_at)) = tx
            .query_row(
                "SELECT agent_id, lease_expires_at_ms FROM task_reservations WHERE task_id = ?1",
                params![task_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
        {
            return Err(ReservationError::TaskReserved {
                task_id,
                agent_id: owner,
                lease_expires_at_ms: expires_at,
            });
        }

        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM task_reservations WHERE agent_id = ?1 AND lease_expires_at_ms > ?2",
            params![agent_id, now_ms],
            |row| row.get(0),
        )?;
        if active >= max_concurrency.max(1) as i64 {
            return Err(ReservationError::WorkerCapacityExhausted {
                agent_id: agent_id.to_string(),
            });
        }

        let lease_token = Uuid::new_v4();
        let lease_expires_at_ms = now_ms.saturating_add(lease_duration_ms.max(1));
        tx.execute(
            r#"INSERT INTO task_reservations (
                task_id, agent_id, lease_token, lease_expires_at_ms, version, updated_at_ms
            ) VALUES (?1, ?2, ?3, ?4, 1, ?5)"#,
            params![
                task_id.to_string(),
                agent_id,
                lease_token.to_string(),
                lease_expires_at_ms,
                now_ms,
            ],
        )?;
        tx.commit()?;

        Ok(TaskReservation {
            task_id,
            agent_id: agent_id.to_string(),
            lease_token,
            lease_expires_at_ms,
            version: 1,
        })
    }

    pub fn renew(
        &self,
        reservation: &TaskReservation,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<TaskReservation, ReservationError> {
        let conn = Connection::open(&self.path)?;
        let lease_expires_at_ms = now_ms.saturating_add(lease_duration_ms.max(1));
        let changed = conn.execute(
            r#"UPDATE task_reservations
            SET lease_expires_at_ms = ?1, version = version + 1, updated_at_ms = ?2
            WHERE task_id = ?3 AND lease_token = ?4 AND version = ?5 AND lease_expires_at_ms > ?2"#,
            params![
                lease_expires_at_ms,
                now_ms,
                reservation.task_id.to_string(),
                reservation.lease_token.to_string(),
                reservation.version,
            ],
        )?;
        if changed != 1 {
            return Err(ReservationError::ReservationLost {
                task_id: reservation.task_id,
            });
        }

        let mut renewed = reservation.clone();
        renewed.lease_expires_at_ms = lease_expires_at_ms;
        renewed.version += 1;
        Ok(renewed)
    }

    pub fn release(&self, reservation: &TaskReservation) -> Result<(), ReservationError> {
        let conn = Connection::open(&self.path)?;
        let changed = conn.execute(
            "DELETE FROM task_reservations WHERE task_id = ?1 AND lease_token = ?2 AND version = ?3",
            params![
                reservation.task_id.to_string(),
                reservation.lease_token.to_string(),
                reservation.version,
            ],
        )?;
        if changed != 1 {
            return Err(ReservationError::ReservationLost {
                task_id: reservation.task_id,
            });
        }
        Ok(())
    }

    pub fn active_count_for_worker(
        &self,
        agent_id: &str,
        now_ms: i64,
    ) -> Result<usize, ReservationError> {
        let conn = Connection::open(&self.path)?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM task_reservations WHERE agent_id = ?1 AND lease_expires_at_ms > ?2",
            params![agent_id, now_ms],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (TaskReservationStore, PathBuf) {
        let path = std::env::temp_dir().join(format!("nulang-reservations-{}.db", Uuid::new_v4()));
        (TaskReservationStore::open(&path).unwrap(), path)
    }

    #[test]
    fn duplicate_task_claim_is_rejected_until_expiry() {
        let (store, path) = store();
        let task_id = Uuid::new_v4();
        store
            .reserve_with_capacity(task_id, "worker-a", 1, 1_000, 500)
            .unwrap();

        assert!(matches!(
            store.reserve_with_capacity(task_id, "worker-b", 1, 1_100, 500),
            Err(ReservationError::TaskReserved { .. })
        ));
        assert!(store
            .reserve_with_capacity(task_id, "worker-b", 1, 1_501, 500)
            .is_ok());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn worker_capacity_is_reserved_transactionally() {
        let (store, path) = store();
        store
            .reserve_with_capacity(Uuid::new_v4(), "worker", 1, 1_000, 500)
            .unwrap();
        assert!(matches!(
            store.reserve_with_capacity(Uuid::new_v4(), "worker", 1, 1_001, 500),
            Err(ReservationError::WorkerCapacityExhausted { .. })
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stale_cas_version_cannot_release_renewed_lease() {
        let (store, path) = store();
        let original = store
            .reserve_with_capacity(Uuid::new_v4(), "worker", 1, 1_000, 500)
            .unwrap();
        let renewed = store.renew(&original, 1_100, 500).unwrap();

        assert!(matches!(
            store.release(&original),
            Err(ReservationError::ReservationLost { .. })
        ));
        store.release(&renewed).unwrap();
        let _ = std::fs::remove_file(path);
    }
}
