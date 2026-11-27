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
