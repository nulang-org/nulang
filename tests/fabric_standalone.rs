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
        let child = Command::new(env!("CARGO_BIN_EXE_nulang-fabric"))
            .args(["--bind", &address.to_string()])
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
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
    let _server = Server::start(
        addr,
        &[
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
        ],
    );
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
