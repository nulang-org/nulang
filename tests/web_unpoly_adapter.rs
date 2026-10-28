//! Check Unpoly compatibility using the shipped compiler and a live loopback Web server.
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn unpoly_example_typechecks_without_a_custom_browser_runtime() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--check")
        .arg(root.join("examples/unpoly-compat/src/main.nula"))
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
    let dir = std::env::temp_dir().join(format!("nulang-unpoly-test-{}-{now}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).expect("make example fixture");
    std::fs::write(
        dir.join("Nulang.toml"),
        include_str!("../examples/unpoly-compat/Nulang.toml"),
    )
    .expect("write manifest");
    std::fs::write(
        dir.join("src/main.nula"),
        include_str!("../examples/unpoly-compat/src/main.nula"),
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
        "GET /search HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{headers}\r\n"
    )
    .expect("send request");
    stream.shutdown(Shutdown::Write).expect("finish request");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read reply");
    String::from_utf8(bytes).expect("valid UTF-8")
}

#[test]
fn unpoly_only_optimizes_exact_known_fragment_target() {
    let (_server, port) = start_example();
    let full = request(port, "");
    assert!(
        full.contains("<html>"),
        "ordinary navigation must render document: {full}"
    );
    let exact = request(port, "X-Up-Version: 3.14\r\nX-Up-Target: #results\r\n");
    assert!(
        !exact.contains("<html>"),
        "exact target should render fragment: {exact}"
    );
    assert!(
        exact.contains("<main id=\"results\">"),
        "missing requested fragment: {exact}"
    );

    let combined = request(
        port,
        "X-Up-Version: 3.14\r\nX-Up-Target: #results, #notifications\r\n",
    );
    assert!(
        combined.contains("<html>"),
        "merged target requires full fallback: {combined}"
    );
    assert!(
        combined.contains("id=\"results\"") && combined.contains("id=\"notifications\""),
        "merged targets must both be present: {combined}"
    );
    let other = request(port, "X-Up-Version: 3.14\r\nX-Up-Target: body\r\n");
    assert!(
        other.contains("<html>"),
        "unknown target requires full fallback: {other}"
    );
    let missing = request(port, "X-Up-Version: 3.14\r\n");
    assert!(
        missing.contains("<html>"),
        "missing target requires full fallback: {missing}"
    );
    for response in [full, exact, combined, other, missing] {
        assert!(
            response.contains("Vary: X-Up-Version, X-Up-Target"),
            "every response must vary by fragment negotiation: {response}"
        );
    }
}
