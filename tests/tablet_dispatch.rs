use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::dispatch::{
    TabletDispatchChannels, TabletDispatchConfigError, TabletDispatchError,
    TabletDispatchOutcome, TabletDispatcher, TabletExecutionError, TabletOwner,
    TabletPlacementMap,
};
use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletWrite,
};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_dispatch_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}

fn descriptor(id: u64, epoch: u64) -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(id).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        epoch,
    )
    .unwrap()
}

fn put(descriptor: &TabletDescriptor, previous: u64, value: &[u8]) -> TabletWrite {
    TabletWrite::prepare(
        descriptor,
        descriptor.ownership_epoch(),
        previous,
        previous,
        vec![TabletMutation::Put {
            key: b"k".to_vec(),
            value: value.to_vec(),
        }],
    )
    .unwrap()
}

#[test]
fn same_shard_owner_executes_directly_without_queueing() {
    let wal_path = temp_wal("direct");
    cleanup(&wal_path);

    let descriptor = descriptor(101, 9);
    let tablet_id = descriptor.id();
    let tablet = WalBackedTablet::open(descriptor.clone(), &wal_path).unwrap();

    let (channels, _inboxes) = TabletDispatchChannels::new(2, 1).unwrap();
    let mut placement = TabletPlacementMap::new();
    placement.insert(tablet_id, TabletOwner::new(1, 0, 9).unwrap());

    let dispatcher = TabletDispatcher::new(1, 0, placement, channels).unwrap();
    let mut tablets = BTreeMap::from([(tablet_id, tablet)]);

    let outcome = dispatcher
        .dispatch_write(&mut tablets, put(&descriptor, 0, b"v1"))
        .unwrap();

    assert!(matches!(
        outcome,
        TabletDispatchOutcome::Executed { sequence: 1 }
    ));
    assert_eq!(
        tablets.get(&tablet_id).unwrap().read_latest(b"k"),
        Some(&b"v1"[..])
    );

    cleanup(&wal_path);
}

#[test]
fn cross_shard_handoff_is_bounded_and_returns_a_typed_reply() {
    let wal_path = temp_wal("cross_shard");
    cleanup(&wal_path);

    let descriptor = descriptor(102, 4);
    let tablet_id = descriptor.id();
    let tablet = WalBackedTablet::open(descriptor.clone(), &wal_path).unwrap();

    let (channels, mut inboxes) = TabletDispatchChannels::new(2, 1).unwrap();
    let mut placement = TabletPlacementMap::new();
    placement.insert(tablet_id, TabletOwner::new(1, 1, 4).unwrap());

    let dispatcher = TabletDispatcher::new(1, 0, placement, channels).unwrap();
    let mut ingress_tablets = BTreeMap::new();

    let first = dispatcher
        .dispatch_write(&mut ingress_tablets, put(&descriptor, 0, b"v1"))
        .unwrap();
    let reply = match first {
        TabletDispatchOutcome::LocalQueued { shard: 1, reply } => reply,
        other => panic!("unexpected first dispatch outcome: {other:?}"),
    };

    let second = dispatcher.dispatch_write(&mut ingress_tablets, put(&descriptor, 0, b"v2"));
    assert_eq!(second.unwrap_err(), TabletDispatchError::QueueFull(1));

    let mut owner_tablets = BTreeMap::from([(tablet_id, tablet)]);
    assert!(inboxes[1].try_process_one(&mut owner_tablets));
    assert_eq!(reply.recv().unwrap(), 1);
    assert_eq!(
        owner_tablets.get(&tablet_id).unwrap().read_latest(b"k"),
        Some(&b"v1"[..])
    );

    cleanup(&wal_path);
}

