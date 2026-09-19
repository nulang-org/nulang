//! Minimal JSON-over-HTTP gateway for replicated Fabric Queue operations.
//!
//! This is intentionally a queue protocol surface, not a Redis compatibility
//! layer. The BullMQ adapter and other clients translate their semantics into
//! these native queue operations.

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use super::{
    FabricQueueAddOptions, FabricQueueConfig, FabricQueueJobStatus, Runtime,
};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_ACCEPTS_PER_POLL: usize = 16;

#[derive(Debug)]
pub struct FabricQueueApiServer {
    listener: TcpListener,
}

impl FabricQueueApiServer {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Process a bounded number of queued HTTP requests without taking
    /// ownership of the Runtime. The node event loop calls this between
    /// cluster-network and scheduler work.
    pub fn poll(&mut self, runtime: &mut Runtime) -> io::Result<usize> {
        let mut handled = 0usize;
        while handled < MAX_ACCEPTS_PER_POLL {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                    if let Err(error) = Self::handle_connection(runtime, &mut stream) {
                        let (_, response) = json_error_response(
                            400,
                            error_kind_name(error.kind()),
                            error.to_string(),
                        );
                        let head = format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            response.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&response);
                        let _ = stream.flush();
                    }
                    handled += 1;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(handled)
    }

    fn handle_connection(runtime: &mut Runtime, stream: &mut TcpStream) -> io::Result<()> {
        let body = read_http_body(stream)?;
        let (status, response) = dispatch_queue_api(runtime, &body);
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            413 => "Payload Too Large",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            503 => "Service Unavailable",
            _ => "Error",
        };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(&response)?;
        stream.flush()
    }
}

fn read_http_body(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut data = Vec::with_capacity(4096);
    let mut scratch = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut scratch)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "queue API request ended before headers completed",
            ));
        }
        data.extend_from_slice(&scratch[..n]);
        if data.len() > MAX_HEADER_BYTES + MAX_BODY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "queue API request exceeds maximum size",
            ));
        }
        if let Some(index) = find_header_end(&data) {
            break index;
        }
        if data.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "queue API headers exceed maximum size",
            ));
        }
    };

    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut request = httparse::Request::new(&mut headers);
    request
        .parse(&data[..header_end])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    if request.method != Some("POST") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "queue API accepts POST only",
        ));
    }
    if request.path != Some("/v1/queue") {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "queue API endpoint not found",
        ));
    }
    let content_length = request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-length"))
        .and_then(|header| std::str::from_utf8(header.value).ok())
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "queue API requires Content-Length",
            )
        })?;
    if content_length > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "queue API body exceeds maximum size",
        ));
    }

    let body_start = header_end;
    while data.len().saturating_sub(body_start) < content_length {
        let n = stream.read(&mut scratch)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "queue API request body was truncated",
            ));
        }
        data.extend_from_slice(&scratch[..n]);
    }
    Ok(data[body_start..body_start + content_length].to_vec())
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum QueueApiRequest {
    Ensure {
        queue: String,
        partition: u16,
        replication_factor: usize,
        visibility_timeout_ms: u64,
        max_attempts: u32,
    },
    Add {
        queue: String,
        name: String,
        payload_hex: String,
        job_id: Option<String>,
        priority: i32,
        delay_ms: u64,
        max_attempts: Option<u32>,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    },
    Acquire {
        queue: String,
        consumer: String,
        operation_id: String,
        lease_duration_ms: u64,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    },
    Ack {
        queue: String,
        sequence: u64,
        consumer: String,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: String,
        result_hex: Option<String>,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    },
    Nack {
        queue: String,
        sequence: u64,
        consumer: String,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: String,
        delay_ms: u64,
        error: Option<String>,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    },
    Renew {
        queue: String,
        sequence: u64,
        consumer: String,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: String,
        extension_ms: u64,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    },
    Reap {
        queue: String,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    },
    Reschedule {
        queue: String,
        job_id: String,
        operation_id: String,
        available_at_ms: u64,
        partition: u16,
        replication_factor: usize,
    },
    Requeue {
        queue: String,
        job_id: String,
        operation_id: String,
        expected_status: String,
        available_at_ms: u64,
        reset_deliveries: bool,
        partition: u16,
        replication_factor: usize,
    },
    Info {
        queue: String,
    },
    Job {
        queue: String,
        job_id: String,
    },
    Ready {
        queue: String,
        now_ms: u64,
    },
}

