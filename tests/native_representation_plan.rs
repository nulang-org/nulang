#![cfg(feature = "native-codegen")]

use nulang::mir::{FunctionBuilder, Module, Terminator};
use nulang::native_abi::NativeValueRepr;
use nulang::native_module_plan::NativeModulePlan;
use nulang::native_plan::NativeFunctionPlan;
use nulang::types::Type;

#[test]
fn compiler_owned_plan_proves_pure_int_leaf_without_changing_runtime_abi() {
    let mut builder = FunctionBuilder::new("identity", Some(Type::int()));
    let arg = builder.add_param("x", Type::int());
    builder.terminate(Terminator::Return(Some(arg)));
    let function = builder.build();

    let function_plan = NativeFunctionPlan::for_function(&function);
    assert_eq!(function_plan.params, vec![NativeValueRepr::I64]);
    assert_eq!(function_plan.ret, Some(NativeValueRepr::I64));

    let mut module = Module::new("identity");
    module.functions.push(function);
    let module_plan = NativeModulePlan::for_module(&module);
    assert!(module_plan.is_unboxed_int_function(0));
}

#[test]
fn unknown_callee_keeps_raw_caller_ineligible() {
    use nulang::mir::{FuncRef, RValue};

    let mut builder = FunctionBuilder::new("unresolved", Some(Type::int()));
    let arg = builder.add_param("x", Type::int());
    let out = builder.add_temp(Type::int());
    builder.assign(
        out,
        RValue::Call {
            func: FuncRef::Index(99),
            args: vec![arg],
        },
    );
    builder.terminate(Terminator::Return(Some(out)));

    let mut module = Module::new("unknown-target");
    module.functions.push(builder.build());
    let module_plan = NativeModulePlan::for_module(&module);
    assert!(
        !module_plan.is_unboxed_int_function(0),
        "nonexistent callee index must fail closed"
    );
}
