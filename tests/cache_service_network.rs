#![cfg(feature = "cache-server")]

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream as StdTcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nulang::runtime::{
    cache_transport_bridge, redis_slot, CacheAdvertisedEndpoint, CacheMigrationJournal,
    CacheMigrationKey, CacheServiceBuilder, CacheServiceHandle, CacheServiceShardConfig,
    CacheShardOwner, CacheSlotMap, CacheTransportInbound, CacheTransportMessage,
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

fn temp_journal_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "nulang-cache-{name}-{}-{nonce}.journal",
        std::process::id()
    ))
}

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

fn read_resp_line(client: &mut StdTcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        client.read_exact(&mut byte).unwrap();
        response.push(byte[0]);
        if response.ends_with(b"\r\n") {
            return response;
        }
    }
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
    service_a.send_network_message(node_b, stale_set).unwrap();
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

#[test]
fn remote_slot_migration_moves_data_then_commits_ownership() {
    let journal_path = temp_journal_path("full-migration");
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33401".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33402".parse().unwrap();
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

    let key = b"remote-migrate-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();
    assert_eq!(base.owner_for_slot(slot), Some(source));

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53402))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, base.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53401))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    // Seed the source before entering migration.
    let mut source_client = StdTcpStream::connect(service_a.local_addrs()[0]).unwrap();
    source_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    source_client
        .write_all(&frame(&[b"SET", key, b"value"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating.clone()).unwrap();
    wait_epoch(&service_a, 1);
    wait_epoch(&service_b, 1);

    let pending = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 9001)
        .unwrap();
    assert_eq!(pending.batch.entries.len(), 1);

    let ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    let report = service_a
        .complete_remote_slot_batch(&pending, &ack)
        .unwrap();
    assert_eq!(report.imported, 1);
    assert_eq!(report.finalized_removed, 1);
    assert_eq!(report.conflicts, 0);
    assert_eq!(report.stale_source_versions, 0);
    assert!(report.source_drained());

    service_a
        .send_remote_migration_probe(0, target, slot, 9002)
        .unwrap();
    let probe = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    let convergence = service_a.complete_remote_migration_probe(&probe).unwrap();
    assert_eq!(convergence.probe_id, 9002);
    assert_eq!(convergence.source_remaining, 0);
    assert_eq!(convergence.target_live_entries, 1);
    assert_eq!(convergence.target_import_fences, 1);
    assert_eq!(convergence.target_conflicts, 0);
    assert_eq!(convergence.target_wrong_slot, 0);
    assert!(convergence.durable_history_satisfied);
    assert!(convergence.ready_for_live_commit());

    let journal_key = CacheMigrationKey {
        started_epoch: 1,
        slot,
        source,
        target,
    };
    let recovered = service_a
        .recovered_remote_migration(journal_key)
        .expect("durable migration proof");
    assert_eq!(
        recovered.source_incarnation,
        service_a.migration_incarnation()
    );
    assert_eq!(recovered.source_remaining, Some(0));
    assert!(recovered.all_sent_transfers_acked());
    assert_eq!(recovered.expected_import_fences(), 1);
    assert_eq!(
        recovered
            .convergence
            .as_ref()
            .map(|evidence| evidence.probe_id),
        Some(9002)
    );

    // The source is drained but remains stable owner until commit, so it asks.
    source_client.write_all(&frame(&[b"GET", key])).unwrap();
    let ask = String::from_utf8(read_resp_line(&mut source_client)).unwrap();
    assert!(ask.starts_with(&format!("-ASK {slot} ")));

    // The target has the imported value but serves it only after ASKING.
    let mut target_client = StdTcpStream::connect(service_b.local_addrs()[0]).unwrap();
    target_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    target_client.write_all(&frame(&[b"ASKING"])).unwrap();
    assert_eq!(read_resp_line(&mut target_client), b"+OK\r\n");
    target_client.write_all(&frame(&[b"GET", key])).unwrap();
    let mut imported = [0u8; 11];
    target_client.read_exact(&mut imported).unwrap();
    assert_eq!(&imported, b"$5\r\nvalue\r\n");

    migrating.commit_migration(2, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating).unwrap();
    wait_epoch(&service_a, 2);
    wait_epoch(&service_b, 2);
    service_b.clear_local_transfer_imports(0, slot).unwrap();

    target_client.write_all(&frame(&[b"GET", key])).unwrap();
    let mut stable = [0u8; 11];
    target_client.read_exact(&mut stable).unwrap();
    assert_eq!(&stable, b"$5\r\nvalue\r\n");

    source_client.write_all(&frame(&[b"GET", key])).unwrap();
    let moved = String::from_utf8(read_resp_line(&mut source_client)).unwrap();
    assert!(moved.starts_with(&format!("-MOVED {slot} ")));

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();

    let journal = CacheMigrationJournal::open(&journal_path).unwrap();
    let recovered = journal
        .recovery_state(journal_key)
        .expect("journal should survive service shutdown");
    assert_eq!(recovered.source_remaining, Some(0));
    assert!(recovered.all_sent_transfers_acked());
    assert_eq!(recovered.pending_commit_epoch, None);
    assert_eq!(recovered.completed_commit_epoch, Some(2));
    std::fs::remove_file(journal_path).unwrap();
}

