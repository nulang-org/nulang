//! Source-agnostic VM execution for compiler-bound web handler arguments.
//!
//! Request decoding determines *what* values populate handler slots. This module
//! owns only the VM ABI step: validate a complete argument bank, materialize it
//! into registers, call the non-capturing handler, and render the result.

use crate::bytecode::{CodeModule, Instruction, OpCode};
use crate::vm::VM;
use crate::web::runtime_bindings::BoundRouteArgument;

/// Execute a web handler using already-decoded compiler-bound arguments.
///
/// Arguments may originate from path, query, headers, cookies, body, form, or
/// future transports; only `handler_index` matters at this layer.
pub fn render_bound_handler(
    module: &CodeModule,
    func_idx: usize,
    handler_param_count: usize,
    args: &[BoundRouteArgument],
) -> Result<String, String> {
    validate_argument_bank(handler_param_count, args)?;

    let mut vm = VM::new();
    let mut executable = module.clone();
    let entry_offset = executable.instructions.len();

    for arg in args {
        if let Some(fields) = &arg.record_fields {
            if executable.record_field_names.is_empty()
                && executable
                    .instructions
                    .iter()
                    .any(|instruction| matches!(instruction.opcode, OpCode::RecL | OpCode::RecS))
            {
                return Err("compiled record field-layout metadata is missing; refusing unsafe JSON DTO injection".to_string());
            }
            // Compiler record fields are indexed module-wide, not in the
            // source DTO's local declaration order. Preserve those IDs.
            let mut field_indices = Vec::with_capacity(fields.len());
            for (name, _) in fields {
                let index = match executable
                    .record_field_names
                    .iter()
                    .position(|field| field == name)
                {
                    Some(index) => index,
                    None => {
                        executable.record_field_names.push(name.clone());
                        executable.record_field_names.len() - 1
                    }
                };
                field_indices.push(index);
            }
            let slot_count = executable.record_field_names.len();
            if slot_count > u8::MAX as usize {
                return Err("JSON record exceeds the VM's 255-slot record ABI".to_string());
            }
            let scratch = handler_param_count
                .checked_add(1)
                .filter(|slot| *slot <= u8::MAX as usize)
                .ok_or("JSON record requires an available scratch VM register")?
                as u8;
            executable.emit(Instruction::new2(
                OpCode::RecMk,
                slot_count as u8,
                arg.handler_index as u8,
            ));
            for ((_, field_value), field_idx) in fields.iter().zip(field_indices) {
                let constant_index = executable.add_constant(field_value.clone());
                if constant_index > u16::MAX as usize {
                    return Err(
                        "route argument constant pool exceeds 16-bit ConstU index".to_string()
                    );
                }
                executable.emit(Instruction::new3(
                    OpCode::ConstU,
                    ((constant_index >> 8) & 0xFF) as u8,
                    (constant_index & 0xFF) as u8,
                    scratch,
                ));
                executable.emit(Instruction::new3(
                    OpCode::RecS,
                    arg.handler_index as u8,
                    field_idx as u8,
                    scratch,
                ));
            }
        } else {
            let constant_index = executable.add_constant(arg.value.clone());
            if constant_index > u16::MAX as usize {
                return Err("route argument constant pool exceeds 16-bit ConstU index".to_string());
            }
            executable.emit(Instruction::new3(
                OpCode::ConstU,
                ((constant_index >> 8) & 0xFF) as u8,
                (constant_index & 0xFF) as u8,
                arg.handler_index as u8,
            ));
        }
    }

    let closure_reg = handler_param_count as u8;
    if func_idx > u16::MAX as usize {
        return Err(format!(
            "handler function index {func_idx} exceeds the 16-bit closure operand"
        ));
    }
    executable.emit(Instruction::new3(
        OpCode::Closure,
        ((func_idx >> 8) & 0xFF) as u8,
        (func_idx & 0xFF) as u8,
        closure_reg,
    ));
    executable.emit(Instruction::new3(
        OpCode::ClosureCall,
        closure_reg,
        handler_param_count as u8,
        0,
    ));
    executable.emit(Instruction::new0(OpCode::Ret));

    vm.load_module(executable);
    let result = vm
        .run_from(0, entry_offset)
        .map_err(|error| format!("route handler execution failed: {error}"))?;
    Ok(vm.value_to_string(0, result))
}

fn validate_argument_bank(
    handler_param_count: usize,
    args: &[BoundRouteArgument],
) -> Result<(), String> {
    if handler_param_count > u8::MAX as usize {
        return Err(format!(
            "route handler has {handler_param_count} parameters; VM call ABI supports at most {}",
            u8::MAX
        ));
    }
    if args.len() != handler_param_count {
        return Err(format!(
            "handler expects {handler_param_count} bound arguments, got {}",
            args.len()
        ));
    }

    let mut occupied = vec![false; handler_param_count];
    for arg in args {
        if arg.handler_index >= handler_param_count {
            return Err(format!(
                "bound argument targets handler slot {} but handler has {} parameters",
                arg.handler_index, handler_param_count
            ));
        }
        if occupied[arg.handler_index] {
            return Err(format!(
                "multiple bound arguments target handler slot {}",
                arg.handler_index
            ));
        }
        occupied[arg.handler_index] = true;
    }

    if let Some(index) = occupied.iter().position(|occupied| !occupied) {
        return Err(format!(
            "handler parameter slot {index} has no bound argument"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Constant;

    fn arg(index: usize) -> BoundRouteArgument {
        BoundRouteArgument {
            handler_index: index,
            value: Constant::Int(index as i64),
            record_fields: None,
        }
    }

    #[test]
    fn legacy_record_bytecode_without_field_layout_fails_closed() {
        let mut module = CodeModule::new("old-record-bytecode");
        module.emit(Instruction::new3(OpCode::RecL, 0, 0, 1));
        let arg = BoundRouteArgument {
            handler_index: 0,
            value: Constant::Nil,
            record_fields: Some(vec![("name".into(), Constant::String("Ada".into()))]),
        };
        let error = render_bound_handler(&module, 0, 1, &[arg]).unwrap_err();
        assert!(
            error.contains("field-layout metadata is missing"),
            "{error}"
        );
    }

    #[test]
    fn accepts_complete_argument_bank_in_any_order() {
        assert!(validate_argument_bank(3, &[arg(2), arg(0), arg(1)]).is_ok());
    }

    #[test]
    fn rejects_duplicate_and_missing_slots() {
        let duplicate = validate_argument_bank(2, &[arg(0), arg(0)]).unwrap_err();
        assert!(duplicate.contains("multiple bound arguments"));

        let missing = validate_argument_bank(2, &[arg(0)]).unwrap_err();
        assert!(missing.contains("expects 2 bound arguments"));
    }

    #[test]
    fn rejects_out_of_range_slots_and_abi_overflow() {
        assert!(validate_argument_bank(1, &[arg(1)])
            .unwrap_err()
            .contains("targets handler slot 1"));
        assert!(validate_argument_bank(256, &[])
            .unwrap_err()
            .contains("supports at most 255"));
    }
}
