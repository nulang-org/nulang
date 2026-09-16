//! Deferred receipt-backed HTTP execution for durable actor turns.
//!
//! `effect_receipt_http::DurableHttpClient` is a useful standalone convenience:
//! it persists the terminal receipt before returning. A durable actor turn needs
//! a stricter boundary: provider observation, actor state, receiver inbox/dedup,
//! and the terminal receipt must become authoritative together. This module
//! therefore executes/replays HTTP POST but returns a pending terminal receipt
//! to the caller instead of persisting it immediately.

use std::fmt;
use std::io::Read;

use crate::effect_receipt::{
    EffectIntent, EffectOutcome, EffectReplayDecision, EffectReceipt, RecordedEffectFailure,
};
use crate::effect_receipt_fence::EffectReceiptFence;
use crate::effect_receipt_http::http_post_request_fingerprint;
use crate::effect_receipt_libsql::{LibsqlEffectReceiptError, LibsqlEffectReceiptStore};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpTurnObservation {
    Succeeded(Vec<u8>),
    Failed(RecordedEffectFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHttpTurnEffect {
    pub observation: HttpTurnObservation,
    pub receipt: EffectReceipt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DurableHttpTurnOutcome {
    /// A compatible terminal receipt already exists. No network call occurred.
    Replayed(HttpTurnObservation),
    /// Provider execution completed, but the terminal receipt is intentionally
    /// not durable yet. Commit `receipt` through the atomic durable-turn store.
    Pending(PendingHttpTurnEffect),
}

pub struct DurableHttpTurnClient<'a> {
    store: &'a LibsqlEffectReceiptStore,
}

#[derive(Debug)]
pub enum DurableHttpTurnError {
    WrongEffectIdentity { effect: String, operation: String },
    RequestFingerprintMismatch,
    MissingIdempotencyKey,
    Persistence(LibsqlEffectReceiptError),
    Transport(String),
    ResponseRead(String),
}

impl fmt::Display for DurableHttpTurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongEffectIdentity { effect, operation } => {
                write!(f, "durable HTTP turn requires Http.post, got {effect}.{operation}")
            }
            Self::RequestFingerprintMismatch => {
                f.write_str("durable HTTP turn request fingerprint mismatch")
            }
            Self::MissingIdempotencyKey => {
                f.write_str("durable HTTP turn requires a persisted provider idempotency key")
            }
            Self::Persistence(error) => write!(f, "{error}"),
            Self::Transport(error) => write!(f, "HTTP transport failed: {error}"),
            Self::ResponseRead(error) => write!(f, "failed reading HTTP response: {error}"),
        }
    }
}

