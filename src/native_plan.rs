//! Native representation planning for typed MIR functions.
//!
//! This module sits between frontend type knowledge and native backends. It
//! describes which values are eligible for raw machine representation and why
//! a function must retain a boxed ABI/body. Backends may become more capable
//! over time without changing this semantic boundary contract.

use crate::ast::BinOp;
use crate::mir;
use crate::native_abi::{NativeBoundary, NativeValueRepr};

/// A reason the current native backend must retain tagged values for a
/// function instead of selecting the integer-only raw fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativePlanConstraint {
    RuntimeBoundary(NativeBoundary),
    TaggedHeapValue,
    TaggedRuntimeValue,
    CapturedClosure,
    DynamicCall,
    CrossFunctionCall,
    NullableArithmetic,
}

/// A closure-valued local is a proven direct target only if its sole
/// definition is a capture-free closure and that definition precedes the call
/// in the same basic block. Cross-block use needs dominance analysis.
pub(crate) fn stable_direct_closure_targets(
    func: &mir::Function,
) -> std::collections::HashMap<mir::LocalId, (usize, usize, usize)> {
    use std::collections::{HashMap, HashSet};

    let mut seen: HashSet<mir::LocalId> = func
        .params
        .iter()
        .chain(func.captures.iter())
        .copied()
        .collect();
    let mut targets = HashMap::new();
    for (block_idx, block) in func.blocks.iter().enumerate() {
        for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
            let mir::Stmt::Assign { dst, op } = stmt else {
                continue;
            };
            if !seen.insert(*dst) {
                // A reassignment, including to an argument, invalidates the
                // proof even when both writes happen to name the same target.
                targets.remove(dst);
                continue;
            }
            if let mir::RValue::Closure {
                func: target,
                captures,
            } = op
            {
                if captures.is_empty() {
                    targets.insert(*dst, (block_idx, stmt_idx, *target));
                }
            }
        }
    }
    targets
}

