use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use nulang::runtime::{ClusterAction, ClusterState, NodeId, VirtualClock};

fn addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn emits_gossip(actions: &[ClusterAction]) -> bool {
    actions
        .iter()
        .any(|action| matches!(action, ClusterAction::SendGossip { .. }))
}

#[test]
fn gossip_is_rate_limited_to_cluster_maintenance_cadence() {
    let local_addr = addr(31_001);
    let peer_addr = addr(31_002);
    let local = NodeId::new(&local_addr);
    let peer = NodeId::new(&peer_addr);
    let mut clock = VirtualClock::new();
    let mut cluster = ClusterState::new(local, local_addr);
    cluster.set_clock(clock.clone());

    cluster.join_cluster_with_id(peer, peer_addr);
    cluster.handle_heartbeat(peer, peer_addr);

    let first = cluster.tick();
    assert!(
        emits_gossip(&first),
        "a healthy peer should receive an initial gossip round"
    );

    let second = cluster.tick();
    assert!(
        !emits_gossip(&second),
        "gossip must be rate-limited; a hot process_network loop must not enqueue one gossip packet per poll"
    );

    clock.advance(Duration::from_millis(99));
    cluster.set_clock(clock.clone());
    assert!(
        !emits_gossip(&cluster.tick()),
        "gossip should remain suppressed before the 100 ms maintenance cadence elapses"
    );

    clock.advance(Duration::from_millis(1));
    cluster.set_clock(clock);
    assert!(
        emits_gossip(&cluster.tick()),
        "gossip should resume once the 100 ms maintenance cadence elapses"
    );
}
