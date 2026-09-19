//! Durable work-queue semantics layered on Nulang Fabric Streams.
//!
//! Actor mailboxes remain actor-addressed protocol delivery. Fabric queues are
//! named resources with visibility leases, redelivery, retry limits, delayed
//! availability, priority ordering, and durable terminal state.
//!
//! This first slice is intentionally local to one stream store. Replicated
//! consumer ownership and NATS JetStream/BullMQ protocol adapters build on the
//! same state machine after the native semantics stabilize.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{FabricStreamConfig, FabricStreamRecord, FileFabricStreamStore, Runtime};

const QUEUE_FORMAT_VERSION: u16 = 1;
const QUEUE_STREAM_PREFIX: &str = "__queue.";
const QUEUE_MUTATION_STREAM_PREFIX: &str = "__queue_meta.";
const QUEUE_STATE_FILE: &str = "queue_state.json";
const RECONCILE_BATCH: usize = 1024;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricQueueConfig {
    /// Default worker lease. An un-ACKed delivery becomes eligible for
    /// redelivery after this interval.
    pub visibility_timeout_ms: u64,
    /// Maximum number of deliveries before a job becomes terminal.
    pub max_attempts: u32,
    /// Optional logical dead-letter destination. This slice records
    /// DeadLettered terminal state; forwarding to the destination is a
    /// follow-up transport concern.
    pub dead_letter_queue: Option<String>,
}

impl Default for FabricQueueConfig {
    fn default() -> Self {
        Self {
            visibility_timeout_ms: 30_000,
            max_attempts: 3,
            dead_letter_queue: None,
        }
    }
}

