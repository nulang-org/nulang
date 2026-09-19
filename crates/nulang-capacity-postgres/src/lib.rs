//! PostgreSQL persistence for Nulang capacity allocation state.
//!
//! The provider-neutral allocation model lives in nulang-capacity. This crate
//! contains only the hosted control-plane persistence adapter so database
//! dependencies do not leak into the scheduler core.

use nulang_capacity::state::{
    AllocationLedgerStore, LedgerCasFuture, LedgerCasResult, LedgerLoadFuture,
    ProviderAllocationLedgerSnapshot,
};
use std::convert::TryFrom;
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls};

pub const MIGRATION_0001: &str = include_str!("../migrations/0001_provider_ledgers.sql");

const LOAD_PROVIDER_SQL: &str = r#"
SELECT generation, snapshot::text
FROM nulang_capacity_provider_ledgers
WHERE provider_id = $1
"#;

const UPDATE_PROVIDER_SQL: &str = r#"
UPDATE nulang_capacity_provider_ledgers
SET generation = $3,
    snapshot = $4::text::jsonb,
    updated_at = NOW()
WHERE provider_id = $1
  AND generation = $2
"#;

const INSERT_PROVIDER_SQL: &str = r#"
INSERT INTO nulang_capacity_provider_ledgers (
    provider_id,
    generation,
    snapshot
)
VALUES ($1, $2, $3::text::jsonb)
ON CONFLICT (provider_id) DO NOTHING
"#;

#[derive(Debug, Error)]
pub enum PostgresCapacityError {
    #[error("provider id must not be empty")]
    EmptyProviderId,
    #[error("provider generation must be >= 1")]
    InvalidGeneration,
    #[error(
        "next provider generation must equal expected generation + 1; expected {expected}, next {next}"
    )]
    InvalidTransition { expected: u64, next: u64 },
    #[error(
        "snapshot provider {snapshot_provider} does not match requested provider {requested_provider}"
    )]
    ProviderMismatch {
        requested_provider: String,
        snapshot_provider: String,
    },
    #[error(
        "database generation {database_generation} does not match serialized snapshot generation {snapshot_generation} for provider {provider}"
    )]
    SnapshotGenerationMismatch {
        provider: String,
        database_generation: u64,
        snapshot_generation: u64,
    },
    #[error("postgres generation {0} cannot be represented by the capacity model")]
    GenerationOutOfRange(i64),
    #[error("capacity generation {0} exceeds PostgreSQL BIGINT")]
    GenerationTooLarge(u64),
    #[error("postgres error: {0}")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("snapshot serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub struct PostgresAllocationLedgerStore {
    client: Client,
}

impl PostgresAllocationLedgerStore {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Connect with tokio-postgres and return the connection driver separately
    /// so the hosted control plane can monitor its lifecycle.
    pub async fn connect(
        database_url: &str,
    ) -> Result<(Self, JoinHandle<Result<(), tokio_postgres::Error>>), PostgresCapacityError> {
        let (client, connection) = tokio_postgres::connect(database_url, NoTls).await?;
        let connection_task = tokio::spawn(connection);
        Ok((Self::new(client), connection_task))
    }

    pub async fn migrate(&self) -> Result<(), PostgresCapacityError> {
        self.client.batch_execute(MIGRATION_0001).await?;
        Ok(())
    }

    pub async fn load_provider_snapshot(
        &self,
        provider_id: &str,
    ) -> Result<ProviderAllocationLedgerSnapshot, PostgresCapacityError> {
        validate_provider_id(provider_id)?;

        let Some(row) = self
            .client
            .query_opt(LOAD_PROVIDER_SQL, &[&provider_id])
            .await?
        else {
            return Ok(empty_snapshot(provider_id));
        };

        let database_generation = decode_generation(row.get::<_, i64>(0))?;
        let snapshot_json: String = row.get(1);
        let snapshot: ProviderAllocationLedgerSnapshot = serde_json::from_str(&snapshot_json)?;

        if snapshot.provider_id != provider_id {
            return Err(PostgresCapacityError::ProviderMismatch {
                requested_provider: provider_id.to_string(),
                snapshot_provider: snapshot.provider_id,
            });
        }
        if snapshot.generation != database_generation {
            return Err(PostgresCapacityError::SnapshotGenerationMismatch {
                provider: provider_id.to_string(),
                database_generation,
                snapshot_generation: snapshot.generation,
            });
        }

        Ok(snapshot)
    }

