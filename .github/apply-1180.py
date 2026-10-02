from pathlib import Path

network = Path('src/runtime/network.rs')
s = network.read_text()

old = '''#[derive(Debug, Clone)]
pub struct OutgoingPacket {
    pub to_node: NodeId,
    pub to_addr: SocketAddr,
    pub packet: Packet,
}
'''
new = old + '''
/// Observable outcome for an ordinary remote send that the transport could
/// not confirm. Ordinary actor delivery is at-most-once: failures are surfaced
/// to the sender, but packets are never retried after an ambiguous write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryFailureKind {
    Connect,
    WriteAmbiguous,
    SenderStopped,
    InvalidPayload,
}

impl DeliveryFailureKind {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Connect => "transport connect failed",
            Self::WriteAmbiguous => "transport write failed",
            Self::SenderStopped => "transport sender stopped",
            Self::InvalidPayload => "transport payload invalid",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeliveryFailure {
    pub to_node: NodeId,
    pub to_addr: SocketAddr,
    pub packet: Packet,
    pub kind: DeliveryFailureKind,
    pub error: String,
}
'''
assert old in s
s = s.replace(old, new, 1)

old = '''    fn receive(&self) -> Vec<IncomingPacket>;
    fn node_id(&self) -> NodeId;
'''
new = '''    fn receive(&self) -> Vec<IncomingPacket>;
    fn take_delivery_failures(&self) -> Vec<DeliveryFailure> {
        Vec::new()
    }
    fn node_id(&self) -> NodeId;
'''
assert old in s
s = s.replace(old, new, 1)

old = '''    fn receive(&self) -> Vec<IncomingPacket> {
        (**self).receive()
    }
    fn node_id(&self) -> NodeId {
'''
new = '''    fn receive(&self) -> Vec<IncomingPacket> {
        (**self).receive()
    }
    fn take_delivery_failures(&self) -> Vec<DeliveryFailure> {
        (**self).take_delivery_failures()
    }
    fn node_id(&self) -> NodeId {
'''
assert old in s
s = s.replace(old, new, 1)

old = '''    /// Channel endpoint used to enqueue packets for transmission.
    outgoing_tx: mpsc::SyncSender<OutgoingPacket>,
    /// Background thread handles.
'''
new = '''    /// Channel endpoint used to enqueue packets for transmission.
    outgoing_tx: mpsc::SyncSender<OutgoingPacket>,
    /// Asynchronous failures produced by the sender thread.
    failure_tx: mpsc::Sender<DeliveryFailure>,
    failure_rx: mpsc::Receiver<DeliveryFailure>,
    /// Background thread handles.
'''
assert old in s
s = s.replace(old, new, 1)

old = '''        let (incoming_tx, incoming_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (outgoing_tx, outgoing_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
'''
new = old + '''        let (failure_tx, failure_rx) = mpsc::channel();
'''
assert old in s
s = s.replace(old, new, 1)

old = '''            let tls = tls_config.clone();
            let handle = thread::Builder::new()
                .name("nulang-net-sender".into())
                .spawn(move || {
                    sender_thread(outgoing_rx, conns, flag, local_id, in_tx, tls);
                })?;
'''
new = '''            let tls = tls_config.clone();
            let failures = failure_tx.clone();
            let handle = thread::Builder::new()
                .name("nulang-net-sender".into())
                .spawn(move || {
                    sender_thread(outgoing_rx, conns, flag, local_id, in_tx, tls, failures);
                })?;
'''
assert old in s
s = s.replace(old, new, 1)

old = '''            incoming_tx,
            outgoing_tx,
            threads: Arc::new(Mutex::new(handles)),
'''
new = '''            incoming_tx,
            outgoing_tx,
            failure_tx,
            failure_rx,
            threads: Arc::new(Mutex::new(handles)),
'''
assert old in s
s = s.replace(old, new, 1)

old = '''        if !packet_payload_wire_safe(&packet) {
            warn!(
                "nulang-net: dropping packet to node {:?} (addr {}): payload value cannot cross the wire (heap pointer, nil, or string without content)",
                to_node, to_addr
            );
            return;
        }
'''
new = '''        if !packet_payload_wire_safe(&packet) {
            warn!(
                "nulang-net: dropping packet to node {:?} (addr {}): payload value cannot cross the wire (heap pointer, nil, or string without content)",
                to_node, to_addr
            );
            let _ = self.failure_tx.send(DeliveryFailure {
                to_node,
                to_addr,
                packet,
                kind: DeliveryFailureKind::InvalidPayload,
                error: "payload cannot cross the wire losslessly".to_string(),
            });
            return;
        }
'''
assert old in s
s = s.replace(old, new, 1)

old = '''        if self.outgoing_tx.send(outgoing).is_err() {
            warn!(
                "nulang-net: dropping packet to node {:?} (addr {}): sender thread shut down",
                to_node, to_addr
            );
        }
    }

    /// Receive incoming packets (non-blocking).
'''
new = '''        if let Err(err) = self.outgoing_tx.send(outgoing) {
            warn!(
                "nulang-net: dropping packet to node {:?} (addr {}): sender thread shut down",
                to_node, to_addr
            );
            let outgoing = err.0;
            let _ = self.failure_tx.send(DeliveryFailure {
                to_node: outgoing.to_node,
                to_addr: outgoing.to_addr,
                packet: outgoing.packet,
                kind: DeliveryFailureKind::SenderStopped,
                error: "sender thread shut down".to_string(),
            });
        }
    }

    /// Drain asynchronous delivery failures (non-blocking).
    pub fn take_delivery_failures(&self) -> Vec<DeliveryFailure> {
        let mut failures = Vec::new();
        loop {
            match self.failure_rx.try_recv() {
                Ok(failure) => failures.push(failure),
                Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        failures
    }

    /// Receive incoming packets (non-blocking).
'''
assert old in s
s = s.replace(old, new, 1)