impl FabricQueueConfig {
    fn validate(&self) -> io::Result<()> {
        if self.visibility_timeout_ms == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue visibility_timeout_ms must be greater than zero",
            ));
        }
        if self.max_attempts == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue max_attempts must be greater than zero",
            ));
        }
        if let Some(name) = &self.dead_letter_queue {
            validate_queue_name(name)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FabricQueueAddOptions {
    /// Stable application job id. Reusing an id deduplicates enqueue and
    /// returns the original stream sequence.
    pub job_id: Option<String>,
    /// Higher values are delivered first; equal priorities remain FIFO.
    pub priority: i32,
    /// Delay before the job becomes visible to workers.
    pub delay_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueAddResult {
    pub sequence: u64,
    pub job_id: String,
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FabricQueueJobStatus {
    Waiting,
    Active,
    Completed,
    Failed,
    DeadLettered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueDelivery {
    pub sequence: u64,
    /// Queue ownership epoch that fenced this delivery. Local-only queues use 0.
    pub queue_epoch: u64,
    pub job_id: String,
    pub name: String,
    pub payload: Vec<u8>,
    pub priority: i32,
    /// Number of times this job has been leased to a worker, including the
    /// current delivery.
    pub deliveries: u32,
    /// Monotonic fencing token for this delivery. ACK/NACK/renew must present
    /// the exact token so a stale execution cannot complete a newer lease.
    pub lease_token: u64,
    pub lease_until_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueNackResult {
    pub status: FabricQueueJobStatus,
    pub deliveries: u32,
    pub available_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueInfo {
    pub name: String,
    pub waiting: usize,
    pub active: usize,
    pub completed: usize,
    pub failed: usize,
    pub dead_lettered: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueueEnvelope {
    pub(crate) name: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) job_id: Option<String>,
    pub(crate) priority: i32,
    pub(crate) created_at_ms: u64,
    pub(crate) available_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueJobState {
    job_id: Option<String>,
    available_at_ms: u64,
    priority: i32,
    deliveries: u32,
    #[serde(default)]
    lease_token: u64,
    status: FabricQueueJobStatus,
    consumer: Option<String>,
    lease_until_ms: Option<u64>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueStateFile {
    version: u16,
    config: FabricQueueConfig,
    #[serde(default)]
    event_cursor: u64,
    jobs: BTreeMap<u64, QueueJobState>,
}

impl QueueStateFile {
    fn new(config: FabricQueueConfig) -> Self {
        Self {
            version: QUEUE_FORMAT_VERSION,
            config,
            event_cursor: 0,
            jobs: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum QueueMutation {
    QueueCreated {
        config: FabricQueueConfig,
    },
    LeaseAcquired {
        sequence: u64,
        consumer: String,
        lease_token: u64,
        lease_until_ms: u64,
        deliveries: u32,
        #[serde(default)]
        queue_epoch: u64,
        #[serde(default)]
        operation_id: Option<String>,
    },
    Completed {
        sequence: u64,
        consumer: String,
        lease_token: u64,
    },
    Nacked {
        sequence: u64,
        consumer: String,
        lease_token: u64,
        status: FabricQueueJobStatus,
        available_at_ms: Option<u64>,
        last_error: Option<String>,
    },
    LeaseRenewed {
        sequence: u64,
        consumer: String,
        lease_token: u64,
        lease_until_ms: u64,
    },
    LeaseExpired {
        sequence: u64,
        consumer: String,
        lease_token: u64,
        status: FabricQueueJobStatus,
        available_at_ms: Option<u64>,
    },
}

pub(crate) fn encode_queue_created_mutation(
    config: &FabricQueueConfig,
) -> io::Result<Vec<u8>> {
    config.validate()?;
    serde_json::to_vec(&QueueMutation::QueueCreated {
        config: config.clone(),
    })
    .map_err(json_error)
}

pub(crate) fn decode_queue_created_mutation(
    bytes: &[u8],
) -> io::Result<FabricQueueConfig> {
    let event: QueueMutation = serde_json::from_slice(bytes).map_err(json_error)?;
    match event {
        QueueMutation::QueueCreated { config } => {
            config.validate()?;
            Ok(config)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "first Fabric queue mutation must be QueueCreated",
        )),
    }
}

pub(crate) fn encode_queue_envelope(
    queue: &str,
    name: &str,
    payload: &[u8],
    options: &FabricQueueAddOptions,
    now_ms: u64,
) -> io::Result<Vec<u8>> {
    validate_queue_name(queue)?;
    validate_job_name(name)?;
    if let Some(job_id) = options.job_id.as_deref() {
        validate_job_id(job_id)?;
    }
    serde_json::to_vec(&QueueEnvelope {
        name: name.to_string(),
        payload: payload.to_vec(),
        job_id: options.job_id.clone(),
        priority: options.priority,
        created_at_ms: now_ms,
        available_at_ms: now_ms.saturating_add(options.delay_ms),
    })
    .map_err(json_error)
}

pub(crate) fn decode_queue_envelope_bytes(bytes: &[u8]) -> io::Result<QueueEnvelope> {
    serde_json::from_slice(bytes).map_err(json_error)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FabricQueueLeaseMutation {
    pub sequence: u64,
    pub consumer: String,
    pub lease_token: u64,
    pub lease_until_ms: u64,
    pub deliveries: u32,
    pub queue_epoch: u64,
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct FabricQueueLeasePlan {
    pub mutation_bytes: Vec<u8>,
    pub delivery: FabricQueueDelivery,
}

pub(crate) fn decode_queue_lease_mutation(
    bytes: &[u8],
) -> io::Result<Option<FabricQueueLeaseMutation>> {
    let event: QueueMutation = serde_json::from_slice(bytes).map_err(json_error)?;
    match event {
        QueueMutation::LeaseAcquired {
            sequence,
            consumer,
            lease_token,
            lease_until_ms,
            deliveries,
            queue_epoch,
            operation_id,
        } => Ok(Some(FabricQueueLeaseMutation {
            sequence,
            consumer,
            lease_token,
            lease_until_ms,
            deliveries,
            queue_epoch,
            operation_id,
        })),
        _ => Ok(None),
    }
}


/// Queue state machine borrowing an existing Fabric stream store.
///
/// The payload stream and queue mutation stream are the durable sources of
/// truth. queue_state.json is only a rebuildable materialized index. Queue
/// mutations are appended and fsynced before the cache is replaced; recovery
/// replays any events beyond its persisted event cursor.
pub struct FabricQueueStore<'a> {
    streams: &'a mut FileFabricStreamStore,
}

impl<'a> FabricQueueStore<'a> {
    pub fn new(streams: &'a mut FileFabricStreamStore) -> Self {
        Self { streams }
    }

    pub fn create_queue(&mut self, name: &str, config: FabricQueueConfig) -> io::Result<()> {
        validate_queue_name(name)?;
        config.validate()?;
        let stream = queue_stream_name(name);
        let mutation_stream = queue_mutation_stream_name(name);
        self.ensure_stream(&stream)?;
        self.ensure_stream(&mutation_stream)?;

        let existing = self.streams.read_from(&mutation_stream, 1, 1)?;
        if let Some(record) = existing.first() {
            let event: QueueMutation =
                serde_json::from_slice(&record.payload).map_err(json_error)?;
            match event {
                QueueMutation::QueueCreated {
                    config: existing_config,
                } if existing_config == config => {}
                QueueMutation::QueueCreated { .. } => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("Fabric queue {name:?} already exists with different config"),
                    ));
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "first Fabric queue mutation must be QueueCreated",
                    ));
                }
            }
        } else {
            let payload_info = self.streams.stream_info(&stream)?;
            if payload_info.last_sequence.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric queue payload stream has history but no QueueCreated mutation; explicit migration is required",
                ));
            }
            self.append_mutation(
                name,
                &QueueMutation::QueueCreated {
                    config: config.clone(),
                },
            )?;
        }

        let state = self.load_state(name)?;
        if state.config != config {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Fabric queue {name:?} already exists with different config"),
            ));
        }
        Ok(())
    }

    pub fn add(
        &mut self,
        queue: &str,
        name: &str,
        payload: &[u8],
        options: FabricQueueAddOptions,
    ) -> io::Result<FabricQueueAddResult> {
        self.add_at(queue, name, payload, options, unix_now_ms()?)
    }

    pub fn add_at(
        &mut self,
        queue: &str,
        name: &str,
        payload: &[u8],
        options: FabricQueueAddOptions,
        now_ms: u64,
    ) -> io::Result<FabricQueueAddResult> {
        validate_queue_name(queue)?;
        validate_job_name(name)?;
        if let Some(job_id) = options.job_id.as_deref() {
            validate_job_id(job_id)?;
        }

        let mut state = self.load_state(queue)?;
        if let Some(job_id) = options.job_id.as_deref() {
            if let Some((&sequence, _)) = state
                .jobs
                .iter()
                .find(|(_, job)| job.job_id.as_deref() == Some(job_id))
            {
                return Ok(FabricQueueAddResult {
                    sequence,
                    job_id: job_id.to_string(),
                    deduplicated: true,
                });
            }
        }

        let available_at_ms = now_ms.saturating_add(options.delay_ms);
        let bytes = encode_queue_envelope(queue, name, payload, &options, now_ms)?;
        let sequence = self.streams.append(&queue_stream_name(queue), &bytes)?;

        state.jobs.insert(
            sequence,
            QueueJobState {
                job_id: options.job_id.clone(),
                available_at_ms,
                priority: options.priority,
                deliveries: 0,
                lease_token: 0,
                status: FabricQueueJobStatus::Waiting,
                consumer: None,
                lease_until_ms: None,
                last_error: None,
            },
        );
        self.write_state(queue, &state)?;

        Ok(FabricQueueAddResult {
            sequence,
            job_id: options
                .job_id
                .unwrap_or_else(|| sequence.to_string()),
            deduplicated: false,
        })
    }

    pub fn acquire(
        &mut self,
        queue: &str,
        consumer: &str,
    ) -> io::Result<Option<FabricQueueDelivery>> {
        self.acquire_at(queue, consumer, unix_now_ms()?)
    }

    pub fn acquire_at(
        &mut self,
        queue: &str,
        consumer: &str,
        now_ms: u64,
    ) -> io::Result<Option<FabricQueueDelivery>> {
        validate_queue_name(queue)?;
        validate_consumer_name(consumer)?;
        let mut state = self.load_state(queue)?;
        let expired = self.expire_due(queue, &mut state, now_ms)?;

        let mut candidate: Option<(u64, i32)> = None;
        for (&sequence, job) in &state.jobs {
            if job.status != FabricQueueJobStatus::Waiting
                || job.available_at_ms > now_ms
                || job.deliveries >= state.config.max_attempts
            {
                continue;
            }
            match candidate {
                None => candidate = Some((sequence, job.priority)),
                Some((best_sequence, best_priority))
                    if job.priority > best_priority
                        || (job.priority == best_priority && sequence < best_sequence) =>
                {
                    candidate = Some((sequence, job.priority))
                }
                _ => {}
            }
        }

        let Some((sequence, _)) = candidate else {
            if expired > 0 {
                self.write_state(queue, &state)?;
            }
            return Ok(None);
        };

        let envelope = self.read_envelope(queue, sequence)?;
        let (deliveries, lease_token, priority) = {
            let current = state.jobs.get(&sequence).expect("candidate must exist");
            let deliveries = current.deliveries.saturating_add(1);
            let lease_token = current.lease_token.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "Fabric queue lease token overflow")
            })?;
            (deliveries, lease_token, current.priority)
        };
        let lease_until_ms = now_ms.saturating_add(state.config.visibility_timeout_ms);
        let event = QueueMutation::LeaseAcquired {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
            lease_until_ms,
            deliveries,
            queue_epoch: 0,
            operation_id: None,
        };
        let event_sequence = self.append_mutation(queue, &event)?;
        apply_mutation(&mut state, &event)?;
        state.event_cursor = event_sequence;
        self.write_state(queue, &state)?;

        Ok(Some(FabricQueueDelivery {
            sequence,
            queue_epoch: 0,
            job_id: envelope.job_id.unwrap_or_else(|| sequence.to_string()),
            name: envelope.name,
            payload: envelope.payload,
            priority,
            deliveries,
            lease_token,
            lease_until_ms,
        }))
    }

    pub fn ack(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
    ) -> io::Result<()> {
        self.ack_at(queue, sequence, consumer, lease_token, unix_now_ms()?)
    }

    pub fn ack_at(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        now_ms: u64,
    ) -> io::Result<()> {
        validate_consumer_name(consumer)?;
        let mut state = self.load_state(queue)?;
        validate_active_job(&state, sequence, consumer, lease_token, now_ms)?;
        let event = QueueMutation::Completed {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
        };
        let event_sequence = self.append_mutation(queue, &event)?;
        apply_mutation(&mut state, &event)?;
        state.event_cursor = event_sequence;
        self.write_state(queue, &state)
    }

    pub fn nack(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        delay_ms: u64,
        error: Option<&str>,
    ) -> io::Result<FabricQueueNackResult> {
        self.nack_at(
            queue,
            sequence,
            consumer,
            lease_token,
            delay_ms,
            error,
            unix_now_ms()?,
        )
    }

    pub fn nack_at(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        delay_ms: u64,
        error: Option<&str>,
        now_ms: u64,
    ) -> io::Result<FabricQueueNackResult> {
        validate_consumer_name(consumer)?;
        let mut state = self.load_state(queue)?;
        validate_active_job(&state, sequence, consumer, lease_token, now_ms)?;
        let deliveries = state
            .jobs
            .get(&sequence)
            .expect("validated queue job must exist")
            .deliveries;
        let (status, available_at_ms) = if deliveries >= state.config.max_attempts {
            (
                if state.config.dead_letter_queue.is_some() {
                    FabricQueueJobStatus::DeadLettered
                } else {
                    FabricQueueJobStatus::Failed
                },
                None,
            )
        } else {
            (
                FabricQueueJobStatus::Waiting,
                Some(now_ms.saturating_add(delay_ms)),
            )
        };

        let event = QueueMutation::Nacked {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
            status,
            available_at_ms,
            last_error: error.map(ToOwned::to_owned),
        };
        let event_sequence = self.append_mutation(queue, &event)?;
        apply_mutation(&mut state, &event)?;
        state.event_cursor = event_sequence;
        self.write_state(queue, &state)?;
        Ok(FabricQueueNackResult {
            status,
            deliveries,
            available_at_ms,
        })
    }

    pub fn renew(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        extension_ms: u64,
    ) -> io::Result<u64> {
        self.renew_at(
            queue,
            sequence,
            consumer,
            lease_token,
            extension_ms,
            unix_now_ms()?,
        )
    }

    pub fn renew_at(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        extension_ms: u64,
        now_ms: u64,
    ) -> io::Result<u64> {
        if extension_ms == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue lease extension must be greater than zero",
            ));
        }
        validate_consumer_name(consumer)?;
        let mut state = self.load_state(queue)?;
        validate_active_job(&state, sequence, consumer, lease_token, now_ms)?;
        let lease_until_ms = now_ms.saturating_add(extension_ms);
        let event = QueueMutation::LeaseRenewed {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
            lease_until_ms,
        };
        let event_sequence = self.append_mutation(queue, &event)?;
        apply_mutation(&mut state, &event)?;
        state.event_cursor = event_sequence;
        self.write_state(queue, &state)?;
        Ok(lease_until_ms)
    }

    pub fn reap_expired(&mut self, queue: &str) -> io::Result<usize> {
        self.reap_expired_at(queue, unix_now_ms()?)
    }

    pub fn reap_expired_at(&mut self, queue: &str, now_ms: u64) -> io::Result<usize> {
        let mut state = self.load_state(queue)?;
        let changed = self.expire_due(queue, &mut state, now_ms)?;
        if changed > 0 {
            self.write_state(queue, &state)?;
        }
        Ok(changed)
    }

    pub fn info(&mut self, queue: &str) -> io::Result<FabricQueueInfo> {
        self.info_at(queue, unix_now_ms()?)
    }

    pub fn info_at(&mut self, queue: &str, now_ms: u64) -> io::Result<FabricQueueInfo> {
        self.reap_expired_at(queue, now_ms)?;
        let state = self.load_state(queue)?;
        Ok(queue_info_from_state(queue, &state))
    }

    /// Reconstruct an immutable queue view using only quorum-committed stream
    /// prefixes. This deliberately ignores queue_state.json and does not
    /// perform local lease expiry, because either would allow a replica to
    /// expose state that has not passed the replicated mutation boundary.
    pub(crate) fn committed_info(&mut self, queue: &str) -> io::Result<FabricQueueInfo> {
        let state = self.load_committed_state(queue)?;
        Ok(queue_info_from_state(queue, &state))
    }

    fn ensure_stream(&mut self, name: &str) -> io::Result<()> {
        match self
            .streams
            .create_stream(name, FabricStreamConfig::default())
        {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn load_committed_state(&mut self, queue: &str) -> io::Result<QueueStateFile> {
        validate_queue_name(queue)?;
        let payload_stream = queue_stream_name(queue);
        let mutation_stream = queue_mutation_stream_name(queue);
        self.streams.stream_info(&payload_stream)?;
        self.streams.stream_info(&mutation_stream)?;

        let first = self.streams.read_committed(&mutation_stream, 1, 1)?;
        let first = first.first().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue {queue:?} is not visible because QueueCreated is not quorum committed"
                ),
            )
        })?;
        if first.sequence != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "committed Fabric queue mutation prefix does not start at sequence 1",
            ));
        }
        let config = decode_queue_created_mutation(&first.payload)?;
        let mut state = QueueStateFile::new(config);
        state.event_cursor = 1;

        self.reconcile_committed_payloads(queue, &mut state)?;
        self.replay_committed_mutations(queue, &mut state)?;
        Ok(state)
    }

    fn reconcile_committed_payloads(
        &mut self,
        queue: &str,
        state: &mut QueueStateFile,
    ) -> io::Result<()> {
        let stream = queue_stream_name(queue);
        let mut next = 1u64;

        loop {
            let records = self.streams.read_committed(&stream, next, RECONCILE_BATCH)?;
            if records.is_empty() {
                break;
            }
            for record in &records {
                if record.sequence != next {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "committed Fabric queue payload prefix has a gap: expected {next}, found {}",
                            record.sequence
                        ),
                    ));
                }
                let envelope = decode_envelope(record)?;
                state.jobs.insert(
                    record.sequence,
                    QueueJobState {
                        job_id: envelope.job_id,
                        available_at_ms: envelope.available_at_ms,
                        priority: envelope.priority,
                        deliveries: 0,
                        lease_token: 0,
                        status: FabricQueueJobStatus::Waiting,
                        consumer: None,
                        lease_until_ms: None,
                        last_error: None,
                    },
                );
                next = next.saturating_add(1);
            }
            if records.len() < RECONCILE_BATCH {
                break;
            }
        }
        Ok(())
    }

    fn replay_committed_mutations(
        &mut self,
        queue: &str,
        state: &mut QueueStateFile,
    ) -> io::Result<()> {
        let stream = queue_mutation_stream_name(queue);
        let mut next = 2u64;

        loop {
            let records = self.streams.read_committed(&stream, next, RECONCILE_BATCH)?;
            if records.is_empty() {
                break;
            }
            for record in &records {
                if record.sequence != next {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "committed Fabric queue mutation prefix has a gap: expected {next}, found {}",
                            record.sequence
                        ),
                    ));
                }
                let event: QueueMutation =
                    serde_json::from_slice(&record.payload).map_err(json_error)?;
                if matches!(event, QueueMutation::QueueCreated { .. }) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Fabric queue mutation log contains duplicate QueueCreated event",
                    ));
                }
                apply_mutation(state, &event)?;
                state.event_cursor = record.sequence;
                next = next.saturating_add(1);
            }
            if records.len() < RECONCILE_BATCH {
                break;
            }
        }
        Ok(())
    }

    fn load_state(&mut self, queue: &str) -> io::Result<QueueStateFile> {
        validate_queue_name(queue)?;
        self.streams.stream_info(&queue_stream_name(queue))?;
        self.streams
            .stream_info(&queue_mutation_stream_name(queue))?;

        let path = self.state_path(queue);
        let mut state = match fs::read(&path) {
            Ok(bytes) => {
                let state: QueueStateFile =
                    serde_json::from_slice(&bytes).map_err(json_error)?;
                if state.version != QUEUE_FORMAT_VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "unsupported Fabric queue format version {}",
                            state.version
                        ),
                    ));
                }
                state.config.validate()?;
                state
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.rebuild_state_from_logs(queue)?
            }
            Err(error) => return Err(error),
        };

        let payload_changed = self.reconcile_payloads(queue, &mut state)?;
        let mutation_changed = self.replay_mutations(queue, &mut state)?;
        if payload_changed || mutation_changed || !path.exists() {
            self.write_state(queue, &state)?;
        }
        Ok(state)
    }

    fn rebuild_state_from_logs(&mut self, queue: &str) -> io::Result<QueueStateFile> {
        let events = self
            .streams
            .read_from(&queue_mutation_stream_name(queue), 1, 1)?;
        let first = events.first().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric queue mutation stream is missing QueueCreated event",
            )
        })?;
        let event: QueueMutation = serde_json::from_slice(&first.payload).map_err(json_error)?;
        let QueueMutation::QueueCreated { config } = event else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "first Fabric queue mutation must be QueueCreated",
            ));
        };
        config.validate()?;
        Ok(QueueStateFile::new(config))
    }

    fn reconcile_payloads(
        &mut self,
        queue: &str,
        state: &mut QueueStateFile,
    ) -> io::Result<bool> {
        let stream = queue_stream_name(queue);
        // queue_state.json is a cache, so scan from the durable beginning and
        // repair any missing materialized job entry rather than trusting the
        // cache's highest key to imply an intact prefix.
        let mut next = 1;
        let mut changed = false;

        loop {
            let records = self.streams.read_from(&stream, next, RECONCILE_BATCH)?;
            if records.is_empty() {
                break;
            }
            for record in &records {
                if !state.jobs.contains_key(&record.sequence) {
                    let envelope = decode_envelope(record)?;
                    state.jobs.insert(
                        record.sequence,
                        QueueJobState {
                            job_id: envelope.job_id,
                            available_at_ms: envelope.available_at_ms,
                            priority: envelope.priority,
                            deliveries: 0,
                            lease_token: 0,
                            status: FabricQueueJobStatus::Waiting,
                            consumer: None,
                            lease_until_ms: None,
                            last_error: None,
                        },
                    );
                    changed = true;
                }
                next = record.sequence.saturating_add(1);
            }
            if records.len() < RECONCILE_BATCH {
                break;
            }
        }
        let tail = self
            .streams
            .stream_info(&stream)?
            .last_sequence
            .unwrap_or(0);
        if state.jobs.keys().next_back().copied().unwrap_or(0) > tail
            || state.jobs.len() as u64 != tail
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric queue materialized job index does not match durable payload stream",
            ));
        }
        Ok(changed)
    }

    fn replay_mutations(
        &mut self,
        queue: &str,
        state: &mut QueueStateFile,
    ) -> io::Result<bool> {
        let stream = queue_mutation_stream_name(queue);
        let tail = self
            .streams
            .stream_info(&stream)?
            .last_sequence
            .unwrap_or(0);
        if state.event_cursor > tail {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Fabric queue event cursor {} is beyond mutation stream tail {tail}",
                    state.event_cursor
                ),
            ));
        }
        let mut next = state.event_cursor.saturating_add(1).max(1);
        let mut changed = false;

        loop {
            let records = self.streams.read_from(&stream, next, RECONCILE_BATCH)?;
            if records.is_empty() {
                break;
            }
            for record in &records {
                let event: QueueMutation =
                    serde_json::from_slice(&record.payload).map_err(json_error)?;
                apply_mutation(state, &event)?;
                state.event_cursor = record.sequence;
                next = record.sequence.saturating_add(1);
                changed = true;
            }
            if records.len() < RECONCILE_BATCH {
                break;
            }
        }
        Ok(changed)
    }

    fn append_mutation(&mut self, queue: &str, event: &QueueMutation) -> io::Result<u64> {
        let bytes = serde_json::to_vec(event).map_err(json_error)?;
        self.streams
            .append(&queue_mutation_stream_name(queue), &bytes)
    }

    fn expire_due(
        &mut self,
        queue: &str,
        state: &mut QueueStateFile,
        now_ms: u64,
    ) -> io::Result<usize> {
        let expired: Vec<(u64, String, u64, u32)> = state
            .jobs
            .iter()
            .filter_map(|(&sequence, job)| {
                if job.status == FabricQueueJobStatus::Active
                    && job.lease_until_ms.is_some_and(|deadline| deadline <= now_ms)
                {
                    Some((
                        sequence,
                        job.consumer.clone().unwrap_or_default(),
                        job.lease_token,
                        job.deliveries,
                    ))
                } else {
                    None
                }
            })
            .collect();

        for (sequence, consumer, lease_token, deliveries) in &expired {
            let (status, available_at_ms) = if *deliveries >= state.config.max_attempts {
                (
                    if state.config.dead_letter_queue.is_some() {
                        FabricQueueJobStatus::DeadLettered
                    } else {
                        FabricQueueJobStatus::Failed
                    },
                    None,
                )
            } else {
                (FabricQueueJobStatus::Waiting, Some(now_ms))
            };
            let event = QueueMutation::LeaseExpired {
                sequence: *sequence,
                consumer: consumer.clone(),
                lease_token: *lease_token,
                status,
                available_at_ms,
            };
            let event_sequence = self.append_mutation(queue, &event)?;
            apply_mutation(state, &event)?;
            state.event_cursor = event_sequence;
        }
        Ok(expired.len())
    }

    fn read_envelope(&mut self, queue: &str, sequence: u64) -> io::Result<QueueEnvelope> {
        let records = self
            .streams
            .read_from(&queue_stream_name(queue), sequence, 1)?;
        match records.first() {
            Some(record) if record.sequence == sequence => decode_envelope(record),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Fabric queue record {sequence} is missing"),
            )),
        }
    }

    fn read_committed_envelope(
        &mut self,
        queue: &str,
        sequence: u64,
    ) -> io::Result<QueueEnvelope> {
        let records = self
            .streams
            .read_committed(&queue_stream_name(queue), sequence, 1)?;
        match records.first() {
            Some(record) if record.sequence == sequence => decode_envelope(record),
            _ => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue record {sequence} is not quorum committed"
                ),
            )),
        }
    }

    fn state_path(&self, queue: &str) -> PathBuf {
        self.streams
            .root()
            .join(queue_stream_name(queue))
            .join(QUEUE_STATE_FILE)
    }

    fn write_state(&self, queue: &str, state: &QueueStateFile) -> io::Result<()> {
        write_json_atomic(&self.state_path(queue), state)
    }
}

