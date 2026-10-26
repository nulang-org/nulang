//! Authenticated, destination-pinned HTTPS delivery through an explicitly
//! trusted host. This is an outbound client; server-side idempotency remains
//! mandatory and is demonstrated with a local receiver in the unit tests.
//!
//! Not callable as an ambient VM effect. A trusted host authorizes each
//! destination and actor before invoking the durable delivery coordinator.

use std::io::{self, Error, ErrorKind, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

const MAX_RESPONSE_BYTES: usize = 4096;
const MAX_REQUEST_BYTES: usize = 64 * 1024;

pub struct AuthenticatedWorkflowHttpProvider {
    endpoint: String,
    destination: String,
    authorization: String,
    agent: ureq::Agent,
}

impl AuthenticatedWorkflowHttpProvider {
    /// Configure an approved external HTTPS endpoint. The trusted host is
    /// pinned to the *exact* configured hostname; neither untrusted workflow
    /// data nor remote redirects may select an outbound destination.
    ///
    /// The caller must additionally enforce DNS/egress restrictions to stop
    /// internal-address resolution or DNS rebinding of a trusted hostname.
    /// Tokens are never logged and should be supplied by a secret provider.
    pub fn new(
        endpoint: &str,
        trusted_host: &str,
        destination: &str,
        bearer_token: &str,
    ) -> io::Result<Self> {
        if !valid_dns_hostname(trusted_host) {
            return Err(invalid("invalid trusted outbound hostname"));
        }
        let authority = endpoint
            .strip_prefix("https://")
            .and_then(|tail| tail.split_once('/'))
            .ok_or_else(|| invalid("HTTPS endpoint must include a path"))?;
        if authority.0 != trusted_host && authority.0 != format!("{trusted_host}:443") {
            return Err(invalid("endpoint host is not approved"));
        }
        if authority.1.is_empty()
            || authority.1.contains(['?', '#', '\\'])
            || endpoint.chars().any(char::is_control)
        {
            return Err(invalid("endpoint path may not contain query or fragment"));
        }
        Self::build(endpoint, destination, bearer_token, Some(trusted_host))
    }

    fn build(
        endpoint: &str,
        destination: &str,
        bearer_token: &str,
        trusted_host: Option<&str>,
    ) -> io::Result<Self> {
        if !valid_header_value(destination, 2048) || !valid_bearer_token(bearer_token) {
            return Err(invalid("invalid destination or bearer credential"));
        }
        let builder = ureq::AgentBuilder::new()
            .redirects(0)
            .try_proxy_from_env(false)
            .timeout_connect(Duration::from_secs(2))
            .timeout_read(Duration::from_secs(5))
            .timeout_write(Duration::from_secs(5))
            .timeout(Duration::from_secs(10));
        // Validate the actual socket addresses returned *at connection time*.
        // Resolving ahead of the request would allow DNS rebinding between
        // validation and connection. The TLS URL keeps the original host for
        // certificate and SNI checks.
        let builder = match trusted_host {
            Some(host) => builder.resolver(PublicDnsResolver {
                trusted_host: host.to_string(),
            }),
            None => builder, // private test-only loopback constructor
        };
        let agent = builder.build();
        Ok(Self {
            endpoint: endpoint.to_string(),
            destination: destination.to_string(),
            authorization: format!("Bearer {bearer_token}"),
            agent,
        })
    }

    /// The destination is checked again at dispatch, guarding against a host
    /// accidentally using one configured credential for a different webhook.
    /// The remote provider MUST atomically deduplicate `key` and return the
    /// same acceptance receipt bytes for duplicate requests.
    pub fn dispatch(&self, key: &str, destination: &str, payload: &[u8]) -> io::Result<Vec<u8>> {
        if destination != self.destination || !valid_key(key) || payload.len() > MAX_REQUEST_BYTES {
            return Err(invalid(
                "invalid idempotency key, destination or payload size",
            ));
        }
        let result = self
            .agent
            .post(&self.endpoint)
            .set("Authorization", &self.authorization)
            .set("Idempotency-Key", key)
            .set("X-Nulang-Delivery-Destination", destination)
            .set("Content-Type", "application/octet-stream")
            .set("Cache-Control", "no-store")
            .send_bytes(payload);
        let response = match result {
            Ok(response) => response,
            Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    "provider rejected authorization",
                ));
            }
            Err(ureq::Error::Status(_, _)) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    "provider did not accept request",
                ));
            }
            Err(ureq::Error::Transport(_)) => {
                // Do not expose raw TLS, proxy, endpoint or credential details.
                return Err(Error::new(
                    ErrorKind::Other,
                    "provider transport failed; acceptance is ambiguous",
                ));
            }
        };
        if !matches!(response.status(), 200 | 201) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "provider did not return an acceptance status",
            ));
        }
        if response.header("Idempotency-Key") != Some(key) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "provider acceptance key does not match request",
            ));
        }
        let mut receipt = Vec::new();
        response
            .into_reader()
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut receipt)?;
        if receipt.is_empty() || receipt.len() > MAX_RESPONSE_BYTES {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "provider acceptance receipt is empty or oversized",
            ));
        }
        Ok(receipt)
    }

    #[cfg(test)]
    fn for_loopback_test(endpoint: &str, destination: &str, token: &str) -> io::Result<Self> {
        // This exception exists only in `cargo test` inside this module. There
        // is no public 'allow insecure HTTP' flag or runtime environment knob.
        let authority = endpoint
            .strip_prefix("http://")
            .and_then(|tail| tail.split_once('/'))
            .ok_or_else(|| invalid("invalid loopback test URL"))?
            .0;
        let addr: std::net::SocketAddr = authority
            .parse()
            .map_err(|_| invalid("invalid loopback address"))?;
        if !addr.ip().is_loopback() {
            return Err(invalid("test-only HTTP must bind loopback"));
        }
        Self::build(endpoint, destination, token, None)
    }
}