    pub async fn compare_and_swap_provider_snapshot(
        &self,
        expected_generation: u64,
        next: &ProviderAllocationLedgerSnapshot,
    ) -> Result<LedgerCasResult, PostgresCapacityError> {
        validate_transition(expected_generation, next)?;

        let expected_i64 = encode_generation(expected_generation)?;
        let next_i64 = encode_generation(next.generation)?;
        let snapshot_json = serde_json::to_string(next)?;

        let updated = self
            .client
            .execute(
                UPDATE_PROVIDER_SQL,
                &[&next.provider_id, &expected_i64, &next_i64, &snapshot_json],
            )
            .await?;

        if updated == 1 {
            return Ok(LedgerCasResult::Stored);
        }

        // Missing providers have logical generation 1. Only a caller that
        // planned against that initial generation may materialize the row.
        if expected_generation == 1 {
            let inserted = self
                .client
                .execute(
                    INSERT_PROVIDER_SQL,
                    &[&next.provider_id, &next_i64, &snapshot_json],
                )
                .await?;
            if inserted == 1 {
                return Ok(LedgerCasResult::Stored);
            }
        }

        Ok(LedgerCasResult::GenerationChanged)
    }
}

impl AllocationLedgerStore for PostgresAllocationLedgerStore {
    fn load_provider<'a>(&'a self, provider_id: &'a str) -> LedgerLoadFuture<'a> {
        Box::pin(async move {
            self.load_provider_snapshot(provider_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn compare_and_swap_provider<'a>(
        &'a self,
        expected_generation: u64,
        next: &'a ProviderAllocationLedgerSnapshot,
    ) -> LedgerCasFuture<'a> {
        Box::pin(async move {
            self.compare_and_swap_provider_snapshot(expected_generation, next)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

fn empty_snapshot(provider_id: &str) -> ProviderAllocationLedgerSnapshot {
    ProviderAllocationLedgerSnapshot {
        provider_id: provider_id.to_string(),
        generation: 1,
        allocations: Default::default(),
        retired_allocation_ids: Default::default(),
    }
}

fn validate_provider_id(provider_id: &str) -> Result<(), PostgresCapacityError> {
    if provider_id.trim().is_empty() {
        Err(PostgresCapacityError::EmptyProviderId)
    } else {
        Ok(())
    }
}

fn validate_transition(
    expected_generation: u64,
    next: &ProviderAllocationLedgerSnapshot,
) -> Result<(), PostgresCapacityError> {
    validate_provider_id(&next.provider_id)?;
    if expected_generation == 0 || next.generation == 0 {
        return Err(PostgresCapacityError::InvalidGeneration);
    }

    let required_next =
        expected_generation
            .checked_add(1)
            .ok_or(PostgresCapacityError::GenerationTooLarge(
                expected_generation,
            ))?;
    if next.generation != required_next {
        return Err(PostgresCapacityError::InvalidTransition {
            expected: expected_generation,
            next: next.generation,
        });
    }
    encode_generation(expected_generation)?;
    encode_generation(next.generation)?;
    Ok(())
}

fn encode_generation(generation: u64) -> Result<i64, PostgresCapacityError> {
    i64::try_from(generation).map_err(|_| PostgresCapacityError::GenerationTooLarge(generation))
}

fn decode_generation(generation: i64) -> Result<u64, PostgresCapacityError> {
    if generation < 1 {
        return Err(PostgresCapacityError::GenerationOutOfRange(generation));
    }
    u64::try_from(generation).map_err(|_| PostgresCapacityError::GenerationOutOfRange(generation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn snapshot(provider: &str, generation: u64) -> ProviderAllocationLedgerSnapshot {
        ProviderAllocationLedgerSnapshot {
            provider_id: provider.to_string(),
            generation,
            allocations: BTreeMap::new(),
            retired_allocation_ids: BTreeSet::new(),
        }
    }

    #[test]
    fn first_materialized_snapshot_moves_generation_one_to_two() {
        assert!(validate_transition(1, &snapshot("host-a", 2)).is_ok());
    }

    #[test]
    fn rejects_skipped_or_replayed_generation() {
        assert!(matches!(
            validate_transition(4, &snapshot("host-a", 4)),
            Err(PostgresCapacityError::InvalidTransition {
                expected: 4,
                next: 4
            })
        ));
        assert!(matches!(
            validate_transition(4, &snapshot("host-a", 6)),
            Err(PostgresCapacityError::InvalidTransition {
                expected: 4,
                next: 6
            })
        ));
    }

    #[test]
    fn rejects_empty_provider_and_generation_overflow() {
        assert!(matches!(
            validate_transition(1, &snapshot("", 2)),
            Err(PostgresCapacityError::EmptyProviderId)
        ));
        assert!(matches!(
            encode_generation(u64::MAX),
            Err(PostgresCapacityError::GenerationTooLarge(u64::MAX))
        ));
    }

    #[test]
    fn sql_binds_json_as_text_before_server_side_jsonb_cast() {
        assert!(UPDATE_PROVIDER_SQL.contains("$4::text::jsonb"));
        assert!(INSERT_PROVIDER_SQL.contains("$3::text::jsonb"));
    }

    #[test]
    fn migration_has_provider_primary_key_and_generation_guard() {
        assert!(MIGRATION_0001.contains("provider_id TEXT PRIMARY KEY"));
        assert!(MIGRATION_0001.contains("generation BIGINT NOT NULL CHECK (generation >= 1)"));
        assert!(MIGRATION_0001.contains("snapshot JSONB NOT NULL"));
    }
}
