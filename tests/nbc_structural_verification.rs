//! RED-first regression contract for opt-in NBC structural verification.
//! Existing `from_nbc` remains the compatibility decoder, not a verifier.

use nulang::bytecode::{CodeModule, Constant, Instruction, OpCode};
use nulang::format::verify::verify_module_structure;

fn simple_module() -> CodeModule {
    let mut module = CodeModule::new("verified");
    module.emit(Instruction::new1(OpCode::Const0, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module
}

fn assert_rejected(module: &CodeModule, expected: &str) {
    let bytes = module.to_nbc(None).expect("serializable test fixture");
    // Keep legacy decoding compatible; structural validation is an opt-in gate.
    assert!(CodeModule::from_nbc(&bytes).is_ok());
    let error = CodeModule::from_nbc_with_structural_verification(&bytes)
        .expect_err("malformed module must fail structural admission");
    assert!(
        error.to_string().contains(expected),
        "expected {expected:?}, got {error}"
    );
}

#[test]
fn nbc_structural_verifier_admits_valid_serialized_module() {
    let mut module = simple_module();
    module.entry_point = Some(0);
    module.function_table.push(0);
    module.function_local_counts.push(1);
    module.constants.push(Constant::FunctionRef(0));
    assert!(verify_module_structure(&module).is_ok());

    let bytes = module.to_nbc(None).unwrap();
    let decoded = CodeModule::from_nbc_with_structural_verification(&bytes).unwrap();
    assert_eq!(decoded.module, module);
}

#[test]
fn nbc_structural_verifier_rejects_out_of_range_entry_point() {
    let mut module = simple_module();
    module.entry_point = Some(module.instructions.len());
    assert_rejected(&module, "entry point");
}

#[test]
fn nbc_structural_verifier_rejects_out_of_range_function_offset() {
    let mut module = simple_module();
    module.function_table.push(usize::MAX);
    assert_rejected(&module, "function");
}

#[test]
fn nbc_structural_verifier_rejects_inconsistent_function_local_counts() {
    let mut module = simple_module();
    module.function_table.push(0);
    module.function_local_counts = vec![1, 2];
    assert_rejected(&module, "function local counts");
}

#[test]
fn nbc_structural_verifier_rejects_bad_constant_function_ref() {
    let mut module = simple_module();
    module.constants.push(Constant::FunctionRef(3));
    assert_rejected(&module, "function reference");
}

#[test]
fn nbc_structural_verifier_rejects_bad_constant_behavior_ref() {
    let mut module = simple_module();
    module.constants.push(Constant::BehaviorRef(3));
    assert_rejected(&module, "behavior reference");
}

#[test]
fn nbc_structural_verifier_rejects_out_of_bounds_jump_targets() {
    let mut module = simple_module();
    module.instructions[0] = Instruction::new2(OpCode::Jmp, 0xff, 0xff);
    assert_rejected(&module, "jump target");

    module.instructions[0] = Instruction::new3(OpCode::JmpT, 0, 0, 2);
    assert_rejected(&module, "jump target");

    module.instructions[0] = Instruction::new3(OpCode::JmpF, 0, 0xff, 0xff);
    assert_rejected(&module, "jump target");
}

#[test]
fn nbc_structural_verifier_accepts_valid_backward_jump() {
    let mut module = simple_module();
    module.instructions[1] = Instruction::new2(OpCode::Jmp, 0xff, 0xff);
    assert!(verify_module_structure(&module).is_ok());
}

#[test]
fn nbc_structural_verifier_rejects_invalid_instruction_references() {
    let mut module = simple_module();
    module.instructions[0] = Instruction::new3(OpCode::ConstU, 0, 1, 0);
    assert_rejected(&module, "constant");

    module.instructions[0] = Instruction::new3(OpCode::Closure, 0, 0, 0);
    assert_rejected(&module, "function");

    module.instructions[0] = Instruction::new1(OpCode::Handle, 0);
    assert_rejected(&module, "handler table");

    module.instructions[0] = Instruction::new3(OpCode::PerformDirect, 0, 0, 0);
    assert_rejected(&module, "handler table");
}

#[test]
fn nbc_structural_verifier_rejects_invalid_effect_constant_type() {
    let mut module = simple_module();
    module.constants.push(Constant::Int(42));
    module.instructions[0] = Instruction::new3(OpCode::Perform, 0, 0, 0);
    assert_rejected(&module, "string constant");
}