impl std::error::Error for DurableHttpTurnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Persistence(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LibsqlEffectReceiptError> for DurableHttpTurnError {
    fn from(value: LibsqlEffectReceiptError) -> Self {
        Self::Persistence(value)
    }
}

impl<'a> DurableHttpTurnClient<'a> {
    pub fn new(store: &'a LibsqlEffectReceiptStore) -> Self {
        Self { store }
    }

    /// Execute or replay a durable HTTP POST without committing a new terminal
    /// receipt independently of the surrounding actor turn.
    ///
    /// For a new/indeterminate invocation this method:
    /// 1. persists/re-observes the intent under the current activation fence;
    /// 2. executes the provider using the persisted idempotency key;
    /// 3. returns a `PendingHttpTurnEffect` containing the terminal receipt.
    ///
    /// The caller must only make that receipt authoritative through the same
    /// physical transaction that commits actor state + inbox/dedup state.
    pub fn prepare_post<F: EffectReceiptFence>(
        &self,
        actor_id: u64,
        fence: &F,
        intent: &EffectIntent,
        url: &str,
        body: &[u8],
    ) -> Result<DurableHttpTurnOutcome, DurableHttpTurnError> {
        validate_request(intent, url, body)?;

        let durable_intent = match self.store.decide(actor_id, intent)? {
            EffectReplayDecision::ReturnReceipt(receipt) => {
                return Ok(DurableHttpTurnOutcome::Replayed(observation_from_receipt(
                    receipt,
                )));
            }
            EffectReplayDecision::ExecuteNew => {
                self.store
                    .create_intent_fenced(actor_id, fence, intent.clone())?;
                intent.clone()
            }
            EffectReplayDecision::RecoverIndeterminate(existing) => {
                self.store
                    .create_intent_fenced(actor_id, fence, existing.clone())?;
                existing
            }
        };

        let provider_key = durable_intent
            .provider_idempotency_key
            .as_deref()
            .ok_or(DurableHttpTurnError::MissingIdempotencyKey)?;

        let response = match ureq::post(url)
            .set("Idempotency-Key", provider_key)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(body)
        {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let payload = read_response(response)?;
                let failure = RecordedEffectFailure {
                    code: format!("http_status_{status}"),
                    message: String::from_utf8_lossy(&payload).into_owned(),
                };
                let receipt = EffectReceipt::failure(
                    &durable_intent,
                    durable_intent.first_attempt,
                    failure.code.clone(),
                    failure.message.clone(),
                );
                return Ok(DurableHttpTurnOutcome::Pending(PendingHttpTurnEffect {
                    observation: HttpTurnObservation::Failed(failure),
                    receipt,
                }));
            }
            Err(ureq::Error::Transport(error)) => {
                // The provider may already have committed. Keep the intent
                // indeterminate; recovery must reuse the same provider key.
                return Err(DurableHttpTurnError::Transport(error.to_string()));
            }
        };

        let payload = read_response(response)?;
        let receipt = EffectReceipt::success(
            &durable_intent,
            durable_intent.first_attempt,
            payload.clone(),
        );
        Ok(DurableHttpTurnOutcome::Pending(PendingHttpTurnEffect {
            observation: HttpTurnObservation::Succeeded(payload),
            receipt,
        }))
    }
}

fn validate_request(
    intent: &EffectIntent,
    url: &str,
    body: &[u8],
) -> Result<(), DurableHttpTurnError> {
    if intent.effect_identity.effect != "Http" || intent.effect_identity.operation != "post" {
        return Err(DurableHttpTurnError::WrongEffectIdentity {
            effect: intent.effect_identity.effect.clone(),
            operation: intent.effect_identity.operation.clone(),
        });
    }
    if intent.request_fingerprint != http_post_request_fingerprint(url, body) {
        return Err(DurableHttpTurnError::RequestFingerprintMismatch);
    }
    if intent.provider_idempotency_key.is_none() {
        return Err(DurableHttpTurnError::MissingIdempotencyKey);
    }
    Ok(())
}

fn read_response(response: ureq::Response) -> Result<Vec<u8>, DurableHttpTurnError> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|error| DurableHttpTurnError::ResponseRead(error.to_string()))?;
    Ok(bytes)
}

