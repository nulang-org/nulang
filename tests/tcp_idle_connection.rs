#![cfg(feature = "tcp")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread;
use std::time::{Duration, Instant};

use nulang::runtime::{NodeId, Runtime, TlsConfig};

fn distributed_runtime() -> Runtime {
    let mut runtime = Runtime::new();
    runtime
        .enable_distribution(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            TlsConfig::PlaintextInsecure,
        )
        .expect("loopback distribution should bind");
    runtime
}

fn endpoint(runtime: &Runtime) -> (NodeId, SocketAddr) {
    let node = runtime
        .distributed
        .node_id
        .expect("distribution should assign a node id");
    let addr = runtime
        .distributed
        .transport
        .as_ref()
        .expect("distribution should own a transport")
        .listen_addr();
    (node, addr)
}

fn connection_count(runtime: &Runtime) -> usize {
    runtime
        .distributed
        .transport
        .as_ref()
        .expect("distribution should own a transport")
        .connection_count()
}

#[test]
fn established_idle_tcp_link_survives_reader_poll_timeout() {
    let mut left = distributed_runtime();
    let right = distributed_runtime();
    let (right_node, right_addr) = endpoint(&right);

    left.distributed
        .transport
        .as_mut()
        .expect("left transport")
        .connect(right_node, right_addr)
        .expect("left should connect to right");

    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if connection_count(&left) > 0 && connection_count(&right) > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }

    assert!(
        connection_count(&left) > 0 && connection_count(&right) > 0,
        "both transports should register the established loopback link"
    );

    // TcpTransport readers poll with a 50 ms socket read timeout. An idle
    // connection must remain cached across several such poll intervals; an
    // ordinary lack of traffic is not a disconnect signal.
    thread::sleep(Duration::from_millis(250));

    assert!(
        connection_count(&left) > 0 && connection_count(&right) > 0,
        "an established idle TCP link must survive the reader poll timeout"
    );
}
