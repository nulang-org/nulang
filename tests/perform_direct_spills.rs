//! A single-shot direct effect must restore every live component of its
//! activation frame, including spill slots in large functions.

use nulang::bytecode::{CodeModule, Constant, HandlerBinding, HandlerTable, Instruction, OpCode};
use nulang::vm::VM;

#[test]
fn perform_direct_single_shot_restores_spilled_locals_on_resume() {
    let mut module = CodeModule::new("direct-spill-continuation");
    module.add_handler_table(HandlerTable {
        bindings: vec![HandlerBinding {
            effect_name: "Store.read".to_string(),
            handler_offset: 9,
            arg_count: 0,
            result_reg: 0,
            single_shot: true,
        }],
        fallback_offset: None,
    });
    let original = module.add_constant(Constant::Int(77));
    let overwritten = module.add_constant(Constant::Int(99));
    let resumed = module.add_constant(Constant::Int(10));

    // The handled computation stores 77 in spill slot zero, then performs a
    // direct effect. The handler overwrites that slot with 99 before resuming.
    // The continuation must see the original 77 after Resume.
    module.emit(Instruction::new1(OpCode::Handle, 0)); // PC 0
    module.emit(Instruction::new3(
        OpCode::ConstU,
        (original >> 8) as u8,
        original as u8,
        4,
    )); // PC 1
    module.emit(Instruction::new3(OpCode::SpillStore, 4, 0, 0)); // PC 2
    module.emit(Instruction::new3(OpCode::PerformDirect, 0, 0, 1)); // PC 3
    module.emit(Instruction::new3(OpCode::SpillLoad, 0, 0, 0)); // PC 4
    module.emit(Instruction::new0(OpCode::Unwind)); // PC 5
    module.emit(Instruction::new0(OpCode::Halt)); // PC 6
    module.emit(Instruction::new0(OpCode::Nop)); // PC 7
    module.emit(Instruction::new0(OpCode::Nop)); // PC 8
    module.emit(Instruction::new3(
        OpCode::ConstU,
        (overwritten >> 8) as u8,
        overwritten as u8,
        4,
    )); // PC 9
    module.emit(Instruction::new3(OpCode::SpillStore, 4, 0, 0)); // PC 10
    module.emit(Instruction::new3(
        OpCode::ConstU,
        (resumed >> 8) as u8,
        resumed as u8,
        0,
    )); // PC 11
    module.emit(Instruction::new1(OpCode::Resume, 0)); // PC 12
    module.entry_point = Some(0);

    let mut vm = VM::new();
    vm.load_module(module);
    let result = vm.run().expect("single-shot direct effect should resume");
    assert_eq!(
        result.as_int(),
        Some(77),
        "Resume must restore spill slots as well as registers",
    );
}