pub(crate) fn dispatch_queue_api(runtime: &mut Runtime, body: &[u8]) -> (u16, Vec<u8>) {
    let request: QueueApiRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => {
            return json_error_response(
                400,
                "invalid_input",
                format!("invalid queue API JSON: {error}"),
            )
        }
    };

    match dispatch_request(runtime, request) {
        Ok(result) => (
            200,
            serde_json::to_vec(&json!({ "ok": true, "result": result }))
                .expect("queue API JSON serialization must succeed"),
        ),
        Err(error) => {
            let status = match error.kind() {
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => 400,
                io::ErrorKind::PermissionDenied => 403,
                io::ErrorKind::NotFound => 404,
                io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut
                | io::ErrorKind::AlreadyExists => 409,
                io::ErrorKind::Unsupported => 501,
                io::ErrorKind::NotConnected | io::ErrorKind::ConnectionRefused => 503,
                _ => 500,
            };
            json_error_response(status, error_kind_name(error.kind()), error.to_string())
        }
    }
}

fn dispatch_request(runtime: &mut Runtime, request: QueueApiRequest) -> io::Result<Value> {
    match request {
        QueueApiRequest::Ensure {
            queue,
            partition,
            replication_factor,
            visibility_timeout_ms,
            max_attempts,
        } => {
            let result = runtime.fabric_queue_create_replicated(
                &queue,
                FabricQueueConfig {
                    visibility_timeout_ms,
                    max_attempts,
                    dead_letter_queue: None,
                },
                partition,
                replication_factor,
            )?;
            Ok(json!({
                "created": result.created,
                "policyReady": result.policy.ready,
                "mutationSequence": result.mutation_sequence,
                "committed": result.replication.map(|status| status.committed).unwrap_or(false)
            }))
        }
        QueueApiRequest::Add {
            queue,
            name,
            payload_hex,
            job_id,
            priority,
            delay_ms,
            max_attempts,
            partition,
            replication_factor,
            now_ms,
        } => {
            let payload = decode_hex(&payload_hex)?;
            let result = runtime.fabric_queue_add_replicated(
                &queue,
                &name,
                &payload,
                FabricQueueAddOptions {
                    job_id,
                    priority,
                    delay_ms,
                    max_attempts,
                },
                partition,
                replication_factor,
                now_ms,
            )?;
            Ok(json!({
                "sequence": result.sequence,
                "deduplicated": result.deduplicated,
                "enqueued": result.enqueued,
                "committed": result.replication.map(|status| status.committed).unwrap_or(false)
            }))
        }
        QueueApiRequest::Acquire {
            queue,
            consumer,
            operation_id,
            lease_duration_ms,
            partition,
            replication_factor,
            now_ms,
        } => {
            let result = runtime.fabric_queue_acquire_replicated_with_lease_duration(
                &queue,
                &consumer,
                &operation_id,
                lease_duration_ms,
                partition,
                replication_factor,
                now_ms,
            )?;
            let delivery = result.delivery.map(|delivery| {
                json!({
                    "sequence": delivery.sequence,
                    "queueEpoch": delivery.queue_epoch,
                    "jobId": delivery.job_id,
                    "name": delivery.name,
                    "payloadHex": hex::encode(delivery.payload),
                    "priority": delivery.priority,
                    "deliveries": delivery.deliveries,
                    "leaseToken": delivery.lease_token,
                    "leaseUntilMs": delivery.lease_until_ms
                })
            });
            Ok(json!({
                "mutationSequence": result.mutation_sequence,
                "committed": result.replication.map(|status| status.committed).unwrap_or(false),
                "delivery": delivery,
                "resumed": result.resumed
            }))
        }
        QueueApiRequest::Ack {
            queue,
            sequence,
            consumer,
            queue_epoch,
            lease_token,
            operation_id,
            result_hex,
            partition,
            replication_factor,
            now_ms,
        } => {
            let result_bytes = match result_hex {
                Some(value) => Some(decode_hex(&value)?),
                None => None,
            };
            let result = match result_bytes.as_deref() {
                Some(bytes) => runtime.fabric_queue_ack_replicated_with_result(
                    &queue,
                    sequence,
                    &consumer,
                    queue_epoch,
                    lease_token,
                    &operation_id,
                    bytes,
                    partition,
                    replication_factor,
                    now_ms,
                )?,
                None => runtime.fabric_queue_ack_replicated(
                    &queue,
                    sequence,
                    &consumer,
                    queue_epoch,
                    lease_token,
                    &operation_id,
                    partition,
                    replication_factor,
                    now_ms,
                )?,
            };
            Ok(json!({
                "mutationSequence": result.mutation_sequence,
                "committed": result.replication.map(|status| status.committed).unwrap_or(false),
                "completed": result.completed,
                "resumed": result.resumed
            }))
        }
        QueueApiRequest::Nack {
            queue,
            sequence,
            consumer,
            queue_epoch,
            lease_token,
            operation_id,
            delay_ms,
            error,
            partition,
            replication_factor,
            now_ms,
        } => {
            let result = runtime.fabric_queue_nack_replicated(
                &queue,
                sequence,
                &consumer,
                queue_epoch,
                lease_token,
                &operation_id,
                delay_ms,
                error.as_deref(),
                partition,
                replication_factor,
                now_ms,
            )?;
            let transition = result.result.map(|transition| {
                json!({
                    "status": status_name(transition.status),
                    "deliveries": transition.deliveries,
                    "availableAtMs": transition.available_at_ms
                })
            });
            Ok(json!({
                "mutationSequence": result.mutation_sequence,
                "committed": result.replication.map(|status| status.committed).unwrap_or(false),
                "transition": transition,
                "resumed": result.resumed
            }))
        }
        QueueApiRequest::Renew {
            queue,
            sequence,
            consumer,
            queue_epoch,
            lease_token,
            operation_id,
            extension_ms,
            partition,
            replication_factor,
            now_ms,
        } => {
            let result = runtime.fabric_queue_renew_replicated(
                &queue,
                sequence,
                &consumer,
                queue_epoch,
                lease_token,
                &operation_id,
                extension_ms,
                partition,
                replication_factor,
                now_ms,
            )?;
            Ok(json!({
                "mutationSequence": result.mutation_sequence,
                "committed": result.replication.map(|status| status.committed).unwrap_or(false),
                "leaseUntilMs": result.lease_until_ms,
                "resumed": result.resumed
            }))
        }
        QueueApiRequest::Reap {
            queue,
            partition,
            replication_factor,
            now_ms,
        } => {
            let result = runtime.fabric_queue_reap_expired_replicated(
                &queue,
                partition,
                replication_factor,
                now_ms,
            )?;
            Ok(json!({
                "mutationSequence": result.mutation_sequence,
                "committed": result.replication.map(|status| status.committed).unwrap_or(false),
                "expiredSequence": result.expired_sequence,
                "transition": result.result.map(|transition| json!({
                    "status": status_name(transition.status),
                    "deliveries": transition.deliveries,
                    "availableAtMs": transition.available_at_ms
                })),
                "resumed": result.resumed
            }))
        }
        QueueApiRequest::Reschedule {
            queue,
            job_id,
            operation_id,
            available_at_ms,
            partition,
            replication_factor,
        } => {
            let result = runtime.fabric_queue_reschedule_replicated(
                &queue,
                &job_id,
                &operation_id,
                available_at_ms,
                partition,
                replication_factor,
            )?;
            Ok(json!({
                "mutationSequence": result.mutation_sequence,
                "committed": result.replication.map(|status| status.committed).unwrap_or(false),
                "sequence": result.sequence,
                "availableAtMs": result.available_at_ms,
                "updated": result.updated,
                "resumed": result.resumed
            }))
        }
        QueueApiRequest::Requeue {
            queue,
            job_id,
            operation_id,
            expected_status,
            available_at_ms,
            reset_deliveries,
            partition,
            replication_factor,
        } => {
            let expected_status = match expected_status.as_str() {
                "completed" => FabricQueueJobStatus::Completed,
                "failed" => FabricQueueJobStatus::Failed,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "queue API requeue expected_status must be completed or failed",
                    ))
                }
            };
            let result = runtime.fabric_queue_requeue_replicated(
                &queue,
                &job_id,
                &operation_id,
                expected_status,
                available_at_ms,
                reset_deliveries,
                partition,
                replication_factor,
            )?;
            Ok(json!({
                "mutationSequence": result.mutation_sequence,
                "committed": result.replication.map(|status| status.committed).unwrap_or(false),
                "sequence": result.sequence,
                "updated": result.updated,
                "resumed": result.resumed
            }))
        }
        QueueApiRequest::Info { queue } => {
            let info = runtime.fabric_queue_info_replicated(&queue)?;
            Ok(json!({
                "name": info.name,
                "waiting": info.waiting,
                "active": info.active,
                "completed": info.completed,
                "failed": info.failed,
                "deadLettered": info.dead_lettered,
                "total": info.total
            }))
        }
        QueueApiRequest::Job { queue, job_id } => {
            let job = runtime.fabric_queue_job_replicated(&queue, &job_id)?;
            Ok(match job {
                Some(job) => json!({
                    "sequence": job.sequence,
                    "jobId": job.job_id,
                    "name": job.name,
                    "payloadHex": hex::encode(job.payload),
                    "priority": job.priority,
                    "status": status_name(job.status),
                    "deliveries": job.deliveries,
                    "availableAtMs": job.available_at_ms,
                    "leaseUntilMs": job.lease_until_ms,
                    "lastError": job.last_error,
                    "resultHex": job.result.map(hex::encode)
                }),
                None => Value::Null,
            })
        }
        QueueApiRequest::Ready { queue, now_ms } => {
            let signal = runtime.fabric_queue_ready_replicated(&queue, now_ms)?;
            Ok(json!({
                "ready": signal.ready,
                "nextAvailableAtMs": signal.next_available_at_ms
            }))
        }
    }
}

