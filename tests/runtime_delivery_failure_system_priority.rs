use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;

use nulang::runtime::{
    process_network_packets, AddressResolver, ClusterState, DeliveryFailure, DeliveryFailureKind,
    IncomingPacket, Mailbox, MessagePriority, NetworkTransport, NodeId, Packet, Runtime,
};
use nulang::vm::Value;

struct FailureTransport {
    failures: Mutex<Vec<DeliveryFailure>>,
    node_id: NodeId,
    addr: SocketAddr,
}

impl NetworkTransport for FailureTransport {
    fn connect(&mut self, _node_id: NodeId, _addr: SocketAddr) -> std::io::Result<()> {
        Ok(())
    }

    fn send(&mut self, _to_node: NodeId, _to_addr: SocketAddr, _packet: Packet) {}

    fn receive(&self) -> Vec<IncomingPacket> {
        Vec::new()
    }

    fn take_delivery_failures(&self) -> Vec<DeliveryFailure> {
        std::mem::take(&mut *self.failures.lock().expect("failure queue lock"))
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

fn addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

#[test]
fn transport_failure_notification_is_system_priority_and_bypasses_full_mailbox() {
    let local_addr = addr(41_000);
    let local_node = NodeId::new(&local_addr);
    let remote_addr = addr(41_001);
    let remote_node = NodeId::new(&remote_addr);

    let mut runtime = Runtime::new();
    let sender = runtime.spawn_actor(Box::new(Vec::new));
    {
        let actor = runtime.actors.get_mut(&sender).expect("sender actor");
        actor.mailbox = Mailbox::new(1);
        actor.register_behavior("busy", |_actor, _args| {});
    }

    runtime.send_message_by_id(sender, 0, &[]);
    assert_eq!(runtime.actors[&sender].mailbox.len(), 1);

    let failed_packet = Packet::ActorMessage {
        target_actor: 999,
        behavior_name: "work".to_string(),
        content_hash: None,
        required_protocol_id: None,
        payload: Vec::new(),
        string_table: Vec::new(),
        object_table: Vec::new(),
        sender_actor: sender,
        sender_node: local_node,
        priority: MessagePriority::Normal,
        trace_id: None,
    };
    let failure = DeliveryFailure {
        to_node: remote_node,
        to_addr: remote_addr,
        packet: failed_packet,
        kind: DeliveryFailureKind::Connect,
        error: "connection refused".to_string(),
    };
    let mut transport = FailureTransport {
        failures: Mutex::new(vec![failure]),
        node_id: local_node,
        addr: local_addr,
    };
    let mut cluster = ClusterState::new(local_node, local_addr);
    let mut resolver = AddressResolver::new(local_node);

    process_network_packets(&mut runtime, &mut transport, &mut cluster, &mut resolver);

    let actor = runtime.actors.get_mut(&sender).expect("sender actor");
    assert_eq!(
        actor.mailbox.len(),
        2,
        "delivery-failure system notification must bypass normal mailbox capacity"
    );

    let failure_message = actor
        .mailbox
        .pop()
        .expect("system delivery-failure notification");
    assert_eq!(failure_message.priority, MessagePriority::System);
    assert_eq!(failure_message.behavior_id, 0);
    assert_eq!(failure_message.payload[0].as_int(), Some(8));
    assert_eq!(failure_message.payload[1], Value::nil());

    let original_message = actor.mailbox.pop().expect("original normal message");
    assert_eq!(original_message.priority, MessagePriority::Normal);
    assert!(actor.mailbox.is_empty());
}
