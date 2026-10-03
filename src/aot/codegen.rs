//! Thin public wrapper around the stable AOT code generator implementation.
//!
//! The large implementation remains byte-for-byte unchanged in
//! `codegen_impl.rs`. This wrapper centralizes the raw-ABI eligibility gate so
//! representation planning can become stricter without editing machine-code
//! emission at the same time.

#[path = "codegen_impl.rs"]
mod implementation;

pub use implementation::*;

/// Conservative whole-function eligibility for the current raw-Int AOT path.
///
/// Function-local planning intentionally rejects cross-function calls until the
/// module-level ABI planner is wired into AOT dispatch. That prevents raw Int
/// arguments from flowing through the boxed function table and also excludes
/// unit returns from the current Int-only boxing wrapper.
pub fn is_all_int(func: &crate::mir::Function) -> bool {
    crate::native_plan::NativeFunctionPlan::for_function(func).supports_unboxed_int_path()
}
