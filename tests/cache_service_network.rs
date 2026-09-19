#![cfg(feature = "cache-server")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nulang::runtime::{
    cache_transport_bridge, redis_slot, CacheAdvertisedEndpoint, CacheServiceBuilder,
    CacheServiceHandle, CacheServiceShardConfig, CacheShardOwner, CacheSlotMap,
    CacheTransportInbound, CacheTransportMessage, DeterministicNetworkTransport, IncomingPacket,
    NodeId, OutgoingPacket, Runtime,
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

fn frame(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", parts.len()).into_bytes();
    for part in parts {
        out.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
        out.extend_from_slice(part);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn wait_event(
    a: &mut Runtime,
    b: &mut Runtime,
    handle: &CacheServiceHandle,
) -> CacheTransportInbound {
    for _ in 0..500 {
        a.process_network();
        b.process_network();
        if let Some(event) = handle.try_recv_network_event().unwrap() {
            return event;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("timed out waiting for cache network event");
}

fn wait_epoch(handle: &CacheServiceHandle, epoch: u64) {
    for _ in 0..500 {
        if handle
            .shard_placement_epochs()
            .iter()
            .all(|installed| *installed >= epoch)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!(
        "cache service did not install epoch {epoch}: {:?}",
        handle.shard_placement_epochs()
    );
}

#[test]
fn remote_cache_command_executes_on_owning_reactor_and_stale_epoch_fails_closed() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33301".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33302".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut runtime_a = distributed_runtime(addr_a, bus.clone());
    let mut runtime_b = distributed_runtime(addr_b, bus);
    runtime_a
        .distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_b, addr_b);
    runtime_b
        .distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_a, addr_a);

    let key = b"remote-cache-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let mut placement = CacheSlotMap::new_local(node_a.0, 1).unwrap();
    placement
        .apply_epoch(
            1,
            &[nulang::runtime::CacheSlotRange {
                start: slot,
                end: slot,
                owner: target,
            }],
        )
        .unwrap();
    assert_eq!(placement.owner_for_slot(slot), Some(target));

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, placement.clone())
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 7100))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, placement.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 7200))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let set = CacheTransportMessage::CommandRequest {
        request_id: 1,
        placement_epoch: 1,
        slot,
        target,
        frame: frame(&[b"SET", key, b"value"]),
    };
    service_a.send_network_message(node_b, set).unwrap();
    let response = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    match response.message {
        CacheTransportMessage::CommandResponse {
            request_id,
            response,
            ..
        } => {
            assert_eq!(request_id, 1);
            assert_eq!(response, b"+OK\r\n");
        }
        other => panic!("unexpected cache response: {other:?}"),
    }

    let get = CacheTransportMessage::CommandRequest {
        request_id: 2,
        placement_epoch: 1,
        slot,
        target,
        frame: frame(&[b"GET", key]),
    };
    service_a.send_network_message(node_b, get).unwrap();
    let response = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    match response.message {
        CacheTransportMessage::CommandResponse {
            request_id,
            response,
            ..
        } => {
            assert_eq!(request_id, 2);
            assert_eq!(response, b"$5\r\nvalue\r\n");
        }
        other => panic!("unexpected cache response: {other:?}"),
    }

    let mut newer = placement.clone();
    newer.apply_epoch(2, &[]).unwrap();
    service_b.install_placement(newer).unwrap();
    wait_epoch(&service_b, 2);

    let stale_set = CacheTransportMessage::CommandRequest {
        request_id: 3,
        placement_epoch: 1,
        slot,
        target,
        frame: frame(&[b"SET", key, b"stale"]),
    };
    service_a
        .send_network_message(node_b, stale_set)
        .unwrap();
    let response = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    match response.message {
        CacheTransportMessage::CommandResponse {
            request_id,
            response,
            ..
        } => {
            assert_eq!(request_id, 3);
            assert_eq!(response, b"-TRYAGAIN cache topology changed\r\n");
        }
        other => panic!("unexpected cache response: {other:?}"),
    }

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
}
