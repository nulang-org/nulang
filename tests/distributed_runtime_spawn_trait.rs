use nulang::runtime::{
    Actor, DeterministicNetworkTransport, DistributedRuntime, DistributedRuntimeImpl, NodeId, Runtime,
};
use nulang::vm::Value;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};

static STORED: AtomicI64 = AtomicI64::new(-1);
static CAPTURED_COUNT: AtomicI64 = AtomicI64::new(-1);

fn addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn store(_actor: &mut Actor, args: &[Value]) {
    STORED.store(
        args.first().and_then(Value::as_int).unwrap_or(-1),
        Ordering::SeqCst,
    );
}

fn capture_count(actor: &mut Actor, _args: &[Value]) {
    CAPTURED_COUNT.store(
        actor
            .get_state_field("count")
            .and_then(|value| value.as_int())
            .unwrap_or(-1),
        Ordering::SeqCst,
    );
}

#[test]
fn distributed_runtime_trait_local_spawn_returns_real_actor() {
    let mut runtime = Runtime::new();
    let mut transport = DeterministicNetworkTransport::bind(addr(18_997)).unwrap();

    let spawned = {
        let mut distributed = DistributedRuntimeImpl::new(&mut runtime);
        DistributedRuntime::spawn_on_node(
            &mut distributed,
            &mut transport,
            NodeId::LOCAL,
            "store",
            vec![],
        )
    };

    assert!(spawned.is_local());
    assert_ne!(
        spawned.actor_id(),
        0,
        "the trait must return the id of a real local actor, not the reserved invalid actor id"
    );
}

#[test]
fn distributed_runtime_trait_local_spawn_preserves_initial_state() {
    CAPTURED_COUNT.store(-1, Ordering::SeqCst);

    let mut runtime = Runtime::new();
    runtime.register_spawnable_behavior("capture_count", capture_count);
    let mut transport = DeterministicNetworkTransport::bind(addr(18_998)).unwrap();

    let spawned = {
        let mut distributed = DistributedRuntimeImpl::new(&mut runtime);
        DistributedRuntime::spawn_on_node(
            &mut distributed,
            &mut transport,
            NodeId::LOCAL,
            "capture_count",
            vec![("count".to_string(), Value::int(41))],
        )
    };

    runtime.send_message(spawned.actor_id(), "capture_count", &[]);
    runtime.run_scheduler();

    assert_eq!(
        CAPTURED_COUNT.load(Ordering::SeqCst),
        41,
        "the trait must preserve the supplied initial state on the local actor"
    );
}

#[test]
fn distributed_runtime_trait_local_spawn_wires_registered_behavior() {
    STORED.store(-1, Ordering::SeqCst);

    let mut runtime = Runtime::new();
    runtime.register_spawnable_behavior("store", store);
    let mut transport = DeterministicNetworkTransport::bind(addr(18_999)).unwrap();

    let spawned = {
        let mut distributed = DistributedRuntimeImpl::new(&mut runtime);
        DistributedRuntime::spawn_on_node(
            &mut distributed,
            &mut transport,
            NodeId::LOCAL,
            "store",
            vec![],
        )
    };

    runtime.send_message(spawned.actor_id(), "store", &[Value::int(7)]);
    runtime.run_scheduler();

    assert_eq!(
        STORED.load(Ordering::SeqCst),
        7,
        "the local trait path must install and execute the requested spawnable behavior"
    );
}