/// Fail-closed DNS resolver for a single preapproved public HTTPS origin.
/// All returned addresses must be eligible, not just the first candidate.
/// TLS still validates the original hostname when the HTTP client connects.
struct PublicDnsResolver {
    trusted_host: String,
}

impl ureq::Resolver for PublicDnsResolver {
    fn resolve(&self, netloc: &str) -> io::Result<Vec<SocketAddr>> {
        let answers = netloc.to_socket_addrs()?.collect();
        validate_dns_answers(netloc, &self.trusted_host, answers)
    }
}

fn validate_dns_answers(
    netloc: &str,
    trusted_host: &str,
    answers: Vec<SocketAddr>,
) -> io::Result<Vec<SocketAddr>> {
    // The connector is pinned to HTTPS/443 with redirects and proxies off.
    // Reject any attempt to resolve an alternate hostname or port, including
    // unexpectedly requested netlocs from future HTTP-client changes.
    if netloc != format!("{trusted_host}:443") || answers.is_empty() {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "outbound DNS origin not approved",
        ));
    }
    if answers
        .iter()
        .any(|address| address.port() != 443 || !public_address(address.ip()))
    {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            "outbound DNS returned a restricted address",
        ));
    }
    Ok(answers)
}

fn public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(value) => public_ipv4(value),
        IpAddr::V6(value) => public_ipv6(value),
    }
}

fn public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    // Deny IANA special-purpose, private, loopback, link-local, shared,
    // documentation, benchmark, broadcast, reserved and multicast ranges.
    if a == 0
        || a == 10
        || a == 127
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
    {
        return false;
    }
    true
}

fn public_ipv6(address: Ipv6Addr) -> bool {
    // Accept native globally scoped unicast only. Block documentation and
    // transition/special-purpose ranges even when under 2000::/3. In
    // particular, 6to4 could tunnel an embedded private IPv4 address.
    let segments = address.segments();
    let first = segments[0];
    first & 0xe000 == 0x2000
        && !(first == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8))
        && first != 0x2002 // 6to4 transition addresses
        && first != 0x3fff // documentation range
}

