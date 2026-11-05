//! Transactional PostgreSQL sink for NuDB receipt-managed WAL consumers.
//! The committed PostgreSQL inbox row IS the downstream effect. Arbitrary
//! business side effects require the consumer to include them in the same
//! native database transaction. No distributed exactly-once guarantee.

use super::retention::RetentionClass;
use super::retention_store::{DurableRetentionRegistry, RegistryError};
use postgres::{config::Host, Client, NoTls};
use std::fmt;

const MAX_EFFECT_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum PgSinkError {
    Postgres(postgres::Error),
    Registry(RegistryError),
    InvalidSchema,
    InsecureTransport,
    IdentityMismatch,
    InvalidDelivery,
    Conflict,
    Corrupt,
}
impl fmt::Display for PgSinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Postgres(e) => write!(f, "PostgreSQL sink failure: {e}"),
            Self::Registry(e) => write!(f, "NuDB registry failure: {e}"),
            Self::InvalidSchema => f.write_str("invalid PostgreSQL schema or tablet identity"),
            Self::InsecureTransport => {
                f.write_str("NoTls sink requires an explicit loopback IP or trusted Unix socket")
            }
            Self::IdentityMismatch => {
                f.write_str("PostgreSQL sink tablet/epoch differs from registry")
            }
            Self::InvalidDelivery => {
                f.write_str("invalid class, consumer, delivery ID, cursor or effect size")
            }
            Self::Conflict => f.write_str("delivery ID is committed with different contents"),
            Self::Corrupt => f.write_str("PostgreSQL effect and receipt disagree"),
        }
    }
}
impl std::error::Error for PgSinkError {}
impl From<postgres::Error> for PgSinkError {
    fn from(e: postgres::Error) -> Self {
        Self::Postgres(e)
    }
}
impl From<RegistryError> for PgSinkError {
    fn from(e: RegistryError) -> Self {
        Self::Registry(e)
    }
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
}
fn class_name(class: RetentionClass) -> Option<&'static str> {
    match class {
        RetentionClass::Changefeed => Some("changefeed"),
        RetentionClass::Backup => Some("backup"),
        RetentionClass::Replica => Some("replica"),
        RetentionClass::Branch => Some("branch"),
        RetentionClass::Snapshot => None,
    }
}
fn validate(
    class: RetentionClass,
    consumer: &str,
    cursor: u64,
    delivery_id: &str,
    effect: &[u8],
) -> Result<&'static str, PgSinkError> {
    let class = class_name(class).ok_or(PgSinkError::InvalidDelivery)?;
    if !valid_id(consumer)
        || !valid_id(delivery_id)
        || cursor == 0
        || cursor > i64::MAX as u64
        || effect.is_empty()
        || effect.len() > MAX_EFFECT_BYTES
    {
        return Err(PgSinkError::InvalidDelivery);
    }
    Ok(class)
}
fn hash_field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}
fn receipt(
    tablet_id: i64,
    epoch: i64,
    class: &str,
    consumer: &str,
    delivery_id: &str,
    cursor: i64,
    effect: &[u8],
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"NuDB PostgreSQL effect v1");
    h.update(&tablet_id.to_be_bytes());
    h.update(&epoch.to_be_bytes());
    hash_field(&mut h, class.as_bytes());
    hash_field(&mut h, consumer.as_bytes());
    hash_field(&mut h, delivery_id.as_bytes());
    h.update(&cursor.to_be_bytes());
    hash_field(&mut h, effect);
    *h.finalize().as_bytes()
}