/// Resolve a call only when a single capture-free closure definition
/// dominates it trivially (earlier in the same basic block).
pub(crate) fn stable_direct_closure_call(
    targets: &std::collections::HashMap<mir::LocalId, (usize, usize, usize)>,
    local: mir::LocalId,
    block_idx: usize,
    stmt_idx: usize,
) -> Option<usize> {
    targets.get(&local).and_then(|&(defined_block, defined_stmt, target)| {
        (defined_block == block_idx && defined_stmt < stmt_idx).then_some(target)
    })
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
    /// The plan distinguishes *representation knowledge* from *backend
    /// eligibility*. A Float local is recorded as `F64` even though today's
    /// AOT whole-function fast path only accepts raw Int parameters/results.
    /// Runtime boundaries and operations that can materialize tagged/nil/heap
    /// values are recorded as constraints instead of erasing type knowledge.
    pub fn for_function(func: &mir::Function) -> Self {
        let locals: Vec<NativeValueRepr> = func
            .locals
            .iter()
            .map(|local| NativeBoundary::Internal.representation_for(&local.ty))
            .collect();

        let repr_for_local = |id: mir::LocalId| {
            locals
                .get(id.0 as usize)
                .copied()
                .unwrap_or(NativeValueRepr::Tagged)
        };

        let params = func.params.iter().copied().map(repr_for_local).collect();
        let captures = func.captures.iter().copied().map(repr_for_local).collect();
        let ret = func
            .ret
            .as_ref()
            .map(|ty| NativeBoundary::Internal.representation_for(ty));

        let mut constraints = Vec::new();
        let mut push_constraint = |constraint: NativePlanConstraint| {
            if !constraints.contains(&constraint) {
                constraints.push(constraint);
            }
        };

        if !func.captures.is_empty() {
            push_constraint(NativePlanConstraint::CapturedClosure);
        }

        // Analyze single-assignment closure locals once. A local whose
        // definition may not dominate its call must remain dynamically boxed.
        let direct_closure_targets = stable_direct_closure_targets(func);

        for (block_idx, block) in func.blocks.iter().enumerate() {
            for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
                match stmt {
                    mir::Stmt::Assign { op, .. } => match op {
                        mir::RValue::Binary(BinOp::Div | BinOp::Mod | BinOp::Pow, ..) => {
                            push_constraint(NativePlanConstraint::NullableArithmetic);
                        }
                        mir::RValue::ArrayLit(_)
                        | mir::RValue::ArrayLoad { .. }
                        | mir::RValue::ArrayLen(_)
                        | mir::RValue::Record(_)
                        | mir::RValue::Tuple(_)
                        | mir::RValue::RecordUpdate { .. }
                        | mir::RValue::LoadFieldNamed { .. }
                        | mir::RValue::LoadFieldPos { .. } => {
                            push_constraint(NativePlanConstraint::TaggedHeapValue);
                        }
                        mir::RValue::FFICall { .. } => {
                            push_constraint(NativePlanConstraint::RuntimeBoundary(
                                NativeBoundary::Ffi,
                            ));
                        }
                        mir::RValue::Perform { .. } | mir::RValue::PerformAsync { .. } => {
                            push_constraint(NativePlanConstraint::RuntimeBoundary(
                                NativeBoundary::EffectRuntime,
                            ));
                        }
                        mir::RValue::SignalWait { .. } => {
                            push_constraint(NativePlanConstraint::RuntimeBoundary(
                                NativeBoundary::DurableRuntime,
                            ));
                        }
                        mir::RValue::Spawn { .. }
                        | mir::RValue::Send { .. }
                        | mir::RValue::Ask { .. }
                        | mir::RValue::Receive
                        | mir::RValue::ReceiveMatch { .. }
                        | mir::RValue::ReceiveWait { .. }
                        | mir::RValue::ReceiveCommit
                        | mir::RValue::Migrate { .. }
                        | mir::RValue::SelfRef
                        | mir::RValue::StateGet { .. } => {
                            push_constraint(NativePlanConstraint::RuntimeBoundary(
                                NativeBoundary::ActorRuntime,
                            ));
                        }
                        mir::RValue::Resume(_) => {
                            push_constraint(NativePlanConstraint::RuntimeBoundary(
                                NativeBoundary::EffectRuntime,
                            ));
                        }
                        mir::RValue::CapabilityCheck { .. } => {
                            push_constraint(NativePlanConstraint::TaggedRuntimeValue);
                        }
                        mir::RValue::Closure { captures, .. } if !captures.is_empty() => {
                            push_constraint(NativePlanConstraint::CapturedClosure);
                        }
                        mir::RValue::Call { func: target, .. } => {
                            push_constraint(NativePlanConstraint::CrossFunctionCall);
                            if let mir::FuncRef::Local(local) = target {
                                if stable_direct_closure_call(
                                    &direct_closure_targets,
                                    *local,
                                    block_idx,
                                    stmt_idx,
                                )
                                .is_none()
                                {
                                    push_constraint(NativePlanConstraint::DynamicCall);
                                }
                            }
                        }
                        _ => {}
                    },
                    mir::Stmt::StoreFieldNamed { .. } | mir::Stmt::ArrayStore { .. } => {
                        push_constraint(NativePlanConstraint::TaggedHeapValue);
                    }
                    mir::Stmt::EnterHandle { .. } | mir::Stmt::PopHandler => {
                        push_constraint(NativePlanConstraint::RuntimeBoundary(
                            NativeBoundary::EffectRuntime,
                        ));
                    }
                    mir::Stmt::Emit { .. } => {
                        push_constraint(NativePlanConstraint::RuntimeBoundary(
                            NativeBoundary::DurableRuntime,
                        ));
                    }
                    mir::Stmt::StateSet { .. } => {
                        push_constraint(NativePlanConstraint::RuntimeBoundary(
                            NativeBoundary::ActorRuntime,
                        ));
                    }
                    mir::Stmt::ParallelMarker { .. } => {}
                }
            }

            if matches!(block.terminator, mir::Terminator::Resume(_)) {
                push_constraint(NativePlanConstraint::RuntimeBoundary(
                    NativeBoundary::EffectRuntime,
                ));
            }
        }

        Self {
            params,
            captures,
            locals,
            ret,
            constraints,
        }
    }

    /// Whether today's AOT integer fast path can safely use raw i64 arguments
    /// and results for this function in isolation.
    ///
    /// The current boxed entry wrapper always tags the raw return as Int, so a
    /// unit/void function cannot use this path: its native body returns tagged
    /// nil, which the wrapper would otherwise retag as integer zero.
    ///
    /// A module-level ABI planner must additionally prove every call edge is
    /// raw-compatible before removing `CrossFunctionCall`; until then direct
    /// inter-function calls stay boxed rather than passing raw integers into a
    /// tagged callee by accident.
    pub fn supports_unboxed_int_path(&self) -> bool {
        self.constraints.is_empty()
            && self.captures.is_empty()
            && self.params.iter().all(|repr| *repr == NativeValueRepr::I64)
            && self.ret == Some(NativeValueRepr::I64)
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

        assert_eq!(
            plan.params,
            vec![NativeValueRepr::I64, NativeValueRepr::I64]
        );
        assert_eq!(plan.ret, Some(NativeValueRepr::I64));
        assert!(plan.constraints.is_empty());
        assert!(plan.supports_unboxed_int_path());
    }

    #[test]
    fn unit_return_is_not_supported_by_current_int_wrapper() {
        let mut builder = FunctionBuilder::new("unit", None);
        builder.add_param("x", Type::int());
        builder.terminate(Terminator::Return(None));
        let plan = NativeFunctionPlan::for_function(&builder.build());

        assert_eq!(plan.ret, None);
        assert!(plan.constraints.is_empty());
        assert!(!plan.supports_unboxed_int_path());
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

        assert!(plan
            .constraints
            .contains(&NativePlanConstraint::RuntimeBoundary(NativeBoundary::Ffi)));
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
        assert!(effect_plan
            .constraints
            .contains(&NativePlanConstraint::RuntimeBoundary(
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
        assert!(actor_plan
            .constraints
            .contains(&NativePlanConstraint::RuntimeBoundary(
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
        assert!(plan
            .constraints
            .contains(&NativePlanConstraint::DynamicCall));
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

        assert!(plan
            .constraints
            .contains(&NativePlanConstraint::RuntimeBoundary(
                NativeBoundary::ActorRuntime
            )));
    }
}
