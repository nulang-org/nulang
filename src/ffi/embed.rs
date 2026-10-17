//! Native Rust embedding of Nulang's typed core runtime.

use std::ffi::CString;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::c_api::{
    nulang_call_function_owned, nulang_compile, nulang_run_owned, NulangRuntime, OwnedHostValue,
};

/// A Rust-owned value copied across the Nulang VM boundary.
///
/// Heap objects, actor references, and closures are intentionally excluded.
#[derive(Debug, Clone, PartialEq)]
pub enum RustValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Nil,
    Unit,
}

impl From<OwnedHostValue> for RustValue {
    fn from(value: OwnedHostValue) -> Self {
        match value {
            OwnedHostValue::Int(n) => Self::Int(n),
            OwnedHostValue::Float(n) => Self::Float(n),
            OwnedHostValue::Bool(n) => Self::Bool(n),
            OwnedHostValue::String(s) => Self::String(s),
            OwnedHostValue::Nil => Self::Nil,
            OwnedHostValue::Unit => Self::Unit,
        }
    }
}

impl From<RustValue> for OwnedHostValue {
    fn from(value: RustValue) -> Self {
        match value {
            RustValue::Int(n) => Self::Int(n),
            RustValue::Float(n) => Self::Float(n),
            RustValue::Bool(n) => Self::Bool(n),
            RustValue::String(s) => Self::String(s),
            RustValue::Nil => Self::Nil,
            RustValue::Unit => Self::Unit,
        }
    }
}

/// An embedding diagnostic without a borrowed pointer or runtime lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedError(pub String);

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EmbedError {}

/// A compiled module bound to the Rust embedding runtime that created it.
///
/// The numeric VM module handle cannot be transferred to another runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RustModule {
    runtime_id: u64,
    module_handle: i64,
}

static NEXT_RUST_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

/// Embed Nulang's typed core VM within an existing Rust application.
///
/// No Rust caller must work with unsafe pointers, raw NaN-tagged VM values,
/// C string lifetimes, or native callback registries. This wrapper runs
/// synchronously on its owning thread and is deliberately !Send + !Sync.
///
/// This is not yet a persistent actor-runtime or durable workflow API.
pub struct RustEmbedder {
    inner: NulangRuntime,
    runtime_id: u64,
    _single_threaded: PhantomData<Rc<()>>,
}

