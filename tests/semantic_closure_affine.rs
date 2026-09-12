//! Affine-continuation semantic regression tests.
//!
//! The compiler's `single_shot` flag is an optimization decision. The language
//! invariant is stronger: `Resume` requires a live captured continuation and
//! must fail closed when no continuation is available.

use nulang::bytecode::{CodeModule, Constant, Instruction, OpCode};
use nulang::types::NuError;
use nulang::vm::VM;

#[test]
fn resume_without_live_continuation_traps_deterministically() {
    let mut module = CodeModule::new("affine-resume-without-capture");
    let one = module.add_constant(Constant::Int(1));
    module.emit(Instruction::new2(OpCode::ConstU, 0, one as u8));
    module.emit(Instruction::new1(OpCode::Resume, 0));
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.load_module(module);
    let err = vm
        .run()
        .expect_err("resume without a captured continuation must trap");

    match err {
        NuError::VMError { msg, .. } => {
            assert_eq!(msg, "resume called without a captured continuation");
        }
        other => panic!("expected VMError, got {other:?}"),
    }
}
