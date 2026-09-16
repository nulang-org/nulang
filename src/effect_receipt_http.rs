//! Receipt-backed durable HTTP mutation adapter.
//!
//! This is the first real external-effect consumer of RFC 0030. It deliberately
//! lives beside the existing Experimental `Http.post` VM builtin until the
//! receipt protocol has hardened. A transport failure after provider execution
//! leaves the persisted intent indeterminate; replay reuses the same provider
//! idempotency key instead of allocating a new logical operation.

use std::fmt;
use std::io::Read;

use crate::effect_receipt::{
    EffectIntent, EffectOutcome, EffectReplayDecision, EffectReceipt, RecordedEffectFailure,
    RequestFingerprint,
};
use crate::effect_receipt_fence::EffectReceiptFence;
use crate::effect_receipt_libsql::{LibsqlEffectReceiptError, LibsqlEffectReceiptStore};

pub struct DurableHttpClient<'a> {
    store: &'a LibsqlEffectReceiptStore,
}

#[derive(Debug)]
pub enum DurableHttpError {
    WrongEffectIdentity { effect: String, operation: String },
    RequestFingerprintMismatch,
    MissingIdempotencyKey,
    Persistence(LibsqlEffectReceiptError),
    Transport(String),
    ResponseRead(String),
    HttpStatus { status: u16, body: Vec<u8> },
    RecordedFailure(RecordedEffectFailure),
}

impl fmt::Display for DurableHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongEffectIdentity { effect, operation } => {
                write!(f, "durable HTTP POST requires Http.post, got {effect}.{operation}")
            }
            Self::RequestFingerprintMismatch => {
                f.write_str("durable HTTP POST request fingerprint mismatch")
            }
            Self::MissingIdempotencyKey => {
                f.write_str("durable HTTP POST requires a persisted provider idempotency key")
            }
            Self::Persistence(error) => write!(f, "{error}"),
            Self::Transport(error) => write!(f, "HTTP transport failed: {error}"),
            Self::ResponseRead(error) => write!(f, "failed reading HTTP response: {error}"),
            Self::HttpStatus { status, .. } => write!(f, "HTTP provider returned status {status}"),
            Self::RecordedFailure(failure) => {
                write!(f, "recorded HTTP effect failure {}: {}", failure.code, failure.message)
            }
        }
    }
}

impl std::error::Error for DurableHttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Persistence(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LibsqlEffectReceiptError> for DurableHttpError {
    fn from(value: LibsqlEffectReceiptError) -> Self {
        Self::Persistence(value)
    }
}

impl<'a> DurableHttpClient<'a> {
    pub fn new(store: &'a LibsqlEffectReceiptStore) -> Self {
        Self { store }
    }

    /// Execute or replay one durable HTTP POST.
    ///
    /// `intent` must carry the fingerprint returned by
    /// [`http_post_request_fingerprint`] and a persisted provider idempotency
    /// key. A terminal receipt returns without touching the network.
    pub fn post<F: EffectReceiptFence>(
        &self,
        actor_id: u64,
        fence: &F,
        intent: &EffectIntent,
        url: &str,
        body: &[u8],
    ) -> Result<Vec<u8>, DurableHttpError> {
        validate_request(intent, url, body)?;

        let durable_intent = match self.store.decide(actor_id, intent)? {
            EffectReplayDecision::ReturnReceipt(receipt) => {
                return replay_receipt(receipt);
            }
            EffectReplayDecision::ExecuteNew => {
                self.store
                    .create_intent_fenced(actor_id, fence, intent.clone())?;
                intent.clone()
            }
            EffectReplayDecision::RecoverIndeterminate(existing) => {
                // Re-observing the identical intent under the current fence is
                // what advances authority after failover before retrying the
                // provider. The logical invocation/provider key do not change.
                self.store
                    .create_intent_fenced(actor_id, fence, existing.clone())?;
                existing
            }
        };

        let provider_key = durable_intent
            .provider_idempotency_key
            .as_deref()
            .ok_or(DurableHttpError::MissingIdempotencyKey)?;

        let response = match ureq::post(url)
            .set("Idempotency-Key", provider_key)
            .set("Content-Type", "application/octet-stream")
            .send_bytes(body)
        {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let payload = read_response(response)?;
                let failure_message = String::from_utf8_lossy(&payload).into_owned();
                let receipt = EffectReceipt::failure(
                    &durable_intent,
                    durable_intent.first_attempt,
                    format!("http_status_{status}"),
                    failure_message,
                );
                self.store
                    .commit_receipt_fenced(actor_id, fence, receipt)?;
                return Err(DurableHttpError::HttpStatus {
                    status,
                    body: payload,
                });
            }
            Err(ureq::Error::Transport(error)) => {
                // No terminal receipt: the provider may already have committed.
                // Recovery must retry/query with the same persisted key.
                return Err(DurableHttpError::Transport(error.to_string()));
            }
        };

        let payload = read_response(response)?;
        let receipt = EffectReceipt::success(
            &durable_intent,
            durable_intent.first_attempt,
            payload.clone(),
        );
        self.store
            .commit_receipt_fenced(actor_id, fence, receipt)?;
        Ok(payload)
    }
}

pub fn http_post_request_fingerprint(url: &str, body: &[u8]) -> RequestFingerprint {
    let mut canonical = Vec::with_capacity(url.len() + body.len() + 32);
    push_part(&mut canonical, b"POST");
    push_part(&mut canonical, url.as_bytes());
    push_part(&mut canonical, body);
    RequestFingerprint::from_canonical_bytes(&canonical)
}

fn push_part(target: &mut Vec<u8>, part: &[u8]) {
    target.extend_from_slice(&(part.len() as u64).to_le_bytes());
    target.extend_from_slice(part);
}

