//! Minimal, opt-in SQL-backed durable jobs for local SQLite/libSQL.
//!
//! Unlike actor mailbox sends, an enqueued job is persisted. Insert with
//! `enqueue_in` inside the caller's SQL transaction to atomically commit
//! business data and work. Delivery is *at least once*: consumers must be
//! idempotent when performing external effects.

use std::io;

/// A job to enqueue. Times are Unix milliseconds, supplied by the caller
/// (which also enables deterministic scheduling tests).
#[derive(Debug, Clone)]
pub struct JobRequest<'a> {
    pub queue: &'a str,
    pub worker: &'a str,
    pub payload_json: &'a str,
    pub unique_key: Option<&'a str>,
    pub run_at_ms: i64,
    pub max_attempts: i64,
}

impl<'a> JobRequest<'a> {
    pub fn new(queue: &'a str, worker: &'a str, payload_json: &'a str, run_at_ms: i64) -> Self {
        Self {
            queue,
            worker,
            payload_json,
            unique_key: None,
            run_at_ms,
            max_attempts: 5,
        }
    }

    pub fn with_unique_key(mut self, key: &'a str) -> Self {
        self.unique_key = Some(key);
        self
    }

    pub fn with_max_attempts(mut self, attempts: i64) -> Self {
        self.max_attempts = attempts;
        self
    }

    fn validate(&self) -> io::Result<()> {
        if self.queue.trim().is_empty()
            || self.worker.trim().is_empty()
            || self.unique_key.is_some_and(|key| key.trim().is_empty())
            || self.run_at_ms < 0
            || self.max_attempts < 1
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid durable job metadata",
            ));
        }
        serde_json::from_str::<serde_json::Value>(self.payload_json)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Ok(())
    }
}

/// Claim token is the monotonically increasing attempt number for this job.
/// After a new claim, stale workers cannot complete or reschedule the job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobClaim {
    pub id: i64,
    pub queue: String,
    pub worker: String,
    pub payload_json: String,
    pub attempt: i64,
    pub lease_until_ms: i64,
}

pub type JobResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// A storage-level primitive; callers own the database connection and the
/// worker execution loop. It is not a replacement for actor scheduling.
pub struct DurableSqlJobs;

