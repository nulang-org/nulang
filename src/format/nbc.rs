//! The `.nbc` (Nulang Bytecode) durable artifact format.
//!
//! A `.nbc` file is a frozen, versioned, content-addressable encoding of a
//! compiled [`crate::bytecode::CodeModule`]. It is the format a Nulang program
//! is *distributed* in: a `.nbc` minted in 2026 must load and run on any
//! conforming runtime in 2126 without the original source or compiler.
//!
//! # Byte layout (all integers big-endian)
//!
//! ```text
//! offset  size           field
//! 0       4              magic = b"NLBC"               (BYTECODE_MAGIC)
//! 4       4              format_version (u32)          (BYTECODE_VERSION)
//! 8       4              language_version (u32)        (LANGUAGE_VERSION)
//! 12      32             source_hash (blake3; 0x00..00 if unknown)
//! 44      4              instr_count (u32)
//! 48      4*instr_count  instructions (Instruction::encode() -> u32, BE)
//! 48+4n   4              meta_len (u32)
//! 52+4n   meta_len       metadata = serde_json::to_vec(&CodeModule with
//!                        instructions cleared)
//! ```
//!
//! # Design rationale
//!
//! The header is hand-rolled binary so a runtime can check magic + version +
//! length in O(1) without pulling in a serde format dependency. The
//! instruction stream is hand-rolled binary (4 bytes/instruction via
//! [`crate::bytecode::Instruction::encode`]) so the format is coupled to the
//! *frozen opcode values* — an unknown opcode is rejected with
//! [`FormatError::UnknownOpcode`], never reinterpreted. The metadata
//! (constants, behavior tables, handler tables, actor metadata, etc.) is
//! JSON: universally parseable by any conforming runtime in any host language,
//! debuggable, and stable across serde revisions. A future compact-binary
//! metadata encoding would be an additive v2 extension with a migration in
//! [`crate::format::migrate`].

use crate::bytecode::{CodeModule, Constant, Instruction};
use crate::format::constants::{
    FormatError, BYTECODE_MAGIC, BYTECODE_MAX_VERSION, BYTECODE_VERSION, LANGUAGE_VERSION,
    NBC_HEADER_LEN,
};

/// A `.nbc` artifact decoded into memory, plus the provenance recorded in its
/// header.
#[derive(Debug, Clone)]
pub struct NbcArtifact {
    /// The decoded module.
    pub module: CodeModule,
    /// BLAKE3 hash of the originating source, if the header carried a non-zero
    /// hash. `None` when the artifact was emitted without source provenance.
    pub source_hash: Option<[u8; 32]>,
    /// The `.nbc` format version recorded in the header.
    pub format_version: u32,
    /// The Nulang language version recorded in the header.
    pub language_version: u32,
}

