//! Process-level tests for the single-shard Fabric executable.
#![cfg(all(unix, feature = "cache-server"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Server {
    fn start(address: SocketAddr, args: &[&str]) -> Self {
        Self::start_with(address, args, |_| {})
    }

    fn start_with(
        address: SocketAddr,
        args: &[&str],
        configure: impl FnOnce(&mut Command),
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nulang-fabric"));
        command
            .args(["--bind", &address.to_string()])
            .args(args)
            .stdout(Stdio::piped());
        configure(&mut command);
        let child = command.spawn().unwrap();
        let mut server = Self(child);
        let stdout = server.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send((result, line));
        });
        let (result, line) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server readiness");
        assert!(result.unwrap() > 0);
        assert!(line.starts_with(&format!("READY {address} ")), "{line}");
        server
    }
}

struct Temp(PathBuf);
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn request(address: SocketAddr, wire: &[u8], expected: &[u8]) {
    let mut connection = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    connection.write_all(wire).unwrap();
    let mut actual = vec![0; expected.len()];
    connection.read_exact(&mut actual).unwrap();
    assert_eq!(actual, expected);
}

const SET: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\nvalue\r\n";
const GET: &[u8] = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";

#[test]
fn memory_mode_does_not_recover_after_process_kill() {
    let address = port();
    let server = Server::start(address, &[]);
    request(address, SET, b"+OK\r\n");
    drop(server);
    let _server = Server::start(address, &[]);
    request(address, GET, b"$-1\r\n");
}

