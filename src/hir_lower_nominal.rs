//! Public HIR lowering facade with nominal actor-dispatch preservation.
//!
//! Keep the mature AST -> HIR implementation isolated in `hir_lower.rs` and
//! apply a narrow, semantics-preserving identity bridge to its output. This
//! avoids coupling the large legacy lowering pass to actor protocol identity
//! while ensuring statically known actor sends/asks reach MIR with an exact
//! actor-schema receiver hint.

#[path = "actor_identity_bridge.rs"]
mod actor_identity_bridge;
#[path = "hir_lower.rs"]
mod legacy;

pub use legacy::{lower_body, lower_expr};

pub fn lower_module(
    ast: &crate::ast::AstModule,
    inferred_decl_types: &rustc_hash::FxHashMap<String, crate::types::Type>,
) -> crate::hir::Module {
    let mut module = legacy::lower_module(ast, inferred_decl_types);
    actor_identity_bridge::preserve_nominal_dispatch_identity(&mut module);
    module
}