fn observation_from_receipt(receipt: EffectReceipt) -> HttpTurnObservation {
    match receipt.outcome {
        EffectOutcome::Succeeded(payload) => HttpTurnObservation::Succeeded(payload),
        EffectOutcome::Failed(failure) => HttpTurnObservation::Failed(failure),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_turn_libsql::{DurableTurnRecord, LibsqlDurableTurnStore};
    use crate::effect_receipt::{EffectIdentity, EffectInvocationId, EffectSiteId};
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

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

    fn intent(actor_id: u64, url: &str, body: &[u8]) -> EffectIntent {
        let site = EffectSiteId::from_semantic_bytes(b"tests.Http.turn.post#0");
        let owner = format!("actor:{actor_id}");
        let invocation = EffectInvocationId::derive(owner.as_bytes(), 21, site, 0);
        EffectIntent::new(
            invocation,
            site,
            EffectIdentity::new("Http", "post").unwrap(),
            http_post_request_fingerprint(url, body),
            Some(invocation.provider_idempotency_key("http-turn-v1")),
        )
    }

    #[test]
    fn provider_result_stays_indeterminate_until_atomic_turn_commit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = calls.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (_key, _body) = read_request(&mut stream);
            observed_calls.fetch_add(1, Ordering::SeqCst);
            write_response(&mut stream, 200, b"provider-ok");
        });

        let actor_id = 42;
        let fence = TestFence { actor_id, epoch: 1 };
        let path = temp_db("http-turn-deferred");
        let url = format!("http://{addr}/mutate");
        let body = b"payload";
        let effect = intent(actor_id, &url, body);

        let effects = LibsqlEffectReceiptStore::new(&path).unwrap();
        let client = DurableHttpTurnClient::new(&effects);
        let pending = match client
            .prepare_post(actor_id, &fence, &effect, &url, body)
            .unwrap()
        {
            DurableHttpTurnOutcome::Pending(pending) => pending,
            DurableHttpTurnOutcome::Replayed(_) => panic!("first execution must be pending"),
        };
        server.join().unwrap();

        // Provider completed, but no independent terminal receipt has been
        // committed. Recovery still sees the intent as indeterminate.
        assert!(matches!(
            effects.decide(actor_id, &effect).unwrap(),
            EffectReplayDecision::RecoverIndeterminate(_)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let turns = LibsqlDurableTurnStore::new(&path).unwrap();
        let turn = DurableTurnRecord::new(
            actor_id,
            21,
            r#"{"count":1}"#,
            r#"{"capacity":8,"committed":["message-21"]}"#,
            effect.invocation_id,
        )
        .unwrap();
        turns
            .commit_turn(actor_id, &fence, turn.clone(), pending.receipt.clone())
            .unwrap();

        assert_eq!(turns.load_turn(actor_id).unwrap(), Some(turn));
        assert!(matches!(
            effects.decide(actor_id, &effect).unwrap(),
            EffectReplayDecision::ReturnReceipt(_)
        ));

        cleanup(&path);
    }

    #[test]
    fn committed_turn_replays_http_without_network() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (_key, _body) = read_request(&mut stream);
            write_response(&mut stream, 200, b"provider-ok");
        });

        let actor_id = 42;
        let fence = TestFence { actor_id, epoch: 1 };
        let path = temp_db("http-turn-replay");
        let url = format!("http://{addr}/mutate");
        let body = b"payload";
        let effect = intent(actor_id, &url, body);
        let effects = LibsqlEffectReceiptStore::new(&path).unwrap();
        let client = DurableHttpTurnClient::new(&effects);

        let pending = match client
            .prepare_post(actor_id, &fence, &effect, &url, body)
            .unwrap()
        {
            DurableHttpTurnOutcome::Pending(pending) => pending,
            DurableHttpTurnOutcome::Replayed(_) => panic!("first execution must be pending"),
        };
        server.join().unwrap();

        let turns = LibsqlDurableTurnStore::new(&path).unwrap();
        turns
            .commit_turn(
                actor_id,
                &fence,
                DurableTurnRecord::new(
                    actor_id,
                    21,
                    r#"{"count":1}"#,
                    r#"{"capacity":8,"committed":["message-21"]}"#,
                    effect.invocation_id,
                )
                .unwrap(),
                pending.receipt,
            )
            .unwrap();

        // The listening socket is gone. Success proves the terminal receipt
        // short-circuits the network after the atomic turn commit.
        assert_eq!(
            client
                .prepare_post(actor_id, &fence, &effect, &url, body)
                .unwrap(),
            DurableHttpTurnOutcome::Replayed(HttpTurnObservation::Succeeded(
                b"provider-ok".to_vec()
            ))
        );

        cleanup(&path);
    }

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang-{name}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
        let mut bytes = Vec::new();
        let header_end = loop {
            if let Some(index) = find_subslice(&bytes, b"\r\n\r\n") {
                break index + 4;
            }
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "connection closed before HTTP headers completed");
            bytes.extend_from_slice(&chunk[..read]);
        };

        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let mut content_length = 0usize;
        let mut idempotency_key = None;
        for line in headers.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse().unwrap();
            }
            if name.eq_ignore_ascii_case("Idempotency-Key") {
                idempotency_key = Some(value.trim().to_string());
            }
        }

        while bytes.len() < header_end + content_length {
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "connection closed before HTTP body completed");
            bytes.extend_from_slice(&chunk[..read]);
        }

        (
            idempotency_key.expect("durable HTTP turn must send Idempotency-Key"),
            bytes[header_end..header_end + content_length].to_vec(),
        )
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let reason = if status == 200 { "OK" } else { "Error" };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }
}
