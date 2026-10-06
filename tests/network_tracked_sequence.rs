use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use nulang::runtime::{
    DeterministicNetworkTransport, IncomingPacket, NetworkTransport, NodeId, OutgoingPacket, Packet,
};

fn bus() -> Arc<
    parking_lot::Mutex<
        HashMap<
            NodeId,
            (
                std::sync::mpsc::SyncSender<IncomingPacket>,
                std::sync::mpsc::SyncSender<OutgoingPacket>,
            ),
        >,
    >,
> {
    Arc::new(parking_lot::Mutex::new(HashMap::new()))
}

#[test]
fn deterministic_transport_returns_the_wire_sequence_it_delivers() {
    let bus = bus();
    let addr_a: SocketAddr = "127.0.0.1:34101".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34102".parse().unwrap();
    let node_b = NodeId::new(&addr_b);

    let mut a = DeterministicNetworkTransport::bind_with_bus(addr_a, bus.clone()).unwrap();
    let b = DeterministicNetworkTransport::bind_with_bus(addr_b, bus).unwrap();
    a.register_on_bus();
    b.register_on_bus();

    let first = a.send(
        node_b,
        addr_b,
        Packet::Heartbeat {
            node_id: a.node_id(),
            timestamp: 1,
        },
    );
    let second = a.send(
        node_b,
        addr_b,
        Packet::Heartbeat {
            node_id: a.node_id(),
            timestamp: 2,
        },
    );

    assert_eq!(first, 1);
    assert_eq!(second, 2);

    let received = b.receive();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].seq, first);
    assert_eq!(received[1].seq, second);
}
