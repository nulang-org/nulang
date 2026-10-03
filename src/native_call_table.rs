//! Native call-target table selection for AOT raw-ABI compilation.
//!
//! `NativeModulePlan` proves which MIR functions may use the raw integer ABI.
//! This module turns that proof into the exact function table a raw caller must
//! see during code generation: proven raw callees resolve to their unboxed
//! entry points; every other slot remains the ordinary boxed entry point.

use crate::native_module_plan::NativeModulePlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCallTableError {
    TableLengthMismatch {
        planned: usize,
        boxed: usize,
        unboxed: usize,
    },
    MissingUnboxedTarget {
        caller_index: usize,
        callee_index: usize,
    },
}

impl std::fmt::Display for NativeCallTableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TableLengthMismatch {
                planned,
                boxed,
                unboxed,
            } => write!(
                f,
                "native call table length mismatch: plan={planned}, boxed={boxed}, unboxed={unboxed}"
            ),
            Self::MissingUnboxedTarget {
                caller_index,
                callee_index,
            } => write!(
                f,
                "raw caller {caller_index} requires missing unboxed callee {callee_index}"
            ),
        }
    }
}

impl std::error::Error for NativeCallTableError {}

/// Build the function-id table for compiling one caller.
///
/// Boxed callers always receive the ordinary boxed table. Raw callers may see
/// unboxed entries, but only for functions the module-level planner proved raw
/// compatible. A proven-raw callee without an unboxed declaration is a hard
/// error: substituting its boxed target would pass raw arguments into a tagged
/// ABI and recreate the representation mismatch this planner exists to prevent.
pub fn select_native_call_table<T: Copy>(
    plan: &NativeModulePlan,
    caller_index: usize,
    boxed: &[T],
    unboxed: &[Option<T>],
) -> Result<Vec<T>, NativeCallTableError> {
    let planned = plan.functions.len();
    if boxed.len() != planned || unboxed.len() != planned {
        return Err(NativeCallTableError::TableLengthMismatch {
            planned,
            boxed: boxed.len(),
            unboxed: unboxed.len(),
        });
    }

    if !plan.is_unboxed_int_function(caller_index) {
        return Ok(boxed.to_vec());
    }

    boxed
        .iter()
        .enumerate()
        .map(|(callee_index, boxed_target)| {
            if !plan.is_unboxed_int_function(callee_index) {
                return Ok(*boxed_target);
            }

            unboxed[callee_index].ok_or(NativeCallTableError::MissingUnboxedTarget {
                caller_index,
                callee_index,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::BinOp;
    use crate::bytecode::Constant;
    use crate::mir::{FuncRef, FunctionBuilder, Module, RValue, Terminator};
    use crate::types::Type;

    fn int_identity(name: &str) -> crate::mir::Function {
        let mut builder = FunctionBuilder::new(name, Some(Type::int()));
        let x = builder.add_param("x", Type::int());
        builder.terminate(Terminator::Return(Some(x)));
        builder.build()
    }

    #[test]
    fn boxed_caller_keeps_boxed_table() {
        let mut caller = FunctionBuilder::new("caller", Some(Type::int()));
        let x = caller.add_param("x", Type::int());
        let one = caller.add_temp(Type::int());
        let out = caller.add_temp(Type::int());
        caller.assign(one, RValue::Const(Constant::Int(1)));
        caller.assign(out, RValue::Binary(BinOp::Div, x, one));
        caller.terminate(Terminator::Return(Some(out)));

        let mut module = Module::new("boxed-caller");
        module.functions.push(caller.build());
        module.functions.push(int_identity("leaf"));
        let plan = NativeModulePlan::for_module(&module);

        let boxed = [10u32, 20];
        let unboxed = [None, Some(200)];
        assert_eq!(
            select_native_call_table(&plan, 0, &boxed, &unboxed).unwrap(),
            boxed
        );
    }

    #[test]
    fn raw_caller_sees_proven_raw_callee() {
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

        let mut module = Module::new("raw-edge");
        module.functions.push(caller.build());
        module.functions.push(int_identity("callee"));
        let plan = NativeModulePlan::for_module(&module);
        assert!(plan.is_unboxed_int_function(0));
        assert!(plan.is_unboxed_int_function(1));

        let boxed = [10u32, 20];
        let unboxed = [Some(100), Some(200)];
        assert_eq!(
            select_native_call_table(&plan, 0, &boxed, &unboxed).unwrap(),
            vec![100, 200]
        );
    }

    #[test]
    fn raw_caller_never_substitutes_boxed_only_callee() {
        let mut raw_leaf = FunctionBuilder::new("raw", Some(Type::int()));
        let x = raw_leaf.add_param("x", Type::int());
        raw_leaf.terminate(Terminator::Return(Some(x)));

        let mut boxed = FunctionBuilder::new("boxed", Some(Type::int()));
        let x = boxed.add_param("x", Type::int());
        let one = boxed.add_temp(Type::int());
        let out = boxed.add_temp(Type::int());
        boxed.assign(one, RValue::Const(Constant::Int(1)));
        boxed.assign(out, RValue::Binary(BinOp::Div, x, one));
        boxed.terminate(Terminator::Return(Some(out)));

        let mut module = Module::new("mixed-table");
        module.functions.push(raw_leaf.build());
        module.functions.push(boxed.build());
        let plan = NativeModulePlan::for_module(&module);
        assert!(plan.is_unboxed_int_function(0));
        assert!(!plan.is_unboxed_int_function(1));

        let boxed_targets = [10u32, 20];
        let unboxed_targets = [Some(100), Some(200)];
        assert_eq!(
            select_native_call_table(&plan, 0, &boxed_targets, &unboxed_targets).unwrap(),
            vec![100, 20]
        );
    }

    #[test]
    fn missing_unboxed_target_is_a_hard_error() {
        let mut module = Module::new("missing-target");
        module.functions.push(int_identity("leaf"));
        let plan = NativeModulePlan::for_module(&module);
        assert!(plan.is_unboxed_int_function(0));

        assert_eq!(
            select_native_call_table(&plan, 0, &[10u32], &[None]).unwrap_err(),
            NativeCallTableError::MissingUnboxedTarget {
                caller_index: 0,
                callee_index: 0,
            }
        );
    }

    #[test]
    fn mismatched_function_tables_are_rejected_before_selection() {
        let mut module = Module::new("mismatched-table");
        module.functions.push(int_identity("leaf"));
        let plan = NativeModulePlan::for_module(&module);

        assert_eq!(
            select_native_call_table(&plan, 0, &[10u32], &[]).unwrap_err(),
            NativeCallTableError::TableLengthMismatch {
                planned: 1,
                boxed: 1,
                unboxed: 0,
            }
        );
    }
}
