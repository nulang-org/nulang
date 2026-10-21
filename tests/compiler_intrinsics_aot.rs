#![cfg(feature = "native-codegen")]

use nulang::aot::AotModule;
use nulang::compiler_intrinsics::{IntegerIntrinsic, MirIntrinsic};
use nulang::mir::{self, FunctionBuilder, RValue, Terminator};
use nulang::types::Type;

fn intrinsic_function(name: &str) -> mir::Function {
    let mut builder = FunctionBuilder::new(name, Some(Type::int()));
    let input = builder.add_param("input", Type::int());
    let result = builder.add_temp(Type::int());
    builder.assign(
        result,
        RValue::Intrinsic(MirIntrinsic::new(IntegerIntrinsic::Popcount, vec![input]).unwrap()),
    );
    builder.terminate(Terminator::Return(Some(result)));
    builder.build()
}

#[test]
fn aot_compiles_live_intrinsic_in_unboxed_integer_function() {
    let mut module = mir::Module::new("intrinsic-aot-unboxed");
    module.functions.push(intrinsic_function("main"));

    AotModule::compile(&module).expect("unboxed AOT should lower a live compiler intrinsic");
}

#[test]
fn aot_compiles_live_intrinsic_in_boxed_behavior() {
    let mut module = mir::Module::new("intrinsic-aot-boxed");
    module.behaviors.push(intrinsic_function("Worker.count"));

    AotModule::compile(&module).expect("boxed AOT should lower a live compiler intrinsic");
}
