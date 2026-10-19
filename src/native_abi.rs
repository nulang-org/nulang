//! Stable ABI between the actor runtime and native Nulang behavior entry points.
//!
//! Internal Nulang functions are free to use optimized, type-specialized
//! signatures. Only runtime boundaries use the fixed boxed ABI. Keeping that
//! boundary stable lets native code progressively unbox typed values without
//! coupling the actor scheduler, effect runtime, durability layer, or FFI to
//! one backend's internal representation.

use crate::types::{PrimitiveType, Type};

/// Current native actor ABI version.
///
/// Generated wrappers validate this before reading the rest of the context.
pub const NATIVE_ACTOR_ABI_VERSION: u32 = 1;

/// Native representation selected for a typed value inside compiled code.
///
/// This is deliberately separate from `vm::Value`: the VM/runtime ABI remains
/// NaN-tagged while native backends may keep proven primitive values unboxed.
/// Backends must preserve Nulang semantics (for example Int48 normalization)
/// even when the physical representation is a raw machine integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeValueRepr {
    /// NaN-tagged `vm::Value`; required whenever static representation is not
    /// known or a value crosses a boxed runtime boundary.
    Tagged,
    /// Raw machine integer representation for statically known Nulang `Int`.
    I64,
    /// Raw IEEE-754 binary64 representation for statically known `Float`.
    F64,
    /// Raw backend boolean representation for statically known `Bool`.
    Bool,
}

impl NativeValueRepr {
    /// Conservative representation for a source/MIR type.
    ///
    /// Only scalar primitives with an unambiguous native representation are
    /// unboxed here. Heap values, type variables, actor addresses, unit-like
    /// values, and composite types remain tagged until a dedicated lowering
    /// contract exists for them.
    pub fn for_type(ty: &Type) -> Self {
        match ty {
            Type::Primitive(PrimitiveType::Int) => Self::I64,
            Type::Primitive(PrimitiveType::Float) => Self::F64,
            Type::Primitive(PrimitiveType::Bool) => Self::Bool,
            _ => Self::Tagged,
        }
    }

    pub const fn is_boxed(self) -> bool {
        matches!(self, Self::Tagged)
    }

    pub const fn is_raw_scalar(self) -> bool {
        !self.is_boxed()
    }
}

/// ABI boundary crossed by a compiled Nulang value.
///
/// `Internal` is the only boundary allowed to preserve an unboxed scalar.
/// Runtime-facing boundaries intentionally force `Tagged` today. This keeps
/// native specialization an optimization rather than a semantic requirement
/// and prevents raw values from leaking into code that expects `vm::Value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeBoundary {
    Internal,
    ActorRuntime,
    EffectRuntime,
    DurableRuntime,
    Ffi,
}

impl NativeBoundary {
    pub fn representation_for(self, ty: &Type) -> NativeValueRepr {
        match self {
            Self::Internal => NativeValueRepr::for_type(ty),
            Self::ActorRuntime | Self::EffectRuntime | Self::DurableRuntime | Self::Ffi => {
                NativeValueRepr::Tagged
            }
        }
    }

    pub const fn requires_boxed_values(self) -> bool {
        !matches!(self, Self::Internal)
    }
}

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

    #[test]
    fn typed_internal_values_have_explicit_native_representations() {
        assert_eq!(
            NativeValueRepr::for_type(&Type::int()),
            NativeValueRepr::I64
        );
        assert_eq!(
            NativeValueRepr::for_type(&Type::float()),
            NativeValueRepr::F64
        );
        assert_eq!(
            NativeValueRepr::for_type(&Type::bool()),
            NativeValueRepr::Bool
        );
        assert_eq!(
            NativeValueRepr::for_type(&Type::string()),
            NativeValueRepr::Tagged
        );
    }

    #[test]
    fn runtime_boundaries_force_boxed_values() {
        for boundary in [
            NativeBoundary::ActorRuntime,
            NativeBoundary::EffectRuntime,
            NativeBoundary::DurableRuntime,
            NativeBoundary::Ffi,
        ] {
            assert!(boundary.requires_boxed_values());
            assert_eq!(
                boundary.representation_for(&Type::int()),
                NativeValueRepr::Tagged
            );
            assert_eq!(
                boundary.representation_for(&Type::float()),
                NativeValueRepr::Tagged
            );
        }
    }

    #[test]
    fn internal_boundary_preserves_typed_native_representation() {
        assert!(!NativeBoundary::Internal.requires_boxed_values());
        assert_eq!(
            NativeBoundary::Internal.representation_for(&Type::int()),
            NativeValueRepr::I64
        );
        assert_eq!(
            NativeBoundary::Internal.representation_for(&Type::float()),
            NativeValueRepr::F64
        );
        assert_eq!(
            NativeBoundary::Internal.representation_for(&Type::bool()),
            NativeValueRepr::Bool
        );
    }

    #[test]
    fn composite_and_heap_types_remain_tagged_by_default() {
        let record = Type::record(vec![("x".to_string(), Type::int())]);
        assert!(NativeValueRepr::for_type(&record).is_boxed());
        assert!(NativeValueRepr::for_type(&Type::string()).is_boxed());
        assert!(NativeValueRepr::for_type(&Type::int()).is_raw_scalar());
    }
}