fn validate_request(intent: &EffectIntent, url: &str, body: &[u8]) -> Result<(), DurableHttpError> {
    if intent.effect_identity.effect != "Http" || intent.effect_identity.operation != "post" {
        return Err(DurableHttpError::WrongEffectIdentity {
            effect: intent.effect_identity.effect.clone(),
            operation: intent.effect_identity.operation.clone(),
        });
    }
    if intent.request_fingerprint != http_post_request_fingerprint(url, body) {
        return Err(DurableHttpError::RequestFingerprintMismatch);
    }
    if intent.provider_idempotency_key.is_none() {
        return Err(DurableHttpError::MissingIdempotencyKey);
    }
    Ok(())
}

fn read_response(response: ureq::Response) -> Result<Vec<u8>, DurableHttpError> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|error| DurableHttpError::ResponseRead(error.to_string()))?;
    Ok(bytes)
}

fn replay_receipt(receipt: EffectReceipt) -> Result<Vec<u8>, DurableHttpError> {
    match receipt.outcome {
        EffectOutcome::Succeeded(payload) => Ok(payload),
        EffectOutcome::Failed(failure) => Err(DurableHttpError::RecordedFailure(failure)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect_receipt::{EffectIdentity, EffectInvocationId, EffectSiteId};
    use std::collections::HashMap;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy)]
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
        let site = EffectSiteId::from_semantic_bytes(b"tests.Http.post#0");
        let owner = format!("actor:{actor_id}");
        let invocation = EffectInvocationId::derive(owner.as_bytes(), 11, site, 0);
        let provider_key = Some(invocation.provider_idempotency_key("http-post-v1"));
        EffectIntent::new(
            invocation,
            site,
            EffectIdentity::new("Http", "post").unwrap(),
            http_post_request_fingerprint(url, body),
            provider_key,
        )
    }

    #[test]
    fn terminal_receipt_replays_without_a_second_network_call() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (_key, _body) = read_request(&mut stream);
            observed.fetch_add(1, Ordering::SeqCst);
            write_response(&mut stream, 200, b"provider-ok");
        });

        let actor_id = 42;
        let fence = TestFence { actor_id, epoch: 1 };
        let store = LibsqlEffectReceiptStore::in_memory().unwrap();
        let client = DurableHttpClient::new(&store);
        let url = format!("http://{addr}/mutate");
        let body = b"payload";
        let effect = intent(actor_id, &url, body);

        assert_eq!(
            client.post(actor_id, &fence, &effect, &url, body).unwrap(),
            b"provider-ok"
        );
        server.join().unwrap();

        // The listener is gone. A second network call would fail, so success
        // here proves the receipt short-circuited provider execution.
        assert_eq!(
            client.post(actor_id, &fence, &effect, &url, body).unwrap(),
            b"provider-ok"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lost_provider_response_retries_same_key_without_duplicate_mutation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let applications = Arc::new(AtomicUsize::new(0));
        let provider_state: Arc<Mutex<HashMap<String, Vec<u8>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let observed_applications = applications.clone();
        let observed_state = provider_state.clone();

        let server = std::thread::spawn(move || {
            for request_index in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let (key, _body) = read_request(&mut stream);
                let response = {
                    let mut state = observed_state.lock().unwrap();
                    state
                        .entry(key)
                        .or_insert_with(|| {
                            observed_applications.fetch_add(1, Ordering::SeqCst);
                            b"provider-ok".to_vec()
                        })
                        .clone()
                };

                if request_index == 0 {
                    // Provider mutation committed, but the response is lost.
                    drop(stream);
                } else {
                    write_response(&mut stream, 200, &response);
                }
            }
        });

        let actor_id = 42;
        let fence = TestFence { actor_id, epoch: 1 };
        let store = LibsqlEffectReceiptStore::in_memory().unwrap();
        let client = DurableHttpClient::new(&store);
        let url = format!("http://{addr}/mutate");
        let body = b"payload";
        let effect = intent(actor_id, &url, body);

        assert!(matches!(
            client.post(actor_id, &fence, &effect, &url, body),
            Err(DurableHttpError::Transport(_)) | Err(DurableHttpError::ResponseRead(_))
        ));
        assert!(matches!(
            store.decide(actor_id, &effect).unwrap(),
            EffectReplayDecision::RecoverIndeterminate(_)
        ));

        assert_eq!(
            client.post(actor_id, &fence, &effect, &url, body).unwrap(),
            b"provider-ok"
        );
        server.join().unwrap();
        assert_eq!(applications.load(Ordering::SeqCst), 1);
        assert!(matches!(
            store.decide(actor_id, &effect).unwrap(),
            EffectReplayDecision::ReturnReceipt(_)
        ));
    }

    #[test]
    fn fingerprint_mismatch_fails_before_network_or_persistence() {
        let actor_id = 42;
        let fence = TestFence { actor_id, epoch: 1 };
        let store = LibsqlEffectReceiptStore::in_memory().unwrap();
        let client = DurableHttpClient::new(&store);
        let url = "http://127.0.0.1:1/mutate";
        let effect = intent(actor_id, url, b"original");

        assert!(matches!(
            client.post(actor_id, &fence, &effect, url, b"changed"),
            Err(DurableHttpError::RequestFingerprintMismatch)
        ));
        assert_eq!(store.accepted_epoch(actor_id).unwrap(), None);
        assert!(store.load(actor_id, effect.invocation_id).unwrap().is_none());
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
            idempotency_key.expect("durable HTTP request must send Idempotency-Key"),
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
        haystack.windows(needle.len()).position(|window| window == needle)
    }
}
