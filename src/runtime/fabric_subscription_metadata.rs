//! Bounded binary codec for Fabric subscription metadata snapshots.
//!
//! FAB1 treats metadata payloads as opaque bytes. This module defines the
//! subscription payload carried inside a `FabricMetadataChunk` while keeping
//! allocation and string sizes explicitly bounded at the decode boundary.
//!
//! The FAB1 outer frame already carries owner, generation, and snapshot hash,
//! so those fields are intentionally not repeated per subscription here.
//!
//! Layout (big-endian):
//!
//! ```text
//! magic[4] = "FAS1"
//! subscription_count:u32
//! repeated subscription_count times:
//!   actor_id:u64
//!   pattern_len:u32 | pattern:utf8
//!   behavior_len:u32 | behavior:utf8
//!   group_present:u8
//!   [group_len:u32 | group:utf8] when group_present == 1
//! ```

const FAS1_MAGIC: &[u8; 4] = b"FAS1";
const HEADER_LEN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricSubscriptionMetadataEntry {
    pub pattern: String,
    pub actor_id: u64,
    pub behavior: String,
    pub group: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricSubscriptionMetadataError {
    Truncated,
    BadMagic,
    TooManySubscriptions,
    SnapshotTooLarge,
    StringTooLong,
    InvalidUtf8,
    InvalidGroupFlag,
    InvalidSubscription,
    TrailingBytes,
}

/// Encode one complete, ordered subscription snapshot into the opaque payload
/// consumed by FAB1 chunking.
///
/// The function computes the exact encoded size before allocating the output,
/// so an oversized snapshot is rejected without first materializing an
/// oversized intermediate buffer.
pub fn encode_subscription_metadata(
    subscriptions: &[FabricSubscriptionMetadataEntry],
    max_subscriptions: usize,
    max_snapshot_bytes: usize,
    max_string_bytes: usize,
) -> Result<Vec<u8>, FabricSubscriptionMetadataError> {
    if subscriptions.len() > max_subscriptions || subscriptions.len() > u32::MAX as usize {
        return Err(FabricSubscriptionMetadataError::TooManySubscriptions);
    }

    let mut encoded_len = HEADER_LEN;
    for subscription in subscriptions {
        validate_subscription(subscription)?;
        encoded_len = encoded_len
            .checked_add(8)
            .ok_or(FabricSubscriptionMetadataError::SnapshotTooLarge)?;
        encoded_len = add_string_len(encoded_len, &subscription.pattern, max_string_bytes)?;
        encoded_len = add_string_len(encoded_len, &subscription.behavior, max_string_bytes)?;
        encoded_len = encoded_len
            .checked_add(1)
            .ok_or(FabricSubscriptionMetadataError::SnapshotTooLarge)?;
        if let Some(group) = &subscription.group {
            encoded_len = add_string_len(encoded_len, group, max_string_bytes)?;
        }
        if encoded_len > max_snapshot_bytes {
            return Err(FabricSubscriptionMetadataError::SnapshotTooLarge);
        }
    }

    if encoded_len > max_snapshot_bytes {
        return Err(FabricSubscriptionMetadataError::SnapshotTooLarge);
    }

    let mut bytes = Vec::with_capacity(encoded_len);
    bytes.extend_from_slice(FAS1_MAGIC);
    bytes.extend_from_slice(&(subscriptions.len() as u32).to_be_bytes());
    for subscription in subscriptions {
        bytes.extend_from_slice(&subscription.actor_id.to_be_bytes());
        write_string(&mut bytes, &subscription.pattern);
        write_string(&mut bytes, &subscription.behavior);
        match &subscription.group {
            Some(group) => {
                bytes.push(1);
                write_string(&mut bytes, group);
            }
            None => bytes.push(0),
        }
    }
    debug_assert_eq!(bytes.len(), encoded_len);
    Ok(bytes)
}

/// Decode one complete FAB1 subscription payload.
///
/// Bounds are checked before entry-vector allocation or string copying. The
/// decoder requires exact consumption of the payload; additive extensions
/// belong outside this payload in the outer self-identifying gossip framing.
pub fn decode_subscription_metadata(
    bytes: &[u8],
    max_subscriptions: usize,
    max_snapshot_bytes: usize,
    max_string_bytes: usize,
) -> Result<Vec<FabricSubscriptionMetadataEntry>, FabricSubscriptionMetadataError> {
    if bytes.len() > max_snapshot_bytes {
        return Err(FabricSubscriptionMetadataError::SnapshotTooLarge);
    }
    if bytes.len() < HEADER_LEN {
        return Err(FabricSubscriptionMetadataError::Truncated);
    }
    if bytes.get(..4) != Some(FAS1_MAGIC.as_slice()) {
        return Err(FabricSubscriptionMetadataError::BadMagic);
    }

    let count = read_u32(bytes, 4)? as usize;
    if count > max_subscriptions {
        return Err(FabricSubscriptionMetadataError::TooManySubscriptions);
    }

    let mut offset = HEADER_LEN;
    let mut subscriptions = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let actor_id = read_u64(bytes, offset)?;
        offset = offset
            .checked_add(8)
            .ok_or(FabricSubscriptionMetadataError::Truncated)?;

        let (pattern, consumed) = read_string(bytes, offset, max_string_bytes)?;
        offset = offset
            .checked_add(consumed)
            .ok_or(FabricSubscriptionMetadataError::Truncated)?;

        let (behavior, consumed) = read_string(bytes, offset, max_string_bytes)?;
        offset = offset
            .checked_add(consumed)
            .ok_or(FabricSubscriptionMetadataError::Truncated)?;

        let group_flag = *bytes
            .get(offset)
            .ok_or(FabricSubscriptionMetadataError::Truncated)?;
        offset = offset
            .checked_add(1)
            .ok_or(FabricSubscriptionMetadataError::Truncated)?;
        let group = match group_flag {
            0 => None,
            1 => {
                let (group, consumed) = read_string(bytes, offset, max_string_bytes)?;
                offset = offset
                    .checked_add(consumed)
                    .ok_or(FabricSubscriptionMetadataError::Truncated)?;
                Some(group)
            }
            _ => return Err(FabricSubscriptionMetadataError::InvalidGroupFlag),
        };

        let subscription = FabricSubscriptionMetadataEntry {
            pattern,
            actor_id,
            behavior,
            group,
        };
        validate_subscription(&subscription)?;
        subscriptions.push(subscription);
    }

    if offset != bytes.len() {
        return Err(FabricSubscriptionMetadataError::TrailingBytes);
    }
    Ok(subscriptions)
}

