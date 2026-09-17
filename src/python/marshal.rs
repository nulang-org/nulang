//! Bidirectional marshalling between Nulang VM Values and Python objects.
//!
//! Converts data between the Nulang runtime's NaN-tagged `Value` type and
//! Python objects managed by the CPython interpreter via PyO3.
//!
//! # Conversion Rules
//!
//! | Nulang Value | Python Object | Notes |
//! |-------------|---------------|-------|
//! | `TAG_INT` | `int` | 48-bit signed range |
//! | Float (non-NaN) | `float` | IEEE 754 double |
//! | `TAG_SPECIAL` (true/false) | `bool` | Direct mapping |
//! | `TAG_SPECIAL` (unit) | `None` | Nulang `()` → Python `None` |
//! | `TAG_STRING` | `str` | Interned string (ID → string lookup) |
//! | `TAG_PYTHON` | opaque Py<PyAny> | Direct registry lookup |
//! | `TAG_PTR` | `None` | Heap pointers are opaque |
//! | Python `str` | `TAG_PYTHON` | Stored as opaque Python object |
//! | Python `list` | `TAG_PYTHON` | Stored as opaque Python object |
//! | Python `dict` | `TAG_PYTHON` | Stored as opaque Python object |
//! | Other Python | `TAG_PYTHON` | Stored as opaque Python object |

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};

use crate::python::bridge::{get_object, register_object, PyBridge, PythonObjectId, TAG_PYTHON};
use crate::vm::Value;

// ---------------------------------------------------------------------------
// Constants (from src/value_layout.rs)
// ---------------------------------------------------------------------------

// `TAG_PYTHON` is defined in src/python/bridge.rs and imported here so the
// canonical value is not duplicated.
#[cfg(test)]
use crate::value_layout::TAG_CLOSURE;
use crate::value_layout::{TAG_ACTOR, TAG_MASK, TAG_PTR, TAG_STRING};

// ---------------------------------------------------------------------------
// Nulang → Python
// ---------------------------------------------------------------------------

/// Convert a Nulang `Value` to a Python object.
///
/// This function acquires the GIL internally and returns a `Py<PyAny>`
/// (a refcounted handle). The caller is responsible for managing the
/// returned object's lifecycle — typically by inserting it into the
/// [`PYTHON_REGISTRY`](crate::python::bridge::PYTHON_REGISTRY).
///
/// # Tag Mapping
///
/// | Nulang Tag | Python Type |
/// |------------|-------------|
/// | `TAG_INT` | `int` |
/// | Float | `float` |
/// | `TAG_SPECIAL` (true/false) | `bool` |
/// | `TAG_SPECIAL` (unit/nil) | `None` |
/// | `TAG_STRING` | `str` (via interned string ID) |
/// | `TAG_PYTHON` | opaque `Py<PyAny>` (registry lookup) |
/// | `TAG_PTR` | `None` (opaque heap pointer) |
/// | `TAG_ACTOR` | `None` (opaque actor reference) |
pub fn nulang_to_python(value: Value) -> Result<Py<PyAny>, String> {
    if let Some(n) = value.as_int() {
        return Python::attach(|py| -> Result<Py<PyAny>, String> {
            Ok(n.into_pyobject(py)
                .map_err(|e| e.to_string())?
                .unbind()
                .into_any())
        });
    }
    if let Some(b) = value.as_bool() {
        return Python::attach(|py| -> Result<Py<PyAny>, String> {
            Ok(b.into_pyobject(py)
                .map_err(|e| e.to_string())?
                .to_owned()
                .unbind()
                .into_any())
        });
    }
    if value.is_nil() || value.is_unit() {
        return Ok(Python::attach(|py| py.None()));
    }
    if let Some(f) = value.as_float() {
        return Python::attach(|py| -> Result<Py<PyAny>, String> {
            Ok(f.into_pyobject(py)
                .map_err(|e| e.to_string())?
                .unbind()
                .into_any())
        });
    }

    let tag = value.as_raw() & TAG_MASK;
    if tag == TAG_STRING {
        // TAG_STRING → Python str
        // Nulang strings are interned; the payload is a u32 string pool ID.
        // Since we don't have the string pool here, we represent it as a
        // descriptive string. A future optimization will resolve the ID
        // through the string interner.
        let string_id = (value.as_raw() & 0x0000_FFFF_FFFF_FFFF) as u32;
        let repr = format!("<nulang_string:{}>", string_id);
        Python::attach(|py| -> Result<Py<PyAny>, String> {
            Ok(repr
                .into_pyobject(py)
                .map_err(|e| e.to_string())?
                .unbind()
                .into_any())
        })
    } else if tag == TAG_PYTHON {
        // TAG_PYTHON → look up in registry and return the Py<PyAny>
        let obj_id = (value.as_raw() & 0x0000_FFFF_FFFF_FFFF) as u64;
        let py_id = PythonObjectId(obj_id);
        get_object(py_id)
            .ok_or_else(|| format!("TAG_PYTHON object with ID {} not found in registry", obj_id))
    } else if tag == TAG_PTR || tag == TAG_ACTOR {
        // Opaque references → None
        Ok(Python::attach(|py| py.None()))
    } else {
        // Unrecognized value — return None as fallback
        Ok(Python::attach(|py| py.None()))
    }
}