impl Runtime {
    fn fabric_queue_store(&mut self) -> io::Result<FabricQueueStore<'_>> {
        let streams = self.distributed.fabric_streams.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream storage is not open; call fabric_stream_open first",
            )
        })?;
        Ok(FabricQueueStore::new(streams))
    }

    fn fabric_queue_require_local_mode(&mut self, queue: &str) -> io::Result<()> {
        if self.fabric_queue_has_replication_policy(queue)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Fabric queue {queue:?} has a replication policy; local queue APIs are disabled until the replicated operation path is used"
                ),
            ));
        }
        Ok(())
    }

    fn fabric_queue_local_store(&mut self, queue: &str) -> io::Result<FabricQueueStore<'_>> {
        self.fabric_queue_require_local_mode(queue)?;
        self.fabric_queue_store()
    }

    pub fn fabric_queue_create(
        &mut self,
        name: &str,
        config: FabricQueueConfig,
    ) -> io::Result<()> {
        self.fabric_queue_local_store(name)?.create_queue(name, config)
    }

    pub fn fabric_queue_add(
        &mut self,
        queue: &str,
        name: &str,
        payload: &[u8],
        options: FabricQueueAddOptions,
    ) -> io::Result<FabricQueueAddResult> {
        self.fabric_queue_local_store(queue)?
            .add(queue, name, payload, options)
    }

    pub fn fabric_queue_add_at(
        &mut self,
        queue: &str,
        name: &str,
        payload: &[u8],
        options: FabricQueueAddOptions,
        now_ms: u64,
    ) -> io::Result<FabricQueueAddResult> {
        self.fabric_queue_local_store(queue)?
            .add_at(queue, name, payload, options, now_ms)
    }

    pub fn fabric_queue_acquire(
        &mut self,
        queue: &str,
        consumer: &str,
    ) -> io::Result<Option<FabricQueueDelivery>> {
        self.fabric_queue_local_store(queue)?.acquire(queue, consumer)
    }

    pub fn fabric_queue_acquire_at(
        &mut self,
        queue: &str,
        consumer: &str,
        now_ms: u64,
    ) -> io::Result<Option<FabricQueueDelivery>> {
        self.fabric_queue_local_store(queue)?
            .acquire_at(queue, consumer, now_ms)
    }

    pub fn fabric_queue_ack(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
    ) -> io::Result<()> {
        self.fabric_queue_local_store(queue)?
            .ack(queue, sequence, consumer, lease_token)
    }

    pub fn fabric_queue_ack_at(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        now_ms: u64,
    ) -> io::Result<()> {
        self.fabric_queue_local_store(queue)?
            .ack_at(queue, sequence, consumer, lease_token, now_ms)
    }

    pub fn fabric_queue_nack(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        delay_ms: u64,
        error: Option<&str>,
    ) -> io::Result<FabricQueueNackResult> {
        self.fabric_queue_local_store(queue)?
            .nack(queue, sequence, consumer, lease_token, delay_ms, error)
    }

    pub fn fabric_queue_nack_at(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        delay_ms: u64,
        error: Option<&str>,
        now_ms: u64,
    ) -> io::Result<FabricQueueNackResult> {
        self.fabric_queue_local_store(queue)?
            .nack_at(queue, sequence, consumer, lease_token, delay_ms, error, now_ms)
    }

    pub fn fabric_queue_renew(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        extension_ms: u64,
    ) -> io::Result<u64> {
        self.fabric_queue_local_store(queue)?
            .renew(queue, sequence, consumer, lease_token, extension_ms)
    }

    pub fn fabric_queue_renew_at(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        extension_ms: u64,
        now_ms: u64,
    ) -> io::Result<u64> {
        self.fabric_queue_local_store(queue)?
            .renew_at(queue, sequence, consumer, lease_token, extension_ms, now_ms)
    }

    pub fn fabric_queue_reap_expired(&mut self, queue: &str) -> io::Result<usize> {
        self.fabric_queue_local_store(queue)?.reap_expired(queue)
    }

    pub fn fabric_queue_reap_expired_at(
        &mut self,
        queue: &str,
        now_ms: u64,
    ) -> io::Result<usize> {
        self.fabric_queue_local_store(queue)?
            .reap_expired_at(queue, now_ms)
    }

    /// Inspect a replicated queue using only quorum-committed payload and
    /// mutation prefixes. Uncommitted local tails are never decoded or exposed.
    pub fn fabric_queue_info_replicated(
        &mut self,
        queue: &str,
    ) -> io::Result<FabricQueueInfo> {
        if !self.fabric_queue_has_replication_policy(queue)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Fabric queue {queue:?} does not have a replication policy"),
            ));
        }
        self.fabric_queue_store()?.committed_info(queue)
    }

    pub(crate) fn fabric_queue_plan_committed_lease(
        &mut self,
        queue: &str,
        consumer: &str,
        operation_id: &str,
        queue_epoch: u64,
        now_ms: u64,
    ) -> io::Result<Option<FabricQueueLeasePlan>> {
        validate_queue_name(queue)?;
        validate_consumer_name(consumer)?;
        validate_operation_id(operation_id)?;
        if queue_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replicated Fabric queue lease requires a non-zero queue epoch",
            ));
        }

        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        let mut candidate: Option<(u64, i32)> = None;
        for (&sequence, job) in &state.jobs {
            if job.status != FabricQueueJobStatus::Waiting
                || job.available_at_ms > now_ms
                || job.deliveries >= state.config.max_attempts
            {
                continue;
            }
            match candidate {
                None => candidate = Some((sequence, job.priority)),
                Some((best_sequence, best_priority))
                    if job.priority > best_priority
                        || (job.priority == best_priority && sequence < best_sequence) =>
                {
                    candidate = Some((sequence, job.priority));
                }
                _ => {}
            }
        }

        let Some((sequence, priority)) = candidate else {
            return Ok(None);
        };
        let current = state.jobs.get(&sequence).expect("candidate must exist");
        let deliveries = current.deliveries.saturating_add(1);
        let lease_token = current.lease_token.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "Fabric queue lease token overflow")
        })?;
        let lease_until_ms = now_ms.saturating_add(state.config.visibility_timeout_ms);
        let mutation_bytes = serde_json::to_vec(&QueueMutation::LeaseAcquired {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
            lease_until_ms,
            deliveries,
            queue_epoch,
            operation_id: Some(operation_id.to_string()),
        })
        .map_err(json_error)?;
        let envelope = store.read_committed_envelope(queue, sequence)?;
        let delivery = FabricQueueDelivery {
            sequence,
            queue_epoch,
            job_id: envelope.job_id.unwrap_or_else(|| sequence.to_string()),
            name: envelope.name,
            payload: envelope.payload,
            priority,
            deliveries,
            lease_token,
            lease_until_ms,
        };
        Ok(Some(FabricQueueLeasePlan {
            mutation_bytes,
            delivery,
        }))
    }

    pub(crate) fn fabric_queue_delivery_for_committed_lease(
        &mut self,
        queue: &str,
        lease: &FabricQueueLeaseMutation,
    ) -> io::Result<FabricQueueDelivery> {
        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        let job = state.jobs.get(&lease.sequence).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "committed Fabric queue lease references missing job {}",
                    lease.sequence
                ),
            )
        })?;
        if job.status != FabricQueueJobStatus::Active
            || job.consumer.as_deref() != Some(lease.consumer.as_str())
            || job.lease_token != lease.lease_token
            || job.deliveries != lease.deliveries
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "committed Fabric queue state does not match lease operation for job {}",
                    lease.sequence
                ),
            ));
        }
        let envelope = store.read_committed_envelope(queue, lease.sequence)?;
        Ok(FabricQueueDelivery {
            sequence: lease.sequence,
            queue_epoch: lease.queue_epoch,
            job_id: envelope
                .job_id
                .unwrap_or_else(|| lease.sequence.to_string()),
            name: envelope.name,
            payload: envelope.payload,
            priority: job.priority,
            deliveries: lease.deliveries,
            lease_token: lease.lease_token,
            lease_until_ms: lease.lease_until_ms,
        })
    }

    pub fn fabric_queue_info(&mut self, queue: &str) -> io::Result<FabricQueueInfo> {
        self.fabric_queue_local_store(queue)?.info(queue)
    }

    pub fn fabric_queue_info_at(
        &mut self,
        queue: &str,
        now_ms: u64,
    ) -> io::Result<FabricQueueInfo> {
        self.fabric_queue_local_store(queue)?.info_at(queue, now_ms)
    }
}

