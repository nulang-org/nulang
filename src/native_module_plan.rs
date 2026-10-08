//! Module-level native ABI compatibility planning.
//!
//! Function-local representation facts are insufficient for raw native calls:
//! the caller and every callee on a direct edge must agree on representation.
//! This module proves those edges before a backend may select an unboxed ABI.

use std::collections::VecDeque;

use crate::mir;
use crate::native_abi::NativeValueRepr;
use crate::native_plan::{NativeFunctionPlan, NativePlanConstraint};

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectCallSite {
    callee: usize,
    args: Vec<NativeValueRepr>,
    result: NativeValueRepr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeModulePlan {
    pub functions: Vec<NativeFunctionPlan>,
    unboxed_int_functions: Vec<bool>,
}

impl NativeModulePlan {
    /// Build a module-wide raw-ABI plan.
    ///
    /// A function starts as a candidate only when its own body is compatible
    /// with the current integer fast path. `CrossFunctionCall` is the sole
    /// function-local constraint that may be discharged here. Direct edges are
    /// validated once, then boxed/incompatible callees invalidate their callers
    /// through reverse edges. Eligibility only ever transitions true -> false,
    /// so this reaches the same greatest fixed point in O(functions + calls)
    /// propagation instead of rescanning every function on every round.
    /// Compatible recursive SCCs remain eligible because no member is
    /// invalidated unless an intrinsic or outgoing boundary requires boxing.
    pub fn for_module(module: &mir::Module) -> Self {
        let functions: Vec<NativeFunctionPlan> = module
            .functions
            .iter()
            .map(NativeFunctionPlan::for_function)
            .collect();

        let call_sites: Vec<Vec<DirectCallSite>> = module
            .functions
            .iter()
            .zip(functions.iter())
            .map(|(func, plan)| collect_direct_call_sites(func, plan))
            .collect();

        let mut unboxed_int_functions: Vec<bool> =
            functions.iter().map(is_local_raw_int_candidate).collect();
        let mut reverse_callers = vec![Vec::new(); functions.len()];
        let mut invalid = VecDeque::new();

        // Intrinsically boxed functions are the initial invalidation frontier.
        for (idx, eligible) in unboxed_int_functions.iter().copied().enumerate() {
            if !eligible {
                invalid.push_back(idx);
            }
        }

        // Validate representation compatibility independently of callee
        // eligibility, and build reverse dependency edges for every caller
        // that is still locally eligible. A currently boxed callee will then
        // invalidate those callers through the queue below.
        for (caller_idx, sites) in call_sites.iter().enumerate() {
            if !unboxed_int_functions[caller_idx] {
                continue;
            }

            let compatible = sites.iter().all(|site| {
                let Some(callee_plan) = functions.get(site.callee) else {
                    return false;
                };
                site.args == callee_plan.params
                    && callee_plan.ret == Some(site.result)
                    && site.result == NativeValueRepr::I64
            });

            if !compatible {
                unboxed_int_functions[caller_idx] = false;
                invalid.push_back(caller_idx);
                continue;
            }

            for site in sites {
                reverse_callers[site.callee].push(caller_idx);
            }
        }

        // Eligibility is monotonic. Once a callee is boxed, every otherwise
        // raw-compatible caller must box as well; each reverse edge therefore
        // needs to be processed at most once when its callee becomes invalid.
        while let Some(callee_idx) = invalid.pop_front() {
            for &caller_idx in &reverse_callers[callee_idx] {
                if unboxed_int_functions[caller_idx] {
                    unboxed_int_functions[caller_idx] = false;
                    invalid.push_back(caller_idx);
                }
            }
        }

        Self {
            functions,
            unboxed_int_functions,
        }
    }

    pub fn is_unboxed_int_function(&self, function_index: usize) -> bool {
        self.unboxed_int_functions
            .get(function_index)
            .copied()
            .unwrap_or(false)
    }
}

fn is_local_raw_int_candidate(plan: &NativeFunctionPlan) -> bool {
    plan.captures.is_empty()
        && plan
            .params
            .iter()
            .all(|repr| *repr == NativeValueRepr::I64)
        // The current boxing wrapper always tags the native return as Int.
        // `ret == None` therefore cannot use it: the native body returns the
        // tagged nil sentinel, which the wrapper would incorrectly retag as 0.
        && plan.ret == Some(NativeValueRepr::I64)
        && plan
            .constraints
            .iter()
            .all(|constraint| *constraint == NativePlanConstraint::CrossFunctionCall)
}

fn collect_direct_call_sites(
    func: &mir::Function,
    plan: &NativeFunctionPlan,
) -> Vec<DirectCallSite> {
    let closure_targets = crate::native_plan::stable_direct_closure_targets(func);

    let local_repr = |id: mir::LocalId| {
        plan.locals
            .get(id.0 as usize)
            .copied()
            .unwrap_or(NativeValueRepr::Tagged)
    };

    let mut sites = Vec::new();
    for (block_idx, block) in func.blocks.iter().enumerate() {
        for (stmt_idx, stmt) in block.stmts.iter().enumerate() {
            let mir::Stmt::Assign {
                dst,
                op: mir::RValue::Call { func: target, args },
            } = stmt
            else {
                continue;
            };

            let callee = match target {
                mir::FuncRef::Index(index) => Some(*index),
                mir::FuncRef::Local(local) => crate::native_plan::stable_direct_closure_call(
                    &closure_targets,
                    *local,
                    block_idx,
                    stmt_idx,
                ),
            };

            // Unresolved local calls already carry `DynamicCall`, so they are
            // never local raw candidates. Omitting them here is therefore safe.
            let Some(callee) = callee else {
                continue;
            };

            sites.push(DirectCallSite {
                callee,
                args: args.iter().copied().map(local_repr).collect(),
                result: local_repr(*dst),
            });
        }
    }
    sites
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::BinOp;
    use crate::mir::{FuncRef, FunctionBuilder, Module, RValue, Terminator};
    use crate::types::Type;

    fn int_identity(name: &str) -> mir::Function {
        let mut builder = FunctionBuilder::new(name, Some(Type::int()));
        let x = builder.add_param("x", Type::int());
        builder.terminate(Terminator::Return(Some(x)));
        builder.build()
    }

    fn int_forwarder(name: String, callee: usize) -> mir::Function {
        let mut builder = FunctionBuilder::new(name, Some(Type::int()));
        let x = builder.add_param("x", Type::int());
        let out = builder.add_temp(Type::int());
        builder.assign(
            out,
            RValue::Call {
                func: FuncRef::Index(callee),
                args: vec![x],
            },
        );
        builder.terminate(Terminator::Return(Some(out)));
        builder.build()
    }

    #[test]
    fn rejects_local_closure_reassigned_before_call() {
        let mut caller = FunctionBuilder::new("caller", Some(Type::int()));
        let x = caller.add_param("x", Type::int());
        let closure = caller.add_temp(Type::unit());
        caller.assign(
            closure,
            RValue::Closure {
                func: 1,
                captures: vec![],
            },
        );
        caller.assign(
            closure,
            RValue::Closure {
                func: 2,
                captures: vec![],
            },
        );
        let out = caller.add_temp(Type::int());
        caller.assign(
            out,
            RValue::Call {
                func: FuncRef::Local(closure),
                args: vec![x],
            },
        );
        caller.terminate(Terminator::Return(Some(out)));
        let mut module = Module::new("reassigned");
        module.functions.push(caller.build());
        module.functions.push(int_identity("first"));
        module.functions.push(int_identity("second"));

        let plan = NativeModulePlan::for_module(&module);
        assert!(
            !plan.is_unboxed_int_function(0),
            "a multiply assigned closure local must never qualify for raw ABI calls"
        );
    }

    #[test]
    fn rejects_call_before_local_closure_definition() {
        let mut caller = FunctionBuilder::new("caller", Some(Type::int()));
        let x = caller.add_param("x", Type::int());
        let closure = caller.add_temp(Type::unit());
        let out = caller.add_temp(Type::int());
        caller.assign(
            out,
            RValue::Call {
                func: FuncRef::Local(closure),
                args: vec![x],
            },
        );
        caller.assign(
            closure,
            RValue::Closure {
                func: 1,
                captures: vec![],
            },
        );
        caller.terminate(Terminator::Return(Some(out)));
        let mut module = Module::new("call-before-binding");
        module.functions.push(caller.build());
        module.functions.push(int_identity("leaf"));

        let plan = NativeModulePlan::for_module(&module);
        assert!(
            !plan.is_unboxed_int_function(0),
            "a definition after the call does not prove a static callee"
        );
    }

    #[test]
    fn accepts_single_prior_local_closure_definition_in_same_block() {
        let mut caller = FunctionBuilder::new("caller", Some(Type::int()));
        let x = caller.add_param("x", Type::int());
        let closure = caller.add_temp(Type::unit());
        caller.assign(
            closure,
            RValue::Closure {
                func: 1,
                captures: vec![],
            },
        );
        let out = caller.add_temp(Type::int());
        caller.assign(
            out,
            RValue::Call {
                func: FuncRef::Local(closure),
                args: vec![x],
            },
        );
        caller.terminate(Terminator::Return(Some(out)));
        let mut module = Module::new("stable-direct-closure");
        module.functions.push(caller.build());
        module.functions.push(int_identity("leaf"));

        let plan = NativeModulePlan::for_module(&module);
        assert!(plan.is_unboxed_int_function(0));
        assert!(plan.is_unboxed_int_function(1));
    }

    #[test]
    fn admits_pure_int_leaf() {
        let mut module = Module::new("leaf");
        module.functions.push(int_identity("leaf"));

        let plan = NativeModulePlan::for_module(&module);
        assert!(plan.is_unboxed_int_function(0));
    }

    #[test]
    fn rejects_unit_return_for_integer_boxing_wrapper() {
        let mut builder = FunctionBuilder::new("unit", None);
        builder.add_param("x", Type::int());
        builder.terminate(Terminator::Return(None));

        let mut module = Module::new("unit");
        module.functions.push(builder.build());
        let plan = NativeModulePlan::for_module(&module);

        assert!(!plan.is_unboxed_int_function(0));
    }

    #[test]
    fn admits_type_compatible_direct_int_call_edge() {
        let callee = int_identity("callee");

        let mut caller = FunctionBuilder::new("caller", Some(Type::int()));
        let x = caller.add_param("x", Type::int());
        let out = caller.add_temp(Type::int());
        caller.assign(
            out,
            RValue::Call {
                func: FuncRef::Index(1),
                args: vec![x],
            },
        );
        caller.terminate(Terminator::Return(Some(out)));

        let mut module = Module::new("calls");
        module.functions.push(caller.build());
        module.functions.push(callee);
        let plan = NativeModulePlan::for_module(&module);

        assert!(plan.is_unboxed_int_function(0));
        assert!(plan.is_unboxed_int_function(1));
    }

    #[test]
    fn rejects_call_edge_to_boxed_runtime_function() {
        let mut callee = FunctionBuilder::new("ffi", Some(Type::int()));
        let x = callee.add_param("x", Type::int());
        let out = callee.add_temp(Type::int());
        callee.assign(
            out,
            RValue::FFICall {
                idx: 0,
                args: vec![x],
            },
        );
        callee.terminate(Terminator::Return(Some(out)));

        let mut caller = FunctionBuilder::new("caller", Some(Type::int()));
        let x = caller.add_param("x", Type::int());
        let out = caller.add_temp(Type::int());
        caller.assign(
            out,
            RValue::Call {
                func: FuncRef::Index(1),
                args: vec![x],
            },
        );
        caller.terminate(Terminator::Return(Some(out)));

        let mut module = Module::new("boxed-callee");
        module.functions.push(caller.build());
        module.functions.push(callee.build());
        let plan = NativeModulePlan::for_module(&module);

        assert!(!plan.is_unboxed_int_function(0));
        assert!(!plan.is_unboxed_int_function(1));
    }

    #[test]
    fn boxed_leaf_invalidates_long_direct_call_chain() {
        const DEPTH: usize = 64;
        let mut module = Module::new("long-boxed-chain");
        for idx in 0..DEPTH - 1 {
            module
                .functions
                .push(int_forwarder(format!("f{idx}"), idx + 1));
        }

        let mut leaf = FunctionBuilder::new("ffi_leaf", Some(Type::int()));
        let x = leaf.add_param("x", Type::int());
        let out = leaf.add_temp(Type::int());
        leaf.assign(
            out,
            RValue::FFICall {
                idx: 0,
                args: vec![x],
            },
        );
        leaf.terminate(Terminator::Return(Some(out)));
        module.functions.push(leaf.build());

        let plan = NativeModulePlan::for_module(&module);
        for idx in 0..DEPTH {
            assert!(
                !plan.is_unboxed_int_function(idx),
                "boxed leaf must invalidate transitive caller f{idx}"
            );
        }
    }

    #[test]
    fn admits_mutually_recursive_compatible_int_functions() {
        let mut a = FunctionBuilder::new("a", Some(Type::int()));
        let ax = a.add_param("x", Type::int());
        let aout = a.add_temp(Type::int());
        a.assign(
            aout,
            RValue::Call {
                func: FuncRef::Index(1),
                args: vec![ax],
            },
        );
        a.terminate(Terminator::Return(Some(aout)));

        let mut b = FunctionBuilder::new("b", Some(Type::int()));
        let bx = b.add_param("x", Type::int());
        let bout = b.add_temp(Type::int());
        b.assign(
            bout,
            RValue::Call {
                func: FuncRef::Index(0),
                args: vec![bx],
            },
        );
        b.terminate(Terminator::Return(Some(bout)));

        let mut module = Module::new("recursive");
        module.functions.push(a.build());
        module.functions.push(b.build());
        let plan = NativeModulePlan::for_module(&module);

        assert!(plan.is_unboxed_int_function(0));
        assert!(plan.is_unboxed_int_function(1));
    }

    #[test]
    fn rejects_non_int_call_arguments_even_when_callee_index_exists() {
        let callee = int_identity("callee");

        let mut caller = FunctionBuilder::new("caller", Some(Type::int()));
        let x = caller.add_param("x", Type::float());
        let out = caller.add_temp(Type::int());
        caller.assign(
            out,
            RValue::Call {
                func: FuncRef::Index(1),
                args: vec![x],
            },
        );
        caller.terminate(Terminator::Return(Some(out)));

        let mut module = Module::new("bad-edge");
        module.functions.push(caller.build());
        module.functions.push(callee);
        let plan = NativeModulePlan::for_module(&module);

        assert!(!plan.is_unboxed_int_function(0));
        assert!(plan.is_unboxed_int_function(1));
    }

    #[test]
    fn arithmetic_leaf_remains_eligible() {
        let mut builder = FunctionBuilder::new("add", Some(Type::int()));
        let a = builder.add_param("a", Type::int());
        let b = builder.add_param("b", Type::int());
        let out = builder.add_temp(Type::int());
        builder.assign(out, RValue::Binary(BinOp::Add, a, b));
        builder.terminate(Terminator::Return(Some(out)));

        let mut module = Module::new("math");
        module.functions.push(builder.build());
        let plan = NativeModulePlan::for_module(&module);
        assert!(plan.is_unboxed_int_function(0));
    }
}
