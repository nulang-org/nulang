//! Native representation planning for typed MIR functions.
//!
//! This module sits between frontend type knowledge and native backends. It
//! describes which values are eligible for raw machine representation and why
//! a function must retain a boxed ABI/body. Backends may become more capable
//! over time without changing this semantic boundary contract.

use crate::mir;
use crate::native_abi::{NativeBoundary, NativeValueRepr};

/// A reason the current native backend must retain tagged values for a
/// function instead of selecting the integer-only raw fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativePlanConstraint {
    RuntimeBoundary(NativeBoundary),
    TaggedHeapValue,
    CapturedClosure,
    DynamicCall,
    CrossFunctionCall,
    NullableArithmetic,
}

/// Compiler-owned representation plan for one MIR function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeFunctionPlan {
    pub params: Vec<NativeValueRepr>,
    pub captures: Vec<NativeValueRepr>,
    pub locals: Vec<NativeValueRepr>,
    pub ret: Option<NativeValueRepr>,
    pub constraints: Vec<NativePlanConstraint>,
}

impl NativeFunctionPlan {
    /// Build a conservative representation plan from typed MIR.
    ///
    /// RED phase: the real analysis is implemented in the following commit.
    pub fn for_function(func: &mir::Function) -> Self {
        Self {
            params: vec![NativeValueRepr::Tagged; func.params.len()],
            captures: vec![NativeValueRepr::Tagged; func.captures.len()],
            locals: vec![NativeValueRepr::Tagged; func.locals.len()],
            ret: func.ret.as_ref().map(|_| NativeValueRepr::Tagged),
            constraints: vec![NativePlanConstraint::TaggedHeapValue],
        }
    }