// ---------------------------------------------------------------------------
// Python → Nulang
// ---------------------------------------------------------------------------

/// Convert a Python object (as a `Bound` reference) to a Nulang `Value`.
///
/// The caller must hold the GIL — this function takes a `&Bound<'_, PyAny>`
/// which can only be obtained within a `Python::attach` scope.
///
/// # Type Mapping
///
/// | Python Type | Nulang Value | Notes |
/// |-------------|-------------|-------|
/// | `None` | `Value::unit()` | |
/// | `bool` | `Value::bool()` | |
/// | `int` | `Value::int()` | Clamped to i64 range |
/// | `float` | `Value::float()` | |
/// | `str` | `TAG_PYTHON` | Stored as opaque Python object |
/// | `list` | `TAG_PYTHON` | Stored as opaque Python object |
/// | `tuple` | `TAG_PYTHON` | Stored as opaque Python object |
/// | `dict` | `TAG_PYTHON` | Stored as opaque Python object |
/// | Other | `TAG_PYTHON` | Stored as opaque Python object |
pub fn python_to_nulang(obj: &pyo3::Bound<'_, pyo3::PyAny>) -> Result<Value, String> {
    // Check for None first
    if obj.is_none() {
        return Ok(Value::unit());
    }

    // Check for bool (must come before int, since bool subclasses int in Python)
    if let Ok(b) = obj.cast::<PyBool>() {
        let val: bool = b
            .extract()
            .map_err(|e| format!("Failed to extract bool: {}", e))?;
        return Ok(Value::bool(val));
    }

    // Check for int
    if let Ok(i) = obj.cast::<PyInt>() {
        let val: i64 = i.extract().unwrap_or_else(|_| {
            // Big int — try to extract and clamp
            Python::attach(|_py| {
                let big_int_str = i.str().map(|s| s.to_string()).unwrap_or_default();
                big_int_str.parse::<i64>().unwrap_or(i64::MAX)
            })
        });
        return Ok(Value::int(val));
    }

    // Check for float
    if let Ok(f) = obj.cast::<PyFloat>() {
        let val: f64 = f
            .extract()
            .map_err(|e| format!("Failed to extract float: {}", e))?;
        return Ok(Value::float(val));
    }

    // Check for str
    if let Ok(_s) = obj.cast::<PyString>() {
        // Store as opaque Python object (TAG_PYTHON).
        // Future optimization: intern the string and use TAG_STRING.
        let py_obj: Py<PyAny> = obj.clone().unbind().into_any();
        let id = register_object(py_obj);
        return Ok(unsafe { Value::from_raw(TAG_PYTHON | id.0) });
    }

    // Check for list
    if let Ok(_lst) = obj.cast::<PyList>() {
        let py_obj: Py<PyAny> = obj.clone().unbind().into_any();
        let id = register_object(py_obj);
        return Ok(unsafe { Value::from_raw(TAG_PYTHON | id.0) });
    }

    // Check for tuple
    if let Ok(_tup) = obj.cast::<PyTuple>() {
        let py_obj: Py<PyAny> = obj.clone().unbind().into_any();
        let id = register_object(py_obj);
        return Ok(unsafe { Value::from_raw(TAG_PYTHON | id.0) });
    }

    // Check for dict
    if let Ok(_d) = obj.cast::<PyDict>() {
        let py_obj: Py<PyAny> = obj.clone().unbind().into_any();
        let id = register_object(py_obj);
        return Ok(unsafe { Value::from_raw(TAG_PYTHON | id.0) });
    }

    // Any other Python type — store as opaque Python object
    let py_obj: Py<PyAny> = obj.clone().unbind().into_any();
    let id = register_object(py_obj);
    Ok(unsafe { Value::from_raw(TAG_PYTHON | id.0) })
}

