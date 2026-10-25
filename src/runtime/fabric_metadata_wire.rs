//! Binary NUL0 tail codec for chunked Fabric metadata.
//!
//! Layout (big-endian):
//!
//! ```text
//! magic[4] = "FAB1"
//! kind:u8
//! owner:u64
//! generation:u64
//! snapshot_hash:[u8;32]
//! chunk_index:u32
//! chunk_count:u32
//! payload_len:u32
//! payload:[u8; payload_len]
//! ```
//!
//! The decoder returns the number of consumed bytes so future self-identifying
//! gossip extensions can follow FAB1 without ambiguity.

use super::fabric_metadata::{FabricMetadataChunk, FabricMetadataKind};

const FAB1_MAGIC: &[u8; 4] = b"FAB1";
const FAB1_HEADER_LEN: usize = 4 + 1 + 8 + 8 + 32 + 4 + 4 + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricMetadataWireError {
    Truncated,
    BadMagic,
    UnknownKind,
    InvalidChunk,
    TooManyChunks,
    PayloadTooLarge,
}

pub fn encode_fab1_chunk(
    chunk: &FabricMetadataChunk,
    max_payload_bytes: usize,
) -> Result<Vec<u8>, FabricMetadataWireError> {
    validate_coordinates(chunk.generation, chunk.chunk_index, chunk.chunk_count)?;
    if chunk.payload.len() > max_payload_bytes || chunk.payload.len() > u32::MAX as usize {
        return Err(FabricMetadataWireError::PayloadTooLarge);
    }

    let mut bytes = Vec::with_capacity(FAB1_HEADER_LEN + chunk.payload.len());
    bytes.extend_from_slice(FAB1_MAGIC);
    bytes.push(kind_to_wire(chunk.kind));
    bytes.extend_from_slice(&chunk.owner.to_be_bytes());
    bytes.extend_from_slice(&chunk.generation.to_be_bytes());
    bytes.extend_from_slice(&chunk.snapshot_hash);
    bytes.extend_from_slice(&chunk.chunk_index.to_be_bytes());
    bytes.extend_from_slice(&chunk.chunk_count.to_be_bytes());
    bytes.extend_from_slice(&(chunk.payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&chunk.payload);
    Ok(bytes)
}

pub fn decode_fab1_chunk(
    bytes: &[u8],
    max_payload_bytes: usize,
    max_chunks: u32,
) -> Result<(FabricMetadataChunk, usize), FabricMetadataWireError> {
    if bytes.len() < FAB1_HEADER_LEN {
        return Err(FabricMetadataWireError::Truncated);
    }
    if bytes.get(..4) != Some(FAB1_MAGIC.as_slice()) {
        return Err(FabricMetadataWireError::BadMagic);
    }

    let kind = kind_from_wire(bytes[4])?;
    let owner = read_u64(bytes, 5)?;
    let generation = read_u64(bytes, 13)?;

    let mut snapshot_hash = [0u8; 32];
    snapshot_hash.copy_from_slice(
        bytes
            .get(21..53)
            .ok_or(FabricMetadataWireError::Truncated)?,
    );

    let chunk_index = read_u32(bytes, 53)?;
    let chunk_count = read_u32(bytes, 57)?;
    validate_coordinates(generation, chunk_index, chunk_count)?;
    if chunk_count > max_chunks {
        return Err(FabricMetadataWireError::TooManyChunks);
    }

    let payload_len = read_u32(bytes, 61)? as usize;
    if payload_len > max_payload_bytes {
        return Err(FabricMetadataWireError::PayloadTooLarge);
    }
    let consumed = FAB1_HEADER_LEN
        .checked_add(payload_len)
        .ok_or(FabricMetadataWireError::PayloadTooLarge)?;
    let payload = bytes
        .get(FAB1_HEADER_LEN..consumed)
        .ok_or(FabricMetadataWireError::Truncated)?
        .to_vec();

    Ok((
        FabricMetadataChunk {
            owner,
            kind,
            generation,
            snapshot_hash,
            chunk_index,
            chunk_count,
            payload,
        },
        consumed,
    ))
}

fn validate_coordinates(
    generation: u64,
    chunk_index: u32,
    chunk_count: u32,
) -> Result<(), FabricMetadataWireError> {
    if generation == 0 || chunk_count == 0 || chunk_index >= chunk_count {
        return Err(FabricMetadataWireError::InvalidChunk);
    }
    Ok(())
}

fn kind_to_wire(kind: FabricMetadataKind) -> u8 {
    match kind {
        FabricMetadataKind::Subscriptions => 1,
        FabricMetadataKind::Services => 2,
        FabricMetadataKind::Load => 3,
    }
}

fn kind_from_wire(value: u8) -> Result<FabricMetadataKind, FabricMetadataWireError> {
    match value {
        1 => Ok(FabricMetadataKind::Subscriptions),
        2 => Ok(FabricMetadataKind::Services),
        3 => Ok(FabricMetadataKind::Load),
        _ => Err(FabricMetadataWireError::UnknownKind),
    }
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, FabricMetadataWireError> {
    let raw: [u8; 8] = bytes
        .get(offset..offset + 8)
        .ok_or(FabricMetadataWireError::Truncated)?
        .try_into()
        .map_err(|_| FabricMetadataWireError::Truncated)?;
    Ok(u64::from_be_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, FabricMetadataWireError> {
    let raw: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or(FabricMetadataWireError::Truncated)?
        .try_into()
        .map_err(|_| FabricMetadataWireError::Truncated)?;
    Ok(u32::from_be_bytes(raw))
}