#[test]
fn remote_migration_commit_without_convergence_proof_is_rejected() {
    let addr_a: SocketAddr = "127.0.0.1:33451".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33452".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);
    let key = b"commit-gate-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();

    let service = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53452))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service.install_placement(migrating.clone()).unwrap();
    wait_epoch(&service, 1);

    migrating.commit_migration(2, slot, source, target).unwrap();
    assert!(matches!(
        service.install_placement(migrating),
        Err(nulang::runtime::CacheServiceError::RemoteMigrationNotConverged(
            rejected_slot
        )) if rejected_slot == slot
    ));
    assert_eq!(service.published_placement_epoch(), 1);

    service.shutdown().unwrap();
}

#[test]
fn drained_persistent_migration_recovers_after_both_cache_services_restart() {
    let journal_path = temp_journal_path("drained-process-restart");
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33471".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33472".parse().unwrap();
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

    let key = b"restart-persistent-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53472))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, base.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53471))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut source_client = StdTcpStream::connect(service_a.local_addrs()[0]).unwrap();
    source_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    source_client
        .write_all(&frame(&[b"SET", key, b"value"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating.clone()).unwrap();
    wait_epoch(&service_a, 1);
    wait_epoch(&service_b, 1);

    let pending = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 9501)
        .unwrap();
    let ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    let report = service_a
        .complete_remote_slot_batch(&pending, &ack)
        .unwrap();
    assert!(report.source_drained());

    let journal_key = CacheMigrationKey {
        started_epoch: 1,
        slot,
        source,
        target,
    };
    let old_incarnation = service_a.migration_incarnation();
    assert!(service_a
        .recovered_remote_migration(journal_key)
        .unwrap()
        .drained_restart_replay_safe());

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
    runtime_a.detach_cache_transport();
    runtime_b.detach_cache_transport();

    // Both CacheStores and target import fences are now gone. Recreate fresh
    // cache services and rebuild the target from the exact durable batches.
    let (runtime_bridge_a2, service_bridge_a2) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b2, service_bridge_b2) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a2).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b2).unwrap();

    let service_a2 = CacheServiceBuilder::new(node_a.0, migrating.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 54472))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a2)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b2 = CacheServiceBuilder::new(node_b.0, migrating.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 54471))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b2)
        .build()
        .unwrap()
        .start()
        .unwrap();

    assert_ne!(service_a2.migration_incarnation(), old_incarnation);
    let replayed = service_a2
        .replay_drained_remote_migration_after_restart(journal_key)
        .unwrap();
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].transfer_id, 9501);

    let replay_ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a2);
    let replay_report = service_a2
        .complete_remote_slot_batch(&replayed[0], &replay_ack)
        .unwrap();
    assert_eq!(replay_report.imported, 1);
    assert_eq!(replay_report.finalized_removed, 0);
    assert_eq!(replay_report.finalized_absent, 1);
    assert!(replay_report.source_drained());

    service_a2
        .reprobe_recovered_remote_migration(journal_key, 9502)
        .unwrap();
    let probe = wait_event(&mut runtime_a, &mut runtime_b, &service_a2);
    let convergence = service_a2.complete_remote_migration_probe(&probe).unwrap();
    assert!(convergence.durable_history_satisfied);
    assert_eq!(convergence.target_live_entries, 1);
    assert_eq!(convergence.target_import_fences, 1);
    assert!(convergence.ready_for_live_commit());

    let mut committed = migrating.clone();
    committed.commit_migration(2, slot, source, target).unwrap();
    service_a2.install_placement(committed.clone()).unwrap();
    service_b2.install_placement(committed).unwrap();
    wait_epoch(&service_a2, 2);
    wait_epoch(&service_b2, 2);

    let mut target_client = StdTcpStream::connect(service_b2.local_addrs()[0]).unwrap();
    target_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    target_client.write_all(&frame(&[b"GET", key])).unwrap();
    let mut value = [0u8; 11];
    target_client.read_exact(&mut value).unwrap();
    assert_eq!(&value, b"$5\r\nvalue\r\n");

    service_a2.shutdown().unwrap();
    service_b2.shutdown().unwrap();
    std::fs::remove_file(journal_path).unwrap();
}

