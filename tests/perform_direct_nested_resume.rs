//! Mixed continuation representations must resume the innermost outstanding
//! effect, regardless of whether that effect used the single-shot fast path.

use nulang::bytecode::{CodeModule, Constant, HandlerBinding, HandlerTable, Instruction, OpCode};
use nulang::vm::VM;

#[test]
fn inner_general_continuation_takes_precedence_over_outer_single_shot_state() {
    let mut module = CodeModule::new("mixed-nested-resume");
    module.add_handler_table(HandlerTable {
        bindings: vec![HandlerBinding {
            effect_name: "Outer.get".into(),
            handler_offset: 14,
            arg_count: 0,
            result_reg: 0,
            single_shot: true,
        }],
        fallback_offset: None,
    });
    module.add_handler_table(HandlerTable {
        bindings: vec![HandlerBinding {
            effect_name: "Inner.get".into(),
            handler_offset: 20,
            arg_count: 0,
            result_reg: 0,
            single_shot: false,
        }],
        fallback_offset: None,
    });
    let five = module.add_constant(Constant::Int(5));
    let seven = module.add_constant(Constant::Int(7));

    // Outer effect, resumed with the value computed by its own handler.
    module.emit(Instruction::new1(OpCode::Handle, 0)); // PC 0
    module.emit(Instruction::new3(OpCode::PerformDirect, 0, 0, 1)); // PC 1
    module.emit(Instruction::new2(OpCode::Move, 1, 0)); // PC 2
    module.emit(Instruction::new0(OpCode::Unwind)); // PC 3
    module.emit(Instruction::new0(OpCode::Halt)); // PC 4
    for _ in 5..14 {
        module.emit(Instruction::new0(OpCode::Nop));
    }

    // Outer handler invokes the inner effect, adds five to its answer, and
    // only then resumes the outer continuation.
    module.emit(Instruction::new1(OpCode::Handle, 1)); // PC 14
    module.emit(Instruction::new3(OpCode::PerformDirect, 1, 0, 2)); // PC 15
    module.emit(Instruction::new3(
        OpCode::ConstU,
        (five >> 8) as u8,
        five as u8,
        3,
    )); // PC 16
    module.emit(Instruction::new3(OpCode::IAdd, 2, 3, 0)); // PC 17
    module.emit(Instruction::new0(OpCode::Unwind)); // PC 18
    module.emit(Instruction::new1(OpCode::Resume, 0)); // PC 19

    // Inner general continuation must resume PC 16, not the outer
    // single-shot continuation at PC 2.
    module.emit(Instruction::new3(
        OpCode::ConstU,
        (seven >> 8) as u8,
        seven as u8,
        0,
    )); // PC 20
    module.emit(Instruction::new1(OpCode::Resume, 0)); // PC 21
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.load_module(module);
    let result = vm.run().expect("mixed nested continuations should resume");
    assert_eq!(result.as_int(), Some(12));
    assert!(vm.handler_stack.is_empty(), "every handler must unwind");
}
