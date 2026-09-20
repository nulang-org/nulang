//! Frozen `.nbc` version-1 codec.
//!
//! This module owns the byte-level implementation of the published v1
//! artifact format. The public `CodeModule::to_nbc` / `from_nbc` API lives
//! in `nbc.rs`; keeping the v1 implementation here makes the compatibility
//! obligation attach to this codec rather than to the live VM representation.
//!
//! IMPORTANT: once published, v1 bytes are immutable. Future runtime opcode,
//! register, value-layout, JIT, or GC changes must preserve this decoder
//! (or migrate through `format::migrate`) rather than reinterpret v1.

use super::nbc::NbcArtifact;
use crate::bytecode::{CodeModule, Constant, Instruction};
use crate::format::constants::{
    FormatError, BYTECODE_MAGIC, BYTECODE_MAX_VERSION, BYTECODE_VERSION, LANGUAGE_VERSION,
    NBC_HEADER_LEN,
};

/// Encode a module using the frozen v1 byte layout.
pub(crate) fn encode(
    module: &CodeModule,
    source_hash: Option<[u8; 32]>,
) -> Result<Vec<u8>, FormatError> {
    // Defensive invariant check: the i64-tagged value layout cannot
    // represent non-finite floats (their upper 16 bits collide with type
    // tags), so a well-formed CodeModule never contains them. Reject
    // early with a named error rather than letting serde_json fail
    // opaquely.
    for (i, c) in module.constants.iter().enumerate() {
        if let Constant::Float(f) = c {
            if !f.is_finite() {
                return Err(FormatError::BadConstant(format!(
                    "constant #{i} is non-finite float ({f}); the value layout cannot represent it"
                )));
            }
        }
    }

    let mut buf = Vec::with_capacity(NBC_HEADER_LEN + module.instructions.len() * 4 + 256);

    // Header.
    buf.extend_from_slice(&BYTECODE_MAGIC);
    buf.extend_from_slice(&BYTECODE_VERSION.to_be_bytes());
    buf.extend_from_slice(&LANGUAGE_VERSION.to_be_bytes());
    match source_hash {
        Some(h) => buf.extend_from_slice(&h),
        None => buf.extend_from_slice(&[0u8; 32]),
    }
    buf.extend_from_slice(&(module.instructions.len() as u32).to_be_bytes());

    // Frozen v1 instruction stream. These calls are intentionally contained
    // inside the versioned codec; future live VM instructions may diverge
    // while this adapter continues to preserve v1 bytes.
    for instr in &module.instructions {
        buf.extend_from_slice(&encode_instruction(instr).to_be_bytes());
    }

    // Metadata body (JSON; instructions field cleared).
    let mut meta_module = module.clone();
    meta_module.instructions.clear();
    let meta_bytes =
        serde_json::to_vec(&meta_module).map_err(|e| FormatError::BodyDecode(e.to_string()))?;
    buf.extend_from_slice(&(meta_bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(&meta_bytes);

    Ok(buf)
}

/// Decode the frozen v1 byte layout into the current semantic module.
///
/// Validation behavior is intentionally identical to the original v1 loader,
/// including accepting any version not greater than `BYTECODE_MAX_VERSION`.
/// Tightening version semantics belongs in a separately reviewed format
/// change, not this representation-only refactor.
pub(crate) fn decode(bytes: &[u8]) -> Result<NbcArtifact, FormatError> {
    if bytes.len() < NBC_HEADER_LEN {
        return Err(FormatError::Truncated {
            need: NBC_HEADER_LEN,
            have: bytes.len(),
        });
    }

    let magic: [u8; 4] = bytes[0..4].try_into().unwrap();
    if magic != BYTECODE_MAGIC {
        return Err(FormatError::BadMagic {
            expected: BYTECODE_MAGIC,
            got: magic,
        });
    }

    let format_version = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
    if format_version > BYTECODE_MAX_VERSION {
        return Err(FormatError::UnsupportedVersion {
            max_supported: BYTECODE_MAX_VERSION,
            found: format_version,
        });
    }

    let language_version = u32::from_be_bytes(bytes[8..12].try_into().unwrap());
    if language_version > LANGUAGE_VERSION {
        return Err(FormatError::IncompatibleLanguage {
            runtime: LANGUAGE_VERSION,
            artifact: language_version,
        });
    }

    let mut source_hash = [0u8; 32];
    source_hash.copy_from_slice(&bytes[12..44]);
    let source_hash = if source_hash == [0u8; 32] {
        None
    } else {
        Some(source_hash)
    };

    let instr_count = u32::from_be_bytes(bytes[44..48].try_into().unwrap()) as usize;
    let instr_block = NBC_HEADER_LEN..NBC_HEADER_LEN + instr_count * 4;
    if bytes.len() < instr_block.end {
        return Err(FormatError::Truncated {
            need: instr_block.end,
            have: bytes.len(),
        });
    }

    let mut instructions = Vec::with_capacity(instr_count);
    for i in 0..instr_count {
        let off = instr_block.start + i * 4;
        let encoded = u32::from_be_bytes(bytes[off..off + 4].try_into().unwrap());
        let instr = decode_instruction(encoded).ok_or(FormatError::UnknownOpcode {
            opcode: (encoded >> 24) as u8,
        })?;
        instructions.push(instr);
    }

    let meta_len_off = instr_block.end;
    if bytes.len() < meta_len_off + 4 {
        return Err(FormatError::Truncated {
            need: meta_len_off + 4,
            have: bytes.len(),
        });
    }

    let meta_len =
        u32::from_be_bytes(bytes[meta_len_off..meta_len_off + 4].try_into().unwrap()) as usize;
    let meta_off = meta_len_off + 4;
    if bytes.len() < meta_off + meta_len {
        return Err(FormatError::LengthMismatch {
            declared: meta_len as u32,
            actual: bytes.len() - meta_off,
        });
    }

    let meta_bytes = &bytes[meta_off..meta_off + meta_len];
    let mut module: CodeModule =
        serde_json::from_slice(meta_bytes).map_err(|e| FormatError::BodyDecode(e.to_string()))?;
    module.instructions = instructions;

    Ok(NbcArtifact {
        module,
        source_hash,
        format_version,
        language_version,
    })
}

#[inline]
fn encode_instruction(instruction: &Instruction) -> u32 {
    instruction.encode()
}

#[inline]
fn decode_instruction(encoded: u32) -> Option<Instruction> {
    Instruction::decode(encoded)
}