#[test]
fn drained_restart_replay_preserves_multiple_source_generations() {
    let journal_path = temp_journal_path("drained-generation-order");
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33481".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33482".parse().unwrap();
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

    let key = b"restart-generation-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53482))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();
    let service_b = CacheServiceBuilder::new(node_b.0, base.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53481))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut source_client = StdTcpStream::connect(service_a.local_addrs()[0]).unwrap();
    source_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    source_client
        .write_all(&frame(&[b"SET", key, b"old"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating.clone()).unwrap();
    wait_epoch(&service_a, 1);
    wait_epoch(&service_b, 1);

    let first = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 9602)
        .unwrap();
    let first_ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a);

    // Advance the source generation before applying the first ACK. Finalizing
    // the first transfer must therefore fail stale-version and preserve "new".
    source_client
        .write_all(&frame(&[b"SET", key, b"new"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");
    let first_report = service_a
        .complete_remote_slot_batch(&first, &first_ack)
        .unwrap();
    assert_eq!(first_report.stale_source_versions, 1);
    assert_eq!(first_report.source_remaining, 1);

    // Use a lower transfer id deliberately: durable append order, not numeric
    // request-id order, must control restart replay.
    let second = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 9601)
        .unwrap();
    let second_ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    let second_report = service_a
        .complete_remote_slot_batch(&second, &second_ack)
        .unwrap();
    assert!(second_report.source_drained());

    let journal_key = CacheMigrationKey {
        started_epoch: 1,
        slot,
        source,
        target,
    };
    assert_eq!(
        service_a
            .recovered_remote_migration(journal_key)
            .unwrap()
            .transfer_order,
        vec![9602, 9601]
    );

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
    runtime_a.detach_cache_transport();
    runtime_b.detach_cache_transport();

    let (runtime_bridge_a2, service_bridge_a2) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b2, service_bridge_b2) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a2).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b2).unwrap();

    let service_a2 = CacheServiceBuilder::new(node_a.0, migrating.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 54482))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a2)
        .build()
        .unwrap()
        .start()
        .unwrap();
    let service_b2 = CacheServiceBuilder::new(node_b.0, migrating.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 54481))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b2)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let replayed = service_a2
        .replay_drained_remote_migration_after_restart(journal_key)
        .unwrap();
    assert_eq!(
        replayed
            .iter()
            .map(|pending| pending.transfer_id)
            .collect::<Vec<_>>(),
        vec![9602, 9601]
    );
    for pending in &replayed {
        let ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a2);
        service_a2
            .complete_remote_slot_batch(pending, &ack)
            .unwrap();
    }

    service_a2
        .reprobe_recovered_remote_migration(journal_key, 9603)
        .unwrap();
    let probe = wait_event(&mut runtime_a, &mut runtime_b, &service_a2);
    let convergence = service_a2.complete_remote_migration_probe(&probe).unwrap();
    assert!(convergence.ready_for_live_commit());

    let mut committed = migrating.clone();
    committed.commit_migration(2, slot, source, target).unwrap();
    service_a2.install_placement(committed.clone()).unwrap();
    service_b2.install_placement(committed).unwrap();
    wait_epoch(&service_b2, 2);

    let mut target_client = StdTcpStream::connect(service_b2.local_addrs()[0]).unwrap();
    target_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    target_client.write_all(&frame(&[b"GET", key])).unwrap();
    let mut value = [0u8; 9];
    target_client.read_exact(&mut value).unwrap();
    assert_eq!(&value, b"$3\r\nnew\r\n");

    service_a2.shutdown().unwrap();
    service_b2.shutdown().unwrap();
    std::fs::remove_file(journal_path).unwrap();
}

