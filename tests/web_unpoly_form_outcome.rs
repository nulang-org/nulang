//! Check Unpoly typed form outcome compatibility using the shipped compiler and a live loopback Web server.
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn unpoly_form_outcome_example_typechecks() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--check")
        .arg(root.join("examples/unpoly-form-outcome/src/main.nula"))
        .env("NULANG_STDLIB", root.join("src/stdlib"))
        .output()
        .expect("run Nulang compiler");
    assert!(
        output.status.success(),
        "Unpoly example must typecheck\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct AppServer {
    child: Child,
    dir: std::path::PathBuf,
}
impl Drop for AppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start_example() -> (AppServer, u16) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-unpoly-form-test-{}-{now}",
        std::process::id()
    ));
    std::fs::create_dir_all(dir.join("src")).expect("make example fixture");
    std::fs::write(
        dir.join("Nulang.toml"),
        include_str!("../examples/unpoly-form-outcome/Nulang.toml"),
    )
    .expect("write manifest");
    std::fs::write(
        dir.join("src/main.nula"),
        include_str!("../examples/unpoly-form-outcome/src/main.nula"),
    )
    .expect("write app");

    let probe = TcpListener::bind(("127.0.0.1", 0)).expect("reserve loopback port");
    let port = probe.local_addr().expect("local port").port();
    drop(probe);

    let child = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["nula", "dev", "--port", &port.to_string()])
        .current_dir(&dir)
        .env("NULANG_STDLIB", root.join("src/stdlib"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn Nulang development server");
    let mut server = AppServer { child, dir };
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (server, port);
        }
        if let Some(status) = server.child.try_wait().expect("poll dev server") {
            panic!("dev server exited before readiness: {status}");
        }
        assert!(Instant::now() < deadline, "dev server startup timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn request(port: u16, headers: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    write!(
        stream,
        "GET /state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{headers}\r\n"
    )
    .expect("send request");
    stream.shutdown(Shutdown::Write).expect("finish request");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read reply");
    String::from_utf8(bytes).expect("valid UTF-8")
}

fn post(port: u16, body: &str, headers: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    write!(
        stream,
        "POST /signup HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
        body.len()
    ).expect("write POST");
    stream.shutdown(Shutdown::Write).expect("close request");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read reply");
    String::from_utf8(bytes).expect("UTF-8 response")
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").expect("HTTP separator").1
}

#[test]
fn unpoly_form_validation_and_submission_preserve_status_targets_and_no_write() {
    let (_server, port) = start_example();
    assert!(body(&request(port, "")).contains(">not-saved<"));

    let validation = post(
        port,
        "email=test%40example.com",
        "X-Up-Version: 3.14\r\nX-Up-Validate: email\r\nX-Up-Target: #signup\r\n",
    );
    assert!(
        validation.starts_with("HTTP/1.1 200 OK\r\n"),
        "{validation}"
    );
    assert!(body(&validation).contains("id=\"signup\""), "{validation}");
    assert!(!body(&validation).contains("<html>"), "{validation}");
    assert!(
        validation.contains("Vary: X-Up-Version, X-Up-Target"),
        "{validation}"
    );
    assert!(
        body(&request(port, "")).contains(">not-saved<"),
        "validation must not commit"
    );

    let batched_validation = post(
        port,
        "email=test%40example.com",
        "X-Up-Version: 3.14\r\nX-Up-Validate: email other\r\nX-Up-Target: #signup, #welcome\r\n",
    );
    assert!(
        batched_validation.starts_with("HTTP/1.1 200 OK\r\n"),
        "{batched_validation}"
    );
    assert!(
        body(&batched_validation).contains("<html>"),
        "{batched_validation}"
    );
    assert!(
        body(&batched_validation).contains("id=\"signup\""),
        "{batched_validation}"
    );
    assert!(
        body(&batched_validation).contains("id=\"welcome\""),
        "{batched_validation}"
    );
    assert!(
        body(&request(port, "")).contains(">not-saved<"),
        "batched validation must not commit"
    );

    let invalid = post(
        port,
        "email=",
        "X-Up-Version: 3.14\r\nX-Up-Target: #welcome\r\nX-Up-Fail-Target: #signup\r\n",
    );
    assert!(
        invalid.starts_with("HTTP/1.1 422 Unprocessable Entity\r\n"),
        "{invalid}"
    );
    assert!(body(&invalid).contains("id=\"signup\""), "{invalid}");
    assert!(body(&invalid).contains("Email is required"), "{invalid}");
    assert!(!body(&invalid).contains("<html>"), "{invalid}");
    assert!(
        invalid.contains("Vary: X-Up-Version, X-Up-Target"),
        "{invalid}"
    );

    let merged_failure = post(
        port,
        "email=",
        "X-Up-Version: 3.14\r\nX-Up-Fail-Target: #signup, #welcome\r\n",
    );
    assert!(
        merged_failure.starts_with("HTTP/1.1 422 Unprocessable Entity\r\n"),
        "{merged_failure}"
    );
    assert!(body(&merged_failure).contains("<html>"), "{merged_failure}");
    assert!(
        body(&merged_failure).contains("id=\"welcome\""),
        "{merged_failure}"
    );

    let success = post(
        port,
        "email=test%40example.com",
        "X-Up-Version: 3.14\r\nX-Up-Target: #welcome\r\nX-Up-Fail-Target: #signup\r\n",
    );
    assert!(success.starts_with("HTTP/1.1 200 OK\r\n"), "{success}");
    assert!(body(&success).contains("id=\"welcome\""), "{success}");
    assert!(!body(&success).contains("<html>"), "{success}");
    assert!(
        success.contains("Vary: X-Up-Version, X-Up-Target"),
        "{success}"
    );
    assert!(
        body(&request(port, "")).contains("saved"),
        "valid submission should commit"
    );
}