/// Dedicated durable effect/idempotency table scoped to one logical tablet
/// and ownership epoch. Concurrent writers use PostgreSQL unique constraints.
pub struct PostgresConsumerSink {
    client: Client,
    table: String,
    tablet_id: i64,
    owner_epoch: i64,
}
impl PostgresConsumerSink {
    pub fn connect(
        url: &str,
        schema: &str,
        tablet_id: u64,
        owner_epoch: u64,
    ) -> Result<Self, PgSinkError> {
        if schema.is_empty()
            || schema.len() > 63
            || !(schema.as_bytes()[0].is_ascii_alphabetic() || schema.starts_with('_'))
            || !schema
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || tablet_id == 0
            || tablet_id > i64::MAX as u64
            || owner_epoch == 0
            || owner_epoch > i64::MAX as u64
        {
            return Err(PgSinkError::InvalidSchema);
        }
        // Strict ASCII allowlist protects this identifier interpolation.
        let table = format!("\"{schema}\".nudb_consumer_effects");
        // NoTls is deliberately constrained to local sockets/loopback TCP.
        // Remote connections require an authenticated encrypted tunnel
        // or a future certificate-validated TLS connector.
        let config: postgres::Config = url.parse()?;
        if config.get_hosts().is_empty()
            || !config.get_hosts().iter().all(|host| match host {
                Host::Tcp(host) => host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|addr| addr.is_loopback()),
                #[cfg(unix)]
                Host::Unix(_) => true,
            })
            || !config.get_hostaddrs().iter().all(|addr| addr.is_loopback())
        {
            return Err(PgSinkError::InsecureTransport);
        }
        let mut client = config.connect(NoTls)?;
        client.batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {table} (
              tablet_id BIGINT NOT NULL CHECK (tablet_id > 0),
              owner_epoch BIGINT NOT NULL CHECK (owner_epoch > 0),
              class TEXT NOT NULL,
              consumer TEXT NOT NULL,
              delivery_id TEXT NOT NULL,
              next_needed BIGINT NOT NULL CHECK (next_needed > 0),
              effect BYTEA NOT NULL,
              receipt BYTEA NOT NULL CHECK (octet_length(receipt) = 32),
              PRIMARY KEY (tablet_id,owner_epoch,class,consumer,delivery_id)
            )"
        ))?;
        Ok(Self {
            client,
            table,
            tablet_id: tablet_id as i64,
            owner_epoch: owner_epoch as i64,
        })
    }

    /// Effect and idempotency receipt commit in the SAME PG transaction.
    /// synchronous_commit=on waits for primary WAL flush, NOT remote replicas.
    pub fn apply(
        &mut self,
        class: RetentionClass,
        consumer: &str,
        next_needed: u64,
        delivery_id: &str,
        effect: &[u8],
    ) -> Result<[u8; 32], PgSinkError> {
        let class = validate(class, consumer, next_needed, delivery_id, effect)?;
        let cursor = next_needed as i64;
        let expected = receipt(
            self.tablet_id,
            self.owner_epoch,
            class,
            consumer,
            delivery_id,
            cursor,
            effect,
        );
        let mut tx = self.client.transaction()?;
        tx.batch_execute("SET LOCAL synchronous_commit = on")?;
        tx.execute(&format!(
            "INSERT INTO {} (tablet_id,owner_epoch,class,consumer,delivery_id,next_needed,effect,receipt)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
            self.table
        ), &[&self.tablet_id,&self.owner_epoch,&class,&consumer,&delivery_id,
            &cursor,&effect,&expected.as_slice()])?;
        // Block for any racing transaction, then verify the exact immutable
        // row; a conflicting replay rolls back without modifying the effect.
        let row = tx.query_one(
            &format!(
                "SELECT next_needed,effect,receipt FROM {}
             WHERE tablet_id=$1 AND owner_epoch=$2 AND class=$3 AND consumer=$4
             AND delivery_id=$5 FOR UPDATE",
                self.table
            ),
            &[
                &self.tablet_id,
                &self.owner_epoch,
                &class,
                &consumer,
                &delivery_id,
            ],
        )?;
        let prior_cursor: i64 = row.get(0);
        let prior_effect: Vec<u8> = row.get(1);
        let prior_receipt: Vec<u8> = row.get(2);
        if prior_cursor != cursor || prior_effect != effect {
            return Err(PgSinkError::Conflict);
        }
        if prior_receipt != expected {
            return Err(PgSinkError::Corrupt);
        }
        tx.commit()?;
        Ok(expected)
    }

    pub fn read_effect(
        &mut self,
        class: RetentionClass,
        consumer: &str,
        delivery_id: &str,
    ) -> Result<Option<Vec<u8>>, PgSinkError> {
        let class = class_name(class).ok_or(PgSinkError::InvalidDelivery)?;
        if !valid_id(consumer) || !valid_id(delivery_id) {
            return Err(PgSinkError::InvalidDelivery);
        }
        let row = self.client.query_opt(
            &format!(
                "SELECT next_needed,effect,receipt FROM {}
             WHERE tablet_id=$1 AND owner_epoch=$2 AND class=$3
             AND consumer=$4 AND delivery_id=$5",
                self.table
            ),
            &[
                &self.tablet_id,
                &self.owner_epoch,
                &class,
                &consumer,
                &delivery_id,
            ],
        )?;
        let Some(row) = row else {
            return Ok(None);
        };
        let cursor: i64 = row.get(0);
        let effect: Vec<u8> = row.get(1);
        let ack: Vec<u8> = row.get(2);
        if cursor <= 0
            || effect.is_empty()
            || effect.len() > MAX_EFFECT_BYTES
            || ack
                != receipt(
                    self.tablet_id,
                    self.owner_epoch,
                    class,
                    consumer,
                    delivery_id,
                    cursor,
                    &effect,
                )
        {
            return Err(PgSinkError::Corrupt);
        }
        Ok(Some(effect))
    }
    pub fn effect_count(&mut self) -> Result<i64, PgSinkError> {
        let row = self.client.query_one(
            &format!(
                "SELECT count(*) FROM {} WHERE tablet_id=$1 AND owner_epoch=$2",
                self.table
            ),
            &[&self.tablet_id, &self.owner_epoch],
        )?;
        Ok(row.get(0))
    }

    /// Durable local intent -> native PostgreSQL commit -> durable local ACK.
    /// If the process dies after PG commit, the old pin remains and replay
    /// uses the stable idempotency key to obtain the SAME receipt.
    pub fn deliver_and_ack(
        &mut self,
        registry: &mut DurableRetentionRegistry,
        class: RetentionClass,
        consumer: &str,
        next_needed: u64,
        delivery_id: &str,
        effect: &[u8],
    ) -> Result<u64, PgSinkError> {
        validate(class, consumer, next_needed, delivery_id, effect)?;
        if registry.tablet_identity() != (self.tablet_id as u64, self.owner_epoch as u64) {
            return Err(PgSinkError::IdentityMismatch);
        }
        registry.begin_delivery(class, consumer, next_needed, delivery_id)?;
        let ack = self.apply(class, consumer, next_needed, delivery_id, effect)?;
        Ok(registry.commit_delivery(class, consumer, next_needed, delivery_id, &ack)?)
    }
}