    /// Whether today's AOT integer fast path can safely use raw i64 arguments
    /// and results for this function.
    pub fn supports_unboxed_int_path(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::BinOp;
    use crate::mir::{FuncRef, FunctionBuilder, RValue, Stmt, Terminator};
    use crate::types::Type;

    #[test]
    fn plans_plain_int_function_as_unboxed_i64() {
        let mut builder = FunctionBuilder::new("add", Some(Type::int()));
        let a = builder.add_param("a", Type::int());
        let b = builder.add_param("b", Type::int());
        let out = builder.add_temp(Type::int());
        builder.assign(out, RValue::Binary(BinOp::Add, a, b));
        builder.terminate(Terminator::Return(Some(out)));
        let plan = NativeFunctionPlan::for_function(&builder.build());

        assert_eq!(plan.params, vec![NativeValueRepr::I64, NativeValueRepr::I64]);
        assert_eq!(plan.ret, Some(NativeValueRepr::I64));
        assert!(plan.constraints.is_empty());
        assert!(plan.supports_unboxed_int_path());
    }

    #[test]
    fn records_float_representation_without_enabling_integer_fast_path() {
        let mut builder = FunctionBuilder::new("scale", Some(Type::float()));
        let x = builder.add_param("x", Type::float());
        builder.terminate(Terminator::Return(Some(x)));
        let plan = NativeFunctionPlan::for_function(&builder.build());

        assert_eq!(plan.params, vec![NativeValueRepr::F64]);
        assert_eq!(plan.ret, Some(NativeValueRepr::F64));
        assert!(plan.constraints.is_empty());
        assert!(!plan.supports_unboxed_int_path());
    }

    #[test]
    fn ffi_call_forces_boxed_runtime_boundary() {
        let mut builder = FunctionBuilder::new("ffi", Some(Type::int()));
        let x = builder.add_param("x", Type::int());
        let out = builder.add_temp(Type::int());
        builder.assign(
            out,
            RValue::FFICall {
                idx: 0,
                args: vec![x],
            },
        );
        builder.terminate(Terminator::Return(Some(out)));
        let plan = NativeFunctionPlan::for_function(&builder.build());

        assert!(plan.constraints.contains(&NativePlanConstraint::RuntimeBoundary(
            NativeBoundary::Ffi
        )));
        assert!(!plan.supports_unboxed_int_path());
    }

    #[test]
    fn cross_function_call_is_not_raw_until_callee_abi_is_proven() {
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
        let plan = NativeFunctionPlan::for_function(&builder.build());

        assert!(plan
            .constraints
            .contains(&NativePlanConstraint::CrossFunctionCall));
        assert!(!plan.supports_unboxed_int_path());
    }

    #[test]
    fn effect_and_actor_operations_are_explicit_runtime_boundaries() {
        let mut effect = FunctionBuilder::new("effect", Some(Type::int()));
        let x = effect.add_param("x", Type::int());
        let out = effect.add_temp(Type::int());
        effect.assign(
            out,
            RValue::PerformAsync {
                effect_op: "Timer.sleep".into(),
                args: vec![x],
                resolved_handler: None,
            },
        );
        effect.terminate(Terminator::Return(Some(out)));
        let effect_plan = NativeFunctionPlan::for_function(&effect.build());
        assert!(effect_plan.constraints.contains(&NativePlanConstraint::RuntimeBoundary(
            NativeBoundary::EffectRuntime
        )));

        let mut actor = FunctionBuilder::new("actor", Some(Type::int()));
        let actor_ref = actor.add_param("actor", Type::int());
        let arg = actor.add_param("arg", Type::int());
        let out = actor.add_temp(Type::int());
        actor.assign(
            out,
            RValue::Send {
                actor: actor_ref,
                behavior_idx: 0,
                args: vec![arg],
                remote: false,
            },
        );
        actor.terminate(Terminator::Return(Some(out)));
        let actor_plan = NativeFunctionPlan::for_function(&actor.build());
        assert!(actor_plan.constraints.contains(&NativePlanConstraint::RuntimeBoundary(
            NativeBoundary::ActorRuntime
        )));
    }

    #[test]
    fn heap_and_nullable_operations_block_integer_fast_path() {
        let mut heap = FunctionBuilder::new("heap", Some(Type::int()));
        let x = heap.add_param("x", Type::int());
        let arr = heap.add_temp(Type::record(vec![]));
        heap.assign(arr, RValue::ArrayLit(vec![x]));
        heap.terminate(Terminator::Return(Some(x)));
        let heap_plan = NativeFunctionPlan::for_function(&heap.build());
        assert!(heap_plan
            .constraints
            .contains(&NativePlanConstraint::TaggedHeapValue));

        let mut div = FunctionBuilder::new("div", Some(Type::int()));
        let a = div.add_param("a", Type::int());
        let b = div.add_param("b", Type::int());
        let out = div.add_temp(Type::int());
        div.assign(out, RValue::Binary(BinOp::Div, a, b));
        div.terminate(Terminator::Return(Some(out)));
        let div_plan = NativeFunctionPlan::for_function(&div.build());
        assert!(div_plan
            .constraints
            .contains(&NativePlanConstraint::NullableArithmetic));
        assert!(!div_plan.supports_unboxed_int_path());
    }

    #[test]
    fn captured_closure_and_dynamic_call_are_boxed_constraints() {
        let mut builder = FunctionBuilder::new("closure", Some(Type::int()));
        let x = builder.add_param("x", Type::int());
        let closure = builder.add_temp(Type::int());
        builder.assign(
            closure,
            RValue::Closure {
                func: 0,
                captures: vec![x],
            },
        );
        let out = builder.add_temp(Type::int());
        builder.assign(
            out,
            RValue::Call {
                func: FuncRef::Local(closure),
                args: vec![x],
            },
        );
        builder.terminate(Terminator::Return(Some(out)));
        let plan = NativeFunctionPlan::for_function(&builder.build());

        assert!(plan
            .constraints
            .contains(&NativePlanConstraint::CapturedClosure));
        assert!(plan.constraints.contains(&NativePlanConstraint::DynamicCall));
        assert!(!plan.supports_unboxed_int_path());
    }

    #[test]
    fn actor_state_store_is_runtime_boundary() {
        let mut builder = FunctionBuilder::new("state", None);
        let x = builder.add_param("x", Type::int());
        builder.emit(Stmt::StateSet {
            field: "count".into(),
            src: x,
        });
        builder.terminate(Terminator::Return(None));
        let plan = NativeFunctionPlan::for_function(&builder.build());

        assert!(plan.constraints.contains(&NativePlanConstraint::RuntimeBoundary(
            NativeBoundary::ActorRuntime
        )));
    }
}
