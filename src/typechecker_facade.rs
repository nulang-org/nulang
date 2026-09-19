//! Public typechecker facade.
//!
//! The core Algorithm-W implementation remains in `typechecker_base`; this
//! facade enriches statically known actor calls with protocol constraints
//! before delegating to it. Keeping the annotation pass separate makes the
//! actor-protocol work additive and preserves the existing checker for dynamic
//! actor references.

use crate::ast::AstModule;
use crate::types::{NuError, NuResult, Type};
use rustc_hash::FxHashMap;

pub use crate::typechecker_base::{build_class_tables, ClassInfo, ClassTables, Substitution};

pub struct TypeChecker {
    inner: crate::typechecker_base::TypeChecker,
    pub inferred_decl_types: FxHashMap<String, Type>,
    pub collect_errors: bool,
    pub collected_errors: Vec<NuError>,
}

impl TypeChecker {
    pub fn new() -> Self {
        let inner = crate::typechecker_base::TypeChecker::new();
        Self {
            inferred_decl_types: inner.inferred_decl_types.clone(),
            collect_errors: false,
            collected_errors: Vec::new(),
            inner,
        }
    }

    pub fn check_module(&mut self, module: &AstModule) -> NuResult<Type> {
        self.inner.collect_errors = self.collect_errors;
        self.inner.collected_errors.clear();
        self.collected_errors.clear();

        let annotated = match crate::actor_protocol::annotate_module(module) {
            Ok(module) => module,
            Err(err) if self.collect_errors => {
                self.collected_errors.push(err);
                module.clone()
            }
            Err(err) => return Err(err),
        };

        let result = self.inner.check_module(&annotated);
        self.inferred_decl_types = self.inner.inferred_decl_types.clone();
        self.collected_errors
            .extend(std::mem::take(&mut self.inner.collected_errors));
        result
    }

    /// Preserve the public expression-inference entry point for tooling and
    /// tests that operate below the module level. Actor protocol enrichment is
    /// module-scoped, so standalone expressions retain the base behavior.
    pub fn infer_expr(
        &mut self,
        ctx: &crate::types::TypeContext,
        expr: &crate::ast::Expr,
    ) -> NuResult<(Substitution, Type)> {
        self.inner.infer_expr(ctx, expr)
    }

    pub fn register_class_decls(&mut self, module: &AstModule) {
        self.inner.register_class_decls(module);
    }
}

impl Default for TypeChecker {
    fn default() -> Self {
        Self::new()
    }
}