fn queue_info_from_state(queue: &str, state: &QueueStateFile) -> FabricQueueInfo {
    let mut info = FabricQueueInfo {
        name: queue.to_string(),
        waiting: 0,
        active: 0,
        completed: 0,
        failed: 0,
        dead_lettered: 0,
        total: state.jobs.len(),
    };
    for job in state.jobs.values() {
        match job.status {
            FabricQueueJobStatus::Waiting => info.waiting += 1,
            FabricQueueJobStatus::Active => info.active += 1,
            FabricQueueJobStatus::Completed => info.completed += 1,
            FabricQueueJobStatus::Failed => info.failed += 1,
            FabricQueueJobStatus::DeadLettered => info.dead_lettered += 1,
        }
    }
    info
}

fn validate_active_job(
    state: &QueueStateFile,
    sequence: u64,
    consumer: &str,
    lease_token: u64,
    now_ms: u64,
) -> io::Result<()> {
    let job = state.jobs.get(&sequence).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("Fabric queue job {sequence} does not exist"),
        )
    })?;
    if job.status != FabricQueueJobStatus::Active {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Fabric queue job {sequence} is not active"),
        ));
    }
    if job.consumer.as_deref() != Some(consumer) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Fabric queue job {sequence} is leased by another consumer"),
        ));
    }
    if job.lease_token != lease_token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Fabric queue job {sequence} lease token is stale"),
        ));
    }
    if job.lease_until_ms.is_none_or(|deadline| deadline <= now_ms) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Fabric queue lease for job {sequence} has expired"),
        ));
    }
    Ok(())
}

