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
    /// Optional logical dead-letter destination. Replicated atomic handoff
    /// requires the destination queue to share this queue's ownership policy.
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
    /// Optional per-job delivery-attempt cap. When omitted the queue-level
    /// max_attempts remains authoritative.
    pub max_attempts: Option<u32>,
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
    /// Optional durable worker concurrency domain. This is not a fan-out
    /// subscription group; workers in one group still compete for queue jobs.
    pub consumer_group: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricQueueConsumerGroupConfig {
    pub name: String,
    pub max_concurrency: usize,
}

impl FabricQueueConsumerGroupConfig {
    pub fn new(name: impl Into<String>, max_concurrency: usize) -> io::Result<Self> {
        let config = Self {
            name: name.into(),
            max_concurrency,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> io::Result<()> {
        validate_consumer_group_name(&self.name)?;
        if self.max_concurrency == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue consumer-group max_concurrency must be greater than zero",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueConsumerGroupInfo {
    pub name: String,
    pub max_concurrency: usize,
    pub active: usize,
}

impl FabricQueueConsumerGroupInfo {
    pub fn available(&self) -> usize {
        self.max_concurrency.saturating_sub(self.active)
    }

    pub fn saturated(&self) -> bool {
        self.active >= self.max_concurrency
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueJobInfo {
    pub sequence: u64,
    pub job_id: String,
    pub name: String,
    pub payload: Vec<u8>,
    pub priority: i32,
    pub status: FabricQueueJobStatus,
    pub deliveries: u32,
    pub available_at_ms: u64,
    pub lease_until_ms: Option<u64>,
    pub last_error: Option<String>,
    pub result: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueReadySignal {
    pub ready: bool,
    pub next_available_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueueEnvelope {
    pub(crate) name: String,
    pub(crate) payload: Vec<u8>,
    pub(crate) job_id: Option<String>,
    pub(crate) priority: i32,
    pub(crate) created_at_ms: u64,
    pub(crate) available_at_ms: u64,
    #[serde(default)]
    pub(crate) max_attempts: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueJobState {
    job_id: Option<String>,
    available_at_ms: u64,
    priority: i32,
    #[serde(default)]
    max_attempts: Option<u32>,
    deliveries: u32,
    #[serde(default)]
    lease_token: u64,
    status: FabricQueueJobStatus,
    consumer: Option<String>,
    #[serde(default)]
    consumer_group: Option<String>,
    lease_until_ms: Option<u64>,
    last_error: Option<String>,
    #[serde(default)]
    result: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueStateFile {
    version: u16,
    config: FabricQueueConfig,
    #[serde(default)]
    event_cursor: u64,
    #[serde(default)]
    consumer_groups: BTreeMap<String, FabricQueueConsumerGroupConfig>,
    jobs: BTreeMap<u64, QueueJobState>,
}

impl QueueStateFile {
    fn new(config: FabricQueueConfig) -> Self {
        Self {
            version: QUEUE_FORMAT_VERSION,
            config,
            event_cursor: 0,
            consumer_groups: BTreeMap::new(),
            jobs: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum QueueMutation {
    QueueCreated {
        config: FabricQueueConfig,
    },
    ConsumerGroupConfigured {
        config: FabricQueueConsumerGroupConfig,
    },
    LeaseAcquired {
        sequence: u64,
        consumer: String,
        lease_token: u64,
        lease_until_ms: u64,
        #[serde(default)]
        lease_duration_ms: u64,
        deliveries: u32,
        #[serde(default)]
        consumer_group: Option<String>,
        #[serde(default)]
        queue_epoch: u64,
        #[serde(default)]
        operation_id: Option<String>,
    },
    Completed {
        sequence: u64,
        consumer: String,
        lease_token: u64,
        #[serde(default)]
        queue_epoch: u64,
        #[serde(default)]
        operation_id: Option<String>,
        #[serde(default)]
        result: Option<Vec<u8>>,
    },
    Nacked {
        sequence: u64,
        consumer: String,
        lease_token: u64,
        status: FabricQueueJobStatus,
        available_at_ms: Option<u64>,
        last_error: Option<String>,
        #[serde(default)]
        queue_epoch: u64,
        #[serde(default)]
        operation_id: Option<String>,
    },
    LeaseRenewed {
        sequence: u64,
        consumer: String,
        lease_token: u64,
        lease_until_ms: u64,
        #[serde(default)]
        queue_epoch: u64,
        #[serde(default)]
        operation_id: Option<String>,
    },
    LeaseExpired {
        sequence: u64,
        consumer: String,
        lease_token: u64,
        status: FabricQueueJobStatus,
        available_at_ms: Option<u64>,
        #[serde(default)]
        queue_epoch: u64,
        #[serde(default)]
        operation_id: Option<String>,
    },
}

pub(crate) fn encode_queue_created_mutation(config: &FabricQueueConfig) -> io::Result<Vec<u8>> {
    config.validate()?;
    serde_json::to_vec(&QueueMutation::QueueCreated {
        config: config.clone(),
    })
    .map_err(json_error)
}

pub(crate) fn decode_queue_created_mutation(bytes: &[u8]) -> io::Result<FabricQueueConfig> {
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

pub(crate) fn decode_queue_consumer_group_config(
    bytes: &[u8],
) -> io::Result<Option<FabricQueueConsumerGroupConfig>> {
    let event: QueueMutation = serde_json::from_slice(bytes).map_err(json_error)?;
    match event {
        QueueMutation::ConsumerGroupConfigured { config } => {
            config.validate()?;
            Ok(Some(config))
        }
        _ => Ok(None),
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
    if options.max_attempts == Some(0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric queue per-job max_attempts must be greater than zero",
        ));
    }
    serde_json::to_vec(&QueueEnvelope {
        name: name.to_string(),
        payload: payload.to_vec(),
        job_id: options.job_id.clone(),
        priority: options.priority,
        created_at_ms: now_ms,
        available_at_ms: now_ms.saturating_add(options.delay_ms),
        max_attempts: options.max_attempts,
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
    pub lease_duration_ms: u64,
    pub deliveries: u32,
    pub consumer_group: Option<String>,
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
            lease_duration_ms,
            deliveries,
            consumer_group,
            queue_epoch,
            operation_id,
        } => Ok(Some(FabricQueueLeaseMutation {
            sequence,
            consumer,
            lease_token,
            lease_until_ms,
            lease_duration_ms,
            deliveries,
            consumer_group,
            queue_epoch,
            operation_id,
        })),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FabricQueueOperationKind {
    Acquire,
    Ack,
    Nack,
    Renew,
    Expire,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FabricQueueOperation {
    pub kind: FabricQueueOperationKind,
    pub sequence: u64,
    pub consumer: String,
    pub lease_token: u64,
    pub queue_epoch: u64,
    pub consumer_group: Option<String>,
    pub operation_id: Option<String>,
    pub status: Option<FabricQueueJobStatus>,
    pub available_at_ms: Option<u64>,
    pub lease_until_ms: Option<u64>,
    pub result: Option<Vec<u8>>,
}

pub(crate) fn decode_queue_operation(bytes: &[u8]) -> io::Result<Option<FabricQueueOperation>> {
    let event: QueueMutation = serde_json::from_slice(bytes).map_err(json_error)?;
    let operation = match event {
        QueueMutation::LeaseAcquired {
            sequence,
            consumer,
            lease_token,
            lease_until_ms,
            consumer_group,
            queue_epoch,
            operation_id,
            ..
        } => FabricQueueOperation {
            kind: FabricQueueOperationKind::Acquire,
            sequence,
            consumer,
            lease_token,
            queue_epoch,
            consumer_group,
            operation_id,
            status: None,
            available_at_ms: None,
            lease_until_ms: Some(lease_until_ms),
            result: None,
        },
        QueueMutation::Completed {
            sequence,
            consumer,
            lease_token,
            queue_epoch,
            operation_id,
            result,
        } => FabricQueueOperation {
            kind: FabricQueueOperationKind::Ack,
            sequence,
            consumer,
            lease_token,
            queue_epoch,
            consumer_group: None,
            operation_id,
            status: Some(FabricQueueJobStatus::Completed),
            available_at_ms: None,
            lease_until_ms: None,
            result,
        },
        QueueMutation::Nacked {
            sequence,
            consumer,
            lease_token,
            status,
            available_at_ms,
            queue_epoch,
            operation_id,
            ..
        } => FabricQueueOperation {
            kind: FabricQueueOperationKind::Nack,
            sequence,
            consumer,
            lease_token,
            queue_epoch,
            consumer_group: None,
            operation_id,
            status: Some(status),
            available_at_ms,
            lease_until_ms: None,
            result: None,
        },
        QueueMutation::LeaseRenewed {
            sequence,
            consumer,
            lease_token,
            lease_until_ms,
            queue_epoch,
            operation_id,
        } => FabricQueueOperation {
            kind: FabricQueueOperationKind::Renew,
            sequence,
            consumer,
            lease_token,
            queue_epoch,
            consumer_group: None,
            operation_id,
            status: None,
            available_at_ms: None,
            lease_until_ms: Some(lease_until_ms),
            result: None,
        },
        QueueMutation::LeaseExpired {
            sequence,
            consumer,
            lease_token,
            status,
            available_at_ms,
            queue_epoch,
            operation_id,
        } => FabricQueueOperation {
            kind: FabricQueueOperationKind::Expire,
            sequence,
            consumer,
            lease_token,
            queue_epoch,
            consumer_group: None,
            operation_id,
            status: Some(status),
            available_at_ms,
            lease_until_ms: None,
            result: None,
        },
        QueueMutation::QueueCreated { .. } | QueueMutation::ConsumerGroupConfigured { .. } => {
            return Ok(None);
        }
    };
    Ok(Some(operation))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FabricQueueJobSnapshot {
    pub status: FabricQueueJobStatus,
    pub deliveries: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct FabricQueueExpiryPlan {
    pub sequence: u64,
    pub mutation_bytes: Vec<u8>,
    pub result: FabricQueueNackResult,
}

#[derive(Debug, Clone)]
pub(crate) struct FabricQueueDeadLetterPlan {
    pub source_sequence: u64,
    pub target_queue: String,
    pub target_name: String,
    pub target_payload: Vec<u8>,
    pub target_options: FabricQueueAddOptions,
    pub source_mutation_bytes: Vec<u8>,
    pub result: FabricQueueNackResult,
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
                max_attempts: options.max_attempts,
                deliveries: 0,
                lease_token: 0,
                status: FabricQueueJobStatus::Waiting,
                consumer: None,
                consumer_group: None,
                lease_until_ms: None,
                last_error: None,
                result: None,
            },
        );
        self.write_state(queue, &state)?;

        Ok(FabricQueueAddResult {
            sequence,
            job_id: options.job_id.unwrap_or_else(|| sequence.to_string()),
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
                || job.deliveries >= effective_max_attempts(&state, job)
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
            lease_duration_ms: state.config.visibility_timeout_ms,
            deliveries,
            consumer_group: None,
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
            consumer_group: None,
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
            queue_epoch: 0,
            operation_id: None,
            result: None,
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
        let job = state
            .jobs
            .get(&sequence)
            .expect("validated queue job must exist");
        let deliveries = job.deliveries;
        let max_attempts = effective_max_attempts(&state, job);
        let (status, available_at_ms) = if deliveries >= max_attempts {
            if state.config.dead_letter_queue.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "replicated Fabric queue DLQ terminalization requires crash-safe dead-letter handoff",
                ));
            }
            (FabricQueueJobStatus::Failed, None)
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
            queue_epoch: 0,
            operation_id: None,
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
            queue_epoch: 0,
            operation_id: None,
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
            let records = self
                .streams
                .read_committed(&stream, next, RECONCILE_BATCH)?;
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
                        max_attempts: envelope.max_attempts,
                        deliveries: 0,
                        lease_token: 0,
                        status: FabricQueueJobStatus::Waiting,
                        consumer: None,
                        consumer_group: None,
                        lease_until_ms: None,
                        last_error: None,
                        result: None,
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
            let records = self
                .streams
                .read_committed(&stream, next, RECONCILE_BATCH)?;
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
                let state: QueueStateFile = serde_json::from_slice(&bytes).map_err(json_error)?;
                if state.version != QUEUE_FORMAT_VERSION {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unsupported Fabric queue format version {}", state.version),
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

    fn reconcile_payloads(&mut self, queue: &str, state: &mut QueueStateFile) -> io::Result<bool> {
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
                            max_attempts: envelope.max_attempts,
                            deliveries: 0,
                            lease_token: 0,
                            status: FabricQueueJobStatus::Waiting,
                            consumer: None,
                            consumer_group: None,
                            lease_until_ms: None,
                            last_error: None,
                            result: None,
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

    fn replay_mutations(&mut self, queue: &str, state: &mut QueueStateFile) -> io::Result<bool> {
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
                    && job
                        .lease_until_ms
                        .is_some_and(|deadline| deadline <= now_ms)
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
            let job = state.jobs.get(sequence).expect("expired queue job must exist");
            let max_attempts = effective_max_attempts(state, job);
            let (status, available_at_ms) = if *deliveries >= max_attempts {
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
                queue_epoch: 0,
                operation_id: None,
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

    fn read_committed_envelope(&mut self, queue: &str, sequence: u64) -> io::Result<QueueEnvelope> {
        let records = self
            .streams
            .read_committed(&queue_stream_name(queue), sequence, 1)?;
        match records.first() {
            Some(record) if record.sequence == sequence => decode_envelope(record),
            _ => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("Fabric queue record {sequence} is not quorum committed"),
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

    pub fn fabric_queue_create(&mut self, name: &str, config: FabricQueueConfig) -> io::Result<()> {
        self.fabric_queue_local_store(name)?
            .create_queue(name, config)
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
        self.fabric_queue_local_store(queue)?
            .acquire(queue, consumer)
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
        self.fabric_queue_local_store(queue)?.nack(
            queue,
            sequence,
            consumer,
            lease_token,
            delay_ms,
            error,
        )
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
        self.fabric_queue_local_store(queue)?.nack_at(
            queue,
            sequence,
            consumer,
            lease_token,
            delay_ms,
            error,
            now_ms,
        )
    }

    pub fn fabric_queue_renew(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        lease_token: u64,
        extension_ms: u64,
    ) -> io::Result<u64> {
        self.fabric_queue_local_store(queue)?.renew(
            queue,
            sequence,
            consumer,
            lease_token,
            extension_ms,
        )
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
        self.fabric_queue_local_store(queue)?.renew_at(
            queue,
            sequence,
            consumer,
            lease_token,
            extension_ms,
            now_ms,
        )
    }

    pub fn fabric_queue_reap_expired(&mut self, queue: &str) -> io::Result<usize> {
        self.fabric_queue_local_store(queue)?.reap_expired(queue)
    }

    pub fn fabric_queue_reap_expired_at(&mut self, queue: &str, now_ms: u64) -> io::Result<usize> {
        self.fabric_queue_local_store(queue)?
            .reap_expired_at(queue, now_ms)
    }

    /// Inspect a replicated queue using only quorum-committed payload and
    /// mutation prefixes. Uncommitted local tails are never decoded or exposed.
    pub fn fabric_queue_info_replicated(&mut self, queue: &str) -> io::Result<FabricQueueInfo> {
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
        consumer_group: Option<&str>,
        operation_id: &str,
        queue_epoch: u64,
        lease_duration_ms: Option<u64>,
        now_ms: u64,
    ) -> io::Result<Option<FabricQueueLeasePlan>> {
        validate_queue_name(queue)?;
        validate_consumer_name(consumer)?;
        if let Some(group) = consumer_group {
            validate_consumer_group_name(group)?;
        }
        validate_operation_id(operation_id)?;
        if queue_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replicated Fabric queue lease requires a non-zero queue epoch",
            ));
        }
        if lease_duration_ms == Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replicated Fabric queue lease duration must be greater than zero",
            ));
        }

        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        if let Some(group) = consumer_group {
            let config = state.consumer_groups.get(group).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("Fabric queue consumer group {group:?} is not configured"),
                )
            })?;
            let active = state
                .jobs
                .values()
                .filter(|job| {
                    job.status == FabricQueueJobStatus::Active
                        && job.consumer_group.as_deref() == Some(group)
                })
                .count();
            if active >= config.max_concurrency {
                return Ok(None);
            }
        }
        let mut candidate: Option<(u64, i32)> = None;
        for (&sequence, job) in &state.jobs {
            if job.status != FabricQueueJobStatus::Waiting
                || job.available_at_ms > now_ms
                || job.deliveries >= effective_max_attempts(&state, job)
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
        let lease_duration_ms =
            lease_duration_ms.unwrap_or(state.config.visibility_timeout_ms);
        let lease_until_ms = now_ms.saturating_add(lease_duration_ms);
        let mutation_bytes = serde_json::to_vec(&QueueMutation::LeaseAcquired {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
            lease_until_ms,
            lease_duration_ms,
            deliveries,
            consumer_group: consumer_group.map(ToOwned::to_owned),
            queue_epoch,
            operation_id: Some(operation_id.to_string()),
        })
        .map_err(json_error)?;
        let envelope = store.read_committed_envelope(queue, sequence)?;
        let delivery = FabricQueueDelivery {
            sequence,
            queue_epoch,
            consumer_group: consumer_group.map(ToOwned::to_owned),
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
            || job.consumer_group != lease.consumer_group
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
            consumer_group: lease.consumer_group.clone(),
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

    pub(crate) fn fabric_queue_plan_committed_ack(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: &str,
        result: Option<&[u8]>,
        now_ms: u64,
    ) -> io::Result<Vec<u8>> {
        validate_consumer_name(consumer)?;
        validate_operation_id(operation_id)?;
        if queue_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replicated Fabric queue ACK requires a non-zero queue epoch",
            ));
        }
        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        validate_active_job(&state, sequence, consumer, lease_token, now_ms)?;
        serde_json::to_vec(&QueueMutation::Completed {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
            queue_epoch,
            operation_id: Some(operation_id.to_string()),
            result: result.map(ToOwned::to_owned),
        })
        .map_err(json_error)
    }

    pub(crate) fn fabric_queue_plan_committed_dead_letter_nack(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: &str,
        error: Option<&str>,
        now_ms: u64,
    ) -> io::Result<Option<FabricQueueDeadLetterPlan>> {
        validate_queue_name(queue)?;
        validate_consumer_name(consumer)?;
        validate_operation_id(operation_id)?;
        if queue_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replicated Fabric queue dead-letter handoff requires a non-zero queue epoch",
            ));
        }

        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        validate_active_job(&state, sequence, consumer, lease_token, now_ms)?;
        let job = state
            .jobs
            .get(&sequence)
            .expect("validated queue job must exist");
        if job.deliveries < effective_max_attempts(&state, job) {
            return Ok(None);
        }
        let Some(target_queue) = state.config.dead_letter_queue.clone() else {
            return Ok(None);
        };
        if target_queue == queue {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue cannot dead-letter into itself",
            ));
        }

        let envelope = store.read_committed_envelope(queue, sequence)?;
        let target_job_id = format!("__dlq:{queue}:{sequence}");
        validate_job_id(&target_job_id)?;
        let source_mutation_bytes = serde_json::to_vec(&QueueMutation::Nacked {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
            status: FabricQueueJobStatus::DeadLettered,
            available_at_ms: None,
            last_error: error.map(ToOwned::to_owned),
            queue_epoch,
            operation_id: Some(operation_id.to_string()),
        })
        .map_err(json_error)?;

        Ok(Some(FabricQueueDeadLetterPlan {
            source_sequence: sequence,
            target_queue,
            target_name: envelope.name,
            target_payload: envelope.payload,
            target_options: FabricQueueAddOptions {
                job_id: Some(target_job_id),
                priority: envelope.priority,
                delay_ms: 0,
                max_attempts: None,
            },
            source_mutation_bytes,
            result: FabricQueueNackResult {
                status: FabricQueueJobStatus::DeadLettered,
                deliveries: job.deliveries,
                available_at_ms: None,
            },
        }))
    }

    pub(crate) fn fabric_queue_plan_committed_nack(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: &str,
        delay_ms: u64,
        error: Option<&str>,
        now_ms: u64,
    ) -> io::Result<(Vec<u8>, FabricQueueNackResult)> {
        validate_consumer_name(consumer)?;
        validate_operation_id(operation_id)?;
        if queue_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replicated Fabric queue NACK requires a non-zero queue epoch",
            ));
        }
        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        validate_active_job(&state, sequence, consumer, lease_token, now_ms)?;
        let job = state
            .jobs
            .get(&sequence)
            .expect("validated queue job must exist");
        let deliveries = job.deliveries;
        let max_attempts = effective_max_attempts(&state, job);
        let (status, available_at_ms) = if deliveries >= max_attempts {
            if state.config.dead_letter_queue.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "replicated Fabric queue DLQ expiry requires crash-safe dead-letter handoff",
                ));
            }
            (FabricQueueJobStatus::Failed, None)
        } else {
            (
                FabricQueueJobStatus::Waiting,
                Some(now_ms.saturating_add(delay_ms)),
            )
        };
        let bytes = serde_json::to_vec(&QueueMutation::Nacked {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
            status,
            available_at_ms,
            last_error: error.map(ToOwned::to_owned),
            queue_epoch,
            operation_id: Some(operation_id.to_string()),
        })
        .map_err(json_error)?;
        Ok((
            bytes,
            FabricQueueNackResult {
                status,
                deliveries,
                available_at_ms,
            },
        ))
    }

    pub(crate) fn fabric_queue_plan_committed_renew(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: &str,
        extension_ms: u64,
        now_ms: u64,
    ) -> io::Result<(Vec<u8>, u64)> {
        if extension_ms == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue lease extension must be greater than zero",
            ));
        }
        validate_consumer_name(consumer)?;
        validate_operation_id(operation_id)?;
        if queue_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replicated Fabric queue renew requires a non-zero queue epoch",
            ));
        }
        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        validate_active_job(&state, sequence, consumer, lease_token, now_ms)?;
        let lease_until_ms = now_ms.saturating_add(extension_ms);
        let bytes = serde_json::to_vec(&QueueMutation::LeaseRenewed {
            sequence,
            consumer: consumer.to_string(),
            lease_token,
            lease_until_ms,
            queue_epoch,
            operation_id: Some(operation_id.to_string()),
        })
        .map_err(json_error)?;
        Ok((bytes, lease_until_ms))
    }

    pub(crate) fn fabric_queue_plan_committed_dead_letter_expiry(
        &mut self,
        queue: &str,
        queue_epoch: u64,
        now_ms: u64,
    ) -> io::Result<Option<FabricQueueDeadLetterPlan>> {
        validate_queue_name(queue)?;
        if queue_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replicated Fabric queue expiry requires a non-zero queue epoch",
            ));
        }

        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        let mut candidate: Option<(u64, u64)> = None;
        for (&sequence, job) in &state.jobs {
            if job.status != FabricQueueJobStatus::Active {
                continue;
            }
            let Some(deadline) = job.lease_until_ms else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("active Fabric queue job {sequence} is missing a lease deadline"),
                ));
            };
            if deadline > now_ms {
                continue;
            }
            match candidate {
                None => candidate = Some((sequence, deadline)),
                Some((best_sequence, best_deadline))
                    if deadline < best_deadline
                        || (deadline == best_deadline && sequence < best_sequence) =>
                {
                    candidate = Some((sequence, deadline));
                }
                _ => {}
            }
        }

        let Some((sequence, _)) = candidate else {
            return Ok(None);
        };
        let job = state
            .jobs
            .get(&sequence)
            .expect("expiry candidate must exist");
        if job.deliveries < effective_max_attempts(&state, job) {
            return Ok(None);
        }
        let Some(target_queue) = state.config.dead_letter_queue.clone() else {
            return Ok(None);
        };
        if target_queue == queue {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue cannot dead-letter into itself",
            ));
        }
        let consumer = job.consumer.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("active Fabric queue job {sequence} is missing a consumer"),
            )
        })?;
        let envelope = store.read_committed_envelope(queue, sequence)?;
        let target_job_id = format!("__dlq:{queue}:{sequence}");
        validate_job_id(&target_job_id)?;
        let operation_id = format!(
            "__lease_expire:{queue_epoch}:{sequence}:{}",
            job.lease_token
        );
        let source_mutation_bytes = serde_json::to_vec(&QueueMutation::LeaseExpired {
            sequence,
            consumer,
            lease_token: job.lease_token,
            status: FabricQueueJobStatus::DeadLettered,
            available_at_ms: None,
            queue_epoch,
            operation_id: Some(operation_id),
        })
        .map_err(json_error)?;

        Ok(Some(FabricQueueDeadLetterPlan {
            source_sequence: sequence,
            target_queue,
            target_name: envelope.name,
            target_payload: envelope.payload,
            target_options: FabricQueueAddOptions {
                job_id: Some(target_job_id),
                priority: envelope.priority,
                delay_ms: 0,
                max_attempts: None,
            },
            source_mutation_bytes,
            result: FabricQueueNackResult {
                status: FabricQueueJobStatus::DeadLettered,
                deliveries: job.deliveries,
                available_at_ms: None,
            },
        }))
    }

    pub(crate) fn fabric_queue_plan_committed_expiry(
        &mut self,
        queue: &str,
        queue_epoch: u64,
        now_ms: u64,
    ) -> io::Result<Option<FabricQueueExpiryPlan>> {
        validate_queue_name(queue)?;
        if queue_epoch == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replicated Fabric queue expiry requires a non-zero queue epoch",
            ));
        }

        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        let mut candidate: Option<(u64, u64)> = None;
        for (&sequence, job) in &state.jobs {
            if job.status != FabricQueueJobStatus::Active {
                continue;
            }
            let Some(deadline) = job.lease_until_ms else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("active Fabric queue job {sequence} is missing a lease deadline"),
                ));
            };
            if deadline > now_ms {
                continue;
            }
            match candidate {
                None => candidate = Some((sequence, deadline)),
                Some((best_sequence, best_deadline))
                    if deadline < best_deadline
                        || (deadline == best_deadline && sequence < best_sequence) =>
                {
                    candidate = Some((sequence, deadline));
                }
                _ => {}
            }
        }

        let Some((sequence, _)) = candidate else {
            return Ok(None);
        };
        let job = state
            .jobs
            .get(&sequence)
            .expect("expiry candidate must exist");
        let consumer = job.consumer.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("active Fabric queue job {sequence} is missing a consumer"),
            )
        })?;
        let lease_token = job.lease_token;
        let deliveries = job.deliveries;
        let max_attempts = effective_max_attempts(&state, job);
        let (status, available_at_ms) = if deliveries >= max_attempts {
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
        let operation_id = format!("__lease_expire:{queue_epoch}:{sequence}:{lease_token}");
        let mutation_bytes = serde_json::to_vec(&QueueMutation::LeaseExpired {
            sequence,
            consumer: consumer.clone(),
            lease_token,
            status,
            available_at_ms,
            queue_epoch,
            operation_id: Some(operation_id.clone()),
        })
        .map_err(json_error)?;

        Ok(Some(FabricQueueExpiryPlan {
            sequence,
            mutation_bytes,
            result: FabricQueueNackResult {
                status,
                deliveries,
                available_at_ms,
            },
        }))
    }

    pub(crate) fn fabric_queue_encode_consumer_group_config(
        &mut self,
        queue: &str,
        config: &FabricQueueConsumerGroupConfig,
    ) -> io::Result<Vec<u8>> {
        validate_queue_name(queue)?;
        config.validate()?;
        serde_json::to_vec(&QueueMutation::ConsumerGroupConfigured {
            config: config.clone(),
        })
        .map_err(json_error)
    }

    pub(crate) fn fabric_queue_committed_consumer_group_info(
        &mut self,
        queue: &str,
        group: &str,
    ) -> io::Result<FabricQueueConsumerGroupInfo> {
        validate_consumer_group_name(group)?;
        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        let config = state.consumer_groups.get(group).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Fabric queue consumer group {group:?} is not configured"),
            )
        })?;
        let active = state
            .jobs
            .values()
            .filter(|job| {
                job.status == FabricQueueJobStatus::Active
                    && job.consumer_group.as_deref() == Some(group)
            })
            .count();
        Ok(FabricQueueConsumerGroupInfo {
            name: group.to_string(),
            max_concurrency: config.max_concurrency,
            active,
        })
    }

    pub fn fabric_queue_ready_replicated(
        &mut self,
        queue: &str,
        now_ms: u64,
    ) -> io::Result<FabricQueueReadySignal> {
        if !self.fabric_queue_has_replication_policy(queue)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Fabric queue {queue:?} does not have a replication policy"),
            ));
        }
        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        let mut ready = false;
        let mut next_available_at_ms: Option<u64> = None;
        for job in state.jobs.values() {
            if job.status != FabricQueueJobStatus::Waiting
                || job.deliveries >= effective_max_attempts(&state, job)
            {
                continue;
            }
            if job.available_at_ms <= now_ms {
                ready = true;
                next_available_at_ms = None;
                break;
            }
            next_available_at_ms = Some(
                next_available_at_ms
                    .map(|current| current.min(job.available_at_ms))
                    .unwrap_or(job.available_at_ms),
            );
        }
        Ok(FabricQueueReadySignal {
            ready,
            next_available_at_ms,
        })
    }

    pub fn fabric_queue_job_replicated(
        &mut self,
        queue: &str,
        job_id: &str,
    ) -> io::Result<Option<FabricQueueJobInfo>> {
        if !self.fabric_queue_has_replication_policy(queue)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Fabric queue {queue:?} does not have a replication policy"),
            ));
        }
        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        let mut found: Option<(u64, &QueueJobState)> = None;
        for (&sequence, job) in &state.jobs {
            let current_id = job
                .job_id
                .as_deref()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| sequence.to_string());
            if current_id == job_id {
                if found.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Fabric queue {queue:?} contains duplicate committed job id {job_id:?}"),
                    ));
                }
                found = Some((sequence, job));
            }
        }
        let Some((sequence, job)) = found else {
            return Ok(None);
        };
        let envelope = store.read_committed_envelope(queue, sequence)?;
        Ok(Some(FabricQueueJobInfo {
            sequence,
            job_id: envelope.job_id.unwrap_or_else(|| sequence.to_string()),
            name: envelope.name,
            payload: envelope.payload,
            priority: job.priority,
            status: job.status,
            deliveries: job.deliveries,
            available_at_ms: job.available_at_ms,
            lease_until_ms: job.lease_until_ms,
            last_error: job.last_error.clone(),
            result: job.result.clone(),
        }))
    }

    pub(crate) fn fabric_queue_committed_job_snapshot(
        &mut self,
        queue: &str,
        sequence: u64,
    ) -> io::Result<FabricQueueJobSnapshot> {
        let mut store = self.fabric_queue_store()?;
        let state = store.load_committed_state(queue)?;
        let job = state.jobs.get(&sequence).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("Fabric queue job {sequence} does not exist"),
            )
        })?;
        Ok(FabricQueueJobSnapshot {
            status: job.status,
            deliveries: job.deliveries,
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

fn effective_max_attempts(state: &QueueStateFile, job: &QueueJobState) -> u32 {
    job.max_attempts.unwrap_or(state.config.max_attempts)
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
        QueueMutation::ConsumerGroupConfigured { config } => {
            config.validate()?;
            match state.consumer_groups.get(&config.name) {
                Some(existing) if existing == config => {}
                Some(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Fabric queue consumer group {:?} has conflicting configuration",
                            config.name
                        ),
                    ));
                }
                None => {
                    state
                        .consumer_groups
                        .insert(config.name.clone(), config.clone());
                }
            }
        }
        QueueMutation::LeaseAcquired {
            sequence,
            consumer,
            lease_token,
            lease_until_ms,
            deliveries,
            consumer_group,
            ..
        } => {
            if let Some(group) = consumer_group {
                let config = state.consumer_groups.get(group).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Fabric queue lease references unknown consumer group {group:?}"),
                    )
                })?;
                let active = state
                    .jobs
                    .values()
                    .filter(|job| {
                        job.status == FabricQueueJobStatus::Active
                            && job.consumer_group.as_deref() == Some(group.as_str())
                    })
                    .count();
                if active >= config.max_concurrency {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Fabric queue consumer group {group:?} exceeds max concurrency {}",
                            config.max_concurrency
                        ),
                    ));
                }
            }
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
            job.consumer_group = consumer_group.clone();
            job.lease_token = *lease_token;
            job.lease_until_ms = Some(*lease_until_ms);
            job.deliveries = *deliveries;
        }
        QueueMutation::Completed {
            sequence,
            consumer,
            lease_token,
            result,
            ..
        } => {
            validate_mutation_lease(state, *sequence, consumer, *lease_token)?;
            let job = state
                .jobs
                .get_mut(sequence)
                .expect("validated job must exist");
            job.status = FabricQueueJobStatus::Completed;
            job.consumer = None;
            job.consumer_group = None;
            job.lease_until_ms = None;
            job.last_error = None;
            job.result = result.clone();
        }
        QueueMutation::Nacked {
            sequence,
            consumer,
            lease_token,
            status,
            available_at_ms,
            last_error,
            ..
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
            let job = state
                .jobs
                .get_mut(sequence)
                .expect("validated job must exist");
            job.status = *status;
            job.consumer = None;
            job.consumer_group = None;
            job.lease_until_ms = None;
            job.available_at_ms = available_at_ms.unwrap_or(job.available_at_ms);
            job.last_error = last_error.clone();
            job.result = None;
        }
        QueueMutation::LeaseRenewed {
            sequence,
            consumer,
            lease_token,
            lease_until_ms,
            ..
        } => {
            validate_mutation_lease(state, *sequence, consumer, *lease_token)?;
            let job = state
                .jobs
                .get_mut(sequence)
                .expect("validated job must exist");
            job.lease_until_ms = Some(*lease_until_ms);
        }
        QueueMutation::LeaseExpired {
            sequence,
            consumer,
            lease_token,
            status,
            available_at_ms,
            ..
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
            let job = state
                .jobs
                .get_mut(sequence)
                .expect("validated job must exist");
            job.status = *status;
            job.consumer = None;
            job.consumer_group = None;
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

pub(crate) fn validate_consumer_group_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric queue consumer-group names must be 1..=128 ASCII letters, digits, '.', '_', '-', or ':'",
        ));
    }
    Ok(())
}