old = '''    incoming_tx: mpsc::SyncSender<IncomingPacket>,
    tls_config: TlsConfig,
) {
'''
new = '''    incoming_tx: mpsc::SyncSender<IncomingPacket>,
    tls_config: TlsConfig,
    failure_tx: mpsc::Sender<DeliveryFailure>,
) {
'''
assert old in s
s = s.replace(old, new, 1)

old = '''            if let Err(e) = connect_in_sender(
                &connections,
                &incoming_tx,
                &shutdown_flag,
                local_node_id,
                outgoing.to_node,
                outgoing.to_addr,
                &tls_config,
            ) {
                warn!(
                    "[nulang-net] Failed to connect to {:?} at {}: {}",
                    outgoing.to_node, outgoing.to_addr, e
                );
            }
        }
'''
new = '''            if let Err(e) = connect_in_sender(
                &connections,
                &incoming_tx,
                &shutdown_flag,
                local_node_id,
                outgoing.to_node,
                outgoing.to_addr,
                &tls_config,
            ) {
                warn!(
                    "[nulang-net] Failed to connect to {:?} at {}: {}",
                    outgoing.to_node, outgoing.to_addr, e
                );
                let _ = failure_tx.send(DeliveryFailure {
                    to_node: outgoing.to_node,
                    to_addr: outgoing.to_addr,
                    packet: outgoing.packet,
                    kind: DeliveryFailureKind::Connect,
                    error: e.to_string(),
                });
                continue;
            }
        }
'''
assert old in s
s = s.replace(old, new, 1)

old = '''        if let Err(e) = result {
            warn!(
                "[nulang-net] Send to {:?} failed: {}; removing connection",
                outgoing.to_node, e
            );
            let mut conns = lock_ignore_poison(&connections);
            if let Some(conn) = conns.remove(&outgoing.to_node) {
                let _ = conn.stream.shutdown();
            }
        }
'''
new = '''        if let Err(e) = result {
            warn!(
                "[nulang-net] Send to {:?} failed: {}; removing connection",
                outgoing.to_node, e
            );
            let _ = failure_tx.send(DeliveryFailure {
                to_node: outgoing.to_node,
                to_addr: outgoing.to_addr,
                packet: outgoing.packet,
                kind: DeliveryFailureKind::WriteAmbiguous,
                error: e.to_string(),
            });
            let mut conns = lock_ignore_poison(&connections);
            if let Some(conn) = conns.remove(&outgoing.to_node) {
                let _ = conn.stream.shutdown();
            }
        }
'''
assert old in s
s = s.replace(old, new, 1)

old = '''    fn receive(&self) -> Vec<IncomingPacket> {
        self.receive()
    }
    fn node_id(&self) -> NodeId {
'''
new = '''    fn receive(&self) -> Vec<IncomingPacket> {
        self.receive()
    }
    fn take_delivery_failures(&self) -> Vec<DeliveryFailure> {
        self.take_delivery_failures()
    }
    fn node_id(&self) -> NodeId {
'''
assert old in s
s = s.replace(old, new, 1)

marker = '''    // ------------------------------------------------------------------
    // 2. ActorMessage roundtrip
'''
test = '''    #[cfg(feature = "tcp")]
    #[test]
    fn test_tcp_connect_failure_is_observable_and_not_retried() {
        let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let unreachable = reservation.local_addr().expect("reserved address");
        drop(reservation);

        let mut transport = TcpTransport::bind(
            "127.0.0.1:0".parse().unwrap(),
            TlsConfig::PlaintextInsecure,
        )
        .expect("bind transport");
        let peer = NodeId::new(&unreachable);
        transport.send(
            peer,
            unreachable,
            Packet::Heartbeat {
                node_id: transport.node_id(),
                timestamp: 1,
            },
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        let failure = loop {
            if let Some(failure) = transport.take_delivery_failures().into_iter().next() {
                break failure;
            }
            assert!(Instant::now() < deadline, "connect failure was not surfaced");
            sleep(Duration::from_millis(10));
        };
        assert_eq!(failure.kind, DeliveryFailureKind::Connect);
        assert_eq!(failure.to_node, peer);
        sleep(Duration::from_millis(100));
        assert!(transport.take_delivery_failures().is_empty(), "failed send must not be retried");
        transport.shutdown();
    }

'''
assert marker in s
s = s.replace(marker, test + marker, 1)
network.write_text(s)

p = Path('src/runtime/distributed.rs')
s = p.read_text()
old = '''pub fn process_network_packets(
    runtime: &mut Runtime,
    transport: &mut dyn NetworkTransport,
    cluster: &mut ClusterState,
    resolver: &mut AddressResolver,
) {
    let packets = transport.receive();
'''
new = '''pub fn process_network_packets(
    runtime: &mut Runtime,
    transport: &mut dyn NetworkTransport,
    cluster: &mut ClusterState,
    resolver: &mut AddressResolver,
) {
    for failure in transport.take_delivery_failures() {
        if let Packet::ActorMessage { sender_actor, .. } = &failure.packet {
            notify_delivery_failed(runtime, *sender_actor, failure.kind.reason());
        }
    }

    let packets = transport.receive();
'''
assert old in s
s = s.replace(old, new, 1)
old = '''        "object intern failed on receiver" => 7,
        _ => 5,
'''
new = '''        "object intern failed on receiver" => 7,
        "transport connect failed" => 8,
        "transport write failed" => 9,
        "transport sender stopped" => 10,
        "transport payload invalid" => 11,
        _ => 5,
'''
assert old in s
s = s.replace(old, new, 1)
p.write_text(s)
