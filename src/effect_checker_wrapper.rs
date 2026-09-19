//! Public effect-checker facade.
//!
//! The implementation remains in `effect_checker.rs`; this facade preserves
//! the existing public API while adding RFC 0008 migration-purity validation
//! to the module-level check entry point.

use std::ops::{Deref, DerefMut};

use crate::ast::Decl;
use crate::types::NuResult;

pub use crate::effect_checker_impl::{
    effect_resource_category, effect_row_diff, effect_row_subset, effect_row_union, flatten_decls,
    is_single_shot, parse_effect_name, CapContext, CapabilityAnalyzer, EffectContext,
};

/// Backward-compatible facade over the existing effect checker.
///
/// All existing methods/fields remain reachable through `Deref`; `check_module`
/// is intentionally overridden so migration purity is enforced everywhere that
/// already uses the public `EffectChecker` entry point (CLI, REPL, LSP, DAP,
/// FFI and tests).
pub struct EffectChecker {
    inner: crate::effect_checker_impl::EffectChecker,
}

impl EffectChecker {
    pub fn new() -> Self {
        Self {
            inner: crate::effect_checker_impl::EffectChecker::new(),
        }
    }

    pub fn check_module(&mut self, decls: &[Decl]) -> NuResult<()> {
        self.inner.check_module(decls)?;
        crate::migration_purity::check_module(decls)
    }
}

impl Deref for EffectChecker {
    type Target = crate::effect_checker_impl::EffectChecker;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for EffectChecker {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Default for EffectChecker {
    fn default() -> Self {
        Self::new()
    }
}