#[test]
fn journal_modes_recover_after_process_kill() {
    let temp = Temp(std::env::temp_dir().join(format!(
            "nulang-fabric-process-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir(&temp.0).unwrap();
    for mode in ["buffered", "sync"] {
        let path = temp.0.join(format!("{mode}.wal"));
        let args = ["--mode", mode, "--journal", path.to_str().unwrap()];
        let address = port();
        let server = Server::start(address, &args);
        request(address, SET, b"+OK\r\n");
        // Child::kill sends SIGKILL on Unix; no graceful shutdown or checkpoint.
        drop(server);
        let _server = Server::start(address, &args);
        request(address, GET, b"$5\r\nvalue\r\n");
    }
}

#[test]
fn automatic_checkpoint_keeps_small_journal_writable_across_restart() {
    let temp = Temp(std::env::temp_dir().join(format!(
            "nulang-fabric-auto-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir(&temp.0).unwrap();
    let path = temp.0.join("auto.wal");
    let args = [
        "--mode",
        "sync",
        "--journal",
        path.to_str().unwrap(),
        "--max-journal-bytes",
        "1024",
        "--max-record-bytes",
        "256",
        "--checkpoint-percent",
        "70",
    ];
    let address = port();
    let server = Server::start(address, &args);
    let mut pipeline = Vec::new();
    let mut expected = Vec::new();
    for _ in 0..100 {
        pipeline.extend_from_slice(SET);
        expected.extend_from_slice(b"+OK\r\n");
    }
    request(address, &pipeline, &expected);
    assert!(std::fs::metadata(&path).unwrap().len() <= 1024);
    assert_eq!(&std::fs::read(&path).unwrap()[..8], b"NLCWAL03");
    drop(server);
    let _server = Server::start(address, &args);
    request(address, GET, b"$5\r\nvalue\r\n");
}

#[test]
fn termination_signals_exit_successfully_and_release_journal() {
    let temp = Temp(std::env::temp_dir().join(format!(
            "nulang-fabric-stop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir(&temp.0).unwrap();
    let path = temp.0.join("stop.wal");
    let args = ["--mode", "buffered", "--journal", path.to_str().unwrap()];
    let address = port();
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let mut server = Server::start(address, &args);
        request(address, SET, b"+OK\r\n");
        let mut idle = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        idle.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        // Incomplete input must not hold shutdown open.
        idle.write_all(b"*3\r\n$3\r\nSET\r\n").unwrap();
        // SAFETY: the PID belongs to our live child process.
        assert_eq!(
            unsafe { libc::kill(server.0.id() as libc::pid_t, signal) },
            0
        );
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(status) = server.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "shutdown timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(status.success(), "signal {signal}: {status}");
        let mut byte = [0];
        match idle.read(&mut byte) {
            Ok(0) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("connection was not closed: {other:?}"),
        }
        drop(server);
        let _recovered = Server::start(address, &args);
        request(address, GET, b"$5\r\nvalue\r\n");
    }
}

#[test]
fn mutual_tls_requires_trusted_client_certificate() {
    qualify_mutual_tls(false, "0");
}

#[test]
fn cross_client_commit_with_mutual_tls_preserves_admission_and_pipeline_order() {
    qualify_mutual_tls(true, "0");
}

#[test]
fn coalescing_with_mutual_tls_preserves_admission_and_pipeline_order() {
    qualify_mutual_tls(true, "2000");
}

fn qualify_mutual_tls(cross_client: bool, wait: &str) {
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    use rustls::pki_types::{PrivatePkcs8KeyDer, ServerName};
    use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
    use std::sync::Arc;
    let temp = Temp(std::env::temp_dir().join(format!(
            "nulang-fabric-tls-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir(&temp.0).unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let leaf = |usage| {
        let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.extended_key_usages.push(usage);
        let key = KeyPair::generate().unwrap();
        (params.signed_by(&key, &issuer).unwrap(), key)
    };
    let (server_cert, server_key) = leaf(ExtendedKeyUsagePurpose::ServerAuth);
    let (client_cert, client_key) = leaf(ExtendedKeyUsagePurpose::ClientAuth);
    let ca_path = temp.0.join("ca.pem");
    let cert_path = temp.0.join("server.pem");
    let key_path = temp.0.join("server.key");
    std::fs::write(&ca_path, ca.pem()).unwrap();
    std::fs::write(&cert_path, server_cert.pem()).unwrap();
    std::fs::write(&key_path, server_key.serialize_pem()).unwrap();
    let addr = port();
    let journal = temp.0.join("tls.wal");
    let mut args = vec![
        "--tls-ca",
        ca_path.to_str().unwrap(),
        "--tls-cert",
        cert_path.to_str().unwrap(),
        "--tls-key",
        key_path.to_str().unwrap(),
        "--max-input-bytes",
        "262144",
        "--max-output-bytes",
        "524288",
        "--max-connections",
        "8",
    ];
    if cross_client {
        args.extend([
            "--mode",
            "sync",
            "--journal",
            journal.to_str().unwrap(),
            "--cross-client-group-commit",
            "1",
            "--cross-group-wait-us",
            wait,
        ]);
    }
    let _server = Server::start(addr, &args);
    let mut roots = RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let connect = |config: ClientConfig| {
        let socket = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        StreamOwned::new(
            ClientConnection::new(Arc::new(config), ServerName::try_from("localhost").unwrap())
                .unwrap(),
            socket,
        )
    };
    let mut denied = connect(
        ClientConfig::builder()
            .with_root_certificates(roots.clone())
            .with_no_client_auth(),
    );
    let attempt = denied
        .write_all(SET)
        .and_then(|_| denied.flush())
        .and_then(|_| denied.read_exact(&mut [0; 5]));
    assert!(
        attempt.is_err(),
        "unauthenticated client received a response"
    );
    // Plaintext must never fall through to RESP dispatch on a TLS listener.
    let mut plain = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
    plain
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    plain.write_all(SET).unwrap();
    let mut byte = [0];
    match plain.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("unexpected plaintext response: {other:?}"),
    }
    let unknown = rcgen::generate_simple_self_signed(vec!["unknown".to_string()]).unwrap();
    let mut untrusted = connect(
        ClientConfig::builder()
            .with_root_certificates(roots.clone())
            .with_client_auth_cert(
                vec![unknown.cert.der().clone()],
                PrivatePkcs8KeyDer::from(unknown.signing_key.serialize_der()).into(),
            )
            .unwrap(),
    );
    assert!(untrusted
        .write_all(SET)
        .and_then(|_| untrusted.read_exact(&mut [0; 5]))
        .is_err());
    let trusted = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(
            vec![client_cert.der().clone()],
            PrivatePkcs8KeyDer::from(client_key.serialize_der()).into(),
        )
        .unwrap();
    let mut allowed = connect(trusted.clone());
    allowed.write_all(GET).unwrap();
    let mut result = [0; 5];
    allowed.read_exact(&mut result).unwrap();
    assert_eq!(&result, b"$-1\r\n");
    allowed.write_all(SET).unwrap();
    allowed.read_exact(&mut result).unwrap();
    assert_eq!(&result, b"+OK\r\n");
    let value = vec![b'x'; 128 * 1024];
    let mut set = format!("*3\r\n$3\r\nSET\r\n$1\r\nk\r\n${}\r\n", value.len()).into_bytes();
    set.extend_from_slice(&value);
    set.extend_from_slice(b"\r\n");
    // Fragment the request over multiple TLS records, then pipeline GETs.
    for chunk in set.chunks(3000) {
        allowed.write_all(chunk).unwrap();
    }
    allowed.write_all(&[GET, GET].concat()).unwrap();
    let bulk = format!("${}\r\n", value.len()).into_bytes();
    let mut expected = b"+OK\r\n".to_vec();
    for _ in 0..2 {
        expected.extend_from_slice(&bulk);
        expected.extend_from_slice(&value);
        expected.extend_from_slice(b"\r\n");
    }
    let mut actual = vec![0; expected.len()];
    allowed.read_exact(&mut actual).unwrap();
    assert_eq!(actual, expected);

    // A single MGET would amplify this 128 KiB value into >512 KiB.
    let mut amplified = connect(trusted.clone());
    amplified
        .write_all(b"*6\r\n$4\r\nMGET\r\n$1\r\nk\r\n$1\r\nk\r\n$1\r\nk\r\n$1\r\nk\r\n$1\r\nk\r\n")
        .unwrap();
    match amplified.read(&mut [0; 1]) {
        Ok(0) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ) => {}
        other => panic!("amplified response was not rejected: {other:?}"),
    }

    // A >256 KiB SET must not mutate the store, even with a valid certificate.
    let mut oversized = connect(trusted.clone());
    let mut large_set = b"*3\r\n$3\r\nSET\r\n$8\r\nrejected\r\n$307200\r\n".to_vec();
    large_set.extend(std::iter::repeat_n(b'x', 307200));
    large_set.extend_from_slice(b"\r\n");
    let result = oversized
        .write_all(&large_set)
        .and_then(|_| oversized.read(&mut [0; 1]));
    match result {
        Ok(0) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            ) => {}
        other => panic!("oversized request was not rejected: {other:?}"),
    }
    let mut healthy = connect(trusted);
    healthy
        .write_all(b"*2\r\n$3\r\nGET\r\n$8\r\nrejected\r\n")
        .unwrap();
    let mut miss = [0; 5];
    healthy.read_exact(&mut miss).unwrap();
    assert_eq!(&miss, b"$-1\r\n");
    // Existing authenticated client still works after both failures.
    allowed.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut pong = [0; 7];
    allowed.read_exact(&mut pong).unwrap();
    assert_eq!(&pong, b"+PONG\r\n");
    // A single encrypted pipeline spans multiple command quanta.
    allowed
        .write_all(&b"*1\r\n$4\r\nPING\r\n".repeat(96))
        .unwrap();
    let mut pongs = vec![0; 96 * 7];
    allowed.read_exact(&mut pongs).unwrap();
    assert_eq!(pongs, b"+PONG\r\n".repeat(96));
}

#[cfg(target_os = "linux")]
#[test]
fn descriptor_exhaustion_preserves_existing_client_and_recovers_listener() {
    use std::os::unix::process::CommandExt;
    let addr = port();
    let mut server = Server::start_with(addr, &[], |command| {
        // SAFETY: only async-signal-safe setrlimit is invoked in the child.
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 32,
                    rlim_max: 32,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    });
    let mut established = TcpStream::connect(addr).unwrap();
    established
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let ping = b"*1\r\n$4\r\nPING\r\n";
    let mut pong = [0; 7];
    established.write_all(ping).unwrap();
    established.read_exact(&mut pong).unwrap();
    assert_eq!(&pong, b"+PONG\r\n");
    let mut idle = Vec::new();
    for _ in 0..40 {
        if let Ok(socket) = TcpStream::connect_timeout(&addr, Duration::from_millis(100)) {
            idle.push(socket);
        }
    }
    let fd_path = format!("/proc/{}/fd", server.0.id());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server exited under descriptor pressure"
        );
        if std::fs::read_dir(&fd_path)
            .map(|fds| fds.count())
            .unwrap_or(0)
            >= 32
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "did not reach descriptor limit"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(150));
    established.write_all(ping).unwrap();
    established.read_exact(&mut pong).unwrap();
    assert_eq!(&pong, b"+PONG\r\n");
    drop(idle);
    request(addr, ping, b"+PONG\r\n");
    assert!(server.0.try_wait().unwrap().is_none());
}

fn metrics_request(address: SocketAddr, path: &str) -> String {
    let mut connection = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(connection, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut response = String::new();
    connection.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn operational_metrics_report_activity_and_health_over_loopback() {
    let address = port();
    let metrics = port();
    assert_ne!(address, metrics);
    let _server = Server::start(
        address,
        &[
            "--metrics-bind",
            &metrics.to_string(),
            "--max-connections-per-peer",
            "1",
        ],
    );
    let mut established = TcpStream::connect(address).unwrap();
    established
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    established.write_all(SET).unwrap();
    let mut ok = [0; 5];
    established.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"+OK\r\n");
    established.write_all(GET).unwrap();
    let mut value = [0; 11];
    established.read_exact(&mut value).unwrap();
    assert_eq!(&value, b"$5\r\nvalue\r\n");
    let mut denied = TcpStream::connect(address).unwrap();
    denied
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    match denied.read(&mut [0; 1]) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("peer admission not enforced: {other:?}"),
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let response = metrics_request(metrics, "/metrics");
        assert!(response.starts_with("HTTP/1.1 200"));
        if response.contains("fabric_commands_dispatched_total 2\n")
            && response.contains("fabric_connections_rejected_peer_total 1\n")
        {
            assert!(response.contains("fabric_connections_active 1\n"));
            assert!(response.contains("fabric_cache_entries 1\n"));
            assert!(response.contains("fabric_connection_budget_used_bytes 0\n"));
            assert!(!response.contains("value"));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{response}");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(metrics_request(metrics, "/healthz").starts_with("HTTP/1.1 200"));
    assert!(metrics_request(metrics, "/readyz").starts_with("HTTP/1.1 200"));
    assert!(metrics_request(metrics, "/unknown").starts_with("HTTP/1.1 404"));
    // An incomplete probe must not stall the data reactor or retain the
    // single metrics worker indefinitely.
    let mut slow = TcpStream::connect(metrics).unwrap();
    slow.write_all(b"GET /metrics HTTP/1.1\r\n").unwrap();
    established.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut pong = [0; 7];
    established.read_exact(&mut pong).unwrap();
    assert_eq!(&pong, b"+PONG\r\n");
    assert!(metrics_request(metrics, "/healthz").starts_with("HTTP/1.1 200"));
}

#[test]
fn durable_metrics_include_journal_and_automatic_checkpoint_progress() {
    let temp = Temp(std::env::temp_dir().join(format!(
            "nulang-fabric-metrics-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir(&temp.0).unwrap();
    let path = temp.0.join("cache.wal");
    let address = port();
    let metrics = port();
    assert_ne!(address, metrics);
    let _server = Server::start(
        address,
        &[
            "--mode",
            "buffered",
            "--journal",
            path.to_str().unwrap(),
            "--max-journal-bytes",
            "1024",
            "--max-record-bytes",
            "256",
            "--metrics-bind",
            &metrics.to_string(),
        ],
    );
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client.write_all(&SET.repeat(100)).unwrap();
    let mut replies = vec![0; 500];
    client.read_exact(&mut replies).unwrap();
    assert_eq!(replies, b"+OK\r\n".repeat(100));
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let body = metrics_request(metrics, "/metrics");
        let metric = |name: &str| -> u64 {
            body.lines()
                .find_map(|line| line.strip_prefix(name).and_then(|v| v.trim().parse().ok()))
                .unwrap()
        };
        if metric("fabric_commands_dispatched_total ") == 100 {
            assert_eq!(metric("fabric_durable "), 1);
            assert_eq!(metric("fabric_journal_limit_bytes "), 1024);
            assert!(metric("fabric_journal_bytes ") <= 1024);
            assert!(metric("fabric_checkpoint_completed_total ") > 0);
            assert!(metric("fabric_checkpoint_reclaimed_bytes_total ") > 0);
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{body}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

// All acknowledged grouped writes must survive an abrupt process stop.
#[test]
fn grouped_sync_pipeline_recovers_after_process_kill() {
    let temp = Temp(std::env::temp_dir().join(format!(
            "nulang-fabric-group-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir(&temp.0).unwrap();
    let journal = temp.0.join("cache.wal");
    let address = port();
    let args = [
        "--mode",
        "sync",
        "--journal",
        journal.to_str().unwrap(),
        "--group-commit",
        "1",
    ];
    let server = Server::start(address, &args);
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let pipeline = b"*2\r\n$4\r\nINCR\r\n$1\r\nk\r\n".repeat(64);
    client.write_all(&pipeline).unwrap();
    let expected: String = (1..=64).map(|n| format!(":{n}\r\n")).collect();
    let mut reply = vec![0; expected.len()];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(reply, expected.as_bytes());
    drop(server);
    let _recovered = Server::start(address, &args);
    request(address, GET, b"$2\r\n64\r\n");
}

#[test]
fn cross_client_acknowledged_mutations_recover_after_process_kill() {
    qualify_cross_client_recovery("0");
}

#[test]
fn coalescing_acknowledged_mutations_recover_after_process_kill() {
    qualify_cross_client_recovery("2000");
}

fn qualify_cross_client_recovery(wait: &str) {
    let temp = Temp(std::env::temp_dir().join(format!(
            "nulang-fabric-cross-process-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    std::fs::create_dir(&temp.0).unwrap();
    let journal = temp.0.join("cache.wal");
    let address = port();
    let args = [
        "--mode",
        "sync",
        "--journal",
        journal.to_str().unwrap(),
        "--cross-client-group-commit",
        "1",
        "--cross-group-wait-us",
        wait,
    ];
    let server = Server::start(address, &args);
    let mut clients: Vec<_> = (0..8)
        .map(|_| {
            let connection = TcpStream::connect(address).unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            BufReader::new(connection)
        })
        .collect();
    let pipeline = b"*2\r\n$4\r\nINCR\r\n$1\r\nk\r\n".repeat(8);
    for client in &mut clients {
        client.get_mut().write_all(&pipeline).unwrap();
    }
    let mut seen = Vec::new();
    for client in &mut clients {
        for _ in 0..8 {
            let mut line = String::new();
            client.read_line(&mut line).unwrap();
            seen.push(
                line.strip_prefix(':')
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap(),
            );
        }
    }
    seen.sort_unstable();
    assert_eq!(seen, (1..=64).collect::<Vec<_>>());
    drop(server);
    let _recovered = Server::start(address, &args);
    request(address, GET, b"$2\r\n64\r\n");
}

// A real kernel file-size limit forces a short write followed by EFBIG.
// Only the child is restricted; no filesystem filling or host limits.
#[cfg(target_os = "linux")]
#[test]
fn test_real_wal_file_size_failure_never_acknowledges_uncertain_mutation() {
    use std::os::unix::process::CommandExt;
    let temp = Temp(std::env::temp_dir().join(format!(
        "nulang-fabric-fsize-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    )));
    std::fs::create_dir(&temp.0).unwrap();
    for (index, extra) in [
        &[][..],
        &["--group-commit", "1"][..],
        &["--cross-client-group-commit", "1"][..],
        &[
            "--cross-client-group-commit",
            "1",
            "--cross-group-wait-us",
            "2000",
        ][..],
    ]
    .into_iter()
    .enumerate()
    {
        let journal = temp.0.join(format!("{index}.wal"));
        let address = port();
        let mut args = vec![
            "--mode",
            "sync",
            "--journal",
            journal.to_str().unwrap(),
            "--checkpoint-percent",
            "0",
        ];
        args.extend_from_slice(extra);
        let mut server = Server::start_with(address, &args, |command| {
            // SAFETY: only async-signal-safe calls in the post-fork child.
            unsafe {
                command.pre_exec(|| {
                    if libc::signal(libc::SIGXFSZ, libc::SIG_IGN) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
                    let limit = libc::rlimit {
                        rlim_cur: 256,
                        rlim_max: 256,
                    };
                    if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        });
        request(address, SET, b"+OK\r\n");
        let stable_len = std::fs::metadata(&journal).unwrap().len();
        assert!(stable_len < 256);
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut wire = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$512\r\n".to_vec();
        wire.extend(std::iter::repeat_n(b'x', 512));
        wire.extend_from_slice(b"\r\n");
        wire.extend_from_slice(GET);
        client.write_all(&wire).unwrap();
        match client.read(&mut [0; 1]) {
            Ok(0) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("uncertain WAL write produced response: {other:?}"),
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = server.0.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "poisoned process did not stop"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            std::fs::metadata(&journal).unwrap().len(),
            256,
            "kernel limit was not reached"
        );
        drop(server);
        let _recovered = Server::start(address, &args);
        assert_eq!(std::fs::metadata(&journal).unwrap().len(), stable_len);
        request(address, GET, b"$5\r\nvalue\r\n");
    }
}