#[test]
fn drained_ttl_migration_restart_replay_never_extends_expiry() {
    let journal_path = temp_journal_path("drained-ttl-restart");
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33491".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33492".parse().unwrap();
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

    let key = b"restart-ttl-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53492))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();
    let service_b = CacheServiceBuilder::new(node_b.0, base.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53491))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut source_client = StdTcpStream::connect(service_a.local_addrs()[0]).unwrap();
    source_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    source_client
        .write_all(&frame(&[b"SET", key, b"value", b"PX", b"200"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating.clone()).unwrap();
    wait_epoch(&service_a, 1);
    wait_epoch(&service_b, 1);

    let pending = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 9701)
        .unwrap();
    let exported_ttl = pending.batch.entries[0]
        .ttl_ms
        .expect("TTL should be present");
    assert!(exported_ttl <= 200 && exported_ttl > 0);

    let ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    let report = service_a
        .complete_remote_slot_batch(&pending, &ack)
        .unwrap();
    assert!(report.source_drained());

    let journal_key = CacheMigrationKey {
        started_epoch: 1,
        slot,
        source,
        target,
    };
    let recovered = service_a.recovered_remote_migration(journal_key).unwrap();
    assert!(recovered.drained_restart_replay_safe());
    assert!(recovered
        .transfers
        .get(&9701)
        .unwrap()
        .wall_anchor_unix_ms
        .is_some());

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
    runtime_a.detach_cache_transport();
    runtime_b.detach_cache_transport();

    // Ensure more real wall time has elapsed than the original exported TTL.
    std::thread::sleep(Duration::from_millis(250));

    let (runtime_bridge_a2, service_bridge_a2) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b2, service_bridge_b2) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a2).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b2).unwrap();

    let service_a2 = CacheServiceBuilder::new(node_a.0, migrating.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 54492))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a2)
        .build()
        .unwrap()
        .start()
        .unwrap();
    let service_b2 = CacheServiceBuilder::new(node_b.0, migrating.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 54491))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b2)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let replayed = service_a2
        .replay_drained_remote_migration_after_restart(journal_key)
        .unwrap();
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].batch.entries[0].ttl_ms, Some(0));

    let replay_ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a2);
    let results = service_a2
        .complete_restart_replay_batch(&replayed[0], &replay_ack)
        .unwrap();
    assert_eq!(
        results,
        vec![nulang::runtime::CacheTransferImport::ExpiredInTransit]
    );

    service_a2
        .reprobe_recovered_remote_migration(journal_key, 9702)
        .unwrap();
    let probe = wait_event(&mut runtime_a, &mut runtime_b, &service_a2);
    let convergence = service_a2.complete_remote_migration_probe(&probe).unwrap();
    assert!(convergence.durable_history_satisfied);
    assert_eq!(convergence.target_live_entries, 0);
    assert_eq!(convergence.target_import_fences, 1);
    assert!(convergence.ready_for_live_commit());

    let mut committed = migrating.clone();
    committed.commit_migration(2, slot, source, target).unwrap();
    service_a2.install_placement(committed.clone()).unwrap();
    service_b2.install_placement(committed).unwrap();
    wait_epoch(&service_a2, 2);
    wait_epoch(&service_b2, 2);

    let mut target_client = StdTcpStream::connect(service_b2.local_addrs()[0]).unwrap();
    target_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    target_client.write_all(&frame(&[b"GET", key])).unwrap();
    assert_eq!(read_resp_line(&mut target_client), b"$-1\r\n");

    service_a2.shutdown().unwrap();
    service_b2.shutdown().unwrap();
    std::fs::remove_file(journal_path).unwrap();
}

