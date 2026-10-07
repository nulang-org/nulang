use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(feature = "tcp")]
use std::time::{Duration, Instant};

use nulang::runtime::{
    process_network_packets, AddressResolver, ClusterState, IncomingPacket, NetworkTransport,
    NodeId, Packet, Runtime, TransportSendFailure, TransportSendFailureReason,
};
#[cfg(feature = "tcp")]
use nulang::runtime::{MessagePriority, TcpTransport, TlsConfig, TrackedSendOutcome};
#[cfg(feature = "tcp")]
use nulang::vm::Value;

struct FailureTransport {
    node_id: NodeId,
    addr: SocketAddr,
    failures: Vec<TransportSendFailure>,
}

impl NetworkTransport for FailureTransport {
    fn connect(&mut self, _node_id: NodeId, _addr: SocketAddr) -> std::io::Result<()> {
        Ok(())
    }

    fn send(&mut self, _to_node: NodeId, _to_addr: SocketAddr, _packet: Packet) {}

    fn receive(&self) -> Vec<IncomingPacket> {
        Vec::new()
    }

    fn drain_send_failures(&mut self) -> Vec<TransportSendFailure> {
        std::mem::take(&mut self.failures)
    }

    fn node_id(&self) -> NodeId {
        self.node_id
    }

    fn listen_addr(&self) -> SocketAddr {
        self.addr
    }

    fn disconnect(&mut self, _node_id: NodeId) {}

    fn shutdown(&mut self) {}

    fn connection_count(&self) -> usize {
        0
    }

    fn connection_addr(&self, _node_id: NodeId) -> Option<SocketAddr> {
        None
    }
}

#[test]
fn runtime_surfaces_transport_connect_failure_to_sender_actor() {
    let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41_001);
    let local_node = NodeId::new(&local_addr);
    let remote_node = NodeId(0xBEEF);

    let mut runtime = Runtime::new();
    let sender = runtime.spawn_actor(Box::new(Vec::new));
    let mut cluster = ClusterState::new(local_node, local_addr);
    let mut resolver = AddressResolver::new(local_node);
    let mut transport = FailureTransport {
        node_id: local_node,
        addr: local_addr,
        failures: vec![TransportSendFailure {
            to_node: remote_node,
            packet_seq: 17,
            sender_actor: Some(sender),
            reason: TransportSendFailureReason::Connect,
        }],
    };

    process_network_packets(&mut runtime, &mut transport, &mut cluster, &mut resolver);

    let message = runtime
        .actors
        .get_mut(&sender)
        .expect("sender actor must remain live")
        .mailbox
        .pop()
        .expect("transport failure must become a sender-visible system message");

    assert_eq!(message.behavior_id, 0);
    assert_eq!(message.payload[0].as_int(), Some(8));
}

#[cfg(feature = "tcp")]
#[test]
fn tcp_connect_failure_preserves_sequence_and_sender_identity() {
    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let mut transport = TcpTransport::bind(bind_addr, TlsConfig::PlaintextInsecure)
        .expect("bind local TCP transport");
    let remote_node = NodeId(0xCAFE);
    let unreachable = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let sender_actor = 42;

    let packet = Packet::ActorMessage {
        target_actor: 9,
        behavior_name: "handle".to_string(),
        content_hash: None,
        required_protocol_id: None,
        payload: vec![Value::int(1)],
        string_table: vec![],
        object_table: vec![],
        sender_actor,
        sender_node: transport.node_id(),
        priority: MessagePriority::Normal,
        trace_id: None,
    };

    let sequence = match transport.send_tracked(remote_node, unreachable, packet) {
        TrackedSendOutcome::Sent(sequence) => sequence,
        other => panic!("tracked TCP send must reserve a sequence, got {other:?}"),
    };

    let deadline = Instant::now() + Duration::from_secs(2);
    let failure = loop {
        if let Some(failure) = transport.drain_send_failures().into_iter().next() {
            break failure;
        }
        assert!(
            Instant::now() < deadline,
            "TCP connect failure must be surfaced by the transport"
        );
        std::thread::sleep(Duration::from_millis(10));
    };

    assert_eq!(failure.to_node, remote_node);
    assert_eq!(failure.packet_seq, sequence);
    assert_eq!(failure.sender_actor, Some(sender_actor));
    assert_eq!(failure.reason, TransportSendFailureReason::Connect);

    transport.shutdown();
}
