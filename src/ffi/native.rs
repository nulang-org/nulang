//! Native function and library registry.
//!
//! Provides a thread-safe global registry of dynamically loaded libraries and
//! resolved symbols. Symbols are keyed by `(library_name, symbol_name)` so the
//! same name can be provided by different libraries.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use super::marshal::Signature;

/// A loaded dynamic library.
///
/// When the `ffi` feature is disabled, the library handle is absent and
/// `open`/`resolve` return errors; the registry of *pre-registered* native
/// functions (`register_native_function`) still works without the feature.
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

    /// Open a dynamic library by path.
    ///
    /// # Safety
    /// The caller must ensure the path points to a valid shared library.
    #[cfg(not(feature = "ffi"))]
    pub unsafe fn open(path: &str) -> Result<Self, String> {
        Err(format!(
            "FFI dynamic library loading disabled (feature 'ffi' not enabled): {}",
            path
        ))
    }

    /// Return the path/name used to open this library.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Resolve a symbol from this library as an opaque function pointer.
    ///
    /// The `T` parameter guides `libloading`'s type-checked lookup; the
    /// returned pointer must be transmuted to `T` by the caller.
    ///
    /// # Safety
    /// The caller must ensure the symbol actually has the requested type.
    #[cfg(feature = "ffi")]
    pub unsafe fn resolve<T>(&self, symbol: &[u8]) -> Result<*const c_void, String> {
        self.inner
            .get::<T>(symbol)
            // `try_as_raw_ptr` extracts the raw symbol address without
            // going through `Deref` (which, for function types, would
            // re-derive the pointer from a place expression instead of
            // returning the resolved entry point).
            .map(|s| unsafe { s.try_as_raw_ptr() }.unwrap_or(std::ptr::null_mut()) as *const c_void)
            .map_err(|e| {
                format!(
                    "failed to resolve {}: {}",
                    String::from_utf8_lossy(symbol),
                    e
                )
            })
    }

    /// Resolve a symbol from this library as an opaque function pointer.
    ///
    /// # Safety
    /// The caller must ensure the symbol actually has the requested type.
    #[cfg(not(feature = "ffi"))]
    pub unsafe fn resolve<T>(&self, _symbol: &[u8]) -> Result<*const c_void, String> {
        Err(
            "FFI dynamic library loading disabled (feature 'ffi' not enabled)".to_string(),
        )
    }
}

impl std::fmt::Debug for NativeLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLibrary")
            .field("name", &self.name)
            .finish()
    }
}

/// A native function callable through the FFI layer.
///
/// The function pointer is stored as an opaque `*const c_void` so it can be
/// transmuted to the correct `extern "C"` signature at call time.
#[derive(Debug, Clone)]
pub struct NativeFunction {
    pub ptr: *const c_void,
    pub signature: Signature,
    pub library: Option<String>,
    pub symbol: String,
}

// SAFETY: `*const c_void` is used as an opaque function pointer. The registry
// guarantees that the pointed-to function outlives the registry entry, and all
// access is serialized by the enclosing `Mutex`.
unsafe impl Send for NativeFunction {}
// SAFETY: function pointers are immutable once registered; shared access is
// safe because `call_native` only reads from the pointer.
unsafe impl Sync for NativeFunction {}

impl NativeFunction {
    /// Create a native function entry from a raw C function pointer.
    ///
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

/// Internal registry backing the global `FFI_REGISTRY`.
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
        if let Some(_lib) = self.libraries.get(path) {
            // Library is already open; return a reference-equivalent description.
            return unsafe { NativeLibrary::open(path) };
        }
        let stored = unsafe { NativeLibrary::open(path)? };
        self.libraries.insert(path.to_string(), stored);
        unsafe { NativeLibrary::open(path) }
    }

    /// Resolve a registered native function.
    pub fn resolve(&self, library: Option<&str>, symbol: &str) -> Option<NativeFunction> {
        self.functions
            .get(&(library.map(String::from), symbol.to_string()))
            .cloned()
    }

    /// Register a native function under its symbol (and optional library).
    pub fn register(&mut self, function: NativeFunction) {
        let key = (function.library.clone(), function.symbol.clone());
        self.functions.insert(key, function);
    }

    /// Resolve a native function, loading its library on demand if necessary.
    ///
    /// First tries a pre-registered function under `(Some(library), symbol)`,
    /// then `(None, symbol)`. If neither is found, the library is opened and
    /// the symbol is resolved as an opaque function pointer.
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
        // SAFETY: caller guarantees the symbol exists and has the requested type.
        let ptr = unsafe { lib.resolve::<unsafe extern "C" fn()>(symbol.as_bytes())? };
        let func = NativeFunction::new(ptr, signature, Some(library.to_string()), symbol_name);
        self.register(func.clone());
        Ok(func)
    }
}