#[test]
fn drained_ttl_restart_replay_preserves_only_remaining_lifetime() {
    let journal_path = temp_journal_path("drained-live-ttl-restart");
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33511".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33512".parse().unwrap();
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

    let key = b"restart-live-ttl-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53512))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();
    let service_b = CacheServiceBuilder::new(node_b.0, base.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53511))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut source_client = StdTcpStream::connect(service_a.local_addrs()[0]).unwrap();
    source_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    source_client
        .write_all(&frame(&[b"SET", key, b"value", b"PX", b"10000"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating.clone()).unwrap();
    wait_epoch(&service_a, 1);
    wait_epoch(&service_b, 1);

    let pending = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 9711)
        .unwrap();
    let exported_ttl = pending.batch.entries[0].ttl_ms.unwrap();
    let ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    assert!(service_a
        .complete_remote_slot_batch(&pending, &ack)
        .unwrap()
        .source_drained());

    let journal_key = CacheMigrationKey {
        started_epoch: 1,
        slot,
        source,
        target,
    };

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
    runtime_a.detach_cache_transport();
    runtime_b.detach_cache_transport();
    std::thread::sleep(Duration::from_millis(120));

    let (runtime_bridge_a2, service_bridge_a2) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b2, service_bridge_b2) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a2).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b2).unwrap();

    let service_a2 = CacheServiceBuilder::new(node_a.0, migrating.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 54512))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a2)
        .build()
        .unwrap()
        .start()
        .unwrap();
    let service_b2 = CacheServiceBuilder::new(node_b.0, migrating.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 54511))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b2)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let replayed = service_a2
        .replay_drained_remote_migration_after_restart(journal_key)
        .unwrap();
    let replay_ttl = replayed[0].batch.entries[0].ttl_ms.unwrap();
    assert!(replay_ttl > 0);
    assert!(
        replay_ttl < exported_ttl,
        "restart replay must decrease TTL: exported={exported_ttl}, replay={replay_ttl}"
    );

    let replay_ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a2);
    assert_eq!(
        service_a2
            .complete_restart_replay_batch(&replayed[0], &replay_ack)
            .unwrap(),
        vec![nulang::runtime::CacheTransferImport::Imported]
    );

    service_a2
        .reprobe_recovered_remote_migration(journal_key, 9712)
        .unwrap();
    let probe = wait_event(&mut runtime_a, &mut runtime_b, &service_a2);
    assert!(service_a2
        .complete_remote_migration_probe(&probe)
        .unwrap()
        .ready_for_live_commit());

    let mut committed = migrating.clone();
    committed.commit_migration(2, slot, source, target).unwrap();
    service_a2.install_placement(committed.clone()).unwrap();
    service_b2.install_placement(committed).unwrap();

    let mut target_client = StdTcpStream::connect(service_b2.local_addrs()[0]).unwrap();
    target_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    target_client.write_all(&frame(&[b"GET", key])).unwrap();
    let mut value = [0u8; 11];
    target_client.read_exact(&mut value).unwrap();
    assert_eq!(&value, b"$5\r\nvalue\r\n");

    service_a2.shutdown().unwrap();
    service_b2.shutdown().unwrap();
    std::fs::remove_file(journal_path).unwrap();
}

#[test]
fn stale_remote_transfer_epoch_never_finalizes_source_key() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33501".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33502".parse().unwrap();
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

    let key = b"stale-transfer-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53502))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, base.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53501))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut source_client = StdTcpStream::connect(service_a.local_addrs()[0]).unwrap();
    source_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    source_client
        .write_all(&frame(&[b"SET", key, b"value"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating.clone()).unwrap();
    wait_epoch(&service_a, 1);
    wait_epoch(&service_b, 1);

    // Export and enqueue while both sides agree on epoch 1, but do not pump
    // Runtime A yet, so the batch has not entered NUL0.
    let pending = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 9101)
        .unwrap();
    assert_eq!(pending.placement_epoch, 1);
    assert_eq!(pending.batch.entries.len(), 1);

    // B advances first. Its coordinator must reject the epoch-1 batch before
    // the owning reactor can mutate storage.
    let mut target_newer = migrating.clone();
    target_newer.apply_epoch(2, &[]).unwrap();
    service_b.install_placement(target_newer).unwrap();
    wait_epoch(&service_b, 2);

    let ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    let report = service_a
        .complete_remote_slot_batch(&pending, &ack)
        .unwrap();
    assert_eq!(report.conflicts, 1);
    assert_eq!(report.finalized_removed, 0);
    assert_eq!(report.source_remaining, 1);
    assert!(!report.source_drained());
    assert!(report.restart_scan_required());

    // Source still owns the only authoritative value.
    source_client.write_all(&frame(&[b"GET", key])).unwrap();
    let mut value = [0u8; 11];
    source_client.read_exact(&mut value).unwrap();
    assert_eq!(&value, b"$5\r\nvalue\r\n");

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
}

