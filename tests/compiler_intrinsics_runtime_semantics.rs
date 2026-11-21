use nulang::bytecode::Constant;
use nulang::compiler_intrinsics::{IntegerIntrinsic, MirIntrinsic};
use nulang::mir::LocalId;

#[test]
fn mir_intrinsic_folding_uses_runtime_int48_inputs() {
    let popcount = MirIntrinsic::new(IntegerIntrinsic::Popcount, vec![LocalId(0)]).unwrap();

    // Runtime Int storage keeps only the low 48 payload bits. 2^48 therefore
    // becomes zero before the intrinsic executes.
    assert_eq!(
        popcount.fold_constants(&[Constant::Int(1_i64 << 48)]),
        Some(Constant::Int(0))
    );

    // Payload bit 47 is Nulang Int's sign bit. A source/compiler constant with
    // that bit set is observed by the VM as -2^47, whose sign-extended i64
    // representation has bits 47..63 set (17 one-bits total).
    assert_eq!(
        popcount.fold_constants(&[Constant::Int(1_i64 << 47)]),
        Some(Constant::Int(17))
    );
}