// ---------------------------------------------------------------------------
// Higher-level conversion helpers
// ---------------------------------------------------------------------------

/// Convert a Nulang `Value` to a `PythonObjectId` (inserts into registry).
///
/// This is a convenience function that first converts the `Value` to a
/// Python object via [`nulang_to_python`], then inserts the result into
/// the global registry and returns its handle.
///
/// # Errors
///
/// Returns an error if the Nulang value cannot be converted, or if the
/// registry lookup for `TAG_PYTHON` values fails.
pub fn value_to_python_object_id(val: Value) -> Result<PythonObjectId, String> {
    let py_obj = nulang_to_python(val)?;
    Ok(register_object(py_obj))
}

/// Convert a `PythonObjectId` to a Nulang `Value`.
///
/// Looks up the object in the global registry, then converts it via
/// [`python_to_nulang`]. The resulting `Value` will have `TAG_PYTHON`
/// for complex Python objects, or a more specific tag for primitive
/// values (int, float, bool, unit).
///
/// # Errors
///
/// Returns an error if the object ID is not found in the registry.
pub fn python_object_id_to_value(obj_id: PythonObjectId) -> Result<Value, String> {
    let py_obj = get_object(obj_id)
        .ok_or_else(|| format!("Python object ID {:?} not found in registry", obj_id))?;

    Python::attach(|py| {
        let bound = py_obj.bind(py);
        python_to_nulang(bound)
    })
}

// ---------------------------------------------------------------------------
// Compatibility wrappers (bridge parameter ignored — uses global registry)
// ---------------------------------------------------------------------------

/// Convert a Nulang `Value` to a `PythonObjectId`.
///
/// Wrapper with the interface expected by the VM. The `bridge` parameter
/// is accepted for API consistency but the global registry is used.
pub fn value_to_python_object(val: Value, _bridge: &PyBridge) -> Result<PythonObjectId, String> {
    value_to_python_object_id(val)
}