#[test]
fn duplicate_remote_command_replays_cached_response_without_reexecution() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33601".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33602".parse().unwrap();
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

    let key = b"retry-counter";
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

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, placement.clone())
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53602))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, placement)
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53601))
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
        frame: frame(&[b"SET", key, b"0"]),
    };
    service_a.send_network_message(node_b, set).unwrap();
    let _ = wait_event(&mut runtime_a, &mut runtime_b, &service_a);

    let increment = CacheTransportMessage::CommandRequest {
        request_id: 77,
        placement_epoch: 1,
        slot,
        target,
        frame: frame(&[b"INCR", key]),
    };
    service_a
        .send_network_message(node_b, increment.clone())
        .unwrap();
    let first = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    match first.message {
        CacheTransportMessage::CommandResponse { response, .. } => {
            assert_eq!(response, b":1\r\n");
        }
        other => panic!("unexpected cache response: {other:?}"),
    }

    service_a.send_network_message(node_b, increment).unwrap();
    let duplicate = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    match duplicate.message {
        CacheTransportMessage::CommandResponse { response, .. } => {
            assert_eq!(response, b":1\r\n");
        }
        other => panic!("unexpected cache response: {other:?}"),
    }

    let get = CacheTransportMessage::CommandRequest {
        request_id: 78,
        placement_epoch: 1,
        slot,
        target,
        frame: frame(&[b"GET", key]),
    };
    service_a.send_network_message(node_b, get).unwrap();
    let current = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    match current.message {
        CacheTransportMessage::CommandResponse { response, .. } => {
            assert_eq!(response, b"$1\r\n1\r\n");
        }
        other => panic!("unexpected cache response: {other:?}"),
    }

    let reused_id = CacheTransportMessage::CommandRequest {
        request_id: 77,
        placement_epoch: 1,
        slot,
        target,
        frame: frame(&[b"GET", key]),
    };
    service_a.send_network_message(node_b, reused_id).unwrap();
    let rejected = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    match rejected.message {
        CacheTransportMessage::CommandResponse { response, .. } => {
            assert_eq!(
                response,
                b"-ERR cache request id reused with different payload\r\n"
            );
        }
        other => panic!("unexpected cache response: {other:?}"),
    }

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
}

#[test]
fn duplicate_remote_transfer_replays_original_ack_without_reimport() {
    let journal_path = temp_journal_path("recovered-transfer");
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33701".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33702".parse().unwrap();
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

    let key = b"retry-transfer-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_migration_journal_path(&journal_path)
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53702))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, base.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53701))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut source_client = StdTcpStream::connect(service_a.local_addrs()[0]).unwrap();
    source_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    source_client
        .write_all(&frame(&[b"SET", key, b"value"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating).unwrap();
    wait_epoch(&service_a, 1);
    wait_epoch(&service_b, 1);

    let pending = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 9301)
        .unwrap();
    let journal_key = CacheMigrationKey {
        started_epoch: 1,
        slot,
        source,
        target,
    };
    let recovered_pending = service_a
        .retry_recovered_remote_transfer(journal_key, 9301)
        .unwrap();
    assert_eq!(recovered_pending, pending);

    let first = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    let second = wait_event(&mut runtime_a, &mut runtime_b, &service_a);

    let first_results = match &first.message {
        CacheTransportMessage::TransferAck {
            transfer_id,
            results,
            ..
        } => {
            assert_eq!(*transfer_id, 9301);
            results.clone()
        }
        other => panic!("unexpected first transfer event: {other:?}"),
    };
    let second_results = match &second.message {
        CacheTransportMessage::TransferAck {
            transfer_id,
            results,
            ..
        } => {
            assert_eq!(*transfer_id, 9301);
            results.clone()
        }
        other => panic!("unexpected second transfer event: {other:?}"),
    };

    assert_eq!(first_results, second_results);
    assert_eq!(
        first_results,
        vec![nulang::runtime::CacheTransferImport::Imported]
    );

    let report = service_a
        .complete_remote_slot_batch(&pending, &first)
        .unwrap();
    assert_eq!(report.imported, 1);
    assert_eq!(report.finalized_removed, 1);
    assert!(report.source_drained());

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
    std::fs::remove_file(journal_path).unwrap();
}

#[test]
fn automatic_remote_command_retry_recovers_after_partition_heals() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33801".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33802".parse().unwrap();
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

    let key = b"partition-retry-counter";
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

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, placement.clone())
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53802))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, placement)
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53801))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    runtime_a
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([node_b]));

    let increment = CacheTransportMessage::CommandRequest {
        request_id: 8801,
        placement_epoch: 1,
        slot,
        target,
        frame: frame(&[b"INCR", key]),
    };
    service_a.send_network_message(node_b, increment).unwrap();

    let partition_until = std::time::Instant::now() + Duration::from_millis(60);
    while std::time::Instant::now() < partition_until {
        runtime_a.process_network();
        runtime_b.process_network();
        assert!(service_a.try_recv_network_event().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(1));
    }

    runtime_a
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());

    let response = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    match response.message {
        CacheTransportMessage::CommandResponse { response, .. } => {
            assert_eq!(response, b":1\r\n");
        }
        other => panic!("unexpected cache response: {other:?}"),
    }
    assert!(service_a.try_recv_network_timeout().unwrap().is_none());

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
}