fn invalid(message: &'static str) -> io::Error {
    Error::new(ErrorKind::InvalidInput, message)
}
fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_header_value(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
}
fn valid_bearer_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 2048
        && token.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
}
fn valid_dns_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || host.parse::<IpAddr>().is_ok() || !host.contains('.')
    {
        return false;
    }
    host.split('.').all(|part| {
        !part.is_empty()
            && part.len() <= 63
            && !part.starts_with('-')
            && !part.ends_with('-')
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        MemoryStore, PersistedValue, PersistenceStore, WorkflowActivationId, WorkflowEvent,
        WorkflowReplayEventId,
    };
    use crate::workflow_local_provider::LocalAcceptanceProvider;
    use crate::workflow_output::{read_workflow_output_receipts, WorkflowOutputReceipt};
    use crate::workflow_output_delivery::deliver_output_with_deduplication;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    const ACTOR: u64 = 81;
    fn store() -> (MemoryStore, WorkflowOutputReceipt) {
        let mut store = MemoryStore::new();
        store
            .append_workflow_event(
                ACTOR,
                WorkflowEvent::Custom {
                    sequence: 1,
                    replay_id: Some(WorkflowReplayEventId::new(
                        WorkflowActivationId::new(ACTOR, 13),
                        0,
                    )),
                    name: "WorkflowOutput".into(),
                    args: vec![PersistedValue::String("ready".into())],
                },
            )
            .unwrap();
        let receipt = read_workflow_output_receipts(&store, ACTOR, 0, 1)
            .unwrap()
            .pop()
            .unwrap();
        (store, receipt)
    }
    fn unique_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nulang-http-auth-provider-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn read_request(stream: &mut TcpStream) -> io::Result<(Vec<(String, String)>, Vec<u8>)> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut first = String::new();
        reader.read_line(&mut first)?;
        if !first.starts_with("POST /accept HTTP/1.1") {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "unexpected HTTP request",
            ));
        }
        let mut headers = Vec::new();
        let mut total = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            total += line.len();
            if total > 8192 {
                return Err(Error::new(ErrorKind::InvalidData, "excessive test headers"));
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            let (key, value) = line
                .split_once(':')
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "malformed test header"))?;
            headers.push((key.to_ascii_lowercase(), value.trim().to_owned()));
        }
        let content_len = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing request length"))?
            .1
            .parse::<usize>()
            .map_err(|_| Error::new(ErrorKind::InvalidData, "bad test length"))?;
        if content_len > 65536 {
            return Err(Error::new(ErrorKind::InvalidData, "oversized test request"));
        }
        let mut bytes = vec![0; content_len];
        reader.read_exact(&mut bytes)?;
        Ok((headers, bytes))
    }
    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn reply(
        stream: &mut TcpStream,
        status: &str,
        key: Option<&str>,
        body: &[u8],
        location: Option<&str>,
    ) -> io::Result<()> {
        let mut headers = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(key) = key {
            headers.push_str(&format!("Idempotency-Key: {key}\r\n"));
        }
        if let Some(location) = location {
            headers.push_str(&format!("Location: {location}\r\n"));
        }
        headers.push_str("\r\n");
        stream.write_all(headers.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()
    }

    #[test]
    fn production_dns_resolver_fails_closed_on_internal_or_mixed_answers() {
        use std::net::SocketAddr;
        fn addr(value: &str) -> SocketAddr {
            value.parse().unwrap()
        }
        let public = vec![addr("8.8.8.8:443"), addr("[2606:4700:4700::1111]:443")];
        assert_eq!(
            validate_dns_answers(
                "events.example.com:443",
                "events.example.com",
                public.clone()
            )
            .unwrap(),
            public
        );
        assert!(
            validate_dns_answers("events.example.com:443", "events.example.com", vec![]).is_err()
        );
        assert!(
            validate_dns_answers("evil.example.com:443", "events.example.com", public.clone())
                .is_err()
        );
        assert!(validate_dns_answers(
            "events.example.com:80",
            "events.example.com",
            public.clone()
        )
        .is_err());
        assert!(validate_dns_answers(
            "events.example.com:443",
            "events.example.com",
            vec![public[0], addr("127.0.0.1:443")]
        )
        .is_err());
        for private in [
            "0.0.0.0:443",
            "10.0.0.2:443",
            "127.0.0.1:443",
            "169.254.169.254:443",
            "172.16.2.5:443",
            "192.168.1.1:443",
            "100.64.0.1:443",
            "192.0.2.3:443",
            "198.18.0.1:443",
            "203.0.113.10:443",
            "224.0.0.1:443",
            "[::1]:443",
            "[fd00::1]:443",
            "[fe80::1]:443",
            "[::ffff:127.0.0.1]:443",
            "[2001:db8::1]:443",
            "[2002:c0a8:0101::1]:443",
            "[3fff::1]:443",
        ] {
            assert!(
                validate_dns_answers(
                    "events.example.com:443",
                    "events.example.com",
                    vec![addr(private)]
                )
                .is_err(),
                "unexpected accepted address {private}"
            );
        }
    }

    #[test]
    fn authenticated_http_accept_then_lost_response_retries_same_provider_key() {
        let root = unique_dir();
        let local = LocalAcceptanceProvider::open(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/accept", listener.local_addr().unwrap());
        let task = thread::spawn(move || {
            for request in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (headers, body) = read_request(&mut stream).unwrap();
                assert_eq!(
                    header(&headers, "authorization"),
                    Some("Bearer example-secret")
                );
                assert_eq!(
                    header(&headers, "x-nulang-delivery-destination"),
                    Some("webhook:shipping")
                );
                let key = header(&headers, "idempotency-key").unwrap();
                let accepted = local.accept(key, "webhook:shipping", &body).unwrap();
                if request == 1 {
                    reply(&mut stream, "200 OK", Some(key), &accepted, None).unwrap();
                }
                // Request 0: acceptance was persisted, but socket is dropped
                // without an HTTP response. Retry must keep the same key.
            }
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
            std::fs::remove_dir_all(root).unwrap();
        });
        let client = AuthenticatedWorkflowHttpProvider::for_loopback_test(
            &endpoint,
            "webhook:shipping",
            "example-secret",
        )
        .unwrap();
        let (mut store, receipt) = store();
        let result = deliver_output_with_deduplication(
            &mut store,
            ACTOR,
            1,
            &receipt,
            "webhook:shipping",
            |k, d, p| client.dispatch(k, d, p),
        );
        assert!(result.is_err());
        let mut recovery = store.clone();
        let complete = deliver_output_with_deduplication(
            &mut recovery,
            ACTOR,
            1,
            &receipt,
            "webhook:shipping",
            |k, d, p| client.dispatch(k, d, p),
        )
        .unwrap();
        assert!(!complete.is_empty());
        let replay = deliver_output_with_deduplication(
            &mut recovery,
            ACTOR,
            1,
            &receipt,
            "webhook:shipping",
            |_, _, _| panic!("completed receipt must not contact network"),
        )
        .unwrap();
        assert_eq!(complete, replay);
        task.join().unwrap();
    }

    #[test]
    fn authentication_failure_does_not_accept_or_leak_token_in_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/accept", listener.local_addr().unwrap());
        let task = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (headers, _) = read_request(&mut stream).unwrap();
            assert_eq!(
                header(&headers, "authorization"),
                Some("Bearer wrong-secret")
            );
            reply(&mut stream, "401 Unauthorized", None, b"no", None).unwrap();
        });
        let client = AuthenticatedWorkflowHttpProvider::for_loopback_test(
            &endpoint,
            "webhook:billing",
            "wrong-secret",
        )
        .unwrap();
        let err = client
            .dispatch(&"a".repeat(64), "webhook:billing", b"data")
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert!(!err.to_string().contains("wrong-secret"));
        task.join().unwrap();
    }

    #[test]
    fn redirect_and_mismatched_receipt_key_are_rejected() {
        for action in ["redirect", "wrong-key"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}/accept", listener.local_addr().unwrap());
            let mode = action.to_string();
            let task = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let (headers, _) = read_request(&mut stream).unwrap();
                assert_eq!(header(&headers, "authorization"), Some("Bearer demo"));
                if mode == "redirect" {
                    reply(
                        &mut stream,
                        "307 Temporary Redirect",
                        None,
                        b"moved",
                        Some("http://127.0.0.1:9/accept"),
                    )
                    .unwrap();
                } else {
                    reply(
                        &mut stream,
                        "200 OK",
                        Some(&"b".repeat(64)),
                        b"accepted",
                        None,
                    )
                    .unwrap();
                }
            });
            let client = AuthenticatedWorkflowHttpProvider::for_loopback_test(
                &endpoint,
                "webhook:billing",
                "demo",
            )
            .unwrap();
            let err = client
                .dispatch(&"a".repeat(64), "webhook:billing", b"request")
                .unwrap_err();
            if action == "wrong-key" {
                assert_eq!(err.kind(), ErrorKind::InvalidData);
            }
            task.join().unwrap();
        }
    }
}
