//! Durable ACK/NACK delivery state layered on top of Fabric Streams.
//!
//! This module intentionally uses the stream store's public storage contract
//! (`read_from`, cursors, stream info, and root path) instead of reaching into
//! replication/epoch internals. That keeps consumer leases orthogonal to the
//! newer replicated-stream machinery while preserving crash-safe local
//! delivery semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{FabricStreamRecord, FileFabricStreamStore, Runtime};

const CONSUMER_DELIVERY_FORMAT_VERSION: u16 = 1;
const DELIVERY_SCAN_BATCH: usize = 256;
static DELIVERY_TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricConsumerDelivery {
    pub record: FabricStreamRecord,
    pub attempt: u32,
    pub redelivered: bool,
    pub ack_deadline_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConsumerDeliveryLease {
    deadline_unix_ms: u64,
    attempt: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ConsumerDeliveryState {
    #[serde(default)]
    inflight: BTreeMap<u64, ConsumerDeliveryLease>,
    #[serde(default)]
    acked: BTreeSet<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConsumerDeliveryFile {
    version: u16,
    #[serde(default)]
    consumers: BTreeMap<String, ConsumerDeliveryState>,
}

impl Default for ConsumerDeliveryFile {
    fn default() -> Self {
        Self {
            version: CONSUMER_DELIVERY_FORMAT_VERSION,
            consumers: BTreeMap::new(),
        }
    }
}

impl FileFabricStreamStore {
    /// Deliver records under durable acknowledgement leases.
    ///
    /// An active lease suppresses duplicate delivery until ACK, NACK, or
    /// deadline expiry. Expired/NACKed records retain their durable attempt
    /// counter and are redelivered. Newer records remain eligible while an
    /// earlier lease is still active.
    pub fn deliver_consumer(
        &mut self,
        name: &str,
        consumer: &str,
        limit: usize,
        ack_wait: Duration,
    ) -> io::Result<Vec<FabricConsumerDelivery>> {
        self.deliver_consumer_at(name, consumer, limit, ack_wait, SystemTime::now())
    }

    /// Deterministic-clock variant used by DST and focused persistence tests.
    pub fn deliver_consumer_at(
        &mut self,
        name: &str,
        consumer: &str,
        limit: usize,
        ack_wait: Duration,
        now: SystemTime,
    ) -> io::Result<Vec<FabricConsumerDelivery>> {
        // stream_info validates the stream name/existence before it is used to
        // derive the sidecar path. cursor validates the consumer identifier.
        let _ = self.stream_info(name)?;
        let cursor = self.cursor(name, consumer)?;
        if limit == 0 {
            return Ok(Vec::new());
        }

        let ack_wait_ms = u64::try_from(ack_wait.as_millis()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric ACK wait exceeds u64 milliseconds",
            )
        })?;
        if ack_wait_ms == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric ACK wait must be greater than zero",
            ));
        }
        let now_ms = system_time_unix_ms(now)?;
        let deadline_ms = now_ms.checked_add(ack_wait_ms).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Fabric ACK deadline overflow")
        })?;

        let path = consumer_delivery_path(self, name);
        let mut delivery_file = read_consumer_deliveries(&path)?;
        let state = delivery_file
            .consumers
            .entry(consumer.to_string())
            .or_default();

        // Cursor progress is authoritative after crash recovery. Any sidecar
        // entries at/below it are stale bookkeeping and can be discarded.
        state.acked.retain(|sequence| *sequence > cursor);
        state.inflight.retain(|sequence, _| *sequence > cursor);

        let mut delivered = Vec::with_capacity(limit.min(DELIVERY_SCAN_BATCH));
        let mut start = cursor.saturating_add(1);

        while delivered.len() < limit {
            let records = self.read_from(name, start, DELIVERY_SCAN_BATCH)?;
            if records.is_empty() {
                break;
            }
            let last_sequence = records
                .last()
                .expect("non-empty Fabric scan batch has a last record")
                .sequence;

            for record in records {
                if delivered.len() == limit {
                    break;
                }
                if state.acked.contains(&record.sequence) {
                    continue;
                }

                match state.inflight.get_mut(&record.sequence) {
                    Some(lease) if lease.deadline_unix_ms > now_ms => continue,
                    Some(lease) => {
                        lease.attempt = lease.attempt.saturating_add(1);
                        lease.deadline_unix_ms = deadline_ms;
                        delivered.push(FabricConsumerDelivery {
                            record,
                            attempt: lease.attempt,
                            redelivered: true,
                            ack_deadline_unix_ms: deadline_ms,
                        });
                    }
                    None => {
                        state.inflight.insert(
                            record.sequence,
                            ConsumerDeliveryLease {
                                deadline_unix_ms: deadline_ms,
                                attempt: 1,
                            },
                        );
                        delivered.push(FabricConsumerDelivery {
                            record,
                            attempt: 1,
                            redelivered: false,
                            ack_deadline_unix_ms: deadline_ms,
                        });
                    }
                }
            }

            if last_sequence == u64::MAX {
                break;
            }
            start = last_sequence + 1;
        }

        write_delivery_file_atomic(&path, &delivery_file)?;
        Ok(delivered)
    }

    /// ACK one previously delivered sequence.
    ///
    /// Out-of-order ACKs are durable, but the stream cursor advances only over
    /// a contiguous acknowledged prefix.
    pub fn ack_consumer(&mut self, name: &str, consumer: &str, sequence: u64) -> io::Result<()> {
        let info = self.stream_info(name)?;
        let cursor = self.cursor(name, consumer)?;
        if sequence <= cursor {
            return Ok(());
        }

        let tail = info.last_sequence.unwrap_or(0);
        if sequence > tail {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Fabric ACK {sequence} is beyond stream tail {tail}"),
            ));
        }

        let path = consumer_delivery_path(self, name);
        let mut delivery_file = read_consumer_deliveries(&path)?;
        let state = delivery_file.consumers.get_mut(consumer).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Fabric consumer {consumer} has no deliveries to ACK"),
            )
        })?;

        if !state.acked.contains(&sequence) && state.inflight.remove(&sequence).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Fabric sequence {sequence} was not delivered to consumer {consumer}"),
            ));
        }
        state.acked.insert(sequence);

        let mut next_cursor = cursor;
        loop {
            let Some(candidate) = next_cursor.checked_add(1) else {
                break;
            };
            if !state.acked.remove(&candidate) {
                break;
            }
            state.inflight.remove(&candidate);
            next_cursor = candidate;
        }

        // Cursor progress is the authoritative record. Persist it first so a
        // crash can leave stale lease bookkeeping, never lost progress.
        if next_cursor > cursor {
            self.commit_cursor(name, consumer, next_cursor)?;
            state.acked.retain(|pending| *pending > next_cursor);
            state.inflight.retain(|pending, _| *pending > next_cursor);
        }

        write_delivery_file_atomic(&path, &delivery_file)
    }

    /// NACK one in-flight delivery and make it immediately eligible for
    /// redelivery while preserving its attempt counter.
    pub fn nack_consumer(&mut self, name: &str, consumer: &str, sequence: u64) -> io::Result<()> {
        let _ = self.stream_info(name)?;
        if sequence <= self.cursor(name, consumer)? {
            return Ok(());
        }

        let path = consumer_delivery_path(self, name);
        let mut delivery_file = read_consumer_deliveries(&path)?;
        let state = delivery_file.consumers.get_mut(consumer).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Fabric consumer {consumer} has no deliveries to NACK"),
            )
        })?;
        let lease = state.inflight.get_mut(&sequence).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Fabric sequence {sequence} is not in-flight for consumer {consumer}"),
            )
        })?;
        lease.deadline_unix_ms = 0;
        write_delivery_file_atomic(&path, &delivery_file)
    }
}

