//! Affine-continuation semantic regression tests.
//!
//! The compiler's `single_shot` flag is an optimization decision. The language
//! invariant is stronger: `Resume` consumes a live captured continuation and
//! must fail closed when no continuation remains.

use nulang::bytecode::{CodeModule, Constant, HandlerBinding, HandlerTable, Instruction, OpCode};
use nulang::types::NuError;
use nulang::vm::VM;

fn assert_missing_continuation(err: NuError) {
    match err {
        NuError::VMError { msg, .. } => {
            assert!(
                msg.starts_with("resume called without a captured continuation"),
                "expected missing-continuation trap, got: {msg}"
            );
        }
        other => panic!("expected VMError, got {other:?}"),
    }
}

#[test]
fn resume_without_live_continuation_traps_deterministically() {
    let mut module = CodeModule::new("affine-resume-without-capture");
    let one = module.add_constant(Constant::Int(1));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((one >> 8) & 0xff) as u8,
        (one & 0xff) as u8,
        0,
    ));
    module.emit(Instruction::new1(OpCode::Resume, 0));
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.load_module(module);
    let err = vm
        .run()
        .expect_err("resume without a captured continuation must trap");
    assert_missing_continuation(err);
}

#[test]
fn second_resume_after_successful_resume_traps() {
    let mut module = CodeModule::new("affine-second-resume");
    module.add_handler_table(HandlerTable {
        bindings: vec![HandlerBinding {
            effect_name: "GetOne".to_string(),
            handler_offset: 7,
            arg_count: 0,
            result_reg: 0,
            single_shot: false,
        }],
        fallback_offset: None,
    });

    let effect = module.add_constant(Constant::String("GetOne".to_string()));
    let one = module.add_constant(Constant::Int(1));

    // PC 0: install the handler.
    module.emit(Instruction::new1(OpCode::Handle, 0));
    // PC 1: capture a continuation whose resume point is PC 2.
    module.emit(Instruction::new3(
        OpCode::Perform,
        ((effect >> 8) & 0xff) as u8,
        (effect & 0xff) as u8,
        1,
    ));
    // PC 2: reached only after the first successful Resume. The captured
    // continuation must already have been consumed, so this second Resume
    // is required to trap rather than replay the continuation.
    module.emit(Instruction::new1(OpCode::Resume, 1));
    module.emit(Instruction::new0(OpCode::Unwind));
    module.emit(Instruction::new0(OpCode::Halt));
    module.emit(Instruction::new0(OpCode::Nop));
    module.emit(Instruction::new0(OpCode::Nop));
    // PC 7-8: handler body performs the first, valid resume.
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((one >> 8) & 0xff) as u8,
        (one & 0xff) as u8,
        0,
    ));
    module.emit(Instruction::new1(OpCode::Resume, 0));
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.load_module(module);
    let err = vm
        .run()
        .expect_err("a consumed continuation must not be resumable twice");
    assert_missing_continuation(err);
}
