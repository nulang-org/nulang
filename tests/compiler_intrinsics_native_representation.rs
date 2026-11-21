#![cfg(feature = "native-codegen")]

use cranelift::codegen::ir::{types, AbiParam, Function, InstBuilder};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use nulang::compiler_intrinsics::{
    lower_cranelift_integer, IntegerIntrinsic, NativeIntegerRepresentation,
};

fn render_tagged_lowering(intrinsic: IntegerIntrinsic) -> String {
    let mut function = Function::new();
    for _ in 0..intrinsic.arity() {
        function.signature.params.push(AbiParam::new(types::I64));
    }
    function.signature.returns.push(AbiParam::new(types::I64));

    let mut context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut function, &mut context);
    let block = builder.create_block();
    builder.switch_to_block(block);
    builder.append_block_params_for_function_params(block);
    let args = builder.block_params(block).to_vec();
    let result = lower_cranelift_integer(
        &mut builder,
        intrinsic,
        &args,
        NativeIntegerRepresentation::TaggedInt48,
    )
    .unwrap();
    builder.ins().return_(&[result]);
    builder.seal_all_blocks();
    builder.finalize();
    function.display().to_string()
}

#[test]
fn tagged_lowering_unboxes_uses_native_instruction_and_reboxes() {
    let clif = render_tagged_lowering(IntegerIntrinsic::Popcount);

    assert!(
        clif.contains("popcnt"),
        "CLIF should retain native popcnt:\n{clif}"
    );
    assert!(
        clif.contains("band"),
        "CLIF should mask Int48 payload bits:\n{clif}"
    );
    assert!(
        clif.contains("select"),
        "CLIF should sign-extend Int48 inputs:\n{clif}"
    );
    assert!(
        clif.contains("bor"),
        "CLIF should re-apply TAG_INT:\n{clif}"
    );
}

#[test]
fn raw_lowering_stays_representation_free() {
    let mut function = Function::new();
    function.signature.params.push(AbiParam::new(types::I64));
    function.signature.returns.push(AbiParam::new(types::I64));
    let mut context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut function, &mut context);
    let block = builder.create_block();
    builder.switch_to_block(block);
    builder.append_block_params_for_function_params(block);
    let args = builder.block_params(block).to_vec();
    let result = lower_cranelift_integer(
        &mut builder,
        IntegerIntrinsic::Popcount,
        &args,
        NativeIntegerRepresentation::RawI64,
    )
    .unwrap();
    builder.ins().return_(&[result]);
    builder.seal_all_blocks();
    builder.finalize();
    let clif = function.display().to_string();

    assert!(clif.contains("popcnt"));
    assert!(
        !clif.contains("select"),
        "raw lowering must not sign-extend/tag:\n{clif}"
    );
}
