//! Thin public wrapper around the stable AOT code generator implementation.
//!
//! The large implementation remains byte-for-byte unchanged in
//! `codegen_impl.rs`. This wrapper centralizes the raw-ABI eligibility gate so
//! representation planning can become stricter without editing machine-code
//! emission at the same time.

#[path = "codegen_impl.rs"]
mod implementation;

pub use implementation::{
    compile_actor_entry_wrapper, compile_boxing_wrapper, compile_mir_function_body,
    AotCompileError, AotContext, AotResult, CompileMode,
};

/// Conservative whole-function eligibility for the current raw-Int AOT path.
///
/// Function-local planning intentionally rejects cross-function calls until the
/// module-level ABI planner is wired into AOT dispatch. The current boxing
/// wrapper also assumes an Int return, so unit/void functions must stay boxed.
pub fn is_all_int(func: &crate::mir::Function) -> bool {
    let returns_int = matches!(
        func.ret.as_ref(),
        Some(crate::types::Type::Primitive(
            crate::types::PrimitiveType::Int
        ))
    );
    returns_int
        && crate::native_plan::NativeFunctionPlan::for_function(func).supports_unboxed_int_path()
}

#[cfg(test)]
mod representation_gate_tests {
    use super::is_all_int;
    use crate::mir::{FuncRef, FunctionBuilder, RValue, Terminator};
    use crate::types::Type;

    #[test]
    fn pure_integer_leaf_remains_unboxed_eligible() {
        let mut builder = FunctionBuilder::new("leaf", Some(Type::int()));
        let x = builder.add_param("x", Type::int());
        builder.terminate(Terminator::Return(Some(x)));
        assert!(is_all_int(&builder.build()));
    }

    #[test]
    fn unit_return_is_not_eligible_for_integer_boxing_wrapper() {
        let mut builder = FunctionBuilder::new("unit", None);
        builder.terminate(Terminator::Return(None));
        assert!(!is_all_int(&builder.build()));
    }

    #[test]
    fn direct_call_stays_boxed_until_module_abi_wiring() {
        let mut builder = FunctionBuilder::new("caller", Some(Type::int()));
        let x = builder.add_param("x", Type::int());
        let out = builder.add_temp(Type::int());
        builder.assign(
            out,
            RValue::Call {
                func: FuncRef::Index(1),
                args: vec![x],
            },
        );
        builder.terminate(Terminator::Return(Some(out)));
        assert!(!is_all_int(&builder.build()));
    }
}