fn validate_subscription(
    subscription: &FabricSubscriptionMetadataEntry,
) -> Result<(), FabricSubscriptionMetadataError> {
    if subscription.actor_id == 0
        || subscription.pattern.is_empty()
        || subscription.behavior.is_empty()
        || subscription.group.as_deref() == Some("")
    {
        return Err(FabricSubscriptionMetadataError::InvalidSubscription);
    }
    Ok(())
}

fn add_string_len(
    current: usize,
    value: &str,
    max_string_bytes: usize,
) -> Result<usize, FabricSubscriptionMetadataError> {
    let len = value.len();
    if len > max_string_bytes || len > u32::MAX as usize {
        return Err(FabricSubscriptionMetadataError::StringTooLong);
    }
    current
        .checked_add(4)
        .and_then(|size| size.checked_add(len))
        .ok_or(FabricSubscriptionMetadataError::SnapshotTooLarge)
}

fn write_string(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

fn read_string(
    bytes: &[u8],
    offset: usize,
    max_string_bytes: usize,
) -> Result<(String, usize), FabricSubscriptionMetadataError> {
    let len = read_u32(bytes, offset)? as usize;
    if len > max_string_bytes {
        return Err(FabricSubscriptionMetadataError::StringTooLong);
    }
    let start = offset
        .checked_add(4)
        .ok_or(FabricSubscriptionMetadataError::Truncated)?;
    let end = start
        .checked_add(len)
        .ok_or(FabricSubscriptionMetadataError::Truncated)?;
    let raw = bytes
        .get(start..end)
        .ok_or(FabricSubscriptionMetadataError::Truncated)?;
    let value = std::str::from_utf8(raw)
        .map_err(|_| FabricSubscriptionMetadataError::InvalidUtf8)?
        .to_owned();
    Ok((value, 4 + len))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, FabricSubscriptionMetadataError> {
    let end = offset
        .checked_add(8)
        .ok_or(FabricSubscriptionMetadataError::Truncated)?;
    let raw: [u8; 8] = bytes
        .get(offset..end)
        .ok_or(FabricSubscriptionMetadataError::Truncated)?
        .try_into()
        .map_err(|_| FabricSubscriptionMetadataError::Truncated)?;
    Ok(u64::from_be_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, FabricSubscriptionMetadataError> {
    let end = offset
        .checked_add(4)
        .ok_or(FabricSubscriptionMetadataError::Truncated)?;
    let raw: [u8; 4] = bytes
        .get(offset..end)
        .ok_or(FabricSubscriptionMetadataError::Truncated)?
        .try_into()
        .map_err(|_| FabricSubscriptionMetadataError::Truncated)?;
    Ok(u32::from_be_bytes(raw))
}