impl DurableSqlJobs {
    /// Create schema on the same connection/database as business tables.
    pub async fn install(conn: &libsql::Connection) -> JobResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS nulang_jobs (
               id INTEGER PRIMARY KEY,
               queue TEXT NOT NULL,
               worker TEXT NOT NULL,
               payload_json TEXT NOT NULL,
               unique_key TEXT UNIQUE,
               state TEXT NOT NULL DEFAULT 'available'
                 CHECK (state IN ('available','running','completed','discarded')),
               run_at_ms INTEGER NOT NULL,
               attempts INTEGER NOT NULL DEFAULT 0,
               max_attempts INTEGER NOT NULL,
               lease_until_ms INTEGER,
               last_error TEXT
             );
             CREATE INDEX IF NOT EXISTS nulang_jobs_ready
               ON nulang_jobs(queue, state, run_at_ms, lease_until_ms, id);",
        )
        .await?;
        Ok(())
    }

    /// Enqueue atomically with other writes made using this transaction.
    /// Deduplication keys are intentionally permanent; key reuse requires
    /// an application-defined new key or an explicit retention policy.
    pub async fn enqueue_in(tx: &libsql::Transaction, req: JobRequest<'_>) -> JobResult<i64> {
        req.validate()?;
        let inserted = tx.execute(
            "INSERT INTO nulang_jobs (queue, worker, payload_json, unique_key, run_at_ms, max_attempts)
             VALUES (?1, ?2, ?3, NULLIF(?4, ''), ?5, ?6)
             ON CONFLICT(unique_key) DO NOTHING",
            libsql::params![
                req.queue, req.worker, req.payload_json,
                req.unique_key.unwrap_or(""), req.run_at_ms, req.max_attempts
            ],
        ).await?;
        if inserted == 1 {
            let mut rows = tx.query("SELECT last_insert_rowid()", ()).await?;
            let row = rows.next().await?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "durable job insert returned no id")
            })?;
            return Ok(row.get::<i64>(0)?);
        }
        // A retry with a reused key may only identify the *same* request.
        // Fail closed if a caller accidentally reuses a key with new work.
        let mut rows = tx
            .query(
                "SELECT id, queue, worker, payload_json, run_at_ms, max_attempts
             FROM nulang_jobs WHERE unique_key = ?1",
                libsql::params![req.unique_key.unwrap_or("")],
            )
            .await?;
        let row = rows.next().await?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "durable job key conflict without matching row",
            )
        })?;
        let same_payload = serde_json::from_str::<serde_json::Value>(&row.get::<String>(3)?)?
            == serde_json::from_str::<serde_json::Value>(req.payload_json)?;
        if row.get::<String>(1)? != req.queue
            || row.get::<String>(2)? != req.worker
            || !same_payload
            || row.get::<i64>(4)? != req.run_at_ms
            || row.get::<i64>(5)? != req.max_attempts
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "durable job uniqueness key reused for different work",
            )
            .into());
        }
        Ok(row.get::<i64>(0)?)
    }

    /// Convenience enqueue when the caller has no other transactional writes.
    pub async fn enqueue(conn: &libsql::Connection, req: JobRequest<'_>) -> JobResult<i64> {
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        let id = Self::enqueue_in(&tx, req).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Claim the oldest eligible job within an IMMEDIATE SQLite transaction.
    /// At lease expiry a job becomes claimable again; the previous owner loses
    /// its right to acknowledge once a successor claims it.
    pub async fn claim(
        conn: &libsql::Connection,
        queue: &str,
        now_ms: i64,
        lease_ms: i64,
    ) -> JobResult<Option<JobClaim>> {
        if queue.trim().is_empty()
            || now_ms < 0
            || lease_ms <= 0
            || now_ms.checked_add(lease_ms).is_none()
        {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "invalid claim parameters").into(),
            );
        }
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        // A worker can crash on its last permitted attempt; never leave that
        // row eternally in 'running' once its lease has expired.
        tx.execute(
            "UPDATE nulang_jobs SET state = 'discarded', lease_until_ms = NULL
             WHERE queue = ?1 AND state = 'running' AND lease_until_ms <= ?2
               AND attempts >= max_attempts",
            libsql::params![queue, now_ms],
        )
        .await?;
        let next = {
            let mut rows = tx
                .query(
                    "SELECT id, worker, payload_json, attempts
                 FROM nulang_jobs
                 WHERE queue = ?1 AND attempts < max_attempts
                   AND ((state = 'available' AND run_at_ms <= ?2)
                     OR (state = 'running' AND lease_until_ms <= ?2))
                 ORDER BY run_at_ms ASC, id ASC LIMIT 1",
                    libsql::params![queue, now_ms],
                )
                .await?;
            match rows.next().await? {
                None => None,
                Some(row) => Some((
                    row.get::<i64>(0)?,
                    row.get::<String>(1)?,
                    row.get::<String>(2)?,
                    row.get::<i64>(3)?,
                )),
            }
        };
        let claim = if let Some((id, worker, payload_json, prev_attempts)) = next {
            let attempt = prev_attempts.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "job attempt overflow")
            })?;
            let lease_until_ms = now_ms + lease_ms;
            tx.execute(
                "UPDATE nulang_jobs SET state = 'running', attempts = ?2, lease_until_ms = ?3
                 WHERE id = ?1",
                libsql::params![id, attempt, lease_until_ms],
            )
            .await?;
            Some(JobClaim {
                id,
                queue: queue.to_owned(),
                worker,
                payload_json,
                attempt,
                lease_until_ms,
            })
        } else {
            None
        };
        tx.commit().await?;
        Ok(claim)
    }

    /// Returns false for duplicate, expired, or stale acknowledgments.
    pub async fn complete(
        conn: &libsql::Connection,
        id: i64,
        attempt: i64,
        now_ms: i64,
    ) -> JobResult<bool> {
        let changed = conn
            .execute(
                "UPDATE nulang_jobs SET state = 'completed', lease_until_ms = NULL
             WHERE id = ?1 AND attempts = ?2 AND state = 'running'
               AND lease_until_ms > ?3",
                libsql::params![id, attempt, now_ms],
            )
            .await?;
        Ok(changed == 1)
    }

    /// Mark a claimed execution failed and defer the next attempt; exceeding
    /// the limit discards the job. A negative delay is rejected.
    pub async fn fail(
        conn: &libsql::Connection,
        id: i64,
        attempt: i64,
        now_ms: i64,
        retry_delay_ms: i64,
    ) -> JobResult<bool> {
        if retry_delay_ms < 0 || now_ms.checked_add(retry_delay_ms).is_none() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid retry delay").into());
        }
        let changed = conn
            .execute(
                "UPDATE nulang_jobs SET
               state = CASE WHEN attempts >= max_attempts THEN 'discarded' ELSE 'available' END,
               run_at_ms = ?4, lease_until_ms = NULL
             WHERE id = ?1 AND attempts = ?2 AND state = 'running'
               AND lease_until_ms > ?3",
                libsql::params![id, attempt, now_ms, now_ms + retry_delay_ms],
            )
            .await?;
        Ok(changed == 1)
    }
}