#[test]
fn exhausted_remote_command_retry_reports_unknown_execution_timeout() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:33901".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:33902".parse().unwrap();
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

    let key = b"timeout-counter";
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

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, placement.clone())
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 53902))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, placement)
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 53901))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    runtime_a
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([node_b]));

    service_a
        .send_network_message(
            node_b,
            CacheTransportMessage::CommandRequest {
                request_id: 9901,
                placement_epoch: 1,
                slot,
                target,
                frame: frame(&[b"INCR", key]),
            },
        )
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let timeout = loop {
        runtime_a.process_network();
        runtime_b.process_network();
        if let Some(timeout) = service_a.try_recv_network_timeout().unwrap() {
            break timeout;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for retry exhaustion"
        );
        std::thread::sleep(Duration::from_millis(2));
    };

    assert_eq!(timeout.peer, node_b);
    assert_eq!(timeout.attempts, 6);
    assert!(matches!(
        timeout.operation,
        nulang::runtime::CacheNetworkTimeoutOperation::Command {
            request_id: 9901,
            placement_epoch: 1,
            slot: timeout_slot,
        } if timeout_slot == slot
    ));
    assert!(service_a.try_recv_network_event().unwrap().is_none());

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
}

#[test]
fn exhausted_remote_transfer_retry_preserves_source_without_ack() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34001".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34002".parse().unwrap();
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

    let key = b"timeout-transfer-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 54002))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, base.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 54001))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut source_client = StdTcpStream::connect(service_a.local_addrs()[0]).unwrap();
    source_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    source_client
        .write_all(&frame(&[b"SET", key, b"value"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating).unwrap();
    wait_epoch(&service_a, 1);
    wait_epoch(&service_b, 1);

    runtime_a
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([node_b]));

    let pending = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 10_001)
        .unwrap();
    assert_eq!(pending.batch.entries.len(), 1);

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let timeout = loop {
        runtime_a.process_network();
        runtime_b.process_network();
        if let Some(timeout) = service_a.try_recv_network_timeout().unwrap() {
            break timeout;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for transfer retry exhaustion"
        );
        std::thread::sleep(Duration::from_millis(2));
    };

    assert_eq!(timeout.peer, node_b);
    assert_eq!(timeout.attempts, 6);
    assert!(matches!(
        timeout.operation,
        nulang::runtime::CacheNetworkTimeoutOperation::Transfer {
            transfer_id: 10_001,
            placement_epoch: 1,
            slot: timeout_slot,
        } if timeout_slot == slot
    ));
    assert!(service_a.try_recv_network_event().unwrap().is_none());

    // No application-level TransferAck was received, so source finalization
    // must never have run. The source remains authoritative during migration.
    source_client.write_all(&frame(&[b"GET", key])).unwrap();
    let mut value = [0u8; 11];
    source_client.read_exact(&mut value).unwrap();
    assert_eq!(&value, b"$5\r\nvalue\r\n");

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
}

