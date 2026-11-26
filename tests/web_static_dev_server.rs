//! Exercise the actual no-route `nula dev` entry point, not only WebDevServer.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct DevProcess(Child);
impl Drop for DevProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(port: u16, target: &str, extra_headers: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to static dev");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\n{extra_headers}Connection: close\r\n\r\n").as_bytes()).unwrap();
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read complete HTTP response");
    response
}

#[test]
fn test_static_only_dev_uses_hardened_http_boundary() {
    let temp = TempDir(std::env::temp_dir().join(format!(
        "nulang-static-only-{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    )));
    let root = &temp.0;
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("public")).unwrap();
    std::fs::write(root.join("Nulang.toml"), "[package]\nname = \"static-only\"\nversion = \"0.1.0\"\n[web]\nstatic_dir = \"public\"\noutput_dir = \"dist\"\n").unwrap();
    std::fs::write(root.join("src/main.nula"), "fn main() { 0 }\n").unwrap();
    std::fs::write(root.join("public/index.html"), "<h1>public fixture</h1>").unwrap();
    std::fs::write(root.join("public/data.json"), "{\"public\":true}").unwrap();
    std::fs::write(root.join("outside.txt"), "private-test-marker").unwrap();

    let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let mut dev = DevProcess(
        Command::new(env!("CARGO_BIN_EXE_nulang"))
            .current_dir(root)
            .env("CARGO_BIN_EXE_nulang", env!("CARGO_BIN_EXE_nulang"))
            .env(
                "NULANG_STDLIB",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
            )
            .env_remove("NULANG_WEB_PUBLIC_ORIGIN")
            .args(["nula", "dev", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start static-only dev CLI"),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(
            dev.0.try_wait().unwrap().is_none(),
            "dev exited before listening"
        );
        assert!(Instant::now() < deadline, "dev did not start");
        std::thread::sleep(Duration::from_millis(25));
    }

    let home = request(port, "/", "");
    assert!(home.starts_with("HTTP/1.1 200 "), "{home}");
    assert!(home.ends_with("<h1>public fixture</h1>"), "{home}");

    let traversal = request(port, "/../outside.txt", "");
    assert!(
        !traversal.contains("private-test-marker"),
        "file outside generated output exposed"
    );
    assert!(
        traversal.starts_with("HTTP/1.1 404 ") || traversal.starts_with("HTTP/1.1 400 "),
        "{traversal}"
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("outside.txt"), root.join("dist/linked.txt")).unwrap();
        let linked = request(port, "/linked.txt", "");
        assert!(linked.starts_with("HTTP/1.1 404 "), "{linked}");
        assert!(!linked.contains("private-test-marker"));
    }

    let json = request(port, "/data.json", "");
    assert!(json.starts_with("HTTP/1.1 200 "), "{json}");
    assert!(
        json.contains("Content-Type: application/json; charset=utf-8"),
        "{json}"
    );
    assert!(json.contains("X-Content-Type-Options: nosniff"), "{json}");
    assert!(json.ends_with("{\"public\":true}"), "{json}");

    let duplicate_host = request(port, "/", "Host: localhost\r\n");
    assert!(
        duplicate_host.starts_with("HTTP/1.1 400 "),
        "{duplicate_host}"
    );

    let missing = request(port, "/__nulang/sse/missing", "");
    assert!(
        missing.starts_with("HTTP/1.1 404 "),
        "static-only mode must not expose SSE: {missing}"
    );
}