impl RustEmbedder {
    /// Create a new self-contained embedded runtime.
    pub fn new() -> Result<Self, EmbedError> {
        let id = NEXT_RUST_RUNTIME_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| EmbedError("Rust embedding runtime id space exhausted".to_string()))?;
        Ok(Self {
            inner: NulangRuntime::new(),
            runtime_id: id,
            _single_threaded: PhantomData,
        })
    }

    fn ensure_module(&self, module: &RustModule) -> Result<i64, EmbedError> {
        if module.runtime_id != self.runtime_id {
            return Err(EmbedError("module belongs to another runtime".to_string()));
        }
        Ok(module.module_handle)
    }

    fn take_value(&mut self, handle: u64) -> Result<RustValue, EmbedError> {
        if handle == 0 {
            return Err(EmbedError(self.inner.last_error_message()));
        }
        self.inner
            .take_owned(handle)
            .map(RustValue::from)
            .ok_or_else(|| EmbedError("owned value handle is invalid".to_string()))
    }

    /// Compile a source module and retain the associated bytecode for reuse.
    pub fn compile(&mut self, source: &str) -> Result<RustModule, EmbedError> {
        let source = CString::new(source)
            .map_err(|_| EmbedError("source contains an interior NUL byte".to_string()))?;
        // SAFETY: self exclusively owns a live NulangRuntime and CString.
        let handle = unsafe { nulang_compile(&mut self.inner, source.as_ptr()) };
        if handle < 0 {
            return Err(EmbedError(self.inner.last_error_message()));
        }
        Ok(RustModule {
            runtime_id: self.runtime_id,
            module_handle: handle,
        })
    }

    /// Execute the module and return a copied, fully Rust-owned value.
    pub fn run(&mut self, module: &RustModule) -> Result<RustValue, EmbedError> {
        let handle = self.ensure_module(module)?;
        // SAFETY: runtime and module handle originate from this owner.
        let owned = unsafe { nulang_run_owned(&mut self.inner, handle) };
        self.take_value(owned)
    }

    /// Invoke a compiled function with Rust-native typed values.
    ///
    /// Arguments are copied into the owning runtime for the duration of the
    /// invocation, then released even if execution returns an error.
    pub fn call(
        &mut self,
        module: &RustModule,
        function: &str,
        args: &[RustValue],
    ) -> Result<RustValue, EmbedError> {
        let handle = self.ensure_module(module)?;
        let function = CString::new(function)
            .map_err(|_| EmbedError("function name contains an interior NUL byte".to_string()))?;

        let mut handles = Vec::with_capacity(args.len());
        for arg in args {
            if let RustValue::Int(value) = arg {
                if crate::vm::Value::int(*value).as_int() != Some(*value) {
                    for id in handles {
                        let _ = self.inner.take_owned(id);
                    }
                    return Err(EmbedError("Int is outside the Nulang VM range".to_string()));
                }
            }
            let id = self.inner.store_owned(arg.clone().into());
            if id == 0 {
                for id in handles {
                    let _ = self.inner.take_owned(id);
                }
                return Err(EmbedError(self.inner.last_error_message()));
            }
            handles.push(id);
        }
        let arg_ptr = if handles.is_empty() {
            std::ptr::null()
        } else {
            handles.as_ptr()
        };
        // SAFETY: runtime, module, function name and handle buffer are owned
        // by this object and remain live throughout the synchronous call.
        let result = unsafe {
            nulang_call_function_owned(
                &mut self.inner,
                handle,
                function.as_ptr(),
                arg_ptr,
                handles.len(),
            )
        };
        for id in handles {
            let _ = self.inner.take_owned(id);
        }
        self.take_value(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_embedder_compiles_and_runs_typed_results() {
        let mut engine = RustEmbedder::new().unwrap();
        let module = engine
            .compile("fn add(a: Int, b: Int) -> Int { a + b } add(1, 2)")
            .unwrap();
        assert_eq!(engine.run(&module).unwrap(), RustValue::Int(3));
        assert_eq!(
            engine
                .call(&module, "add", &[RustValue::Int(11), RustValue::Int(31)])
                .unwrap(),
            RustValue::Int(42),
        );
    }

    #[test]
    fn rust_embedder_calls_string_functions_without_exposing_nan_boxed_values() {
        let mut engine = RustEmbedder::new().unwrap();
        let module = engine.compile(
            r#"fn greet(name: String) -> String { perform String.concat("hello ", name) } greet("world")"#,
        ).unwrap();
        assert_eq!(
            engine.run(&module).unwrap(),
            RustValue::String("hello world".into())
        );
        assert_eq!(
            engine
                .call(&module, "greet", &[RustValue::String("Nulang".into())])
                .unwrap(),
            RustValue::String("hello Nulang".into()),
        );
    }

    #[test]
    fn rust_embedder_rejects_modules_from_other_runtime_instances() {
        let mut a = RustEmbedder::new().unwrap();
        let module = a.compile("41 + 1").unwrap();
        let mut b = RustEmbedder::new().unwrap();
        assert!(b
            .run(&module)
            .unwrap_err()
            .to_string()
            .contains("another runtime"));
        assert_eq!(a.run(&module).unwrap(), RustValue::Int(42));
    }

    #[test]
    fn rust_embedder_rejects_ints_that_cannot_round_trip_through_the_vm() {
        let mut engine = RustEmbedder::new().unwrap();
        let module = engine.compile("fn id(x: Int) -> Int { x } id(1)").unwrap();
        assert!(engine
            .call(&module, "id", &[RustValue::Int(i64::MAX)])
            .is_err());
    }
}
