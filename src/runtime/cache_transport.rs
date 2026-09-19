//! NUL0-v1-compatible transport envelope for the RESP cache tier.
//!
//! Cache traffic deliberately reuses the frozen `Packet::ActorMessage` wire
//! shape with reserved actor id 0 and an internal behavior name. This preserves
//! WIRE_VERSION=1 and lets the existing authenticated NetworkTransport carry
//! cache control/data traffic without introducing another socket protocol.

use std::sync::mpsc::{
    self, Receiver, SyncSender, TryRecvError, TrySendError,
};

use super::cache::{
    redis_slot, CacheTransferBatch, CacheTransferCursor, CacheTransferEntry,
    CacheTransferImport, CacheTransferToken, CacheTransferValue,
};
use super::cache_routing::{CacheShardOwner, CacheSlotMap};
use super::cluster::NodeId;
use super::mailbox::MessagePriority;
use super::network::Packet;

pub const CACHE_TRANSPORT_BEHAVIOR: &str = "__nulang_cache_transport_v1";

const ENVELOPE_MAGIC: &[u8; 4] = b"NCC1";
const KIND_COMMAND_REQUEST: u8 = 1;
const KIND_COMMAND_RESPONSE: u8 = 2;
const KIND_TRANSFER_BATCH: u8 = 3;
const KIND_TRANSFER_ACK: u8 = 4;
const KIND_MIGRATION_PROBE_REQUEST: u8 = 5;
const KIND_MIGRATION_PROBE_RESPONSE: u8 = 6;

pub const MAX_CACHE_TRANSPORT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_COMMAND_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_CACHE_TRANSFER_ENTRIES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheTransportMessage {
    CommandRequest {
        request_id: u64,
        placement_epoch: u64,
        slot: u16,
        target: CacheShardOwner,
        frame: Vec<u8>,
    },
    CommandResponse {
        request_id: u64,
        placement_epoch: u64,
        slot: u16,
        responder: CacheShardOwner,
        response: Vec<u8>,
    },
    TransferBatch {
        transfer_id: u64,
        placement_epoch: u64,
        source: CacheShardOwner,
        target: CacheShardOwner,
        batch: CacheTransferBatch,
    },
    TransferAck {
        transfer_id: u64,
        placement_epoch: u64,
        source: CacheShardOwner,
        target: CacheShardOwner,
        slot: u16,
        results: Vec<CacheTransferImport>,
    },
    MigrationProbeRequest {
        probe_id: u64,
        placement_epoch: u64,
        source: CacheShardOwner,
        target: CacheShardOwner,
        slot: u16,
    },
    MigrationProbeResponse {
        probe_id: u64,
        placement_epoch: u64,
        source: CacheShardOwner,
        target: CacheShardOwner,
        slot: u16,
        accepted: bool,
        live_entries: u64,
        import_fences: u64,
        conflicts: u64,
        wrong_slot: u64,
    },
}

