//! Opt-in structural checks for decoded Nulang bytecode artifacts.
//!
//! This is a narrow, non-executing admission pass inspired by JVM class-file
//! verification. It does not prove type safety, definite register assignment,
//! effect/capability authority, bounded execution, or sandbox isolation.
//! The existing `CodeModule::from_nbc` compatibility decoder is unchanged.

use crate::bytecode::{CodeModule, Constant, OpCode};
use crate::format::constants::FormatError;
use crate::format::nbc::NbcArtifact;

fn invalid(message: impl Into<String>) -> FormatError {
    FormatError::BodyDecode(format!("structural verification: {}", message.into()))
}

fn check_pc(kind: &str, offset: usize, instruction_count: usize) -> Result<(), FormatError> {
    if offset >= instruction_count {
        return Err(invalid(format!(
            "{kind} offset {offset} is outside {instruction_count} instructions"
        )));
    }
    Ok(())
}

fn check_index(kind: &str, index: usize, length: usize) -> Result<(), FormatError> {
    if index >= length {
        return Err(invalid(format!("{kind} index {index} is outside {length} entries")));
    }
    Ok(())
}

fn check_string_constant(
    module: &CodeModule,
    opcode: OpCode,
    index: usize,
    pc: usize,
) -> Result<(), FormatError> {
    match module.constants.get(index) {
        Some(Constant::String(_)) => Ok(()),
        _ => Err(invalid(format!(
            "{opcode:?} at pc {pc} requires a string constant at index {index}"
        ))),
    }
}

/// Check structural references of a decoded bytecode module before execution.
///
/// This pass is strictly additive to the versioned NBC decoder. It validates
/// references that can be checked independently of runtime actor state and
/// without executing user code. Unknown future semantics must not be inferred
/// from this pass; consumers requiring untrusted-code isolation still need the
/// sandboxed WASM path and independent resource/capability limits.
pub fn verify_module_structure(module: &CodeModule) -> Result<(), FormatError> {
    let instruction_count = module.instructions.len();

    if let Some(entry) = module.entry_point {
        check_pc("entry point", entry, instruction_count)?;
    }

    if !module.function_local_counts.is_empty()
        && module.function_local_counts.len() != module.function_table.len()
    {
        return Err(invalid(format!(
            "function local counts length {} does not match function table length {}",
            module.function_local_counts.len(),
            module.function_table.len()
        )));
    }

    for (index, &offset) in module.function_table.iter().enumerate() {
        check_pc(&format!("function #{index}"), offset, instruction_count)?;
    }

    for (index, behavior) in module.behaviors.iter().enumerate() {
        check_pc(
            &format!("behavior #{index}"),
            behavior.code_offset,
            instruction_count,
        )?;
        if let Some(offset) = behavior.compensate_offset {
            check_pc(
                &format!("behavior #{index} compensation"),
                offset,
                instruction_count,
            )?;
        }
    }

    for (table_index, table) in module.handler_tables.iter().enumerate() {
        if let Some(offset) = table.fallback_offset {
            check_pc(
                &format!("handler table #{table_index} fallback"),
                offset,
                instruction_count,
            )?;
        }
        for (binding_index, binding) in table.bindings.iter().enumerate() {
            check_pc(
                &format!("handler table #{table_index} binding #{binding_index}"),
                binding.handler_offset,
                instruction_count,
            )?;
        }
    }

    for (index, constant) in module.constants.iter().enumerate() {
        match constant {
            Constant::FunctionRef(function_index) => {
                check_index(
                    &format!("constant #{index} function reference"),
                    *function_index,
                    module.function_table.len(),
                )?;
            }
            Constant::BehaviorRef(behavior_index) => {
                check_index(
                    &format!("constant #{index} behavior reference"),
                    *behavior_index,
                    module.behaviors.len(),
                )?;
            }
            _ => {}
        }
    }

    for (pc, instruction) in module.instructions.iter().enumerate() {
        match instruction.opcode {
            OpCode::Jmp | OpCode::JmpT | OpCode::JmpF => {
                // VM executes a branch relative to the opcode's own PC. Its
                // frame PC is pre-incremented before applying (offset - 1).
                let offset = if instruction.opcode == OpCode::Jmp {
                    instruction.simm16()
                } else {
                    instruction.offset16()
                };
                let target = (pc as i128) + i128::from(offset);
                if target < 0 || target >= instruction_count as i128 {
                    return Err(invalid(format!(
                        "jump target {target} from pc {pc} is outside {instruction_count} instructions"
                    )));
                }
            }
            OpCode::ConstU => {
                check_index(
                    &format!("ConstU constant at pc {pc}"),
                    instruction.imm16() as usize,
                    module.constants.len(),
                )?;
            }
            OpCode::Closure => {
                check_index(
                    &format!("Closure function at pc {pc}"),
                    instruction.imm16() as usize,
                    module.function_table.len(),
                )?;
            }
            OpCode::Handle => {
                check_index(
                    &format!("Handle handler table at pc {pc}"),
                    instruction.op1 as usize,
                    module.handler_tables.len(),
                )?;
            }
            OpCode::PerformDirect => {
                let table_index = instruction.op1 as usize;
                check_index(
                    &format!("PerformDirect handler table at pc {pc}"),
                    table_index,
                    module.handler_tables.len(),
                )?;
                check_index(
                    &format!("PerformDirect binding at pc {pc}"),
                    instruction.op2 as usize,
                    module.handler_tables[table_index].bindings.len(),
                )?;
            }
            OpCode::Perform
            | OpCode::StateGet
            | OpCode::StateSet
            | OpCode::Emit
            | OpCode::SignalWait => {
                check_string_constant(module, instruction.opcode, instruction.imm16() as usize, pc)?;
            }
            _ => {}
        }
    }
    Ok(())
}

impl CodeModule {
    /// Decode an NBC artifact and reject invalid structural references.
    ///
    /// Unlike `from_nbc`, this opts into additional restrictions. It cannot
    /// be used as proof of full bytecode/type verification or safe native
    /// execution of untrusted code.
    pub fn from_nbc_with_structural_verification(
        bytes: &[u8],
    ) -> Result<NbcArtifact, FormatError> {
        let artifact = Self::from_nbc(bytes)?;
        verify_module_structure(&artifact.module)?;
        Ok(artifact)
    }
}