#[test]
fn automatic_remote_transfer_retry_recovers_without_early_source_finalize() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34001".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34002".parse().unwrap();
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

    let key = b"partition-transfer-key";
    let slot = redis_slot(key);
    let source = CacheShardOwner {
        node_id: node_a.0,
        shard: 0,
    };
    let target = CacheShardOwner {
        node_id: node_b.0,
        shard: 0,
    };
    let base = CacheSlotMap::new_local(node_a.0, 1).unwrap();

    let (runtime_bridge_a, service_bridge_a) = cache_transport_bridge(64).unwrap();
    let (runtime_bridge_b, service_bridge_b) = cache_transport_bridge(64).unwrap();
    runtime_a.attach_cache_transport(runtime_bridge_a).unwrap();
    runtime_b.attach_cache_transport(runtime_bridge_b).unwrap();

    let service_a = CacheServiceBuilder::new(node_a.0, base.clone())
        .with_endpoint(target, CacheAdvertisedEndpoint::new("127.0.0.1", 54002))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_a)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let service_b = CacheServiceBuilder::new(node_b.0, base.clone())
        .with_endpoint(source, CacheAdvertisedEndpoint::new("127.0.0.1", 54001))
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .with_transport_endpoint(service_bridge_b)
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut source_client = StdTcpStream::connect(service_a.local_addrs()[0]).unwrap();
    source_client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    source_client
        .write_all(&frame(&[b"SET", key, b"value"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut source_client), b"+OK\r\n");

    let mut migrating = base;
    migrating.begin_migration(1, slot, source, target).unwrap();
    service_a.install_placement(migrating.clone()).unwrap();
    service_b.install_placement(migrating).unwrap();
    wait_epoch(&service_a, 1);
    wait_epoch(&service_b, 1);

    runtime_a
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([node_b]));

    let pending = service_a
        .send_remote_slot_batch(0, target, slot, None, 8, 9401)
        .unwrap();

    let partition_until = std::time::Instant::now() + Duration::from_millis(60);
    while std::time::Instant::now() < partition_until {
        runtime_a.process_network();
        runtime_b.process_network();
        assert!(service_a.try_recv_network_event().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(1));
    }

    // No application ACK exists yet, so the source must still retain the key.
    source_client.write_all(&frame(&[b"GET", key])).unwrap();
    let mut source_value = [0u8; 11];
    source_client.read_exact(&mut source_value).unwrap();
    assert_eq!(&source_value, b"$5\r\nvalue\r\n");

    runtime_a
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());

    let ack = wait_event(&mut runtime_a, &mut runtime_b, &service_a);
    let report = service_a
        .complete_remote_slot_batch(&pending, &ack)
        .unwrap();
    assert_eq!(report.imported, 1);
    assert_eq!(report.finalized_removed, 1);
    assert!(report.source_drained());
    assert!(service_a.try_recv_network_timeout().unwrap().is_none());

    service_a.shutdown().unwrap();
    service_b.shutdown().unwrap();
}


#[test]
fn shard_checkpoint_restores_resp_values_and_reduces_ttl() {
    let snapshot_path = temp_journal_path("shard-checkpoint").with_extension("snapshot");
    let node_id = 4242u64;
    let placement = CacheSlotMap::new_local(node_id, 1).unwrap();

    let service = CacheServiceBuilder::new(node_id, placement.clone())
        .with_shard(CacheServiceShardConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            "127.0.0.1",
        ))
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut client = StdTcpStream::connect(service.local_addrs()[0]).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    client
        .write_all(&frame(&[b"SET", b"persist", b"value"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut client), b"+OK\r\n");
    client
        .write_all(&frame(&[b"SET", b"ttl", b"live", b"PX", b"10000"]))
        .unwrap();
    assert_eq!(read_resp_line(&mut client), b"+OK\r\n");

    let checkpoint = service.checkpoint_shard(0, &snapshot_path).unwrap();
    assert_eq!(checkpoint.entries, 2);
    assert!(checkpoint.bytes > 0);
    service.shutdown().unwrap();

    std::thread::sleep(Duration::from_millis(100));

    let restored = CacheServiceBuilder::new(node_id, placement)
        .with_shard(
            CacheServiceShardConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                "127.0.0.1",
            )
            .restore_from_snapshot(&snapshot_path),
        )
        .build()
        .unwrap()
        .start()
        .unwrap();

    let mut client = StdTcpStream::connect(restored.local_addrs()[0]).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    client
        .write_all(&frame(&[b"GET", b"persist"]))
        .unwrap();
    let mut persistent = [0u8; 11];
    client.read_exact(&mut persistent).unwrap();
    assert_eq!(&persistent, b"$5\r\nvalue\r\n");

    client.write_all(&frame(&[b"GET", b"ttl"])).unwrap();
    let mut ttl_value = [0u8; 10];
    client.read_exact(&mut ttl_value).unwrap();
    assert_eq!(&ttl_value, b"$4\r\nlive\r\n");

    client.write_all(&frame(&[b"TTL", b"ttl"])).unwrap();
    let ttl_reply = read_resp_line(&mut client);
    let ttl_secs: i64 = std::str::from_utf8(&ttl_reply[1..ttl_reply.len() - 2])
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (8..=9).contains(&ttl_secs),
        "restored TTL should reflect downtime instead of resetting: {ttl_secs}"
    );

    restored.shutdown().unwrap();
    std::fs::remove_file(snapshot_path).unwrap();
}
