use nulang::bytecode::Constant;
use nulang::compiler_intrinsics::{IntegerIntrinsic, MirIntrinsic};
use nulang::mir::{self, FunctionBuilder, RValue, Terminator};
use nulang::types::Type;

fn module_with(function: mir::Function) -> mir::Module {
    mir::Module {
        name: "intrinsic-test".into(),
        functions: vec![function],
        behaviors: vec![],
        actor_metadata: vec![],
        compensation_of: vec![],
        parallel_branches_of: vec![],
        foreign_functions: vec![],
    }
}

#[test]
fn constant_intrinsic_folds_before_bytecode_codegen() {
    let mut builder = FunctionBuilder::new("main", Some(Type::int()));
    let value = builder.add_temp(Type::int());
    let result = builder.add_temp(Type::int());
    builder.assign(value, RValue::Const(Constant::Int(0b1011)));
    builder.assign(
        result,
        RValue::Intrinsic(MirIntrinsic::new(IntegerIntrinsic::Popcount, vec![value]).unwrap()),
    );
    builder.terminate(Terminator::Return(Some(result)));
    let mut module = module_with(builder.build());

    nulang::mir_codegen::compile_mir(&mut module, "intrinsic-test").unwrap();

    assert!(module.functions[0].blocks.iter().any(|block| {
        block.stmts.iter().any(|stmt| {
            matches!(
                stmt,
                mir::Stmt::Assign {
                    dst,
                    op: RValue::Const(Constant::Int(3)),
                } if *dst == result
            )
        })
    }));
}

#[test]
fn unused_nonconstant_intrinsic_is_dead_code() {
    let mut builder = FunctionBuilder::new("main", Some(Type::int()));
    let input = builder.add_param("input", Type::int());
    let dead = builder.add_temp(Type::int());
    let answer = builder.add_temp(Type::int());
    builder.assign(
        dead,
        RValue::Intrinsic(MirIntrinsic::new(IntegerIntrinsic::Popcount, vec![input]).unwrap()),
    );
    builder.assign(answer, RValue::Const(Constant::Int(42)));
    builder.terminate(Terminator::Return(Some(answer)));
    let mut module = module_with(builder.build());

    nulang::mir_codegen::compile_mir(&mut module, "intrinsic-test").unwrap();

    assert!(!module.functions[0].blocks.iter().any(|block| {
        block.stmts.iter().any(|stmt| {
            matches!(
                stmt,
                mir::Stmt::Assign {
                    op: RValue::Intrinsic(_),
                    ..
                }
            )
        })
    }));
}

#[test]
fn live_nonconstant_intrinsic_fails_closed_in_bytecode_codegen() {
    let mut builder = FunctionBuilder::new("main", Some(Type::int()));
    let input = builder.add_param("input", Type::int());
    let result = builder.add_temp(Type::int());
    builder.assign(
        result,
        RValue::Intrinsic(MirIntrinsic::new(IntegerIntrinsic::Popcount, vec![input]).unwrap()),
    );
    builder.terminate(Terminator::Return(Some(result)));
    let mut module = module_with(builder.build());

    let error = nulang::mir_codegen::compile_mir(&mut module, "intrinsic-test").unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("compiler intrinsic"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("int.popcount"),
        "unexpected error: {message}"
    );
}
