//! Executable regression for the shared NBC decoder on a 32-bit target.
//! Run from the repository root with a toolchain containing wasm32-wasip1:
//! cargo build --locked --manifest-path crates/nulang-playground/Cargo.toml --target wasm32-wasip1 --example nbc_wasm32_regression
//! Execute the resulting wasm32-wasip1 debug example with a WASI runtime.

use nulang_playground::{
    bytecode::{CodeModule, Instruction, OpCode},
    format::constants::{FormatError, NBC_HEADER_LEN},
};
fn main() {
    assert_eq!(usize::BITS, 32);
    let mut module = CodeModule::new("probe");
    module.emit(Instruction::new0(OpCode::Halt));
    let original = module.to_nbc(None).unwrap();
    assert_eq!(CodeModule::from_nbc(&original).unwrap().module, module);
    for count in [u32::MAX, u32::MAX / 4] {
        let mut bytes = original.clone();
        bytes[44..48].copy_from_slice(&count.to_be_bytes());
        assert!(matches!(
            CodeModule::from_nbc(&bytes),
            Err(FormatError::BodyDecode(_))
        ));
    }
    let mut bytes = original.clone();
    let off = NBC_HEADER_LEN + module.instructions.len() * 4;
    bytes[off..off + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        CodeModule::from_nbc(&bytes).unwrap_err(),
        FormatError::LengthMismatch {
            declared: u32::MAX,
            actual: bytes.len() - off - 4
        }
    );
    for end in 0..original.len() {
        assert!(CodeModule::from_nbc(&original[..end]).is_err());
    }
    println!("32-bit real codec: roundtrip, 2 oversized instruction spans, oversized metadata, {} truncated prefixes passed", original.len());
}