pub(crate) fn validate_operation_id(operation_id: &str) -> io::Result<()> {
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

        let delivery = queues
            .acquire_at("email", "worker-a", 100)
            .unwrap()
            .unwrap();
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
    fn per_job_max_attempts_override_queue_default() {
        let root = test_dir("per-job-attempts");
        let mut streams = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut streams);
        queues
            .create_queue(
                "jobs",
                FabricQueueConfig {
                    visibility_timeout_ms: 100,
                    max_attempts: 5,
                    dead_letter_queue: None,
                },
            )
            .unwrap();

        queues
            .add_at(
                "jobs",
                "single-attempt",
                b"payload",
                FabricQueueAddOptions {
                    job_id: Some("one-shot".to_string()),
                    priority: 0,
                    delay_ms: 0,
                    max_attempts: Some(1),
                },
                10,
            )
            .unwrap();

        let delivery = queues
            .acquire_at("jobs", "worker", 10)
            .unwrap()
            .expect("per-job attempt override should still allow first delivery");
        let result = queues
            .nack_at(
                "jobs",
                delivery.sequence,
                "worker",
                delivery.lease_token,
                0,
                Some("boom"),
                11,
            )
            .unwrap();

        assert_eq!(result.status, FabricQueueJobStatus::Failed);
        assert_eq!(result.deliveries, 1);
        let info = queues.info_at("jobs", 11).unwrap();
        assert_eq!(info.failed, 1);
        assert_eq!(info.waiting, 0);

        drop(queues);
        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        let mut queues = FabricQueueStore::new(&mut reopened);
        let info = queues.info_at("jobs", 11).unwrap();
        assert_eq!(info.failed, 1);
        assert!(queues.acquire_at("jobs", "worker-2", 12).unwrap().is_none());

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

        let stale = queues.ack_at("jobs", first.sequence, "worker", first.lease_token, 11);
        assert_eq!(stale.unwrap_err().kind(), io::ErrorKind::PermissionDenied);

        queues
            .ack_at("jobs", second.sequence, "worker", second.lease_token, 11)
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
                .ack_at("jobs", delivery.sequence, "worker", delivery.lease_token, 1)
                .unwrap();
        }

        fs::remove_file(root.join(queue_stream_name("jobs")).join(QUEUE_STATE_FILE)).unwrap();

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
                        queue_epoch: 0,
                        operation_id: None,
                        result: None,
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
            max_attempts: None,
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
