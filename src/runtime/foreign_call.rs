//! Owned boundary for foreign-runtime calls that may execute off the actor scheduler.
//!
//! VM Values can contain actor refs, heap pointers, closures, and string-pool
//! indices whose meaning is thread/runtime local. None of those representations
//! may be captured directly by blocking-worker jobs. This module converts a
//! call into owned semantic data before it crosses a worker-thread boundary.

use crate::vm::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum OwnedForeignValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Unit,
    Nil,
    String(String),
    /// Backend-owned opaque handle. The handle is only meaningful to the
    /// named backend; the runtime must not dereference it.
    OpaqueHandle {
        backend: &'static str,
        id: u64,
    },
}

impl OwnedForeignValue {
    /// Convert one VM value into owned foreign-call data.
    ///
    /// Strings are resolved by content through the supplied resolver instead
    /// of moving a VM/module-local string-pool id to another thread.
    pub fn from_vm<F>(value: Value, mut resolve_string: F) -> Result<Self, ForeignMarshalError>
    where
        F: FnMut(u32) -> Option<String>,
    {
        if let Some(v) = value.as_int() {
            return Ok(Self::Int(v));
        }
        if let Some(v) = value.as_bool() {
            return Ok(Self::Bool(v));
        }
        if value.is_unit() {
            return Ok(Self::Unit);
        }
        if value.is_nil() {
            return Ok(Self::Nil);
        }
        if let Some(v) = value.as_float() {
            return Ok(Self::Float(v));
        }
        if let Some(id) = value.as_string_id() {
            return resolve_string(id)
                .map(Self::String)
                .ok_or(ForeignMarshalError::UnknownString(id));
        }

        #[cfg(feature = "python")]
        {
            use crate::value_layout::{PAYLOAD_MASK, TAG_MASK};
            if value.as_raw() & TAG_MASK == crate::python::TAG_PYTHON {
                return Ok(Self::OpaqueHandle {
                    backend: "python",
                    id: value.as_raw() & PAYLOAD_MASK,
                });
            }
        }

        Err(ForeignMarshalError::UnsupportedVmValue {
            raw: value.as_raw(),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForeignCallRequest {
    pub module: String,
    pub function: String,
    pub args: Vec<OwnedForeignValue>,
}

impl ForeignCallRequest {
    pub fn new(
        module: impl Into<String>,
        function: impl Into<String>,
        args: Vec<OwnedForeignValue>,
    ) -> Self {
        Self {
            module: module.into(),
            function: function.into(),
            args,
        }
    }

    /// Marshal a VM call into fully owned worker-thread-safe data.
    pub fn from_vm_args<F>(
        module: impl Into<String>,
        function: impl Into<String>,
        args: &[Value],
        mut resolve_string: F,
    ) -> Result<Self, ForeignMarshalError>
    where
        F: FnMut(u32) -> Option<String>,
    {
        let mut owned = Vec::with_capacity(args.len());
        for value in args {
            owned.push(OwnedForeignValue::from_vm(*value, &mut resolve_string)?);
        }
        Ok(Self::new(module, function, owned))
    }
}

/// Result payload returned from a foreign worker before it is re-materialized
/// into a VM Value on the scheduler thread.
pub type ForeignCallResult = Result<OwnedForeignValue, String>;

/// Materialize an owned worker result back into a VM value on the runtime
/// thread.
///
/// Primitive values have canonical VM encodings and are reconstructed
/// directly. Strings and opaque backend handles deliberately require caller
/// callbacks because their concrete representation belongs to runtime/module
/// state that worker threads are forbidden to access.
pub fn materialize_foreign_value<FS, FO>(
    value: OwnedForeignValue,
    mut intern_string: FS,
    mut materialize_opaque: FO,
) -> Result<Value, String>
where
    FS: FnMut(String) -> Result<Value, String>,
    FO: FnMut(&'static str, u64) -> Result<Value, String>,
{
    match value {
        OwnedForeignValue::Int(value) => Ok(Value::int(value)),
        OwnedForeignValue::Float(value) => Ok(Value::float(value)),
        OwnedForeignValue::Bool(value) => Ok(Value::bool(value)),
        OwnedForeignValue::Unit => Ok(Value::unit()),
        OwnedForeignValue::Nil => Ok(Value::nil()),
        OwnedForeignValue::String(value) => intern_string(value),
        OwnedForeignValue::OpaqueHandle { backend, id } => materialize_opaque(backend, id),
    }
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForeignMarshalError {
    UnknownString(u32),
    UnsupportedVmValue { raw: u64 },
}

impl fmt::Display for ForeignMarshalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownString(id) => {
                write!(f, "foreign call references unresolved string id {id}")
            }
            Self::UnsupportedVmValue { raw } => write!(
                f,
                "VM value 0x{raw:016x} cannot cross a foreign worker-thread boundary"
            ),
        }
    }
}

impl std::error::Error for ForeignMarshalError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_static<T: Send + 'static>() {}

    #[test]
    fn materialization_reconstructs_primitives_without_runtime_state() {
        let never_string = |_value: String| -> Result<Value, String> {
            panic!("primitive materialization must not intern strings")
        };
        let never_opaque = |_backend: &'static str, _id: u64| -> Result<Value, String> {
            panic!("primitive materialization must not touch opaque handles")
        };

        assert_eq!(
            materialize_foreign_value(
                OwnedForeignValue::Int(42),
                never_string,
                never_opaque
            )
            .unwrap()
            .as_int(),
            Some(42)
        );
        assert_eq!(
            materialize_foreign_value(
                OwnedForeignValue::Bool(true),
                never_string,
                never_opaque
            )
            .unwrap()
            .as_bool(),
            Some(true)
        );
    }

    #[test]
    fn materialization_delegates_string_to_scheduler_owned_interner() {
        let value = materialize_foreign_value(
            OwnedForeignValue::String("hello".to_string()),
            |content| {
                assert_eq!(content, "hello");
                Ok(Value::string(17))
            },
            |_backend, _id| Err("unexpected opaque handle".to_string()),
        )
        .unwrap();
        assert_eq!(value.as_string_id(), Some(17));
    }

    #[test]
    fn materialization_delegates_opaque_handle_to_backend_boundary() {
        let value = materialize_foreign_value(
            OwnedForeignValue::OpaqueHandle {
                backend: "python",
                id: 91,
            },
            |_content| Err("unexpected string".to_string()),
            |backend, id| {
                assert_eq!(backend, "python");
                assert_eq!(id, 91);
                Ok(Value::int(id as i64))
            },
        )
        .unwrap();
        assert_eq!(value.as_int(), Some(91));
    }

    #[test]
    fn materialization_propagates_runtime_owned_failures() {
        let err = materialize_foreign_value(
            OwnedForeignValue::String("cannot-intern".to_string()),
            |_content| Err("string interner unavailable".to_string()),
            |_backend, _id| Err("opaque backend unavailable".to_string()),
        )
        .unwrap_err();
        assert_eq!(err, "string interner unavailable");
    }

    #[test]
    fn request_and_result_types_are_worker_thread_safe() {
        assert_send_static::<OwnedForeignValue>();
        assert_send_static::<ForeignCallRequest>();
        assert_send_static::<ForeignCallResult>();
    }

    #[test]
    fn primitive_arguments_marshal_by_value() {
        assert_eq!(
            OwnedForeignValue::from_vm(Value::int(42), |_| None).unwrap(),
            OwnedForeignValue::Int(42)
        );
        assert_eq!(
            OwnedForeignValue::from_vm(Value::bool(true), |_| None).unwrap(),
            OwnedForeignValue::Bool(true)
        );
        assert_eq!(
            OwnedForeignValue::from_vm(Value::float(1.5), |_| None).unwrap(),
            OwnedForeignValue::Float(1.5)
        );
        assert_eq!(
            OwnedForeignValue::from_vm(Value::unit(), |_| None).unwrap(),
            OwnedForeignValue::Unit
        );
        assert_eq!(
            OwnedForeignValue::from_vm(Value::nil(), |_| None).unwrap(),
            OwnedForeignValue::Nil
        );
    }

    #[test]
    fn strings_cross_by_content_not_pool_identity() {
        let value = Value::string(7);
        let owned = OwnedForeignValue::from_vm(value, |id| {
            (id == 7).then(|| "hello from nulang".to_string())
        })
        .unwrap();
        assert_eq!(
            owned,
            OwnedForeignValue::String("hello from nulang".to_string())
        );
    }

    #[test]
    fn unresolved_strings_fail_closed() {
        let err = OwnedForeignValue::from_vm(Value::string(99), |_| None).unwrap_err();
        assert_eq!(err, ForeignMarshalError::UnknownString(99));
    }

    #[test]
    fn actor_references_do_not_cross_worker_thread_boundary() {
        let err = OwnedForeignValue::from_vm(Value::actor_ref(123), |_| None).unwrap_err();
        assert!(matches!(
            err,
            ForeignMarshalError::UnsupportedVmValue { .. }
        ));
    }

    #[test]
    fn call_request_owns_module_function_and_arguments() {
        let values = [Value::int(3), Value::string(2)];
        let request = ForeignCallRequest::from_vm_args(
            "math",
            "combine",
            &values,
            |id| (id == 2).then(|| "payload".to_string()),
        )
        .unwrap();

        assert_eq!(request.module, "math");
        assert_eq!(request.function, "combine");
        assert_eq!(
            request.args,
            vec![
                OwnedForeignValue::Int(3),
                OwnedForeignValue::String("payload".to_string())
            ]
        );
    }

    #[cfg(feature = "python")]
    #[test]
    fn python_object_handles_cross_as_opaque_ids_only() {
        use crate::value_layout::PAYLOAD_MASK;
        let id = 77u64;
        let value = unsafe { Value::from_raw(crate::python::TAG_PYTHON | id) };
        let owned = OwnedForeignValue::from_vm(value, |_| None).unwrap();
        assert_eq!(
            owned,
            OwnedForeignValue::OpaqueHandle {
                backend: "python",
                id: id & PAYLOAD_MASK,
            }
        );
    }
}
