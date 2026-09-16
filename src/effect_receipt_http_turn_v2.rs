//! Receipt-backed HTTP execution for durable actor turns, v2.
//!
//! Provider execution is separated from durable turn commit. Preparation is
//! race-safe at the local persistence boundary: replay decision, intent/context
//! persistence, and activation fencing happen in one IMMEDIATE transaction.
//! A provider observation returns its receipt to the caller; the receipt only
//! becomes authoritative when `durable_turn_libsql_v2` commits the surrounding
//! actor turn.

use std::fmt;
use std::io::Read;

use crate::effect_receipt::{
    EffectIntent, EffectOutcome, EffectReceipt, EffectReplayDecision, RecordedEffectFailure,
};
use crate::effect_receipt_fence::EffectReceiptFence;
use crate::effect_receipt_http::http_post_request_fingerprint;
use crate::effect_receipt_libsql_v2::{
    DurableEffectContext, LibsqlDurableEffectError, LibsqlDurableEffectStore,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpTurnObservation {
    Succeeded(Vec<u8>),
    Failed(RecordedEffectFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpTurnEffectSource {
    Provider,
    Replay,
}

/// One terminal observation ready to be included in the surrounding durable
/// turn transaction. Replayed effects retain their exact receipt so recovery can
/// recommit the same turn idempotently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedHttpTurnEffect {
    pub observation: HttpTurnObservation,
    pub receipt: EffectReceipt,
    pub source: HttpTurnEffectSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpStatusDisposition {
    Terminal,
    Retryable,
}

pub struct DurableHttpTurnClient<'a> {
    store: &'a LibsqlDurableEffectStore,
}

#[derive(Debug)]
pub enum DurableHttpTurnError {
    WrongEffectIdentity { effect: String, operation: String },
    RequestFingerprintMismatch,
    MissingIdempotencyKey,
    Persistence(LibsqlDurableEffectError),
    Transport(String),
    ResponseRead(String),
    RetryableStatus { status: u16, body: Vec<u8> },
}

impl fmt::Display for DurableHttpTurnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongEffectIdentity { effect, operation } => {
                write!(
                    f,
                    "durable HTTP turn requires Http.post, got {effect}.{operation}"
                )
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
            Self::RetryableStatus { status, .. } => {
                write!(f, "HTTP provider returned retryable status {status}")
            }
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

impl From<LibsqlDurableEffectError> for DurableHttpTurnError {
    fn from(value: LibsqlDurableEffectError) -> Self {
        Self::Persistence(value)
    }
}

impl<'a> DurableHttpTurnClient<'a> {
    pub fn new(store: &'a LibsqlDurableEffectStore) -> Self {
        Self { store }
    }

    /// Prepare one HTTP POST using the conservative default status policy.
    ///
    /// 408, 425, 429 and 5xx stay indeterminate/retryable. Other HTTP status
    /// errors become terminal failure receipts. Call `prepare_post_with_policy`
    /// when provider-specific semantics differ.
    pub fn prepare_post<F: EffectReceiptFence>(
        &self,
        actor_id: u64,
        fence: &F,
        context: DurableEffectContext,
        intent: &EffectIntent,
        url: &str,
        body: &[u8],
    ) -> Result<PreparedHttpTurnEffect, DurableHttpTurnError> {
        self.prepare_post_with_policy(
            actor_id,
            fence,
            context,
            intent,
            url,
            body,
            default_http_status_disposition,
        )
    }

    pub fn prepare_post_with_policy<F, P>(
        &self,
        actor_id: u64,
        fence: &F,
        context: DurableEffectContext,
        intent: &EffectIntent,
        url: &str,
        body: &[u8],
        classify_status: P,
    ) -> Result<PreparedHttpTurnEffect, DurableHttpTurnError>
    where
        F: EffectReceiptFence,
        P: Fn(u16) -> HttpStatusDisposition,
    {
        validate_request(intent, url, body)?;

        let durable_intent = match self
            .store
            .prepare_fenced(actor_id, fence, intent, context)?
        {
            EffectReplayDecision::ReturnReceipt(receipt) => {
                return Ok(PreparedHttpTurnEffect {
                    observation: observation_from_receipt(&receipt),
                    receipt,
                    source: HttpTurnEffectSource::Replay,
                });
            }
            EffectReplayDecision::ExecuteNew => intent.clone(),
            EffectReplayDecision::RecoverIndeterminate(existing) => existing,
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
                if classify_status(status) == HttpStatusDisposition::Retryable {
                    // No terminal receipt. Recovery remains explicitly
                    // indeterminate and must reuse the same provider key.
                    return Err(DurableHttpTurnError::RetryableStatus {
                        status,
                        body: payload,
                    });
                }
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
                return Ok(PreparedHttpTurnEffect {
                    observation: HttpTurnObservation::Failed(failure),
                    receipt,
                    source: HttpTurnEffectSource::Provider,
                });
            }
            Err(ureq::Error::Transport(error)) => {
                return Err(DurableHttpTurnError::Transport(error.to_string()));
            }
        };

        let payload = read_response(response)?;
        let receipt = EffectReceipt::success(
            &durable_intent,
            durable_intent.first_attempt,
            payload.clone(),
        );
        Ok(PreparedHttpTurnEffect {
            observation: HttpTurnObservation::Succeeded(payload),
            receipt,
            source: HttpTurnEffectSource::Provider,
        })
    }
}

pub fn default_http_status_disposition(status: u16) -> HttpStatusDisposition {
    if matches!(status, 408 | 425 | 429) || (500..=599).contains(&status) {
        HttpStatusDisposition::Retryable
    } else {
        HttpStatusDisposition::Terminal
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

fn observation_from_receipt(receipt: &EffectReceipt) -> HttpTurnObservation {
    match &receipt.outcome {
        EffectOutcome::Succeeded(payload) => HttpTurnObservation::Succeeded(payload.clone()),
        EffectOutcome::Failed(failure) => HttpTurnObservation::Failed(failure.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_turn_libsql_v2::{DurableTurnRecord, LibsqlDurableTurnStore};
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

    fn intent(
        actor_id: u64,
        sequence: u64,
        occurrence: u32,
        url: &str,
        body: &[u8],
    ) -> (EffectIntent, DurableEffectContext) {
        let site = EffectSiteId::from_semantic_bytes(b"tests.Http.turn.post#0");
        let owner = format!("actor:{actor_id}");
        let invocation = EffectInvocationId::derive(owner.as_bytes(), sequence, site, occurrence);
        (
            EffectIntent::new(
                invocation,
                site,
                EffectIdentity::new("Http", "post").unwrap(),
                http_post_request_fingerprint(url, body),
                Some(invocation.provider_idempotency_key("http-turn-v2")),
            ),
            DurableEffectContext::new(actor_id, sequence, occurrence),
        )
    }

    fn bundle(sequence: u64) -> String {
        format!(
            r#"{{"actor_sequence":{sequence},"state":{{"count":1}},"dedup":{{"capacity":8,"committed":[]}}}}"#
        )
    }

    #[test]
    fn provider_result_is_deferred_then_replays_with_exact_receipt() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            observed.fetch_add(1, Ordering::SeqCst);
            write_response(&mut stream, 200, b"provider-ok");
        });

        let actor_id = 42;
        let sequence = 21;
        let fence = TestFence { actor_id, epoch: 1 };
        let path = temp_db("http-turn-v2");
        let url = format!("http://{addr}/mutate");
        let body = b"payload";
        let (effect, context) = intent(actor_id, sequence, 0, &url, body);
        let effects = LibsqlDurableEffectStore::new(&path).unwrap();
        let client = DurableHttpTurnClient::new(&effects);

        let prepared = client
            .prepare_post(actor_id, &fence, context, &effect, &url, body)
            .unwrap();
        assert_eq!(prepared.source, HttpTurnEffectSource::Provider);
        server.join().unwrap();
        assert!(matches!(
            effects.load(actor_id, effect.invocation_id).unwrap(),
            Some(crate::effect_receipt::PersistedEffectState::Intent(_))
        ));

        let turn =
            DurableTurnRecord::new(actor_id, bundle(sequence), vec![effect.invocation_id]).unwrap();
        LibsqlDurableTurnStore::new(&path)
            .unwrap()
            .commit_turn(actor_id, &fence, turn, vec![prepared.receipt.clone()])
            .unwrap();

        // Server is gone. Replay must return the same receipt without network.
        let replayed = client
            .prepare_post(actor_id, &fence, context, &effect, &url, body)
            .unwrap();
        assert_eq!(replayed.source, HttpTurnEffectSource::Replay);
        assert_eq!(replayed.receipt, prepared.receipt);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cleanup(&path);
    }

    #[test]
    fn retryable_status_leaves_intent_indeterminate() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            write_response(&mut stream, 503, b"try later");
        });

        let actor_id = 42;
        let sequence = 5;
        let fence = TestFence { actor_id, epoch: 1 };
        let path = temp_db("http-turn-retryable-v2");
        let url = format!("http://{addr}/mutate");
        let (effect, context) = intent(actor_id, sequence, 0, &url, b"payload");
        let effects = LibsqlDurableEffectStore::new(&path).unwrap();
        let client = DurableHttpTurnClient::new(&effects);

        assert!(matches!(
            client.prepare_post(actor_id, &fence, context, &effect, &url, b"payload"),
            Err(DurableHttpTurnError::RetryableStatus { status: 503, .. })
        ));
        server.join().unwrap();
        assert!(matches!(
            effects.load(actor_id, effect.invocation_id).unwrap(),
            Some(crate::effect_receipt::PersistedEffectState::Intent(_))
        ));
        cleanup(&path);
    }

    #[test]
    fn provider_specific_policy_can_make_status_terminal() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            write_response(&mut stream, 409, b"already exists");
        });

        let actor_id = 42;
        let sequence = 6;
        let fence = TestFence { actor_id, epoch: 1 };
        let path = temp_db("http-turn-policy-v2");
        let url = format!("http://{addr}/mutate");
        let (effect, context) = intent(actor_id, sequence, 0, &url, b"payload");
        let effects = LibsqlDurableEffectStore::new(&path).unwrap();
        let client = DurableHttpTurnClient::new(&effects);

        let prepared = client
            .prepare_post_with_policy(actor_id, &fence, context, &effect, &url, b"payload", |_| {
                HttpStatusDisposition::Terminal
            })
            .unwrap();
        server.join().unwrap();
        assert!(matches!(
            prepared.observation,
            HttpTurnObservation::Failed(_)
        ));
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

    fn read_request(stream: &mut TcpStream) {
        let mut bytes = Vec::new();
        let header_end = loop {
            if let Some(index) = find_subslice(&bytes, b"\r\n\r\n") {
                break index + 4;
            }
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&chunk[..read]);
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let mut content_length = 0usize;
        let mut has_idempotency_key = false;
        for line in headers.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = value.trim().parse().unwrap();
            }
            if name.eq_ignore_ascii_case("Idempotency-Key") {
                has_idempotency_key = !value.trim().is_empty();
            }
        }
        while bytes.len() < header_end + content_length {
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0);
            bytes.extend_from_slice(&chunk[..read]);
        }
        assert!(has_idempotency_key);
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let reason = if status < 400 { "OK" } else { "Error" };
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
