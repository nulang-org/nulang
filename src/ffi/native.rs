//! Native function and library registry.
//!
//! Provides a thread-safe registry of dynamically loaded libraries and
//! resolved symbols. Symbols are keyed by `(library_name, symbol_name)` so the
//! same symbol can be bound independently by different embedding runtimes.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use super::marshal::Signature;

/// A loaded dynamic library.
pub struct NativeLibrary {
    #[cfg(feature = "ffi")]
    inner: libloading::Library,
    name: String,
}

impl NativeLibrary {
    /// Open a dynamic library by path.
    ///
    /// # Safety
    /// The caller must ensure the path points to a valid shared library.
    #[cfg(feature = "ffi")]
    pub unsafe fn open(path: &str) -> Result<Self, String> {
        let inner = unsafe { libloading::Library::new(path) }.map_err(|e| e.to_string())?;
        Ok(Self {
            inner,
            name: path.to_string(),
        })
    }

    #[cfg(not(feature = "ffi"))]
    pub unsafe fn open(path: &str) -> Result<Self, String> {
        Err(format!(
            "FFI dynamic library loading disabled (feature 'ffi' not enabled): {}",
            path
        ))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Resolve a symbol from this library as an opaque function pointer.
    ///
    /// # Safety
    /// The caller must ensure the symbol actually has the requested type.
    #[cfg(feature = "ffi")]
    pub unsafe fn resolve<T>(&self, symbol: &[u8]) -> Result<*const c_void, String> {
        self.inner
            .get::<T>(symbol)
            .map(|s| unsafe { s.try_as_raw_ptr() }.unwrap_or(std::ptr::null_mut()) as *const c_void)
            .map_err(|e| {
                format!(
                    "failed to resolve {}: {}",
                    String::from_utf8_lossy(symbol),
                    e
                )
            })
    }

    #[cfg(not(feature = "ffi"))]
    pub unsafe fn resolve<T>(&self, _symbol: &[u8]) -> Result<*const c_void, String> {
        Err("FFI dynamic library loading disabled (feature 'ffi' not enabled)".to_string())
    }
}

impl std::fmt::Debug for NativeLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLibrary")
            .field("name", &self.name)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct NativeFunction {
    pub ptr: *const c_void,
    pub signature: Signature,
    pub library: Option<String>,
    pub symbol: String,
}

unsafe impl Send for NativeFunction {}
unsafe impl Sync for NativeFunction {}

impl NativeFunction {
    /// # Safety
    /// `ptr` must point to a function whose ABI matches `signature`.
    pub unsafe fn new(
        ptr: *const c_void,
        signature: Signature,
        library: Option<String>,
        symbol: String,
    ) -> Self {
        Self {
            ptr,
            signature,
            library,
            symbol,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub enum FfiPolicy {
    #[default]
    AllowAll,
    Allowlist(HashSet<String>),
}

#[derive(Debug, Default)]
pub struct FfiRegistry {
    functions: HashMap<(Option<String>, String), NativeFunction>,
    libraries: HashMap<String, NativeLibrary>,
    policy: FfiPolicy,
}

impl FfiRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_policy(&mut self, policy: FfiPolicy) {
        self.policy = policy;
    }

    pub fn is_lib_allowed(&self, path: &str) -> bool {
        match &self.policy {
            FfiPolicy::AllowAll => true,
            FfiPolicy::Allowlist(allowed) => allowed.contains(path),
        }
    }

    /// Load a dynamic library and keep it open for symbol resolution.
    ///
    /// # Safety
    /// The caller must ensure `path` points to a valid shared library.
    pub unsafe fn load_library(&mut self, path: &str) -> Result<NativeLibrary, String> {
        if !self.is_lib_allowed(path) {
            return Err(format!("FFI: library '{}' not in allowlist", path));
        }
        if self.libraries.contains_key(path) {
            return unsafe { NativeLibrary::open(path) };
        }
        let stored = unsafe { NativeLibrary::open(path)? };
        self.libraries.insert(path.to_string(), stored);
        unsafe { NativeLibrary::open(path) }
    }

    pub fn resolve(&self, library: Option<&str>, symbol: &str) -> Option<NativeFunction> {
        self.functions
            .get(&(library.map(String::from), symbol.to_string()))
            .cloned()
    }

    pub fn register(&mut self, function: NativeFunction) {
        let key = (function.library.clone(), function.symbol.clone());
        self.functions.insert(key, function);
    }

    /// Remove all pre-registered functions associated with one embedding
    /// namespace. Dynamic libraries are intentionally unaffected: runtime
    /// namespaces are synthetic names used only for host callbacks.
    pub fn unregister_namespace(&mut self, namespace: &str) -> usize {
        let before = self.functions.len();
        self.functions
            .retain(|(library, _), _| library.as_deref() != Some(namespace));
        before - self.functions.len()
    }

    /// Resolve a native function, loading its library on demand if necessary.
    /// Exact `(library, symbol)` registrations win over the process-global
    /// `(None, symbol)` compatibility registration.
    ///
    /// # Safety
    /// `library` must name a valid shared library when the function is not
    /// pre-registered.
    pub unsafe fn resolve_or_load(
        &mut self,
        library: &str,
        symbol: &str,
        signature: Signature,
    ) -> Result<NativeFunction, String> {
        if let Some(func) = self.resolve(Some(library), symbol) {
            return Ok(func);
        }
        if let Some(func) = self.resolve(None, symbol) {
            return Ok(func);
        }
        let lib = self.load_library(library)?;
        let symbol_name = symbol.to_string();
        let ptr = unsafe { lib.resolve::<unsafe extern "C" fn()>(symbol.as_bytes())? };
        let func = unsafe {
            NativeFunction::new(ptr, signature, Some(library.to_string()), symbol_name)
        };
        self.register(func.clone());
        Ok(func)
    }
}

/// Process-wide backing registry. Runtime-scoped embedding registrations are
/// stored under private synthetic library namespaces, so the existing VM/JIT
/// resolver path can preserve backend-invariant lookup semantics.
pub static FFI_REGISTRY: OnceLock<Mutex<FfiRegistry>> = OnceLock::new();

fn global_registry() -> &'static Mutex<FfiRegistry> {
    FFI_REGISTRY.get_or_init(|| Mutex::new(FfiRegistry::new()))
}

/// Register a legacy process-global native function.
///
/// # Safety
/// `ptr` must point to a function whose C ABI matches `signature`.
pub unsafe fn register_native_function(
    name: &str,
    ptr: *const c_void,
    signature: Signature,
) -> Result<(), String> {
    let func = unsafe { NativeFunction::new(ptr, signature, None, name.to_string()) };
    let mut reg = global_registry().lock().map_err(|e| e.to_string())?;
    reg.register(func);
    Ok(())
}

/// Register a native function under one private embedding-runtime namespace.
/// Exact namespaced lookup wins over the legacy process-global fallback.
///
/// # Safety
/// `ptr` must point to a function whose C ABI matches `signature` and remain
/// valid for the lifetime of the owning embedding runtime.
pub unsafe fn register_native_function_in_namespace(
    namespace: &str,
    name: &str,
    ptr: *const c_void,
    signature: Signature,
) -> Result<(), String> {
    let func = unsafe {
        NativeFunction::new(
            ptr,
            signature,
            Some(namespace.to_string()),
            name.to_string(),
        )
    };
    let mut reg = global_registry().lock().map_err(|e| e.to_string())?;
    reg.register(func);
    Ok(())
}

/// Remove all callbacks owned by an embedding-runtime namespace.
pub fn unregister_native_namespace(namespace: &str) -> Result<usize, String> {
    let mut reg = global_registry().lock().map_err(|e| e.to_string())?;
    Ok(reg.unregister_namespace(namespace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::marshal::{CType, Signature};

    #[test]
    fn test_ffi_allowlist() {
        let mut reg = FfiRegistry::new();
        let nonexistent = "libnonexistent_does_not_exist.so";
        let err = unsafe { reg.load_library(nonexistent) }.unwrap_err();
        assert!(!err.contains("not in allowlist"));

        let mut allowed = HashSet::new();
        allowed.insert("liballowed.so".to_string());
        reg.set_policy(FfiPolicy::Allowlist(allowed));
        let err_denied = unsafe { reg.load_library(nonexistent) }.unwrap_err();
        assert_eq!(
            err_denied,
            format!("FFI: library '{}' not in allowlist", nonexistent)
        );
    }

    #[test]
    fn test_registry_register_and_lookup() {
        let mut registry = FfiRegistry::new();
        let dummy_ptr = std::ptr::null::<c_void>();
        let func = unsafe {
            NativeFunction::new(
                dummy_ptr,
                Signature::new(vec![], CType::Unit),
                None,
                "test_fn".to_string(),
            )
        };
        registry.register(func);
        assert_eq!(registry.resolve(None, "test_fn").unwrap().symbol, "test_fn");
    }

    #[test]
    fn test_exact_namespace_wins_over_global_fallback() {
        let mut registry = FfiRegistry::new();
        let global_ptr = 0x10usize as *const c_void;
        let local_ptr = 0x20usize as *const c_void;
        let sig = Signature::new(vec![], CType::I64);
        registry.register(unsafe {
            NativeFunction::new(global_ptr, sig.clone(), None, "value".to_string())
        });
        registry.register(unsafe {
            NativeFunction::new(
                local_ptr,
                sig.clone(),
                Some("runtime-a".to_string()),
                "value".to_string(),
            )
        });

        let exact = unsafe { registry.resolve_or_load("runtime-a", "value", sig) }.unwrap();
        assert_eq!(exact.ptr, local_ptr);
    }

    #[test]
    fn test_unregister_namespace_is_scoped() {
        let mut registry = FfiRegistry::new();
        let ptr = std::ptr::null::<c_void>();
        let sig = Signature::new(vec![], CType::Unit);
        for namespace in ["runtime-a", "runtime-b"] {
            registry.register(unsafe {
                NativeFunction::new(
                    ptr,
                    sig.clone(),
                    Some(namespace.to_string()),
                    "same".to_string(),
                )
            });
        }
        assert_eq!(registry.unregister_namespace("runtime-a"), 1);
        assert!(registry.resolve(Some("runtime-a"), "same").is_none());
        assert!(registry.resolve(Some("runtime-b"), "same").is_some());
    }
}