fn decode_hex(value: &str) -> io::Result<Vec<u8>> {
    hex::decode(value).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid hex payload: {error}"),
        )
    })
}

fn status_name(status: FabricQueueJobStatus) -> &'static str {
    match status {
        FabricQueueJobStatus::Waiting => "waiting",
        FabricQueueJobStatus::Active => "active",
        FabricQueueJobStatus::Completed => "completed",
        FabricQueueJobStatus::Failed => "failed",
        FabricQueueJobStatus::DeadLettered => "dead-lettered",
    }
}

fn error_kind_name(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::InvalidInput => "invalid_input",
        io::ErrorKind::InvalidData => "invalid_data",
        io::ErrorKind::PermissionDenied => "permission_denied",
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::WouldBlock => "would_block",
        io::ErrorKind::TimedOut => "timed_out",
        io::ErrorKind::AlreadyExists => "already_exists",
        io::ErrorKind::Unsupported => "unsupported",
        io::ErrorKind::NotConnected => "not_connected",
        _ => "internal",
    }
}

fn json_error_response(status: u16, kind: &str, message: String) -> (u16, Vec<u8>) {
    (
        status,
        serde_json::to_vec(&json!({
            "ok": false,
            "error": {
                "kind": kind,
                "message": message
            }
        }))
        .expect("queue API error JSON serialization must succeed"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::cluster_dst::DeterministicCluster;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn json_protocol_drives_rf1_queue_and_persists_result() {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39991);
        let mut cluster = DeterministicCluster::new(&[address], 0x5155455545);
        cluster.run_rounds(8);

        let root = std::env::temp_dir().join(format!(
            "nulang-queue-api-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        cluster.node_mut(0).fabric_stream_open(&root).unwrap();

        let call = |runtime: &mut Runtime, value: Value| -> Value {
            let body = serde_json::to_vec(&value).unwrap();
            let (status, bytes) = dispatch_queue_api(runtime, &body);
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
            serde_json::from_slice::<Value>(&bytes).unwrap()["result"].clone()
        };

        let ensured = call(
            cluster.node_mut(0),
            json!({
                "op": "ensure",
                "queue": "jobs",
                "partition": 0,
                "replication_factor": 1,
                "visibility_timeout_ms": 30_000,
                "max_attempts": 1
            }),
        );
        assert_eq!(ensured["created"], true);

        let added = call(
            cluster.node_mut(0),
            json!({
                "op": "add",
                "queue": "jobs",
                "name": "render",
                "payload_hex": hex::encode(b"{\"version\":1}"),
                "job_id": "job-1",
                "priority": 7,
                "delay_ms": 0,
                "max_attempts": 1,
                "partition": 0,
                "replication_factor": 1,
                "now_ms": 100
            }),
        );
        assert_eq!(added["enqueued"], true);

        let delayed = call(
            cluster.node_mut(0),
            json!({
                "op": "reschedule",
                "queue": "jobs",
                "job_id": "job-1",
                "operation_id": "delay-1",
                "available_at_ms": 1_000,
                "partition": 0,
                "replication_factor": 1
            }),
        );
        assert_eq!(delayed["updated"], true);
        let delayed_job = call(
            cluster.node_mut(0),
            json!({
                "op": "job",
                "queue": "jobs",
                "job_id": "job-1"
            }),
        );
        assert_eq!(delayed_job["availableAtMs"], 1_000);

        let promoted = call(
            cluster.node_mut(0),
            json!({
                "op": "reschedule",
                "queue": "jobs",
                "job_id": "job-1",
                "operation_id": "promote-1",
                "available_at_ms": 150,
                "partition": 0,
                "replication_factor": 1
            }),
        );
        assert_eq!(promoted["updated"], true);

        let acquired = call(
            cluster.node_mut(0),
            json!({
                "op": "acquire",
                "queue": "jobs",
                "consumer": "worker-1",
                "operation_id": "acquire-1",
                "lease_duration_ms": 5_000,
                "partition": 0,
                "replication_factor": 1,
                "now_ms": 200
            }),
        );
        assert_eq!(acquired["delivery"]["jobId"], "job-1");
        assert_eq!(acquired["delivery"]["leaseUntilMs"], 5_200);

        let epoch = acquired["delivery"]["queueEpoch"].as_u64().unwrap();
        let token = acquired["delivery"]["leaseToken"].as_u64().unwrap();
        let acked = call(
            cluster.node_mut(0),
            json!({
                "op": "ack",
                "queue": "jobs",
                "sequence": 1,
                "consumer": "worker-1",
                "queue_epoch": epoch,
                "lease_token": token,
                "operation_id": "ack-1",
                "result_hex": hex::encode(b"{\"rendered\":true}"),
                "partition": 0,
                "replication_factor": 1,
                "now_ms": 300
            }),
        );
        assert_eq!(acked["completed"], true);

        let stored = call(
            cluster.node_mut(0),
            json!({
                "op": "job",
                "queue": "jobs",
                "job_id": "job-1"
            }),
        );
        assert_eq!(stored["status"], "completed");
        assert_eq!(
            stored["resultHex"],
            hex::encode(b"{\"rendered\":true}")
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