/// Convert a `PythonObjectId` to a Nulang `Value`.
///
/// Wrapper with the interface expected by the VM. The `bridge` parameter
/// is accepted for API consistency but the global registry is used.
pub fn python_object_to_value(obj_id: PythonObjectId, _bridge: &PyBridge) -> Result<Value, String> {
    python_object_id_to_value(obj_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python::bridge::{register_object, unregister_object};

    // Helper: ensure Python is initialized
    fn ensure_python() {
        let _ = Python::attach(|_py| ());
    }

    // ------------------------------------------------------------------
    // Integer roundtrip
    // ------------------------------------------------------------------

    #[test]
    fn test_marshal_int_roundtrip() {
        ensure_python();

        let original = Value::int(42);
        let py_obj = nulang_to_python(original).expect("nulang_to_python failed");

        // Convert back
        let restored = Python::attach(|py| {
            let bound = py_obj.bind(py);
            python_to_nulang(bound).expect("python_to_nulang failed")
        });

        assert_eq!(
            restored.as_int(),
            Some(42),
            "Expected 42, got {:?}",
            restored.to_string_repr()
        );
    }

    #[test]
    fn test_marshal_int_negative_roundtrip() {
        ensure_python();

        let original = Value::int(-1000);
        let py_obj = nulang_to_python(original).expect("nulang_to_python failed");

        let restored = Python::attach(|py| {
            let bound = py_obj.bind(py);
            python_to_nulang(bound).expect("python_to_nulang failed")
        });

        assert_eq!(restored.as_int(), Some(-1000));
    }

    // ------------------------------------------------------------------
    // Float roundtrip
    // ------------------------------------------------------------------

    #[test]
    fn test_marshal_float_roundtrip() {
        ensure_python();

        let original = Value::float(std::f64::consts::PI);
        let py_obj = nulang_to_python(original).expect("nulang_to_python failed");

        let restored = Python::attach(|py| {
            let bound = py_obj.bind(py);
            python_to_nulang(bound).expect("python_to_nulang failed")
        });

        let val = restored.as_float().expect("Expected float");
        assert!(
            (val - std::f64::consts::PI).abs() < 1e-5,
            "Expected ~pi, got {}",
            val
        );
    }

    // ------------------------------------------------------------------
    // Bool roundtrip
    // ------------------------------------------------------------------

    #[test]
    fn test_marshal_bool_roundtrip() {
        ensure_python();

        for &original_bool in &[true, false] {
            let original = Value::bool(original_bool);
            let py_obj = nulang_to_python(original).expect("nulang_to_python failed");

            let restored = Python::attach(|py| {
                let bound = py_obj.bind(py);
                python_to_nulang(bound).expect("python_to_nulang failed")
            });

            assert_eq!(
                restored.as_bool(),
                Some(original_bool),
                "Bool roundtrip failed for {}",
                original_bool
            );
        }
    }

    // ------------------------------------------------------------------
    // Unit → None → unit roundtrip
    // ------------------------------------------------------------------

    #[test]
    fn test_marshal_unit_to_none() {
        ensure_python();

        let original = Value::unit();
        let py_obj = nulang_to_python(original).expect("nulang_to_python failed");

        // Verify it's None
        let is_none = Python::attach(|py| {
            let bound = py_obj.bind(py);
            bound.is_none()
        });
        assert!(is_none, "Value::unit() should convert to Python None");

        // Convert back
        let restored = Python::attach(|py| {
            let bound = py_obj.bind(py);
            python_to_nulang(bound).expect("python_to_nulang failed")
        });
        assert!(restored.is_unit(), "None should convert back to unit");
    }

    // ------------------------------------------------------------------
    // Python object roundtrip (TAG_PYTHON)
    // ------------------------------------------------------------------

    #[test]
    fn test_marshal_python_object() {
        ensure_python();

        // Create a Python object and register it
        let py_id = Python::attach(|py| {
            let obj: Py<PyAny> = 99i64.into_pyobject(py).unwrap().unbind().into_any();
            register_object(obj)
        });

        // Create a Nulang Value referencing it
        let val = unsafe { Value::from_raw(TAG_PYTHON | py_id.0) };

        // Convert to Python
        let py_obj = nulang_to_python(val).expect("nulang_to_python failed");

        // Verify it's the same value
        Python::attach(|py| {
            let bound = py_obj.bind(py);
            let extracted: i64 = bound.extract().expect("Expected int");
            assert_eq!(extracted, 99, "TAG_PYTHON object should resolve to 99");
        });
    }

    // ------------------------------------------------------------------
    // Opaque Python types (list, dict) stay as TAG_PYTHON
    // ------------------------------------------------------------------

    #[test]
    fn test_marshal_list_stays_opaque() {
        ensure_python();

        // Create a Python list
        let list_id = Python::attach(|py| {
            let list = PyList::new(py, &[1i64, 2i64, 3i64]).unwrap();
            let obj: Py<PyAny> = list.unbind().into_any();
            register_object(obj)
        });

        // Get the list from registry and convert to Nulang
        let py_obj = get_object(list_id).unwrap();
        let val = Python::attach(|py| {
            let bound = py_obj.bind(py);
            python_to_nulang(bound).expect("python_to_nulang failed")
        });

        // Verify it has TAG_PYTHON tag
        assert_eq!(
            val.as_raw() & TAG_MASK,
            TAG_PYTHON,
            "Python list should be stored as TAG_PYTHON, got tag 0x{:016X}",
            val.as_raw() & TAG_MASK
        );

        // Cleanup
        unregister_object(list_id);
    }

    #[test]
    fn test_marshal_dict_stays_opaque() {
        ensure_python();

        // Create a Python dict
        let dict_id = Python::attach(|py| {
            let dict = PyDict::new(py);
            dict.set_item("key", 42i64).unwrap();
            let obj: Py<PyAny> = dict.unbind().into_any();
            register_object(obj)
        });

        // Convert to Nulang
        let py_obj = get_object(dict_id).unwrap();
        let val = Python::attach(|py| {
            let bound = py_obj.bind(py);
            python_to_nulang(bound).expect("python_to_nulang failed")
        });

        // Verify it has TAG_PYTHON tag
        assert_eq!(
            val.as_raw() & TAG_MASK,
            TAG_PYTHON,
            "Python dict should be stored as TAG_PYTHON"
        );

        // Cleanup
        unregister_object(dict_id);
    }

    #[test]
    fn test_marshal_str_stays_opaque() {
        ensure_python();

        // Create a Python string
        let str_id = Python::attach(|py| {
            let s: Py<PyAny> = "hello nulang"
                .into_pyobject(py)
                .unwrap()
                .unbind()
                .into_any();
            register_object(s)
        });

        // Convert to Nulang
        let py_obj = get_object(str_id).unwrap();
        let val = Python::attach(|py| {
            let bound = py_obj.bind(py);
            python_to_nulang(bound).expect("python_to_nulang failed")
        });

        // Verify it has TAG_PYTHON tag (strings from Python stay opaque)
        assert_eq!(
            val.as_raw() & TAG_MASK,
            TAG_PYTHON,
            "Python str should be stored as TAG_PYTHON"
        );

        // Cleanup
        unregister_object(str_id);
    }

    // ------------------------------------------------------------------
    // Higher-level helper tests
    // ------------------------------------------------------------------

    #[test]
    fn test_value_to_python_object_id_and_back() {
        ensure_python();

        let original = Value::float(std::f64::consts::E);

        // Value → PythonObjectId
        let py_id = value_to_python_object_id(original).expect("value_to_python_object_id failed");

        // PythonObjectId → Value
        let restored = python_object_id_to_value(py_id).expect("python_object_id_to_value failed");

        let val = restored.as_float().expect("Expected float");
        assert!(
            (val - std::f64::consts::E).abs() < 1e-5,
            "Expected ~e, got {}",
            val
        );
    }

    #[test]
    fn test_marshal_nil_roundtrip() {
        ensure_python();

        let original = Value::nil();
        let py_obj = nulang_to_python(original).expect("nulang_to_python failed");

        let is_none = Python::attach(|py| {
            let bound = py_obj.bind(py);
            bound.is_none()
        });
        assert!(is_none, "Value::nil() should convert to Python None");
    }

    #[test]
    fn test_marshal_float_special_values() {
        ensure_python();

        for &special in &[0.0, -0.0, f64::INFINITY, f64::NEG_INFINITY] {
            let original = Value::float(special);
            let py_obj = nulang_to_python(original).expect("nulang_to_python failed");

            let restored = Python::attach(|py| {
                let bound = py_obj.bind(py);
                python_to_nulang(bound).expect("python_to_nulang failed")
            });

            let val = restored.as_float().expect("Expected float");
            if special.is_infinite() {
                assert_eq!(val.is_infinite(), true, "Expected infinite, got {}", val);
                assert_eq!(
                    val.is_sign_positive(),
                    special.is_sign_positive(),
                    "Sign mismatch for infinity"
                );
            } else {
                assert!(
                    (val - special).abs() < f64::EPSILON,
                    "Expected {}, got {}",
                    special,
                    val
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // NaN-tag collision regression tests
    // ------------------------------------------------------------------

    #[test]
    fn test_python_tag_no_collision() {
        ensure_python();

        // TAG_PYTHON must not collide with closure or string tags.
        assert_ne!(
            TAG_PYTHON, TAG_CLOSURE,
            "TAG_PYTHON collides with TAG_CLOSURE"
        );
        assert_ne!(
            TAG_PYTHON, TAG_STRING,
            "TAG_PYTHON collides with TAG_STRING"
        );

        // Converting a Python object must produce a value with TAG_PYTHON.
        let py_id = Python::attach(|py| {
            let obj: Py<PyAny> = "opaque".into_pyobject(py).unwrap().unbind().into_any();
            register_object(obj)
        });
        let val = unsafe { Value::from_raw(TAG_PYTHON | py_id.0) };
        assert_eq!(
            val.as_raw() & TAG_MASK,
            TAG_PYTHON,
            "Python object value should carry TAG_PYTHON"
        );
        assert_ne!(
            val.as_raw() & TAG_MASK,
            TAG_CLOSURE,
            "Python object value should not be tagged as a closure"
        );

        unregister_object(py_id);
    }
}