impl CacheTransportMessage {
    pub fn placement_epoch(&self) -> u64 {
        match self {
            Self::CommandRequest {
                placement_epoch, ..
            }
            | Self::CommandResponse {
                placement_epoch, ..
            }
            | Self::TransferBatch {
                placement_epoch, ..
            }
            | Self::TransferAck {
                placement_epoch, ..
            }
            | Self::MigrationProbeRequest {
                placement_epoch, ..
            }
            | Self::MigrationProbeResponse {
                placement_epoch, ..
            } => *placement_epoch,
        }
    }

    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, CacheTransportCodecError> {
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(ENVELOPE_MAGIC);
        match self {
            Self::CommandRequest {
                request_id,
                placement_epoch,
                slot,
                target,
                frame,
            } => {
                if frame.len() > MAX_CACHE_COMMAND_FRAME_BYTES {
                    return Err(CacheTransportCodecError::TooLarge);
                }
                out.push(KIND_COMMAND_REQUEST);
                write_u64(&mut out, *request_id);
                write_u64(&mut out, *placement_epoch);
                write_u16(&mut out, *slot);
                write_owner(&mut out, *target);
                write_blob(&mut out, frame)?;
            }
            Self::CommandResponse {
                request_id,
                placement_epoch,
                slot,
                responder,
                response,
            } => {
                if response.len() > MAX_CACHE_COMMAND_FRAME_BYTES {
                    return Err(CacheTransportCodecError::TooLarge);
                }
                out.push(KIND_COMMAND_RESPONSE);
                write_u64(&mut out, *request_id);
                write_u64(&mut out, *placement_epoch);
                write_u16(&mut out, *slot);
                write_owner(&mut out, *responder);
                write_blob(&mut out, response)?;
            }
            Self::TransferBatch {
                transfer_id,
                placement_epoch,
                source,
                target,
                batch,
            } => {
                if batch.entries.len() > MAX_CACHE_TRANSFER_ENTRIES {
                    return Err(CacheTransportCodecError::TooManyEntries);
                }
                out.push(KIND_TRANSFER_BATCH);
                write_u64(&mut out, *transfer_id);
                write_u64(&mut out, *placement_epoch);
                write_owner(&mut out, *source);
                write_owner(&mut out, *target);
                write_transfer_batch(&mut out, batch)?;
            }
            Self::TransferAck {
                transfer_id,
                placement_epoch,
                source,
                target,
                slot,
                results,
            } => {
                if results.len() > MAX_CACHE_TRANSFER_ENTRIES {
                    return Err(CacheTransportCodecError::TooManyEntries);
                }
                out.push(KIND_TRANSFER_ACK);
                write_u64(&mut out, *transfer_id);
                write_u64(&mut out, *placement_epoch);
                write_owner(&mut out, *source);
                write_owner(&mut out, *target);
                write_u16(&mut out, *slot);
                write_u32(
                    &mut out,
                    u32::try_from(results.len())
                        .map_err(|_| CacheTransportCodecError::TooManyEntries)?,
                );
                for result in results {
                    out.push(import_result_tag(*result));
                }
            }
            Self::MigrationProbeRequest {
                probe_id,
                placement_epoch,
                source,
                target,
                slot,
            } => {
                out.push(KIND_MIGRATION_PROBE_REQUEST);
                write_u64(&mut out, *probe_id);
                write_u64(&mut out, *placement_epoch);
                write_owner(&mut out, *source);
                write_owner(&mut out, *target);
                write_u16(&mut out, *slot);
            }
            Self::MigrationProbeResponse {
                probe_id,
                placement_epoch,
                source,
                target,
                slot,
                accepted,
                live_entries,
                import_fences,
                conflicts,
                wrong_slot,
            } => {
                out.push(KIND_MIGRATION_PROBE_RESPONSE);
                write_u64(&mut out, *probe_id);
                write_u64(&mut out, *placement_epoch);
                write_owner(&mut out, *source);
                write_owner(&mut out, *target);
                write_u16(&mut out, *slot);
                out.push(u8::from(*accepted));
                write_u64(&mut out, *live_entries);
                write_u64(&mut out, *import_fences);
                write_u64(&mut out, *conflicts);
                write_u64(&mut out, *wrong_slot);
            }
        }

        if out.len() > MAX_CACHE_TRANSPORT_BYTES {
            return Err(CacheTransportCodecError::TooLarge);
        }
        Ok(out)
    }

    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, CacheTransportCodecError> {
        if bytes.len() > MAX_CACHE_TRANSPORT_BYTES {
            return Err(CacheTransportCodecError::TooLarge);
        }
        let mut reader = WireReader::new(bytes);
        if reader.take(4)? != ENVELOPE_MAGIC {
            return Err(CacheTransportCodecError::BadMagic);
        }
        let kind = reader.u8()?;
        let message = match kind {
            KIND_COMMAND_REQUEST => {
                let request_id = reader.u64()?;
                let placement_epoch = reader.u64()?;
                let slot = reader.u16()?;
                let target = reader.owner()?;
                let frame = reader.blob(MAX_CACHE_COMMAND_FRAME_BYTES)?;
                Self::CommandRequest {
                    request_id,
                    placement_epoch,
                    slot,
                    target,
                    frame,
                }
            }
            KIND_COMMAND_RESPONSE => {
                let request_id = reader.u64()?;
                let placement_epoch = reader.u64()?;
                let slot = reader.u16()?;
                let responder = reader.owner()?;
                let response = reader.blob(MAX_CACHE_COMMAND_FRAME_BYTES)?;
                Self::CommandResponse {
                    request_id,
                    placement_epoch,
                    slot,
                    responder,
                    response,
                }
            }
            KIND_TRANSFER_BATCH => {
                let transfer_id = reader.u64()?;
                let placement_epoch = reader.u64()?;
                let source = reader.owner()?;
                let target = reader.owner()?;
                let batch = reader.transfer_batch()?;
                Self::TransferBatch {
                    transfer_id,
                    placement_epoch,
                    source,
                    target,
                    batch,
                }
            }
            KIND_TRANSFER_ACK => {
                let transfer_id = reader.u64()?;
                let placement_epoch = reader.u64()?;
                let source = reader.owner()?;
                let target = reader.owner()?;
                let slot = reader.u16()?;
                let count = reader.u32()? as usize;
                if count > MAX_CACHE_TRANSFER_ENTRIES {
                    return Err(CacheTransportCodecError::TooManyEntries);
                }
                let mut results = Vec::with_capacity(count);
                for _ in 0..count {
                    results.push(import_result_from_tag(reader.u8()?)?);
                }
                Self::TransferAck {
                    transfer_id,
                    placement_epoch,
                    source,
                    target,
                    slot,
                    results,
                }
            }
            KIND_MIGRATION_PROBE_REQUEST => {
                Self::MigrationProbeRequest {
                    probe_id: reader.u64()?,
                    placement_epoch: reader.u64()?,
                    source: reader.owner()?,
                    target: reader.owner()?,
                    slot: reader.u16()?,
                }
            }
            KIND_MIGRATION_PROBE_RESPONSE => {
                let probe_id = reader.u64()?;
                let placement_epoch = reader.u64()?;
                let source = reader.owner()?;
                let target = reader.owner()?;
                let slot = reader.u16()?;
                let accepted = match reader.u8()? {
                    0 => false,
                    1 => true,
                    other => return Err(CacheTransportCodecError::InvalidBoolean(other)),
                };
                Self::MigrationProbeResponse {
                    probe_id,
                    placement_epoch,
                    source,
                    target,
                    slot,
                    accepted,
                    live_entries: reader.u64()?,
                    import_fences: reader.u64()?,
                    conflicts: reader.u64()?,
                    wrong_slot: reader.u64()?,
                }
            }
            other => return Err(CacheTransportCodecError::UnknownKind(other)),
        };
        reader.finish()?;
        Ok(message)
    }