impl CodeModule {
    /// Serialize this module to a `.nbc` byte vector.
    ///
    /// `source_hash` is an optional BLAKE3 digest of the originating source
    /// (e.g. the `.nula` file). Supply `None` to emit a zero hash (provenance
    /// unknown). A non-`None` hash enables offline `--verify` integrity
    /// checks at load time.
    pub fn to_nbc(&self, source_hash: Option<[u8; 32]>) -> Result<Vec<u8>, FormatError> {
        // Defensive invariant check: the i64-tagged value layout cannot
        // represent non-finite floats (their upper 16 bits collide with type
        // tags), so a well-formed CodeModule never contains them. Reject
        // early with a named error rather than letting serde_json fail
        // opaquely.
        for (i, c) in self.constants.iter().enumerate() {
            if let Constant::Float(f) = c {
                if !f.is_finite() {
                    return Err(FormatError::BadConstant(format!(
                        "constant #{i} is non-finite float ({f}); the value layout cannot represent it"
                    )));
                }
            }
        }

        let mut buf = Vec::with_capacity(NBC_HEADER_LEN + self.instructions.len() * 4 + 256);

        // --- Header ------------------------------------------------------
        buf.extend_from_slice(&BYTECODE_MAGIC);
        buf.extend_from_slice(&BYTECODE_VERSION.to_be_bytes());
        buf.extend_from_slice(&LANGUAGE_VERSION.to_be_bytes());
        match source_hash {
            Some(h) => buf.extend_from_slice(&h),
            None => buf.extend_from_slice(&[0u8; 32]),
        }
        buf.extend_from_slice(&(self.instructions.len() as u32).to_be_bytes());

        // --- Instruction stream (binary, 4 bytes each) -------------------
        for instr in &self.instructions {
            buf.extend_from_slice(&instr.encode().to_be_bytes());
        }

        // --- Metadata body (JSON; instructions field cleared) -----------
        let mut meta_module = self.clone();
        meta_module.instructions.clear();
        let meta_bytes =
            serde_json::to_vec(&meta_module).map_err(|e| FormatError::BodyDecode(e.to_string()))?;
        buf.extend_from_slice(&(meta_bytes.len() as u32).to_be_bytes());
        buf.extend_from_slice(&meta_bytes);

        Ok(buf)
    }

    /// Deserialize a `.nbc` byte slice into a module plus its recorded
    /// provenance.
    ///
    /// Returns a named [`FormatError`] for every recognised failure mode
    /// (truncated, bad magic, unsupported version, incompatible language
    /// version, unknown opcode, body decode failure). The runtime never
    /// guesses at a layout it does not understand.
    pub fn from_nbc(bytes: &[u8]) -> Result<NbcArtifact, FormatError> {
        if bytes.len() < NBC_HEADER_LEN {
            return Err(FormatError::Truncated {
                need: NBC_HEADER_LEN,
                have: bytes.len(),
            });
        }

        // --- Header ------------------------------------------------------
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

        // --- Instruction stream -----------------------------------------
        let instr_block = 48..48 + instr_count * 4;
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
            let instr = Instruction::decode(encoded).ok_or(FormatError::UnknownOpcode {
                opcode: (encoded >> 24) as u8,
            })?;
            instructions.push(instr);
        }

        // --- Metadata body ----------------------------------------------
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
        let mut module: CodeModule = serde_json::from_slice(meta_bytes)
            .map_err(|e| FormatError::BodyDecode(e.to_string()))?;
        module.instructions = instructions;

        Ok(NbcArtifact {
            module,
            source_hash,
            format_version,
            language_version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};

    fn sample_module() -> CodeModule {
        let mut m = CodeModule::new("test");
        m.add_constant(Constant::Int(42));
        m.add_constant(Constant::String("hello".into()));
        m.emit(Instruction::new1(OpCode::ConstU, 0));
        m.emit(Instruction::new0(OpCode::Halt));
        m
    }

    #[test]
    fn test_nbc_roundtrip_preserves_module() {
        let m = sample_module();
        let bytes = m.to_nbc(None).expect("encode");
        let art = CodeModule::from_nbc(&bytes).expect("decode");
        assert_eq!(art.module, m, "round-trip must preserve the module");
        assert_eq!(art.format_version, BYTECODE_VERSION);
        assert_eq!(art.language_version, LANGUAGE_VERSION);
        assert!(art.source_hash.is_none(), "no source hash supplied");
    }

    #[test]
    fn test_nbc_roundtrip_with_source_hash() {
        let m = sample_module();
        let h = [0xAA; 32];
        let bytes = m.to_nbc(Some(h)).expect("encode");
        let art = CodeModule::from_nbc(&bytes).expect("decode");
        assert_eq!(art.source_hash, Some(h));
    }

    #[test]
    fn test_nbc_header_has_magic_and_version() {
        let bytes = sample_module().to_nbc(None).unwrap();
        assert_eq!(&bytes[0..4], b"NLBC");
        assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 1);
    }

