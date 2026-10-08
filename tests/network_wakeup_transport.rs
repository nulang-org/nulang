#![cfg(feature = "tcp")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use nulang::runtime::{NetworkTransport, Packet, TcpTransport, TlsConfig};

#[test]
fn tcp_readers_wake_the_runtime_only_after_packet_admission_in_both_directions() {
    let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let mut left = TcpTransport::bind(local, TlsConfig::PlaintextInsecure).unwrap();
    let mut right = TcpTransport::bind(local, TlsConfig::PlaintextInsecure).unwrap();

    // Establish the duplex connection before observing the arrival generations.
    left.connect(right.node_id(), right.listen_addr()).unwrap();

    let right_gen = NetworkTransport::incoming_generation(&right)
        .expect("TCP transports expose a notification generation");
    let first = Packet::Heartbeat {
        node_id: left.node_id(),
        timestamp: 42,
    };
    left.send(right.node_id(), right.listen_addr(), first.clone());
    assert!(
        NetworkTransport::wait_for_incoming(&right, right_gen, Duration::from_secs(2)),
        "inbound TCP reader must notify after queueing a packet"
    );
    assert!(
        right.receive().iter().any(|entry| entry.packet == first),
        "a notification must make the actual admitted packet available"
    );

    let left_gen = NetworkTransport::incoming_generation(&left)
        .expect("dialed TCP transport exposes its generation");
    let second = Packet::Heartbeat {
        node_id: right.node_id(),
        timestamp: 43,
    };
    right.send(left.node_id(), left.listen_addr(), second.clone());
    assert!(
        NetworkTransport::wait_for_incoming(&left, left_gen, Duration::from_secs(2)),
        "dialed outbound TCP reader must also publish notifications"
    );
    assert!(
        left.receive().iter().any(|entry| entry.packet == second),
        "duplex delivery must be visible after wakeup"
    );

    left.shutdown();
    right.shutdown();
}

#[test]
fn idle_tcp_transport_wait_times_out_without_fake_arrivals() {
    let local = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let mut transport = TcpTransport::bind(local, TlsConfig::PlaintextInsecure).unwrap();
    let observed = NetworkTransport::incoming_generation(&transport).unwrap();

    assert!(!NetworkTransport::wait_for_incoming(
        &transport,
        observed,
        Duration::from_millis(3),
    ));
    assert_eq!(
        NetworkTransport::incoming_generation(&transport),
        Some(observed)
    );
    transport.shutdown();
}