fn apply_mutation(state: &mut QueueStateFile, event: &QueueMutation) -> io::Result<()> {
    match event {
        QueueMutation::QueueCreated { config } => {
            if &state.config != config {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric queue QueueCreated config conflicts with materialized state",
                ));
            }
        }
        QueueMutation::LeaseAcquired {
            sequence,
            consumer,
            lease_token,
            lease_until_ms,
            deliveries,
            ..
        } => {
            let job = state.jobs.get_mut(sequence).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Fabric queue lease references missing job {sequence}"),
                )
            })?;
            if job.status != FabricQueueJobStatus::Waiting
                || *lease_token != job.lease_token.saturating_add(1)
                || *deliveries != job.deliveries.saturating_add(1)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid Fabric queue LeaseAcquired transition for job {sequence}"),
                ));
            }
            job.status = FabricQueueJobStatus::Active;
            job.consumer = Some(consumer.clone());
            job.lease_token = *lease_token;
            job.lease_until_ms = Some(*lease_until_ms);
            job.deliveries = *deliveries;
        }
        QueueMutation::Completed {
            sequence,
            consumer,
            lease_token,
        } => {
            validate_mutation_lease(state, *sequence, consumer, *lease_token)?;
            let job = state.jobs.get_mut(sequence).expect("validated job must exist");
            job.status = FabricQueueJobStatus::Completed;
            job.consumer = None;
            job.lease_until_ms = None;
            job.last_error = None;
        }
        QueueMutation::Nacked {
            sequence,
            consumer,
            lease_token,
            status,
            available_at_ms,
            last_error,
        } => {
            validate_mutation_lease(state, *sequence, consumer, *lease_token)?;
            if !matches!(
                status,
                FabricQueueJobStatus::Waiting
                    | FabricQueueJobStatus::Failed
                    | FabricQueueJobStatus::DeadLettered
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Fabric queue NACK target state",
                ));
            }
            let job = state.jobs.get_mut(sequence).expect("validated job must exist");
            job.status = *status;
            job.consumer = None;
            job.lease_until_ms = None;
            job.available_at_ms = available_at_ms.unwrap_or(job.available_at_ms);
            job.last_error = last_error.clone();
        }
        QueueMutation::LeaseRenewed {
            sequence,
            consumer,
            lease_token,
            lease_until_ms,
        } => {
            validate_mutation_lease(state, *sequence, consumer, *lease_token)?;
            let job = state.jobs.get_mut(sequence).expect("validated job must exist");
            job.lease_until_ms = Some(*lease_until_ms);
        }
        QueueMutation::LeaseExpired {
            sequence,
            consumer,
            lease_token,
            status,
            available_at_ms,
        } => {
            validate_mutation_lease(state, *sequence, consumer, *lease_token)?;
            if !matches!(
                status,
                FabricQueueJobStatus::Waiting
                    | FabricQueueJobStatus::Failed
                    | FabricQueueJobStatus::DeadLettered
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Fabric queue lease-expiry target state",
                ));
            }
            let job = state.jobs.get_mut(sequence).expect("validated job must exist");
            job.status = *status;
            job.consumer = None;
            job.lease_until_ms = None;
            job.available_at_ms = available_at_ms.unwrap_or(job.available_at_ms);
        }
    }
    Ok(())
}