    #[test]
    fn test_nbc_rejects_truncated_header() {
        let err = CodeModule::from_nbc(&[0u8; 10]).unwrap_err();
        assert_eq!(
            err,
            FormatError::Truncated {
                need: NBC_HEADER_LEN,
                have: 10
            }
        );
    }

    #[test]
    fn test_nbc_rejects_bad_magic() {
        let mut bytes = sample_module().to_nbc(None).unwrap();
        bytes[0] = b'X';
        let err = CodeModule::from_nbc(&bytes).unwrap_err();
        assert!(matches!(err, FormatError::BadMagic { .. }));
    }

    #[test]
    fn test_nbc_rejects_unsupported_format_version() {
        let mut bytes = sample_module().to_nbc(None).unwrap();
        // Bump the format version field (offset 4) to a future version.
        bytes[4..8].copy_from_slice(&99u32.to_be_bytes());
        let err = CodeModule::from_nbc(&bytes).unwrap_err();
        assert!(matches!(
            err,
            FormatError::UnsupportedVersion { found: 99, .. }
        ));
    }

    #[test]
    fn test_nbc_rejects_incompatible_language_version() {
        let mut bytes = sample_module().to_nbc(None).unwrap();
        // Bump the language version field (offset 8) to a future version.
        bytes[8..12].copy_from_slice(&99u32.to_be_bytes());
        let err = CodeModule::from_nbc(&bytes).unwrap_err();
        assert!(matches!(
            err,
            FormatError::IncompatibleLanguage { artifact: 99, .. }
        ));
    }

    #[test]
    fn test_nbc_rejects_unknown_opcode_in_stream() {
        // Hand-craft a minimal artifact with an unknown opcode byte.
        let mut buf = Vec::new();
        buf.extend_from_slice(b"NLBC");
        buf.extend_from_slice(&1u32.to_be_bytes()); // format version
        buf.extend_from_slice(&1u32.to_be_bytes()); // language version
        buf.extend_from_slice(&[0u8; 32]); // source hash
        buf.extend_from_slice(&1u32.to_be_bytes()); // instr_count = 1
                                                    // One instruction with an unknown opcode 0xFE.
        let bad_instr: u32 = (0xFEu32 << 24) | 0;
        buf.extend_from_slice(&bad_instr.to_be_bytes());
        // Empty metadata body.
        let empty = CodeModule::new("x");
        let meta = serde_json::to_vec(&empty).unwrap();
        buf.extend_from_slice(&(meta.len() as u32).to_be_bytes());
        buf.extend_from_slice(&meta);
        let err = CodeModule::from_nbc(&buf).unwrap_err();
        assert!(matches!(err, FormatError::UnknownOpcode { opcode: 0xFE }));
    }

    #[test]
    fn test_nbc_rejects_non_finite_float_constant() {
        let mut m = sample_module();
        m.add_constant(Constant::Float(f64::NAN));
        let err = m.to_nbc(None).unwrap_err();
        assert!(matches!(err, FormatError::BadConstant(_)));
    }

    #[test]
    fn test_spawn_init_overrides_serde_default() {
        // A .nbc produced by an older build lacks `spawn_init_overrides`.
        // serde(default) must fill it with an empty Vec, not error.
        let json = r#"{
            "name": "test",
            "constants": [],
            "instructions": [],
            "behaviors": [],
            "function_table": [],
            "exports": [],
            "entry_point": null,
            "handler_tables": [],
            "actor_metadata": [],
            "foreign_functions": [],
            "tools": []
        }"#;
        let module: crate::bytecode::CodeModule =
            serde_json::from_str(json).expect("old-format .nbc metadata must deserialize");
        assert!(
            module.spawn_init_overrides.is_empty(),
            "missing field must default to empty Vec"
        );
        assert!(
            module.function_local_counts.is_empty(),
            "missing function_local_counts must default to empty Vec"
        );
    }
}
