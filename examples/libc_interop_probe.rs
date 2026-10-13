//! Test-only cross-libc NUL0 wire and mutual-TLS probe.
//! Compiled twice (GNU and musl); an external harness runs each as a separate process.
use nulang::runtime::{NodeId, Packet, TcpTransport, TlsConfig};
use std::error::Error;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

const HEARTBEAT_TS: u64 = 1_700_000_000;

fn usage() -> &'static str {
    "usage: libc_interop_probe server <bind> <plain|tls> [ca.pem cert.pem key.pem]\n\
     or: libc_interop_probe client <server> <peer_id> <plain|tls> [ca.pem cert.pem key.pem]"
}

fn main() {
    if let Err(err) = run() {
        eprintln!("interop probe failed: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let role = args.first().map(String::as_str).ok_or(usage())?;
    if role != "server" && role != "client" {
        return Err(usage().into());
    }
    let offset = if role == "server" { 2 } else { 3 };
    let mode = args.get(offset).map(String::as_str).ok_or(usage())?;
    let tls = match (mode, args.len() - offset) {
        ("plain", 1) => TlsConfig::PlaintextInsecure,
        ("tls", 4) => TlsConfig::MutualTls {
            ca_cert_pem: std::fs::read(&args[offset + 1])?,
            server_cert_pem: std::fs::read(&args[offset + 2])?,
            server_key_pem: std::fs::read(&args[offset + 3])?,
            server_name: Some("localhost".to_string()),
        },
        _ => return Err(usage().into()),
    };
    if role == "server" {
        let addr: SocketAddr = args[1].parse()?;
        let mut server = TcpTransport::bind(addr, tls)?;
        println!("READY {} {}", server.listen_addr(), server.node_id().0);
        io::stdout().flush()?;
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            for packet in server.receive() {
                if let Packet::Heartbeat { node_id, timestamp } = packet.packet {
                    if timestamp == HEARTBEAT_TS && node_id == packet.from_node {
                        println!("PASS heartbeat from {}", packet.from_node.0);
                        server.shutdown();
                        return Ok(());
                    }
                    return Err("unexpected peer identity or heartbeat payload".into());
                }
                return Err("unexpected NUL0 packet type".into());
            }
            if Instant::now() >= deadline {
                server.shutdown();
                return Err("timeout waiting for verified NUL0 heartbeat".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    let target_addr: SocketAddr = args[1].parse()?;
    let peer_id = NodeId(args[2].parse()?);
    let mut client = TcpTransport::bind("127.0.0.1:0".parse()?, tls)?;
    client.connect(peer_id, target_addr)?;
    client.send(
        peer_id,
        target_addr,
        Packet::Heartbeat {
            node_id: client.node_id(),
            timestamp: HEARTBEAT_TS,
        },
    );
    // The NUL0 sender is asynchronous. Give it time to flush; the server's
    // separately asserted PASS is authoritative proof of delivery.
    thread::sleep(Duration::from_millis(350));
    client.shutdown();
    println!("SENT heartbeat");
    Ok(())
}