    /// Validate the authenticated transport peer's role in this envelope.
    pub fn validate_sender(
        &self,
        authenticated_peer: NodeId,
    ) -> Result<(), CacheTransportValidationError> {
        let claimed = match self {
            Self::CommandRequest { .. } => return Ok(()),
            Self::CommandResponse { responder, .. } => responder.node_id,
            Self::TransferBatch { source, .. } => source.node_id,
            Self::TransferAck { target, .. } => target.node_id,
            Self::MigrationProbeRequest { source, .. } => source.node_id,
            Self::MigrationProbeResponse { target, .. } => target.node_id,
        };
        if claimed != authenticated_peer.0 {
            return Err(CacheTransportValidationError::SenderMismatch {
                authenticated: authenticated_peer.0,
                claimed,
            });
        }
        Ok(())
    }

    /// Fail closed unless this node's installed placement snapshot exactly
    /// authorizes the incoming mutation/routing operation.
    pub fn validate_for_node(
        &self,
        local_node_id: u64,
        placement: &CacheSlotMap,
    ) -> Result<(), CacheTransportValidationError> {
        let received_epoch = self.placement_epoch();
        let current_epoch = placement.epoch();
        if received_epoch < current_epoch {
            return Err(CacheTransportValidationError::StaleEpoch {
                current: current_epoch,
                received: received_epoch,
            });
        }
        if received_epoch > current_epoch {
            return Err(CacheTransportValidationError::FutureEpoch {
                current: current_epoch,
                received: received_epoch,
            });
        }

        match self {
            Self::CommandRequest { slot, target, .. } => {
                if target.node_id != local_node_id {
                    return Err(CacheTransportValidationError::WrongNode {
                        expected: local_node_id,
                        received: target.node_id,
                    });
                }
                let owner = placement
                    .owner_for_slot(*slot)
                    .ok_or(CacheTransportValidationError::UnknownSlot(*slot))?;
                if owner != *target {
                    return Err(CacheTransportValidationError::OwnerMismatch {
                        slot: *slot,
                        expected: owner,
                        received: *target,
                    });
                }
            }
            Self::CommandResponse {
                slot, responder, ..
            } => {
                let owner = placement
                    .owner_for_slot(*slot)
                    .ok_or(CacheTransportValidationError::UnknownSlot(*slot))?;
                if owner != *responder {
                    return Err(CacheTransportValidationError::OwnerMismatch {
                        slot: *slot,
                        expected: owner,
                        received: *responder,
                    });
                }
            }
            Self::TransferBatch {
                source,
                target,
                batch,
                ..
            } => {
                if target.node_id != local_node_id {
                    return Err(CacheTransportValidationError::WrongNode {
                        expected: local_node_id,
                        received: target.node_id,
                    });
                }
                validate_migration(placement, batch.slot, *source, *target)?;
            }
            Self::TransferAck {
                source,
                target,
                slot,
                ..
            } => {
                if source.node_id != local_node_id {
                    return Err(CacheTransportValidationError::WrongNode {
                        expected: local_node_id,
                        received: source.node_id,
                    });
                }
                validate_migration(placement, *slot, *source, *target)?;
            }
            Self::MigrationProbeRequest {
                source,
                target,
                slot,
                ..
            } => {
                if target.node_id != local_node_id {
                    return Err(CacheTransportValidationError::WrongNode {
                        expected: local_node_id,
                        received: target.node_id,
                    });
                }
                validate_migration(placement, *slot, *source, *target)?;
            }
            Self::MigrationProbeResponse {
                source,
                target,
                slot,
                ..
            } => {
                if source.node_id != local_node_id {
                    return Err(CacheTransportValidationError::WrongNode {
                        expected: local_node_id,
                        received: source.node_id,
                    });
                }
                validate_migration(placement, *slot, *source, *target)?;
            }
        }
        Ok(())
    }
}

