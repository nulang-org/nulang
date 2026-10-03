//! Native AOT representation-boundary regression tests.
//!
//! These tests are intentionally backend-level. They lock down failures where
//! an unboxed caller forwards raw machine integers into code that still expects
//! tagged `Value`s, and where the current integer boxing wrapper retags a
//! unit/nil return as an integer.

#![cfg(feature = "native-codegen")]

use nulang::aot::AotModule;
use nulang::ast::BinOp;
use nulang::bytecode::Constant;
use nulang::mir::{self, FuncRef, FunctionBuilder, Module, RValue, Terminator};
use nulang::types::Type;
use nulang::vm::Value;

#[test]
fn native_unit_entry_preserves_nil_in_boxed_result() {
    // `is_all_int` currently accepts a function with no parameters and no
    // return type. The unboxed body emits tagged nil for Return(None), while
    // `compile_boxing_wrapper` then applies the integer tag to that raw result.
    // The runtime-facing entry must preserve nil instead.
    let mut main = FunctionBuilder::new("main", None);
    main.terminate(Terminator::Return(None));

    let mut module = Module::new("native-unit-entry");
    module.functions.push(main.build());

    let compiled = AotModule::compile(&module).expect("native unit entry should compile");
    let raw = compiled.run().expect("native unit entry should run");

    assert_eq!(
        raw,
        Value::nil().as_raw(),
        "unit-returning native entry must preserve the boxed nil representation"
    );
}

#[test]
fn unboxed_caller_does_not_pass_raw_int_to_boxed_callee() {
    // Function 1 is deliberately ineligible for the current unboxed path: Div
    // can produce nil and therefore uses the tagged runtime helper. Function 0
    // is otherwise all-Int and calls it directly. If function 0 is compiled
    // unboxed but resolves function 1 through the boxed function table, its raw
    // `41` reaches `nulang_idiv`; that helper treats non-TAG_INT inputs as 0.
    let mut main = FunctionBuilder::new("main", Some(Type::int()));
    let forty_one = main.add_temp(Type::int());
    let result = main.add_temp(Type::int());
    main.assign(forty_one, RValue::Const(Constant::Int(41)));
    main.assign(
        result,
        RValue::Call {
            func: FuncRef::Index(1),
            args: vec![forty_one],
        },
    );
    main.terminate(Terminator::Return(Some(result)));

    let mut callee = FunctionBuilder::new("divide_by_one", Some(Type::int()));
    let x = callee.add_param("x", Type::int());
    let one = callee.add_temp(Type::int());
    let quotient = callee.add_temp(Type::int());
    callee.assign(one, RValue::Const(Constant::Int(1)));
    callee.assign(quotient, RValue::Binary(BinOp::Div, x, one));
    callee.terminate(Terminator::Return(Some(quotient)));

    let mut module = Module::new("native-cross-function-abi");
    module.functions.push(main.build());
    module.functions.push(callee.build());

    let compiled = AotModule::compile(&module).expect("native direct-call fixture should compile");
    let raw = compiled
        .run()
        .expect("native direct-call fixture should run");
    // SAFETY: this is a raw value produced by this process's AOT backend.
    let value = unsafe { Value::from_raw(raw) };

    assert_eq!(
        value.as_int(),
        Some(41),
        "raw Int arguments must not cross into a boxed callee ABI"
    );
}

#[test]
fn representation_planner_rejects_both_current_mismatches() {
    use nulang::native_module_plan::NativeModulePlan;

    let mut unit = FunctionBuilder::new("main", None);
    unit.terminate(Terminator::Return(None));
    let mut unit_module = Module::new("unit-plan");
    unit_module.functions.push(unit.build());
    assert!(!NativeModulePlan::for_module(&unit_module).is_unboxed_int_function(0));

    let mut caller = FunctionBuilder::new("main", Some(Type::int()));
    let arg = caller.add_temp(Type::int());
    let out = caller.add_temp(Type::int());
    caller.assign(arg, RValue::Const(Constant::Int(41)));
    caller.assign(
        out,
        RValue::Call {
            func: FuncRef::Index(1),
            args: vec![arg],
        },
    );
    caller.terminate(Terminator::Return(Some(out)));

    let mut boxed_callee = FunctionBuilder::new("divide_by_one", Some(Type::int()));
    let x = boxed_callee.add_param("x", Type::int());
    let one = boxed_callee.add_temp(Type::int());
    let quotient = boxed_callee.add_temp(Type::int());
    boxed_callee.assign(one, RValue::Const(Constant::Int(1)));
    boxed_callee.assign(quotient, RValue::Binary(BinOp::Div, x, one));
    boxed_callee.terminate(Terminator::Return(Some(quotient)));

    let mut calls = mir::Module::new("call-plan");
    calls.functions.push(caller.build());
    calls.functions.push(boxed_callee.build());
    let plan = NativeModulePlan::for_module(&calls);
    assert!(!plan.is_unboxed_int_function(0));
    assert!(!plan.is_unboxed_int_function(1));
}