/// Global thread-safe FFI registry.
pub static FFI_REGISTRY: OnceLock<Mutex<FfiRegistry>> = OnceLock::new();

fn global_registry() -> &'static Mutex<FfiRegistry> {
    FFI_REGISTRY.get_or_init(|| Mutex::new(FfiRegistry::new()))
}

/// Register a native function in the global registry.
///
/// # Safety
/// `ptr` must point to a function whose C ABI matches `signature`. The
/// function must remain valid for the lifetime of the registry entry.
pub unsafe fn register_native_function(
    name: &str,
    ptr: *const c_void,
    signature: Signature,
) -> Result<(), String> {
    let func = NativeFunction::new(ptr, signature, None, name.to_string());
    let mut reg = global_registry().lock().map_err(|e| e.to_string())?;
    reg.register(func);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_ffi_allowlist() {
        let mut reg = FfiRegistry::new();
        // By default, AllowAll permits any load. We'll use a nonexistent lib to prove
        // it tries to load it (which fails) rather than rejecting by policy.
        let nonexistent = "libnonexistent_does_not_exist.so";

        let err = unsafe { reg.load_library(nonexistent) }.unwrap_err();
        assert!(
            !err.contains("not in allowlist"),
            "Should not be blocked by policy"
        );

        // Now set a strict allowlist
        let mut allowed = HashSet::new();
        allowed.insert("liballowed.so".to_string());
        reg.set_policy(FfiPolicy::Allowlist(allowed));

        // Unallowed library fails by policy
        let err_denied = unsafe { reg.load_library(nonexistent) }.unwrap_err();
        assert_eq!(
            err_denied,
            format!("FFI: library '{}' not in allowlist", nonexistent)
        );

        // Allowed library fails at load time (since it doesn't exist), not by policy
        let err_allowed = unsafe { reg.load_library("liballowed.so") }.unwrap_err();
        assert!(
            !err_allowed.contains("not in allowlist"),
            "Should not be blocked by policy"
        );
    }

    use crate::ffi::marshal::{CType, Signature};
    use std::ffi::c_void;

    #[test]
    fn test_ffi_registry_new() {
        let registry = FfiRegistry::new();
        assert!(registry.functions.is_empty());
        assert!(registry.libraries.is_empty());
    }

    #[test]
    fn test_registry_register_and_lookup() {
        let mut registry = FfiRegistry::new();
        let dummy_ptr = std::ptr::null::<c_void>();
        // SAFETY: null pointer is never called.
        let func = unsafe {
            NativeFunction::new(
                dummy_ptr,
                Signature::new(vec![], CType::Unit),
                None,
                "test_fn".to_string(),
            )
        };
        registry.register(func);
        let found = registry.resolve(None, "test_fn");
        assert!(found.is_some());
        assert_eq!(found.unwrap().symbol, "test_fn");
    }

    #[test]
    fn test_registry_list() {
        let mut registry = FfiRegistry::new();
        let dummy_ptr = std::ptr::null::<c_void>();
        // SAFETY: null pointers are never called.
        let func1 = unsafe {
            NativeFunction::new(
                dummy_ptr,
                Signature::new(vec![], CType::Unit),
                None,
                "fn_a".to_string(),
            )
        };
        let func2 = unsafe {
            NativeFunction::new(
                dummy_ptr,
                Signature::new(vec![], CType::Unit),
                None,
                "fn_b".to_string(),
            )
        };
        registry.register(func1);
        registry.register(func2);
        assert_eq!(registry.functions.len(), 2);
        let names: Vec<&str> = registry
            .functions
            .values()
            .map(|f| f.symbol.as_str())
            .collect();
        assert!(names.contains(&"fn_a"));
        assert!(names.contains(&"fn_b"));
    }

    #[test]
    fn test_registry_duplicate_name() {
        let mut registry = FfiRegistry::new();
        let dummy_ptr = std::ptr::null::<c_void>();
        // SAFETY: null pointers are never called.
        let func1 = unsafe {
            NativeFunction::new(
                dummy_ptr,
                Signature::new(vec![], CType::Unit),
                None,
                "dup".to_string(),
            )
        };
        let func2 = unsafe {
            NativeFunction::new(
                dummy_ptr,
                Signature::new(vec![], CType::Unit),
                None,
                "dup".to_string(),
            )
        };
        registry.register(func1);
        // Second registration with the same name does not panic (HashMap overwrite).
        registry.register(func2);
        assert_eq!(registry.functions.len(), 1);
        let found = registry.resolve(None, "dup");
        assert!(found.is_some());
        assert_eq!(found.unwrap().symbol, "dup");
    }
}
