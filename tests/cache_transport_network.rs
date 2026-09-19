use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use nulang::runtime::{
    cache_transport_bridge, CacheShardOwner, CacheTransportMessage, CacheTransportOutbound,
    DeterministicNetworkTransport, IncomingPacket, NodeId, OutgoingPacket, Runtime,
};

type Bus = Arc<
    parking_lot::Mutex<
        HashMap<
            NodeId,
            (
                std::sync::mpsc::SyncSender<IncomingPacket>,
                std::sync::mpsc::SyncSender<OutgoingPacket>,
            ),
        >,
    >,
>;

fn distributed_runtime(addr: SocketAddr, bus: Bus) -> Runtime {
    let mut runtime = Runtime::new();
    runtime.install_virtual_clock();
    let transport =
        DeterministicNetworkTransport::bind_with_bus(addr, bus).expect("transport should bind");
    transport.register_on_bus();
    runtime
        .enable_distribution_with_transport(Box::new(transport))
        .expect("distribution should enable");
    runtime
}

#[test]
fn cache_bridge_round_trips_over_existing_nul0_actor_message_transport() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33101".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33102".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut a = distributed_runtime(addr_a, bus.clone());
    let mut b = distributed_runtime(addr_b, bus);

    a.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_b, addr_b);
    b.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_a, addr_a);

    let (runtime_a, service_a) = cache_transport_bridge(8).unwrap();
    let (runtime_b, service_b) = cache_transport_bridge(8).unwrap();
    a.attach_cache_transport(runtime_a).unwrap();
    b.attach_cache_transport(runtime_b).unwrap();

    let request = CacheTransportMessage::CommandRequest {
        request_id: 77,
        placement_epoch: 4,
        slot: 123,
        target: CacheShardOwner {
            node_id: node_b.0,
            shard: 2,
        },
        frame: b"*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n".to_vec(),
    };
    service_a
        .try_send(CacheTransportOutbound {
            to_node: node_b,
            message: request.clone(),
        })
        .unwrap();

    // A's network pump drains service egress to the existing transport.
    a.process_network();
    // B authenticates the transport peer, intercepts the reserved actor-0
    // cache envelope, and forwards it to the bounded cache-service bridge.
    b.process_network();

    let inbound = service_b.try_recv().unwrap().expect("request should arrive");
    assert_eq!(inbound.from_node, node_a);
    assert_eq!(inbound.message, request);

    let response = CacheTransportMessage::CommandResponse {
        request_id: 77,
        placement_epoch: 4,
        slot: 123,
        responder: CacheShardOwner {
            node_id: node_b.0,
            shard: 2,
        },
        response: b"$5\r\nvalue\r\n".to_vec(),
    };
    service_b
        .try_send(CacheTransportOutbound {
            to_node: node_a,
            message: response.clone(),
        })
        .unwrap();

    b.process_network();
    a.process_network();

    let inbound = service_a
        .try_recv()
        .unwrap()
        .expect("response should arrive");
    assert_eq!(inbound.from_node, node_b);
    assert_eq!(inbound.message, response);
}

#[test]
fn outbound_cache_bridge_rejects_claimed_sender_that_is_not_local_node() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33201".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33202".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut a = distributed_runtime(addr_a, bus.clone());
    let mut b = distributed_runtime(addr_b, bus);
    a.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_b, addr_b);
    b.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_a, addr_a);

    let (runtime_a, service_a) = cache_transport_bridge(8).unwrap();
    let (runtime_b, service_b) = cache_transport_bridge(8).unwrap();
    a.attach_cache_transport(runtime_a).unwrap();
    b.attach_cache_transport(runtime_b).unwrap();

    service_a
        .try_send(CacheTransportOutbound {
            to_node: node_b,
            message: CacheTransportMessage::CommandResponse {
                request_id: 1,
                placement_epoch: 0,
                slot: 1,
                responder: CacheShardOwner {
                    node_id: 999,
                    shard: 0,
                },
                response: b"+OK\r\n".to_vec(),
            },
        })
        .unwrap();

    a.process_network();
    b.process_network();

    assert!(service_b.try_recv().unwrap().is_none());
}
