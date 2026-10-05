use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use nulang::runtime::{ClusterAction, ClusterState, NodeId};

fn addr(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn emits_gossip(actions: &[ClusterAction]) -> bool {
    actions
        .iter()
        .any(|action| matches!(action, ClusterAction::SendGossip { .. }))
}

#[test]
fn repeated_ticks_without_time_advance_do_not_emit_unbounded_gossip() {
    let local_addr = addr(31_001);
    let peer_addr = addr(31_002);
    let local = NodeId::new(&local_addr);
    let peer = NodeId::new(&peer_addr);
    let mut cluster = ClusterState::new(local, local_addr);

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
}
