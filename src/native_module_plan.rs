//! Module-level native ABI compatibility planning.
//!
//! Function-local representation facts are insufficient for raw native calls:
//! the caller and every callee on a direct edge must agree on representation.
//! This module proves those edges before a backend may select an unboxed ABI.

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
    /// function-local constraint that may be discharged here. We then iterate
    /// to a fixed point: every direct callee must itself remain raw-compatible,
    /// and each call site's argument/result representations must exactly match
    /// the callee ABI. This admits recursive SCCs without allowing raw values
    /// to leak into the boxed function table.
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

        let mut unboxed_int_functions: Vec<bool> = functions
            .iter()
            .map(is_local_raw_int_candidate)
            .collect();

        loop {
            let previous = unboxed_int_functions.clone();

            for (caller_idx, sites) in call_sites.iter().enumerate() {
                if !previous[caller_idx] {
                    unboxed_int_functions[caller_idx] = false;
                    continue;
                }

                unboxed_int_functions[caller_idx] = sites.iter().all(|site| {
                    let Some(callee_plan) = functions.get(site.callee) else {
                        return false;
                    };
                    previous.get(site.callee).copied().unwrap_or(false)
                        && site.args == callee_plan.params
                        && callee_plan.ret == Some(site.result)
                        && site.result == NativeValueRepr::I64
                });
            }

            if unboxed_int_functions == previous {
                break;
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
    let mut closure_targets = std::collections::HashMap::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let mir::Stmt::Assign {
                dst,
                op:
                    mir::RValue::Closure {
                        func: target,
                        captures,
                    },
            } = stmt
            {
                if captures.is_empty() {
                    closure_targets.insert(*dst, *target);
                }
            }
        }
    }

    let local_repr = |id: mir::LocalId| {
        plan.locals
            .get(id.0 as usize)
            .copied()
            .unwrap_or(NativeValueRepr::Tagged)
    };

    let mut sites = Vec::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            let mir::Stmt::Assign {
                dst,
                op: mir::RValue::Call { func: target, args },
            } = stmt
            else {
                continue;
            };

            let callee = match target {
                mir::FuncRef::Index(index) => Some(*index),
                mir::FuncRef::Local(local) => closure_targets.get(local).copied(),
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