fn validate_migration(
    placement: &CacheSlotMap,
    slot: u16,
    source: CacheShardOwner,
    target: CacheShardOwner,
) -> Result<(), CacheTransportValidationError> {
    let migration = placement
        .migration_for_slot(slot)
        .ok_or(CacheTransportValidationError::MigrationNotFound(slot))?;
    if migration.source != source || migration.target != target {
        return Err(CacheTransportValidationError::MigrationMismatch {
            slot,
            expected_source: migration.source,
            expected_target: migration.target,
            received_source: source,
            received_target: target,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTransportCodecError {
    BadMagic,
    Truncated,
    TrailingBytes,
    UnknownKind(u8),
    UnknownValueKind(u8),
    UnknownImportResult(u8),
    InvalidBoolean(u8),
    InvalidCursor,
    InvalidSlot(u16),
    TooManyEntries,
    TooLarge,
    LengthOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTransportValidationError {
    SenderMismatch {
        authenticated: u64,
        claimed: u64,
    },
    StaleEpoch {
        current: u64,
        received: u64,
    },
    FutureEpoch {
        current: u64,
        received: u64,
    },
    WrongNode {
        expected: u64,
        received: u64,
    },
    UnknownSlot(u16),
    OwnerMismatch {
        slot: u16,
        expected: CacheShardOwner,
        received: CacheShardOwner,
    },
    MigrationNotFound(u16),
    MigrationMismatch {
        slot: u16,
        expected_source: CacheShardOwner,
        expected_target: CacheShardOwner,
        received_source: CacheShardOwner,
        received_target: CacheShardOwner,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTransportPacketError {
    SenderMismatch {
        authenticated: u64,
        claimed: u64,
    },
    InvalidSystemMessage,
    Codec(CacheTransportCodecError),
}

pub fn cache_transport_packet(
    sender_node: NodeId,
    message: &CacheTransportMessage,
) -> Result<Packet, CacheTransportCodecError> {
    let bytes = message.to_wire_bytes()?;
    Ok(Packet::ActorMessage {
        target_actor: 0,
        behavior_name: CACHE_TRANSPORT_BEHAVIOR.to_string(),
        content_hash: None,
        payload: Vec::new(),
        string_table: Vec::new(),
        object_table: vec![(0, bytes)],
        sender_actor: 0,
        sender_node,
        priority: MessagePriority::System,
        trace_id: None,
    })
}

/// Parse one reserved cache system packet.
///
/// `Ok(None)` means this is not a cache-system message and should continue
/// through ordinary actor/Fabric packet handling.
pub fn parse_cache_transport_packet(
    packet: &Packet,
    authenticated_peer: NodeId,
) -> Result<Option<CacheTransportMessage>, CacheTransportPacketError> {
    let Packet::ActorMessage {
        target_actor,
        behavior_name,
        content_hash,
        payload,
        string_table,
        object_table,
        sender_actor,
        sender_node,
        priority,
        trace_id,
    } = packet
    else {
        return Ok(None);
    };

    if *target_actor != 0 || behavior_name != CACHE_TRANSPORT_BEHAVIOR {
        return Ok(None);
    }
    if *sender_node != authenticated_peer {
        return Err(CacheTransportPacketError::SenderMismatch {
            authenticated: authenticated_peer.0,
            claimed: sender_node.0,
        });
    }
    if content_hash.is_some()
        || !payload.is_empty()
        || !string_table.is_empty()
        || *sender_actor != 0
        || *priority != MessagePriority::System
        || trace_id.is_some()
    {
        return Err(CacheTransportPacketError::InvalidSystemMessage);
    }
    let [(0, bytes)] = object_table.as_slice() else {
        return Err(CacheTransportPacketError::InvalidSystemMessage);
    };
    let message =
        CacheTransportMessage::from_wire_bytes(bytes).map_err(CacheTransportPacketError::Codec)?;
    message
        .validate_sender(authenticated_peer)
        .map_err(|error| match error {
            CacheTransportValidationError::SenderMismatch {
                authenticated,
                claimed,
            } => CacheTransportPacketError::SenderMismatch {
                authenticated,
                claimed,
            },
            _ => CacheTransportPacketError::InvalidSystemMessage,
        })?;
    Ok(Some(message))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTransportInbound {
    pub from_node: NodeId,
    pub message: CacheTransportMessage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTransportOutbound {
    pub to_node: NodeId,
    pub message: CacheTransportMessage,
}

pub struct CacheRuntimeTransportEndpoint {
    inbound_tx: SyncSender<CacheTransportInbound>,
    outbound_rx: Receiver<CacheTransportOutbound>,
}

pub struct CacheServiceTransportEndpoint {
    inbound_rx: Receiver<CacheTransportInbound>,
    outbound_tx: SyncSender<CacheTransportOutbound>,
}

#[derive(Clone)]
pub struct CacheServiceTransportSender {
    outbound_tx: SyncSender<CacheTransportOutbound>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTransportBridgeError {
    InvalidCapacity,
    InboundFull,
    InboundDisconnected,
    OutboundFull,
    OutboundDisconnected,
}

pub fn cache_transport_bridge(
    capacity: usize,
) -> Result<(CacheRuntimeTransportEndpoint, CacheServiceTransportEndpoint), CacheTransportBridgeError>
{
    if capacity == 0 {
        return Err(CacheTransportBridgeError::InvalidCapacity);
    }
    let (inbound_tx, inbound_rx) = mpsc::sync_channel(capacity);
    let (outbound_tx, outbound_rx) = mpsc::sync_channel(capacity);
    Ok((
        CacheRuntimeTransportEndpoint {
            inbound_tx,
            outbound_rx,
        },
        CacheServiceTransportEndpoint {
            inbound_rx,
            outbound_tx,
        },
    ))
}

impl CacheRuntimeTransportEndpoint {
    pub(crate) fn try_forward_inbound(
        &self,
        inbound: CacheTransportInbound,
    ) -> Result<(), CacheTransportBridgeError> {
        match self.inbound_tx.try_send(inbound) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(CacheTransportBridgeError::InboundFull),
            Err(TrySendError::Disconnected(_)) => {
                Err(CacheTransportBridgeError::InboundDisconnected)
            }
        }
    }

    pub(crate) fn try_recv_outbound(
        &self,
    ) -> Result<Option<CacheTransportOutbound>, CacheTransportBridgeError> {
        match self.outbound_rx.try_recv() {
            Ok(outbound) => Ok(Some(outbound)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err(CacheTransportBridgeError::OutboundDisconnected)
            }
        }
    }
}

impl CacheServiceTransportSender {
    pub fn try_send(
        &self,
        outbound: CacheTransportOutbound,
    ) -> Result<(), CacheTransportBridgeError> {
        match self.outbound_tx.try_send(outbound) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(CacheTransportBridgeError::OutboundFull),
            Err(TrySendError::Disconnected(_)) => {
                Err(CacheTransportBridgeError::OutboundDisconnected)
            }
        }
    }
}

impl CacheServiceTransportEndpoint {
    pub fn sender(&self) -> CacheServiceTransportSender {
        CacheServiceTransportSender {
            outbound_tx: self.outbound_tx.clone(),
        }
    }

    pub fn try_recv(
        &self,
    ) -> Result<Option<CacheTransportInbound>, CacheTransportBridgeError> {
        match self.inbound_rx.try_recv() {
            Ok(inbound) => Ok(Some(inbound)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err(CacheTransportBridgeError::InboundDisconnected)
            }
        }
    }

    pub fn try_send(
        &self,
        outbound: CacheTransportOutbound,
    ) -> Result<(), CacheTransportBridgeError> {
        match self.outbound_tx.try_send(outbound) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(CacheTransportBridgeError::OutboundFull),
            Err(TrySendError::Disconnected(_)) => {
                Err(CacheTransportBridgeError::OutboundDisconnected)
            }
        }
    }
}

fn write_owner(out: &mut Vec<u8>, owner: CacheShardOwner) {
    write_u64(out, owner.node_id);
    write_u16(out, owner.shard);
}

fn write_transfer_batch(
    out: &mut Vec<u8>,
    batch: &CacheTransferBatch,
) -> Result<(), CacheTransportCodecError> {
    if batch.slot >= super::cache::REDIS_CLUSTER_SLOTS {
        return Err(CacheTransportCodecError::InvalidSlot(batch.slot));
    }
    write_u16(out, batch.slot);
    match batch.next_cursor {
        Some(cursor) => {
            out.push(1);
            write_u64(
                out,
                u64::try_from(cursor.0).map_err(|_| CacheTransportCodecError::InvalidCursor)?,
            );
        }
        None => out.push(0),
    }
    write_u64(
        out,
        u64::try_from(batch.scanned_slots)
            .map_err(|_| CacheTransportCodecError::LengthOverflow)?,
    );
    write_u64(out, batch.exported_at_ms);
    write_u32(
        out,
        u32::try_from(batch.entries.len())
            .map_err(|_| CacheTransportCodecError::TooManyEntries)?,
    );
    for entry in &batch.entries {
        write_transfer_entry(out, entry)?;
    }
    Ok(())
}

fn write_transfer_entry(
    out: &mut Vec<u8>,
    entry: &CacheTransferEntry,
) -> Result<(), CacheTransportCodecError> {
    write_blob(out, &entry.key)?;
    match &entry.value {
        CacheTransferValue::Integer(value) => {
            out.push(0);
            out.extend_from_slice(&value.to_be_bytes());
        }
        CacheTransferValue::Bytes(value) => {
            out.push(1);
            write_blob(out, value)?;
        }
    }
    match entry.ttl_ms {
        Some(ttl) => {
            out.push(1);
            write_u64(out, ttl);
        }
        None => out.push(0),
    }
    write_u32(out, entry.token.source_slot);
    write_u32(out, entry.token.source_generation);
    Ok(())
}

fn import_result_tag(result: CacheTransferImport) -> u8 {
    match result {
        CacheTransferImport::Imported => 0,
        CacheTransferImport::AlreadyImported => 1,
        CacheTransferImport::ExpiredInTransit => 2,
        CacheTransferImport::Conflict => 3,
        CacheTransferImport::WrongSlot => 4,
    }
}

fn import_result_from_tag(tag: u8) -> Result<CacheTransferImport, CacheTransportCodecError> {
    match tag {
        0 => Ok(CacheTransferImport::Imported),
        1 => Ok(CacheTransferImport::AlreadyImported),
        2 => Ok(CacheTransferImport::ExpiredInTransit),
        3 => Ok(CacheTransferImport::Conflict),
        4 => Ok(CacheTransferImport::WrongSlot),
        other => Err(CacheTransportCodecError::UnknownImportResult(other)),
    }
}

fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn write_blob(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), CacheTransportCodecError> {
    let len = u32::try_from(bytes.len()).map_err(|_| CacheTransportCodecError::LengthOverflow)?;
    write_u32(out, len);
    out.extend_from_slice(bytes);
    Ok(())
}

struct WireReader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> WireReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CacheTransportCodecError> {
        let end = self
            .cursor
            .checked_add(len)
            .ok_or(CacheTransportCodecError::LengthOverflow)?;
        let value = self
            .bytes
            .get(self.cursor..end)
            .ok_or(CacheTransportCodecError::Truncated)?;
        self.cursor = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, CacheTransportCodecError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, CacheTransportCodecError> {
        Ok(u16::from_be_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| CacheTransportCodecError::Truncated)?,
        ))
    }

    fn u32(&mut self) -> Result<u32, CacheTransportCodecError> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| CacheTransportCodecError::Truncated)?,
        ))
    }

    fn u64(&mut self) -> Result<u64, CacheTransportCodecError> {
        Ok(u64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| CacheTransportCodecError::Truncated)?,
        ))
    }

    fn owner(&mut self) -> Result<CacheShardOwner, CacheTransportCodecError> {
        Ok(CacheShardOwner {
            node_id: self.u64()?,
            shard: self.u16()?,
        })
    }

    fn blob(&mut self, max: usize) -> Result<Vec<u8>, CacheTransportCodecError> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(CacheTransportCodecError::TooLarge);
        }
        Ok(self.take(len)?.to_vec())
    }

    fn transfer_batch(&mut self) -> Result<CacheTransferBatch, CacheTransportCodecError> {
        let slot = self.u16()?;
        if slot >= super::cache::REDIS_CLUSTER_SLOTS {
            return Err(CacheTransportCodecError::InvalidSlot(slot));
        }
        let next_cursor = match self.u8()? {
            0 => None,
            1 => Some(CacheTransferCursor(
                usize::try_from(self.u64()?)
                    .map_err(|_| CacheTransportCodecError::InvalidCursor)?,
            )),
            other => return Err(CacheTransportCodecError::InvalidBoolean(other)),
        };
        let scanned_slots = usize::try_from(self.u64()?)
            .map_err(|_| CacheTransportCodecError::LengthOverflow)?;
        let exported_at_ms = self.u64()?;
        let count = self.u32()? as usize;
        if count > MAX_CACHE_TRANSFER_ENTRIES {
            return Err(CacheTransportCodecError::TooManyEntries);
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push(self.transfer_entry(slot)?);
        }
        let payload_bytes = entries.iter().map(CacheTransferEntry::payload_bytes).sum();
        Ok(CacheTransferBatch {
            slot,
            entries,
            next_cursor,
            scanned_slots,
            payload_bytes,
            exported_at_ms,
        })
    }

    fn transfer_entry(
        &mut self,
        expected_slot: u16,
    ) -> Result<CacheTransferEntry, CacheTransportCodecError> {
        let key = self.blob(MAX_CACHE_TRANSPORT_BYTES)?;
        if redis_slot(&key) != expected_slot {
            return Err(CacheTransportCodecError::InvalidSlot(redis_slot(&key)));
        }
        let value = match self.u8()? {
            0 => {
                let raw: [u8; 8] = self
                    .take(8)?
                    .try_into()
                    .map_err(|_| CacheTransportCodecError::Truncated)?;
                CacheTransferValue::Integer(i64::from_be_bytes(raw))
            }
            1 => CacheTransferValue::Bytes(self.blob(MAX_CACHE_TRANSPORT_BYTES)?),
            other => return Err(CacheTransportCodecError::UnknownValueKind(other)),
        };
        let ttl_ms = match self.u8()? {
            0 => None,
            1 => Some(self.u64()?),
            other => return Err(CacheTransportCodecError::InvalidBoolean(other)),
        };
        let token = CacheTransferToken {
            source_slot: self.u32()?,
            source_generation: self.u32()?,
        };
        Ok(CacheTransferEntry {
            key,
            value,
            ttl_ms,
            token,
        })
    }

    fn finish(&self) -> Result<(), CacheTransportCodecError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(CacheTransportCodecError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(node_id: u64, shard: u16) -> CacheShardOwner {
        CacheShardOwner { node_id, shard }
    }

    #[test]
    fn command_envelope_round_trips_and_uses_actor_message_wire_shape() {
        let message = CacheTransportMessage::CommandRequest {
            request_id: 7,
            placement_epoch: 3,
            slot: 42,
            target: owner(2, 1),
            frame: b"*1\r\n$4\r\nPING\r\n".to_vec(),
        };
        let bytes = message.to_wire_bytes().unwrap();
        assert_eq!(
            CacheTransportMessage::from_wire_bytes(&bytes).unwrap(),
            message
        );

        let packet = cache_transport_packet(NodeId(1), &message).unwrap();
        let encoded = packet.to_bytes(99);
        let (_, decoded_packet) = Packet::from_bytes(&encoded).unwrap();
        assert!(matches!(decoded_packet, Packet::ActorMessage { .. }));
        assert_eq!(
            parse_cache_transport_packet(&decoded_packet, NodeId(1)).unwrap(),
            Some(message)
        );
    }

    #[test]
    fn transfer_batch_round_trips_losslessly() {
        let key = b"k{move}".to_vec();
        let slot = redis_slot(&key);
        let entry = CacheTransferEntry {
            key,
            value: CacheTransferValue::Bytes(b"value".to_vec()),
            ttl_ms: Some(250),
            token: CacheTransferToken {
                source_slot: 12,
                source_generation: 4,
            },
        };
        let batch = CacheTransferBatch {
            slot,
            entries: vec![entry],
            next_cursor: Some(CacheTransferCursor(17)),
            scanned_slots: 8,
            payload_bytes: 12,
            exported_at_ms: 123,
        };
        let message = CacheTransportMessage::TransferBatch {
            transfer_id: 55,
            placement_epoch: 9,
            source: owner(1, 0),
            target: owner(2, 3),
            batch,
        };

        let encoded = message.to_wire_bytes().unwrap();
        let decoded = CacheTransportMessage::from_wire_bytes(&encoded).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn authenticated_peer_identity_fences_spoofed_system_packet() {
        let message = CacheTransportMessage::TransferAck {
            transfer_id: 4,
            placement_epoch: 2,
            source: owner(1, 0),
            target: owner(2, 1),
            slot: 100,
            results: vec![CacheTransferImport::Imported],
        };
        let packet = cache_transport_packet(NodeId(2), &message).unwrap();

        assert_eq!(
            parse_cache_transport_packet(&packet, NodeId(9)),
            Err(CacheTransportPacketError::SenderMismatch {
                authenticated: 9,
                claimed: 2,
            })
        );
    }

    #[test]
    fn placement_epoch_and_owner_are_validated_before_command_execution() {
        let mut map = CacheSlotMap::new_local(2, 2).unwrap();
        map.apply_epoch(2, &[]).unwrap();
        let slot = 10;
        let target = map.owner_for_slot(slot).unwrap();
        let current = CacheTransportMessage::CommandRequest {
            request_id: 1,
            placement_epoch: map.epoch(),
            slot,
            target,
            frame: b"*1\r\n$4\r\nPING\r\n".to_vec(),
        };
        assert_eq!(current.validate_for_node(2, &map), Ok(()));

        let stale = CacheTransportMessage::CommandRequest {
            request_id: 1,
            placement_epoch: 1,
            slot,
            target,
            frame: b"*1\r\n$4\r\nPING\r\n".to_vec(),
        };
        assert_eq!(
            stale.validate_for_node(2, &map),
            Err(CacheTransportValidationError::StaleEpoch {
                current: 2,
                received: 1,
            })
        );

        let wrong = CacheTransportMessage::CommandRequest {
            request_id: 1,
            placement_epoch: map.epoch(),
            slot,
            target: owner(9, 0),
            frame: b"*1\r\n$4\r\nPING\r\n".to_vec(),
        };
        assert!(matches!(
            wrong.validate_for_node(2, &map),
            Err(CacheTransportValidationError::WrongNode { .. })
        ));
    }

    #[test]
    fn transfer_requires_exact_installed_migration() {
        let mut map = CacheSlotMap::new_local(1, 1).unwrap();
        let key = b"x{migration}";
        let slot = redis_slot(key);
        let source = map.owner_for_slot(slot).unwrap();
        let target = owner(2, 0);
        map.begin_migration(1, slot, source, target).unwrap();

        let batch = CacheTransferBatch {
            slot,
            entries: Vec::new(),
            next_cursor: None,
            scanned_slots: 0,
            payload_bytes: 0,
            exported_at_ms: 0,
        };
        let message = CacheTransportMessage::TransferBatch {
            transfer_id: 1,
            placement_epoch: 1,
            source,
            target,
            batch,
        };
        assert_eq!(message.validate_for_node(2, &map), Ok(()));

        let stale = CacheTransportMessage::TransferBatch {
            transfer_id: 1,
            placement_epoch: 0,
            source,
            target,
            batch: match message {
                CacheTransportMessage::TransferBatch { batch, .. } => batch,
                _ => unreachable!(),
            },
        };
        assert_eq!(
            stale.validate_for_node(2, &map),
            Err(CacheTransportValidationError::StaleEpoch {
                current: 1,
                received: 0,
            })
        );
    }

    #[test]
    fn migration_probe_round_trips_and_authenticates_target_response() {
        let mut map = CacheSlotMap::new_local(1, 1).unwrap();
        let slot = redis_slot(b"k{probe}");
        let source = owner(1, 0);
        let target = owner(2, 0);
        map.begin_migration(7, slot, source, target).unwrap();

        let request = CacheTransportMessage::MigrationProbeRequest {
            probe_id: 44,
            placement_epoch: 7,
            source,
            target,
            slot,
        };
        let encoded = request.to_wire_bytes().unwrap();
        assert_eq!(
            CacheTransportMessage::from_wire_bytes(&encoded).unwrap(),
            request
        );
        assert_eq!(request.validate_sender(NodeId(1)), Ok(()));
        assert_eq!(request.validate_for_node(2, &map), Ok(()));

        let response = CacheTransportMessage::MigrationProbeResponse {
            probe_id: 44,
            placement_epoch: 7,
            source,
            target,
            slot,
            accepted: true,
            live_entries: 3,
            import_fences: 3,
            conflicts: 0,
            wrong_slot: 0,
        };
        let encoded = response.to_wire_bytes().unwrap();
        assert_eq!(
            CacheTransportMessage::from_wire_bytes(&encoded).unwrap(),
            response
        );
        assert_eq!(response.validate_sender(NodeId(2)), Ok(()));
        assert_eq!(response.validate_for_node(1, &map), Ok(()));
    }

    #[test]
    fn bridge_is_bounded_in_both_directions() {
        let (runtime, service) = cache_transport_bridge(1).unwrap();
        let message = CacheTransportMessage::CommandResponse {
            request_id: 1,
            placement_epoch: 0,
            slot: 0,
            responder: owner(1, 0),
            response: b"+OK\r\n".to_vec(),
        };

        service
            .try_send(CacheTransportOutbound {
                to_node: NodeId(2),
                message: message.clone(),
            })
            .unwrap();
        assert_eq!(
            service.try_send(CacheTransportOutbound {
                to_node: NodeId(2),
                message: message.clone(),
            }),
            Err(CacheTransportBridgeError::OutboundFull)
        );
        assert!(runtime.try_recv_outbound().unwrap().is_some());

        runtime
            .try_forward_inbound(CacheTransportInbound {
                from_node: NodeId(2),
                message: message.clone(),
            })
            .unwrap();
        assert_eq!(
            runtime.try_forward_inbound(CacheTransportInbound {
                from_node: NodeId(2),
                message,
            }),
            Err(CacheTransportBridgeError::InboundFull)
        );
        assert!(service.try_recv().unwrap().is_some());
    }
}
