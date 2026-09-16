//! Prepared remote-delivery state shared by direct sends and deferred retries.
//!
//! Remote spawn placeholders and behavior-fetch retries currently store related
//! pieces of one logical delivery in different tuple/struct shapes. This type
//! keeps identity, causality, payload tables, trace context, and content hash
//! together so any retry can preserve the original `MessageId`.

use crate::delivery::{DeliveryEnvelope, DeliveryPriority, DeliveryTarget};
use crate::message::MessageMeta;
use crate::vm::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedRemoteDelivery {
    pub envelope: DeliveryEnvelope<Vec<Value>>,
    /// UTF-8 contents corresponding to string-id values in `envelope.payload`.
    pub string_table: Vec<String>,
    /// Immutable object bytes corresponding to object-id values in the payload.
    pub object_table: Vec<(u64, Vec<u8>)>,
    /// Optional expected behavior implementation hash.
    pub content_hash: Option<[u8; 32]>,
}

impl PreparedRemoteDelivery {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        meta: MessageMeta,
        sender_actor: u64,
        target_node: u64,
        target_actor: u64,
        behavior: impl Into<String>,
        priority: DeliveryPriority,
        trace_context: Option<String>,
        payload: Vec<Value>,
        string_table: Vec<String>,
        object_table: Vec<(u64, Vec<u8>)>,
        content_hash: Option<[u8; 32]>,
    ) -> Result<Self, PreparedDeliveryError> {
        let prepared = Self {
            envelope: DeliveryEnvelope {
                meta,
                sender_actor,
                target: DeliveryTarget::RemoteActor {
                    node_id: target_node,
                    actor_id: target_actor,
                },
                behavior: behavior.into(),
                priority,
                trace_context,
                payload,
            },
            string_table,
            object_table,
            content_hash,
        };
        prepared.validate_tables()?;
        Ok(prepared)
    }

    pub fn meta(&self) -> &MessageMeta {
        &self.envelope.meta
    }

    pub fn target_node(&self) -> u64 {
        match self.envelope.target {
            DeliveryTarget::RemoteActor { node_id, .. } => node_id,
            DeliveryTarget::LocalActor { .. } => {
                unreachable!("PreparedRemoteDelivery always has a remote target")
            }
        }
    }

    pub fn target_actor(&self) -> u64 {
        self.envelope.target.actor_id()
    }

    /// Build the next delivery attempt without creating a new logical message.
    pub fn retry(&self) -> Self {
        let mut retry = self.clone();
        retry.envelope = self.envelope.retry();
        retry
    }

    /// Validate that every table-backed value has a corresponding table entry.
    ///
    /// This mirrors the transport's current fail-closed wire-safety rule but is
    /// usable before the delivery reaches `Packet::ActorMessage`.
    pub fn validate_tables(&self) -> Result<(), PreparedDeliveryError> {
        for value in &self.envelope.payload {
            if let Some(id) = value.as_string_id() {
                if id as usize >= self.string_table.len() {
                    return Err(PreparedDeliveryError::DanglingStringId(id));
                }
            }
            if let Some(id) = value.as_object_id() {
                if id as usize >= self.object_table.len() {
                    return Err(PreparedDeliveryError::DanglingObjectId(id));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedDeliveryError {
    DanglingStringId(u32),
    DanglingObjectId(u64),
}

impl fmt::Display for PreparedDeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DanglingStringId(id) => {
                write!(f, "remote delivery contains dangling string table id {id}")
            }
            Self::DanglingObjectId(id) => {
                write!(f, "remote delivery contains dangling object table id {id}")
            }
        }
    }
}

impl std::error::Error for PreparedDeliveryError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageId;

    fn meta() -> MessageMeta {
        MessageMeta::root(MessageId::new(7, 99))
    }

    #[test]
    fn retry_preserves_payload_tables_hash_and_identity() {
        let delivery = PreparedRemoteDelivery::new(
            meta(),
            1,
            2,
            3,
            "Image.process",
            DeliveryPriority::Normal,
            Some("00-trace-parent-01".into()),
            vec![Value::string(0), Value::object(0), Value::int(42)],
            vec!["hello".into()],
            vec![(0, vec![1, 2, 3])],
            Some([9; 32]),
        )
        .unwrap();

        let retry = delivery.retry();
        assert_eq!(retry.meta().id, delivery.meta().id);
        assert_eq!(retry.meta().attempt, delivery.meta().attempt + 1);
        assert_eq!(retry.envelope.payload, delivery.envelope.payload);
        assert_eq!(retry.string_table, delivery.string_table);
        assert_eq!(retry.object_table, delivery.object_table);
        assert_eq!(retry.content_hash, delivery.content_hash);
        assert_eq!(retry.envelope.trace_context, delivery.envelope.trace_context);
    }

    #[test]
    fn target_accessors_preserve_remote_location() {
        let delivery = PreparedRemoteDelivery::new(
            meta(),
            1,
            22,
            33,
            "Counter.add",
            DeliveryPriority::Normal,
            None,
            vec![Value::int(1)],
            vec![],
            vec![],
            None,
        )
        .unwrap();
        assert_eq!(delivery.target_node(), 22);
        assert_eq!(delivery.target_actor(), 33);
    }

    #[test]
    fn dangling_string_id_fails_before_transport() {
        let err = PreparedRemoteDelivery::new(
            meta(),
            1,
            2,
            3,
            "store",
            DeliveryPriority::Normal,
            None,
            vec![Value::string(1)],
            vec!["only-index-zero".into()],
            vec![],
            None,
        )
        .unwrap_err();
        assert_eq!(err, PreparedDeliveryError::DanglingStringId(1));
    }

    #[test]
    fn dangling_object_id_fails_before_transport() {
        let err = PreparedRemoteDelivery::new(
            meta(),
            1,
            2,
            3,
            "store",
            DeliveryPriority::Normal,
            None,
            vec![Value::object(2)],
            vec![],
            vec![(0, vec![1]), (1, vec![2])],
            None,
        )
        .unwrap_err();
        assert_eq!(err, PreparedDeliveryError::DanglingObjectId(2));
    }
}
