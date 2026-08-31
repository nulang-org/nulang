//! Serialization contract for WASM Component boundary.
//!
//! Defines the Borsh-based wire format shared between the WASM component
//! compiler backend and the host runtime. Values crossing the component
//! boundary are serialized with Borsh for deterministic, fast encoding.
//!
//! Within the component, values use the same i64-tagged NaN-boxed
//! representation as the native VM; this module defines the types used
//! ONLY for cross-boundary serialization.

use crate::vm::Value;

/// A serializable subset of Nulang values for WASM component boundary.
///
/// Actor refs, closures, and heap pointers cannot cross the component
/// boundary — they serialize as `Nil`. The component compiler rejects
/// programs that try to send these types in WASM component mode.
#[derive(Debug, Clone, PartialEq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub enum WireValue {
    Nil,
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Tuple(Vec<WireValue>),
    Record(Vec<(String, WireValue)>),
    Variant(String, Option<Box<WireValue>>),
    Array(Vec<WireValue>),
}

/// A message crossing the WASM component boundary.
#[derive(Debug, Clone, PartialEq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct WireMessage {
    /// Stringified u64 actor id of the sender.
    pub sender: String,
    /// Behavior name for dispatch.
    pub behavior_name: String,
    /// Serialized payload values.
    pub payload: Vec<WireValue>,
}

/// Context needed to read heap-structured values during conversion from a
/// native VM `Value` to a `WireValue`.
///
/// The conversion is recursive, so the resolver is responsible for
/// interpreting the VM's heap layout without `wasm_types.rs` needing to
/// know the details of `ActorHeap`.
pub trait ValueResolver {
    /// UTF-8 bytes of a string value (interned or heap-allocated).
    fn string_bytes(&self, v: &Value) -> Option<Vec<u8>>;
    /// Elements of an array value.
    fn array_elements(&self, v: &Value) -> Option<Vec<Value>>;
    /// Elements of a tuple value.
    fn tuple_elements(&self, v: &Value) -> Option<Vec<Value>>;
    /// Field values of a record value, in declaration order.
    fn record_fields(&self, v: &Value) -> Option<Vec<(String, Value)>>;
}

impl WireValue {
    /// Convert a native VM `Value` to a `WireValue` for serialization.
    ///
    /// Heap pointers (arrays, tuples, records) are traversed recursively.
    /// Actor refs, closures, and raw pointers become `Nil`. Variants are
    /// best-effort: if the runtime stores them as a record whose first field
    /// is the tag name, they become `Variant`; otherwise they become `Nil`.
    pub fn from_value(v: &Value, resolver: &dyn ValueResolver) -> Self {
        if v.is_nil() {
            return WireValue::Nil;
        }
        if v.is_unit() {
            return WireValue::Unit;
        }
        if let Some(b) = v.as_bool() {
            return WireValue::Bool(b);
        }
        if let Some(i) = v.as_int() {
            return WireValue::Int(i);
        }
        if let Some(f) = v.as_float() {
            return WireValue::Float(f);
        }
        if let Some(bytes) = resolver.string_bytes(v) {
            return match String::from_utf8(bytes) {
                Ok(s) => WireValue::String(s),
                Err(_) => WireValue::Nil,
            };
        }
        if let Some(elements) = resolver.array_elements(v) {
            return WireValue::Array(
                elements
                    .iter()
                    .map(|e| WireValue::from_value(e, resolver))
                    .collect(),
            );
        }
        if let Some(elements) = resolver.tuple_elements(v) {
            return WireValue::Tuple(
                elements
                    .iter()
                    .map(|e| WireValue::from_value(e, resolver))
                    .collect(),
            );
        }
        if let Some(fields) = resolver.record_fields(v) {
            // Best-effort variant detection: a record whose first field is
            // named "__tag" is treated as a variant constructor.
            if let Some((name, _)) = fields.first() {
                if name == "__tag" && fields.len() >= 2 {
                    let tag_value = &fields[1].1;
                    let tag = if tag_value.as_string_id().is_some() {
                        resolver.string_bytes(tag_value).and_then(|b| String::from_utf8(b).ok())
                    } else {
                        None
                    };
                    if let Some(tag) = tag {
                        let payload = fields.get(2).map(|(_, v)| {
                            Box::new(WireValue::from_value(v, resolver))
                        });
                        return WireValue::Variant(tag, payload);
                    }
                }
            }
            return WireValue::Record(
                fields
                    .into_iter()
                    .map(|(name, val)| (name, WireValue::from_value(&val, resolver)))
                    .collect(),
            );
        }
        // Actor refs, closures, and unrecognized pointers become Nil.
        WireValue::Nil
    }

    /// Convert a `WireValue` back to a native VM `Value`.
    ///
    /// This is the inverse of `from_value` but only handles the primitive
    /// subset inline. Compound values require an allocator context, so this
    /// method returns `nil` for arrays, tuples, records, and variants unless
    /// a builder is supplied.
    pub fn to_value(&self) -> Value {
        match self {
            WireValue::Nil => Value::nil(),
            WireValue::Unit => Value::unit(),
            WireValue::Bool(b) => Value::bool(*b),
            WireValue::Int(i) => Value::int(*i),
            WireValue::Float(f) => Value::float(*f),
            WireValue::String(_s) => {
                // Compound string allocation requires a builder context.
                Value::nil()
            }
            WireValue::Tuple(_) | WireValue::Record(_) | WireValue::Variant(_, _) | WireValue::Array(_) => {
                // Compound allocations require a builder context.
                Value::nil()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wire_value_nil_roundtrip() {
        let wv = WireValue::Nil;
        let v = wv.to_value();
        assert!(v.is_nil());
    }

    #[test]
    fn test_wire_value_int_roundtrip() {
        let wv = WireValue::Int(42);
        let v = wv.to_value();
        assert_eq!(v.as_int(), Some(42));
    }

    #[test]
    fn test_wire_value_bool_roundtrip() {
        let wv = WireValue::Bool(true);
        let v = wv.to_value();
        assert_eq!(v.as_bool(), Some(true));
    }

    #[test]
    fn test_wire_value_unit_roundtrip() {
        let wv = WireValue::Unit;
        let v = wv.to_value();
        assert!(v.is_unit());
    }

    #[test]
    fn test_borsh_wire_message_roundtrip() {
        let msg = WireMessage {
            sender: "123".to_string(),
            behavior_name: "ping".to_string(),
            payload: vec![
                WireValue::Int(1),
                WireValue::String("hello".to_string()),
                WireValue::Array(vec![WireValue::Bool(true), WireValue::Bool(false)]),
            ],
        };
        let bytes = borsh::to_vec(&msg).unwrap();
        let decoded: WireMessage = borsh::from_slice(&bytes).unwrap();
        assert_eq!(decoded, msg);
    }
}
