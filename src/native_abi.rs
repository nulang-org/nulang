//! Stable ABI between the actor runtime and native Nulang behavior entry points.
//!
//! Internal Nulang functions are free to use optimized, type-specialized
//! signatures. Only the actor/runtime boundary uses this fixed C ABI. Keeping
//! that boundary stable lets the runtime add suspension/yield semantics without
//! forcing every native call through a boxed actor-context signature.

/// Current native actor ABI version.
///
/// Generated wrappers validate this before reading the rest of the context.
pub const NATIVE_ACTOR_ABI_VERSION: u32 = 1;

/// Result status returned by a native actor entry wrapper.
///
/// Only `Completed`, `BadArity`, and `AbiMismatch` are emitted today.
/// The suspension statuses reserve the ABI surface needed by the continuation
/// runtime without changing the calling convention later.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeActorStatus {
    Completed = 0,
    Waiting = 1,
    Yielded = 2,
    Suspended = 3,
    Faulted = 4,
    BadArity = 5,
    AbiMismatch = 6,
}

impl NativeActorStatus {
    pub fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Completed),
            1 => Some(Self::Waiting),
            2 => Some(Self::Yielded),
            3 => Some(Self::Suspended),
            4 => Some(Self::Faulted),
            5 => Some(Self::BadArity),
            6 => Some(Self::AbiMismatch),
            _ => None,
        }
    }
}

/// Stable runtime context passed to every native actor entry wrapper.
///
/// The payload contains boxed Nulang values. The generated wrapper validates
/// `abi_version` and `payload_len`, loads the arguments, then calls the
/// optimized internal behavior function with its ordinary native signature.
///
/// `result` is written by the wrapper before it returns `Completed`. Future
/// continuation support may also use the reserved status values to leave this
/// context populated across scheduler handoff.
#[repr(C)]
#[derive(Debug)]
pub struct NativeActorContext {
    pub abi_version: u32,
    pub flags: u32,
    pub actor_id: u64,
    pub payload_ptr: *const u64,
    pub payload_len: u64,
    pub result: u64,
}

impl NativeActorContext {
    pub fn new(actor_id: u64, payload: &[u64]) -> Self {
        Self {
            abi_version: NATIVE_ACTOR_ABI_VERSION,
            flags: 0,
            actor_id,
            payload_ptr: payload.as_ptr(),
            payload_len: payload.len() as u64,
            result: crate::vm::Value::nil().as_raw(),
        }
    }
}

/// Uniform entry-point type used by the scheduler for native actor behaviors.
pub type NativeActorEntry = unsafe extern "C" fn(*mut NativeActorContext) -> u32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_actor_context_has_stable_field_order() {
        assert_eq!(std::mem::offset_of!(NativeActorContext, abi_version), 0);
        assert_eq!(std::mem::offset_of!(NativeActorContext, flags), 4);
        assert_eq!(std::mem::offset_of!(NativeActorContext, actor_id), 8);
        assert_eq!(std::mem::offset_of!(NativeActorContext, payload_ptr), 16);
        assert_eq!(std::mem::offset_of!(NativeActorContext, payload_len), 24);
        assert_eq!(std::mem::offset_of!(NativeActorContext, result), 32);
    }

    #[test]
    fn native_actor_status_round_trips_known_values() {
        for status in [
            NativeActorStatus::Completed,
            NativeActorStatus::Waiting,
            NativeActorStatus::Yielded,
            NativeActorStatus::Suspended,
            NativeActorStatus::Faulted,
            NativeActorStatus::BadArity,
            NativeActorStatus::AbiMismatch,
        ] {
            assert_eq!(NativeActorStatus::from_raw(status as u32), Some(status));
        }
        assert_eq!(NativeActorStatus::from_raw(u32::MAX), None);
    }
}