fn validate_mutation_lease(
    state: &QueueStateFile,
    sequence: u64,
    consumer: &str,
    lease_token: u64,
) -> io::Result<()> {
    let job = state.jobs.get(&sequence).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Fabric queue mutation references missing job {sequence}"),
        )
    })?;
    if job.status != FabricQueueJobStatus::Active
        || job.consumer.as_deref() != Some(consumer)
        || job.lease_token != lease_token
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Fabric queue mutation has stale lease for job {sequence}"),
        ));
    }
    Ok(())
}

fn decode_envelope(record: &FabricStreamRecord) -> io::Result<QueueEnvelope> {
    serde_json::from_slice(&record.payload).map_err(json_error)
}

pub(crate) fn queue_stream_name(queue: &str) -> String {
    format!("{QUEUE_STREAM_PREFIX}{queue}")
}

pub(crate) fn queue_mutation_stream_name(queue: &str) -> String {
    format!("{QUEUE_MUTATION_STREAM_PREFIX}{queue}")
}

pub(crate) fn validate_queue_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 96
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric queue names must be 1..=96 ASCII letters, digits, '.', '_' or '-'",
        ));
    }
    Ok(())
}

pub(crate) fn validate_consumer_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric queue consumer names must be 1..=128 bytes",
        ));
    }
    Ok(())
}