#[test]
fn remote_owner_returns_an_explicit_handoff_without_touching_local_state() {
    let descriptor = descriptor(103, 12);
    let tablet_id = descriptor.id();

    let (channels, _inboxes) = TabletDispatchChannels::new(2, 8).unwrap();
    let mut placement = TabletPlacementMap::new();
    placement.insert(tablet_id, TabletOwner::new(2, 0, 12).unwrap());

    let dispatcher = TabletDispatcher::new(1, 0, placement, channels).unwrap();
    let mut tablets = BTreeMap::new();

    let outcome = dispatcher
        .dispatch_write(&mut tablets, put(&descriptor, 0, b"remote"))
        .unwrap();

    match outcome {
        TabletDispatchOutcome::Remote { request } => {
            assert_eq!(request.owner(), TabletOwner::new(2, 0, 12).unwrap());
            assert_eq!(request.write().tablet_id(), tablet_id);
            assert_eq!(request.write().sequence(), 1);
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    }
    assert!(tablets.is_empty());
}

#[test]
fn placement_epoch_mismatch_fails_before_local_or_remote_admission() {
    let descriptor = descriptor(104, 6);
    let tablet_id = descriptor.id();

    let (channels, _inboxes) = TabletDispatchChannels::new(1, 4).unwrap();
    let mut placement = TabletPlacementMap::new();
    placement.insert(tablet_id, TabletOwner::new(1, 0, 7).unwrap());

    let dispatcher = TabletDispatcher::new(1, 0, placement, channels).unwrap();
    let mut tablets = BTreeMap::new();

    assert_eq!(
        dispatcher
            .dispatch_write(&mut tablets, put(&descriptor, 0, b"stale"))
            .unwrap_err(),
        TabletDispatchError::OwnershipEpochMismatch {
            placement: 7,
            presented: 6,
        }
    );
}

#[test]
fn same_shard_route_without_owned_tablet_fails_closed() {
    let descriptor = descriptor(105, 3);
    let tablet_id = descriptor.id();

    let (channels, _inboxes) = TabletDispatchChannels::new(1, 4).unwrap();
    let mut placement = TabletPlacementMap::new();
    placement.insert(tablet_id, TabletOwner::new(1, 0, 3).unwrap());

    let dispatcher = TabletDispatcher::new(1, 0, placement, channels).unwrap();
    let mut tablets = BTreeMap::new();

    assert_eq!(
        dispatcher
            .dispatch_write(&mut tablets, put(&descriptor, 0, b"missing"))
            .unwrap_err(),
        TabletDispatchError::MissingLocalTablet(tablet_id)
    );
}

#[test]
fn owner_inbox_reports_missing_tablet_to_the_original_sender() {
    let descriptor = descriptor(106, 2);
    let tablet_id = descriptor.id();

    let (channels, mut inboxes) = TabletDispatchChannels::new(2, 4).unwrap();
    let mut placement = TabletPlacementMap::new();
    placement.insert(tablet_id, TabletOwner::new(1, 1, 2).unwrap());

    let dispatcher = TabletDispatcher::new(1, 0, placement, channels).unwrap();
    let mut ingress_tablets = BTreeMap::new();

    let outcome = dispatcher
        .dispatch_write(&mut ingress_tablets, put(&descriptor, 0, b"missing"))
        .unwrap();
    let reply = match outcome {
        TabletDispatchOutcome::LocalQueued { reply, .. } => reply,
        other => panic!("unexpected dispatch outcome: {other:?}"),
    };

    let mut owner_tablets = BTreeMap::new();
    assert!(inboxes[1].try_process_one(&mut owner_tablets));
    assert_eq!(
        reply.recv().unwrap_err(),
        TabletExecutionError::MissingTablet(tablet_id)
    );
}

#[test]
fn dispatch_configuration_rejects_zero_shards_and_zero_capacity() {
    assert!(matches!(
        TabletDispatchChannels::new(0, 1),
        Err(TabletDispatchConfigError::InvalidShardCount)
    ));
    assert!(matches!(
        TabletDispatchChannels::new(1, 0),
        Err(TabletDispatchConfigError::InvalidQueueCapacity)
    ));
}
