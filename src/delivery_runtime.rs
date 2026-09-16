//! Adapters between the versioned logical delivery model and today's runtime
//! mailbox/address types.
//!
//! The current `runtime::Message` does not yet store `MessageMeta`. These
//! helpers make that boundary explicit: converting an envelope into a legacy
//! mailbox message returns the metadata as a separate value rather than
//! silently discarding it. A future mailbox migration can change its queue
//! element to `MailboxDelivery` without changing envelope semantics.

use crate::delivery::{DeliveryEnvelope, DeliveryPriority, DeliveryTarget};
use crate::message::MessageMeta;
use crate::runtime::{ActorAddress, Message, MessagePriority};
use crate::vm::Value;
use std::sync::Arc;

/// Envelope shape matching the payload representation used by runtime
/// mailboxes today.
pub type MailboxDelivery = DeliveryEnvelope<Arc<Vec<Value>>>;

impl From<MessagePriority> for DeliveryPriority {
    fn from(value: MessagePriority) -> Self {
        match value {
            MessagePriority::System => DeliveryPriority::System,
            MessagePriority::Normal => DeliveryPriority::Normal,
            MessagePriority::Bulk => DeliveryPriority::Bulk,
        }
    }
}

impl From<DeliveryPriority> for MessagePriority {
    fn from(value: DeliveryPriority) -> Self {
        match value {
            DeliveryPriority::System => MessagePriority::System,
            DeliveryPriority::Normal => MessagePriority::Normal,
            DeliveryPriority::Bulk => MessagePriority::Bulk,
        }
    }
}

impl From<ActorAddress> for DeliveryTarget {
    fn from(value: ActorAddress) -> Self {
        match value {
            ActorAddress::Local { actor_id } => DeliveryTarget::LocalActor { actor_id },
            ActorAddress::Remote { node_id, actor_id } => DeliveryTarget::RemoteActor {
                node_id: node_id.0,
                actor_id,
            },
        }
    }
}

/// Project an existing runtime mailbox message into the logical envelope.
///
/// The caller supplies the stable metadata, logical target, and behavior name
/// because today's `Message` stores only receiver-local `behavior_id` and has
/// no target field.
pub fn envelope_from_message(
    meta: MessageMeta,
    target: DeliveryTarget,
    behavior: impl Into<String>,
    message: &Message,
) -> MailboxDelivery {
    DeliveryEnvelope {
        meta,
        sender_actor: message.sender,
        target,
        behavior: behavior.into(),
        priority: message.priority.into(),
        trace_context: message.trace_id.clone(),
        payload: Arc::clone(&message.payload),
    }
}

/// Adapt a logical envelope to today's mailbox representation.
///
/// Metadata is deliberately returned alongside the legacy `Message` instead
/// of being dropped. The future mailbox integration should enqueue both as one
/// element; until then this function prevents callers from assuming the
/// legacy message contains delivery identity.
pub fn message_from_envelope(
    envelope: MailboxDelivery,
    behavior_id: u16,
) -> (MessageMeta, Message) {
    let DeliveryEnvelope {
        meta,
        sender_actor,
        priority,
        trace_context,
        payload,
        ..
    } = envelope;

    (
        meta,
        Message {
            behavior_id,
            payload,
            sender: sender_actor,
            priority: priority.into(),
            trace_id: trace_context,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageId;
    use crate::runtime::NodeId;

    fn runtime_message() -> Message {
        Message {
            behavior_id: 4,
            payload: Arc::new(vec![Value::int(42)]),
            sender: 11,
            priority: MessagePriority::Bulk,
            trace_id: Some("00-abc-def-01".into()),
        }
    }

    #[test]
    fn runtime_message_round_trip_keeps_metadata_out_of_band() {
        let meta = MessageMeta::root(MessageId::new(3, 9));
        let original = runtime_message();
        let envelope = envelope_from_message(
            meta.clone(),
            DeliveryTarget::LocalActor { actor_id: 99 },
            "Counter.add",
            &original,
        );

        assert_eq!(envelope.meta, meta);
        assert_eq!(envelope.sender_actor, original.sender);
        assert_eq!(envelope.priority, DeliveryPriority::Bulk);
        assert_eq!(envelope.trace_context, original.trace_id);
        assert_eq!(*envelope.payload, *original.payload);

        let (returned_meta, returned) = message_from_envelope(envelope, 4);
        assert_eq!(returned_meta, meta);
        assert_eq!(returned, original);
    }

    #[test]
    fn address_conversion_preserves_remote_location() {
        let target: DeliveryTarget = ActorAddress::remote(NodeId(7), 42).into();
        assert_eq!(
            target,
            DeliveryTarget::RemoteActor {
                node_id: 7,
                actor_id: 42,
            }
        );
    }

    #[test]
    fn priorities_round_trip_without_reordering_semantics() {
        for runtime_priority in [
            MessagePriority::System,
            MessagePriority::Normal,
            MessagePriority::Bulk,
        ] {
            let logical: DeliveryPriority = runtime_priority.into();
            let restored: MessagePriority = logical.into();
            assert_eq!(restored, runtime_priority);
        }
    }
}