fn validate_operation_id(operation_id: &str) -> io::Result<()> {
    if operation_id.is_empty() || operation_id.len() > 256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric queue operation ids must be 1..=256 bytes",
        ));
    }
    Ok(())
}

fn validate_job_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric queue job names must be 1..=128 bytes",
        ));
    }
    Ok(())
}

fn validate_job_id(job_id: &str) -> io::Result<()> {
    if job_id.is_empty() || job_id.len() > 256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric queue job ids must be 1..=256 bytes",
        ));
    }
    Ok(())
}

fn unix_now_ms() -> io::Result<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "system clock is before Unix epoch"))?;
    Ok(duration.as_millis().min(u64::MAX as u128) as u64)
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric queue state path has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("queue-state"),
        std::process::id(),
        counter
    ));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        serde_json::to_writer(&mut file, value).map_err(json_error)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nulang-fabric-queue-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn lease_ack_and_priority_are_durable() {
        let root = test_dir("ack");
        let mut streams = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut streams);
        queues
            .create_queue("email", FabricQueueConfig::default())
            .unwrap();

        queues
            .add_at(
                "email",
                "low",
                b"low",
                FabricQueueAddOptions {
                    priority: 1,
                    ..Default::default()
                },
                100,
            )
            .unwrap();
        let high = queues
            .add_at(
                "email",
                "high",
                b"high",
                FabricQueueAddOptions {
                    priority: 10,
                    ..Default::default()
                },
                100,
            )
            .unwrap();

        let delivery = queues.acquire_at("email", "worker-a", 100).unwrap().unwrap();
        assert_eq!(delivery.sequence, high.sequence);
        assert_eq!(delivery.payload, b"high");
        queues
            .ack_at(
                "email",
                delivery.sequence,
                "worker-a",
                delivery.lease_token,
                101,
            )
            .unwrap();

        drop(queues);
        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut reopened);
        let info = queues.info_at("email", 101).unwrap();
        assert_eq!(info.completed, 1);
        assert_eq!(info.waiting, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn expired_lease_redelivers_then_exhausts_attempts() {
        let root = test_dir("redelivery");
        let mut streams = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut streams);
        queues
            .create_queue(
                "jobs",
                FabricQueueConfig {
                    visibility_timeout_ms: 10,
                    max_attempts: 2,
                    dead_letter_queue: None,
                },
            )
            .unwrap();
        queues
            .add_at("jobs", "work", b"x", FabricQueueAddOptions::default(), 0)
            .unwrap();

        let first = queues.acquire_at("jobs", "a", 0).unwrap().unwrap();
        assert_eq!(first.deliveries, 1);
        let second = queues.acquire_at("jobs", "b", 10).unwrap().unwrap();
        assert_eq!(second.deliveries, 2);
        assert!(queues.acquire_at("jobs", "c", 20).unwrap().is_none());
        let info = queues.info_at("jobs", 20).unwrap();
        assert_eq!(info.failed, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stale_same_consumer_delivery_cannot_ack_new_lease() {
        let root = test_dir("fencing");
        let mut streams = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut streams);
        queues
            .create_queue(
                "jobs",
                FabricQueueConfig {
                    visibility_timeout_ms: 10,
                    max_attempts: 3,
                    dead_letter_queue: None,
                },
            )
            .unwrap();
        queues
            .add_at("jobs", "work", b"x", FabricQueueAddOptions::default(), 0)
            .unwrap();

        let first = queues.acquire_at("jobs", "worker", 0).unwrap().unwrap();
        let second = queues.acquire_at("jobs", "worker", 10).unwrap().unwrap();
        assert!(second.lease_token > first.lease_token);

        let stale = queues.ack_at(
            "jobs",
            first.sequence,
            "worker",
            first.lease_token,
            11,
        );
        assert_eq!(stale.unwrap_err().kind(), io::ErrorKind::PermissionDenied);

        queues
            .ack_at(
                "jobs",
                second.sequence,
                "worker",
                second.lease_token,
                11,
            )
            .unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn delayed_and_deduplicated_jobs_follow_native_semantics() {
        let root = test_dir("delay-dedupe");
        let mut streams = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut streams);
        queues
            .create_queue("jobs", FabricQueueConfig::default())
            .unwrap();

        let options = FabricQueueAddOptions {
            job_id: Some("invoice-42".to_string()),
            delay_ms: 50,
            ..Default::default()
        };
        let first = queues
            .add_at("jobs", "invoice", b"one", options.clone(), 100)
            .unwrap();
        let duplicate = queues
            .add_at("jobs", "invoice", b"two", options, 101)
            .unwrap();
        assert_eq!(first.sequence, duplicate.sequence);
        assert!(duplicate.deduplicated);
        assert!(queues.acquire_at("jobs", "w", 149).unwrap().is_none());
        assert_eq!(
            queues
                .acquire_at("jobs", "w", 150)
                .unwrap()
                .unwrap()
                .payload,
            b"one"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn create_queue_is_retry_safe_when_config_matches() {
        let root = test_dir("create-retry");
        let mut streams = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut streams);
        let config = FabricQueueConfig {
            visibility_timeout_ms: 123,
            max_attempts: 4,
            dead_letter_queue: None,
        };
        queues.create_queue("jobs", config.clone()).unwrap();
        queues.create_queue("jobs", config).unwrap();

        let mismatch = queues.create_queue(
            "jobs",
            FabricQueueConfig {
                visibility_timeout_ms: 999,
                max_attempts: 4,
                dead_letter_queue: None,
            },
        );
        assert_eq!(mismatch.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn deleted_materialized_state_rebuilds_from_payload_and_mutation_logs() {
        let root = test_dir("rebuild-state");
        let mut streams = FileFabricStreamStore::open(&root).unwrap();
        {
            let mut queues = FabricQueueStore::new(&mut streams);
            queues
                .create_queue("jobs", FabricQueueConfig::default())
                .unwrap();
            queues
                .add_at("jobs", "work", b"x", FabricQueueAddOptions::default(), 0)
                .unwrap();
            let delivery = queues.acquire_at("jobs", "worker", 0).unwrap().unwrap();
            queues
                .ack_at(
                    "jobs",
                    delivery.sequence,
                    "worker",
                    delivery.lease_token,
                    1,
                )
                .unwrap();
        }

        fs::remove_file(
            root.join(queue_stream_name("jobs"))
                .join(QUEUE_STATE_FILE),
        )
        .unwrap();

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut reopened);
        let info = queues.info_at("jobs", 1).unwrap();
        assert_eq!(info.completed, 1);
        assert_eq!(info.total, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mutation_after_cache_write_boundary_replays_on_restart() {
        let root = test_dir("mutation-replay");
        let mut streams = FileFabricStreamStore::open(&root).unwrap();
        let delivery = {
            let mut queues = FabricQueueStore::new(&mut streams);
            queues
                .create_queue("jobs", FabricQueueConfig::default())
                .unwrap();
            queues
                .add_at("jobs", "work", b"x", FabricQueueAddOptions::default(), 0)
                .unwrap();
            queues.acquire_at("jobs", "worker", 0).unwrap().unwrap()
        };

        {
            let mut queues = FabricQueueStore::new(&mut streams);
            queues
                .append_mutation(
                    "jobs",
                    &QueueMutation::Completed {
                        sequence: delivery.sequence,
                        consumer: "worker".to_string(),
                        lease_token: delivery.lease_token,
                    },
                )
                .unwrap();
            // Intentionally do not update queue_state.json: simulate a crash
            // after the mutation event fsync and before cache replacement.
        }

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut reopened);
        let info = queues.info_at("jobs", 1).unwrap();
        assert_eq!(info.completed, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reconciliation_recovers_append_before_state_update() {
        let root = test_dir("reconcile");
        let mut streams = FileFabricStreamStore::open(&root).unwrap();
        {
            let mut queues = FabricQueueStore::new(&mut streams);
            queues
                .create_queue("jobs", FabricQueueConfig::default())
                .unwrap();
        }

        let envelope = QueueEnvelope {
            name: "recovered".to_string(),
            payload: b"payload".to_vec(),
            job_id: Some("stable".to_string()),
            priority: 0,
            created_at_ms: 10,
            available_at_ms: 10,
        };
        streams
            .append(
                &queue_stream_name("jobs"),
                &serde_json::to_vec(&envelope).unwrap(),
            )
            .unwrap();

        let mut queues = FabricQueueStore::new(&mut streams);
        let delivery = queues.acquire_at("jobs", "worker", 10).unwrap().unwrap();
        assert_eq!(delivery.job_id, "stable");
        assert_eq!(delivery.payload, b"payload");
        let _ = fs::remove_dir_all(root);
    }
}
