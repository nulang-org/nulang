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
    assert_eq!(message.priority, nulang::runtime::MessagePriority::System);
    assert_eq!(message.payload[0].as_int(), Some(8));
    assert_eq!(
        message.payload[1].as_int(),
        Some(0),
        "connect failure must be distinguishable from an ambiguous write failure"
    );
}

#[test]
fn runtime_marks_transport_write_failure_as_ambiguous() {
    let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41_002);
    let local_node = NodeId::new(&local_addr);
    let remote_node = NodeId(0xCAFE);

    let mut runtime = Runtime::new();
    let sender = runtime.spawn_actor(Box::new(Vec::new));
    let mut cluster = ClusterState::new(local_node, local_addr);
    let mut resolver = AddressResolver::new(local_node);
    let mut transport = FailureTransport {
        node_id: local_node,
        addr: local_addr,
        failures: vec![TransportSendFailure {
            to_node: remote_node,
            packet_seq: 23,
            sender_actor: Some(sender),
            reason: TransportSendFailureReason::Write,
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
    assert_eq!(message.priority, nulang::runtime::MessagePriority::System);
    assert_eq!(message.payload[0].as_int(), Some(8));
    assert_eq!(
        message.payload[1].as_int(),
        Some(1),
        "write failure must remain distinguishable as an ambiguous delivery outcome"
    );
}

#[test]
fn runtime_delivery_failure_bypasses_full_bounded_mailbox() {
    let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41_003);
    let local_node = NodeId::new(&local_addr);
    let remote_node = NodeId(0xD00D);

    let mut runtime = Runtime::new();
    let sender = runtime.spawn_actor(Box::new(Vec::new));
    let actor = runtime.actors.get_mut(&sender).unwrap();
    actor.mailbox = nulang::runtime::Mailbox::new(1);
    actor
        .mailbox
        .push_local(nulang::runtime::Message {
            behavior_id: 0,
            payload: nulang::runtime::MessagePayload::from_slice(&[nulang::vm::Value::int(99)]),
            sender: 0,
            priority: nulang::runtime::MessagePriority::Normal,
            trace_id: None,
        })
        .expect("fill the bounded mailbox with ordinary traffic");

    let mut cluster = ClusterState::new(local_node, local_addr);
    let mut resolver = AddressResolver::new(local_node);
    let mut transport = FailureTransport {
        node_id: local_node,
        addr: local_addr,
        failures: vec![TransportSendFailure {
            to_node: remote_node,
            packet_seq: 27,
            sender_actor: Some(sender),
            reason: TransportSendFailureReason::Write,
        }],
    };
    process_network_packets(&mut runtime, &mut transport, &mut cluster, &mut resolver);

    let mailbox = &mut runtime.actors.get_mut(&sender).unwrap().mailbox;
    let failure = mailbox
        .pop()
        .expect("system failure must bypass normal capacity");
    assert_eq!(failure.priority, nulang::runtime::MessagePriority::System);
    assert_eq!(failure.payload[0].as_int(), Some(8));
    assert_eq!(failure.payload[1].as_int(), Some(1));
    let original = mailbox
        .pop()
        .expect("existing normal mail must be retained");
    assert_eq!(original.payload[0].as_int(), Some(99));
}

/// Synchronous routing errors must use the same capacity-exempt feedback path
/// as failures arriving later from a transport worker.
#[test]
fn runtime_sync_resolve_failure_bypasses_full_bounded_mailbox() {
    let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 41_004);
    let local_node = NodeId::new(&local_addr);
    let unknown_node = NodeId(0xD0E0);

    let mut runtime = Runtime::new();
    let sender = runtime.spawn_actor(Box::new(Vec::new));
    let actor = runtime.actors.get_mut(&sender).unwrap();
    actor.mailbox = nulang::runtime::Mailbox::new(1);
    actor
        .mailbox
        .push_local(nulang::runtime::Message {
            behavior_id: 0,
            payload: nulang::runtime::MessagePayload::from_slice(&[nulang::vm::Value::int(99)]),
            sender: 0,
            priority: nulang::runtime::MessagePriority::Normal,
            trace_id: None,
        })
        .expect("fill the sender's bounded mailbox");

    let cluster = ClusterState::new(local_node, local_addr);
    let mut resolver = AddressResolver::new(local_node);
    let mut transport = FailureTransport {
        node_id: local_node,
        addr: local_addr,
        failures: vec![],
    };
    runtime.current_actor = Some(sender);
    nulang::runtime::send_distributed(
        &mut runtime,
        &mut transport,
        &cluster,
        &mut resolver,
        nulang::runtime::ActorAddress::remote(unknown_node, 123),
        "handle",
        &[],
    );
    runtime.current_actor = None;

    let mailbox = &mut runtime.actors.get_mut(&sender).unwrap().mailbox;
    let feedback = mailbox
        .pop()
        .expect("synchronous route error must be visible");
    assert_eq!(feedback.behavior_id, 0);
    assert_eq!(feedback.priority, nulang::runtime::MessagePriority::System);
    assert_eq!(feedback.payload[0].as_int(), Some(5)); // unknown route
    assert_eq!(feedback.payload[1], nulang::vm::Value::nil());
    let original = mailbox
        .pop()
        .expect("original normal message must remain queued");
    assert_eq!(original.payload[0].as_int(), Some(99));
}