impl Runtime {
    fn fabric_consumer_store_mut(&mut self) -> io::Result<&mut FileFabricStreamStore> {
        self.distributed.fabric_streams.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Fabric stream store is not configured",
            )
        })
    }

    pub fn fabric_stream_deliver_consumer(
        &mut self,
        name: &str,
        consumer: &str,
        limit: usize,
        ack_wait: Duration,
    ) -> io::Result<Vec<FabricConsumerDelivery>> {
        self.fabric_consumer_store_mut()?
            .deliver_consumer(name, consumer, limit, ack_wait)
    }

    pub fn fabric_stream_ack_consumer(
        &mut self,
        name: &str,
        consumer: &str,
        sequence: u64,
    ) -> io::Result<()> {
        self.fabric_consumer_store_mut()?
            .ack_consumer(name, consumer, sequence)
    }

    pub fn fabric_stream_nack_consumer(
        &mut self,
        name: &str,
        consumer: &str,
        sequence: u64,
    ) -> io::Result<()> {
        self.fabric_consumer_store_mut()?
            .nack_consumer(name, consumer, sequence)
    }
}

fn consumer_delivery_path(store: &FileFabricStreamStore, name: &str) -> PathBuf {
    store.root().join(name).join("deliveries.json")
}

fn read_consumer_deliveries(path: &Path) -> io::Result<ConsumerDeliveryFile> {
    if !path.exists() {
        return Ok(ConsumerDeliveryFile::default());
    }
    let bytes = fs::read(path)?;
    let deliveries: ConsumerDeliveryFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if deliveries.version != CONSUMER_DELIVERY_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported Fabric consumer delivery version {}",
                deliveries.version
            ),
        ));
    }
    Ok(deliveries)
}

fn system_time_unix_ms(now: SystemTime) -> io::Result<u64> {
    let duration = now.duration_since(UNIX_EPOCH).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric consumer clock is before the Unix epoch",
        )
    })?;
    u64::try_from(duration.as_millis()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric consumer clock exceeds u64 milliseconds",
        )
    })
}

fn write_delivery_file_atomic(path: &Path, value: &ConsumerDeliveryFile) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(json_error)?;
    let counter = DELIVERY_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp-{}-{counter}", std::process::id()));

    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_data()?;
        fs::rename(&temp, path)?;
        if let Some(parent) = path.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
