#![cfg(feature = "wasm-backend")]

use nulang::backends::{DefaultWasmBackend, WasmBackend};
use nulang::bytecode::Constant;
use nulang::compiler_intrinsics::{IntegerIntrinsic, MirIntrinsic};
use nulang::mir::{self, FunctionBuilder, LocalId, RValue, Terminator};
use nulang::types::Type;

fn run_intrinsic(op: IntegerIntrinsic, args: &[i64]) -> i64 {
    let mut builder = FunctionBuilder::new("main", Some(Type::int()));
    let mut arg_locals = Vec::with_capacity(args.len());
    for &arg in args {
        let local = builder.add_temp(Type::int());
        builder.assign(local, RValue::Const(Constant::Int(arg)));
        arg_locals.push(local);
    }
    let result = builder.add_temp(Type::int());
    builder.assign(
        result,
        RValue::Intrinsic(MirIntrinsic::new(op, arg_locals).unwrap()),
    );
    builder.terminate(Terminator::Return(Some(result)));

    let mut module = mir::Module::new("intrinsic-wasm");
    module.functions.push(builder.build());

    let mut backend = DefaultWasmBackend;
    let wasm = backend
        .compile(&module, "intrinsic-wasm")
        .expect("WASM backend should lower compiler intrinsic");
    backend
        .run(&wasm)
        .expect("compiled intrinsic should execute")
        .as_int()
        .expect("integer intrinsic should return an Int")
}

fn expected_runtime_int(op: IntegerIntrinsic, args: &[i64]) -> i64 {
    let locals = (0..args.len())
        .map(|idx| LocalId(idx as u32))
        .collect::<Vec<_>>();
    let intrinsic = MirIntrinsic::new(op, locals).unwrap();
    let constants = args.iter().copied().map(Constant::Int).collect::<Vec<_>>();
    let Constant::Int(folded) = intrinsic.fold_constants(&constants).unwrap() else {
        panic!("integer intrinsic should fold to an integer");
    };
    nulang::value_layout::sext48((folded as u64) & nulang::value_layout::PAYLOAD_MASK)
}

#[test]
fn wasm_executes_first_wave_integer_intrinsics_with_int48_semantics() {
    let cases: &[(IntegerIntrinsic, &[i64])] = &[
        (IntegerIntrinsic::Popcount, &[0b1011]),
        (IntegerIntrinsic::LeadingZeros, &[1]),
        (IntegerIntrinsic::TrailingZeros, &[8]),
        (IntegerIntrinsic::RotateLeft, &[1, 65]),
        (IntegerIntrinsic::RotateRight, &[2, 1]),
        (IntegerIntrinsic::ByteSwap, &[0x0102_0304_0506]),
        (IntegerIntrinsic::Popcount, &[-1]),
    ];

    for &(op, args) in cases {
        assert_eq!(
            run_intrinsic(op, args),
            expected_runtime_int(op, args),
            "WASM result diverged for {}",
            op.stable_name()
        );
    }
}