#[cfg(feature = "tcp")]
fn wait_for_transport_state(mut predicate: impl FnMut() -> bool, message: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !predicate() {
        assert!(Instant::now() < deadline, "{message}");
        std::thread::yield_now();
    }
}

#[cfg(feature = "tcp")]
fn actor_packet(sender_node: NodeId, sender_actor: u64, value: i64) -> Packet {
    Packet::ActorMessage {
        target_actor: 9,
        behavior_name: "handle".to_string(),
        content_hash: None,
        required_protocol_id: None,
        payload: vec![Value::int(value)],
        string_table: vec![],
        object_table: vec![],
        sender_actor,
        sender_node,
        priority: MessagePriority::Normal,
        trace_id: None,
    }
}

#[cfg(feature = "tcp")]
#[test]
fn tcp_send_reconnects_after_observed_disconnect_without_application_retry_loop() {
    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let mut sender =
        TcpTransport::bind(bind_addr, TlsConfig::PlaintextInsecure).expect("bind sender transport");
    let mut receiver = TcpTransport::bind(bind_addr, TlsConfig::PlaintextInsecure)
        .expect("bind receiver transport");

    let receiver_addr = receiver.listen_addr();
    let receiver_node = receiver.node_id();
    sender
        .connect(receiver_node, receiver_addr)
        .expect("establish initial transport connection");
    assert_eq!(
        sender.connection_addr(receiver_node),
        Some(receiver_addr),
        "fixture must begin with a registered live connection"
    );

    receiver.shutdown();
    wait_for_transport_state(
        || sender.connection_addr(receiver_node).is_none(),
        "sender must observe reader EOF and retire the disconnected peer",
    );

    let mut replacement = TcpTransport::bind(receiver_addr, TlsConfig::PlaintextInsecure)
        .expect("rebind replacement transport at the same advertised address");
    assert_eq!(
        replacement.node_id(),
        receiver_node,
        "plaintext node identity must remain stable when the advertised address is reused"
    );

    let sender_actor = 42;
    let outcome = sender.send_tracked(
        receiver_node,
        receiver_addr,
        actor_packet(sender.node_id(), sender_actor, 7),
    );
    assert!(
        matches!(outcome, TrackedSendOutcome::Sent(_)),
        "the first application send after observed disconnect must be admitted for reconnect"
    );

    let mut delivered = None;
    wait_for_transport_state(
        || {
            delivered = replacement
                .receive()
                .into_iter()
                .find(|packet| matches!(packet.packet, Packet::ActorMessage { .. }));
            delivered.is_some()
        },
        "the first application send after rebind must cross the reconnect boundary",
    );

    let delivered = delivered.expect("delivery predicate established a packet");
    assert!(matches!(
        delivered.packet,
        Packet::ActorMessage {
            sender_actor: 42,
            ..
        }
    ));
    assert!(
        sender.drain_send_failures().is_empty(),
        "successful reconnect delivery must not emit a synthetic transport failure"
    );

    sender.shutdown();
    replacement.shutdown();
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
