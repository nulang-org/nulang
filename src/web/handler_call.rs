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
    render_bound_handler_with_response_type(module, func_idx, handler_param_count, args, None)
}

/// Like render_bound_handler, but serializes the VM result when the
/// compiler explicitly declared a typed JSON response contract.
pub fn render_bound_handler_with_response_type(
    module: &CodeModule,
    func_idx: usize,
    handler_param_count: usize,
    args: &[BoundRouteArgument],
    response_type: Option<&str>,
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
            let root = crate::web::json_records::JsonBoundValue::Record(fields.clone());
            // Collect all names first: every nested RecMk needs a record
            // large enough for *module-wide* field IDs (not field order).
            register_json_record_fields(&mut executable, &root)?;
            let scalar_scratch = handler_param_count
                .checked_add(10)
                .filter(|slot| *slot <= u8::MAX as usize)
                .ok_or("JSON record staging requires ten available VM registers")?
                as u8;
            emit_json_argument(
                &mut executable,
                &root,
                arg.handler_index as u8,
                handler_param_count,
                0,
                scalar_scratch,
            )?;
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

    let record_field_names = executable.record_field_names.clone();
    vm.load_module(executable);
    let result = vm
        .run_from(0, entry_offset)
        .map_err(|error| format!("route handler execution failed: {error}"))?;
    if let Some(ty) = response_type
        .and_then(|ty| ty.strip_prefix("Json["))
        .and_then(|ty| ty.strip_suffix(']'))
    {
        let value = typed_json_value(&vm, result, &record_field_names, ty)?;
        return serde_json::to_string(&value)
            .map_err(|_| "failed to serialize JSON response".to_string());
    }
    Ok(vm.value_to_string(0, result))
}

/// Register all field names before any RecMk, preserving compiler-assigned
/// IDs and ensuring records share the same module-global slot count.
fn register_json_record_fields(
    module: &mut CodeModule,
    value: &crate::web::json_records::JsonBoundValue,
) -> Result<(), String> {
    use crate::web::json_records::JsonBoundValue;
    if module.record_field_names.len() > u8::MAX as usize {
        return Err("JSON record field layout exceeds 255 slots".into());
    }
    if let JsonBoundValue::Record(fields) = value {
        for (name, nested) in fields {
            if !module
                .record_field_names
                .iter()
                .any(|existing| existing == name)
            {
                if module.record_field_names.len() >= u8::MAX as usize {
                    return Err("JSON record field layout exceeds 255 slots".into());
                }
                module.record_field_names.push(name.clone());
            }
            register_json_record_fields(module, nested)?;
        }
    }
    Ok(())
}

/// Materialize a tree into Nulang's record ABI before calling the handler.
/// Each recursion level owns its scratch register; the leaf scalar register
/// is separate, so a child cannot overwrite its parent before RecS.
fn emit_json_argument(
    module: &mut CodeModule,
    value: &crate::web::json_records::JsonBoundValue,
    dst: u8,
    param_count: usize,
    depth: usize,
    scalar_scratch: u8,
) -> Result<(), String> {
    use crate::web::json_records::JsonBoundValue;
    match value {
        JsonBoundValue::Scalar(constant) => {
            let index = module.add_constant(constant.clone());
            if index > u16::MAX as usize {
                return Err("JSON constant pool exceeds VM 16-bit index".into());
            }
            module.emit(Instruction::new3(
                OpCode::ConstU,
                (index >> 8) as u8,
                (index & 255) as u8,
                dst,
            ));
        }
        JsonBoundValue::Record(fields) => {
            if depth > 8 {
                return Err("nested JSON record exceeds depth limit".into());
            }
            module.emit(Instruction::new2(
                OpCode::RecMk,
                module.record_field_names.len() as u8,
                dst,
            ));
            for (name, child) in fields {
                let fid = module
                    .record_field_names
                    .iter()
                    .position(|field| field == name)
                    .ok_or("unregistered JSON record field")? as u8;
                let target = if matches!(child, JsonBoundValue::Scalar(_)) {
                    scalar_scratch
                } else {
                    param_count
                        .checked_add(depth + 1)
                        .filter(|index| *index < scalar_scratch as usize)
                        .ok_or("nested JSON record scratch register overflow")?
                        as u8
                };
                emit_json_argument(
                    module,
                    child,
                    target,
                    param_count,
                    depth + 1,
                    scalar_scratch,
                )?;
                module.emit(Instruction::new3(OpCode::RecS, dst, fid, target));
            }
        }
    }
    Ok(())
}

/// Convert VM values only through their checked runtime representations.
/// In particular, a Record pointer must never reach value_to_string, which
/// treats arbitrary pointers as C strings.
fn typed_json_value(
    vm: &VM,
    value: crate::vm::Value,
    record_names: &[String],
    ty: &str,
) -> Result<serde_json::Value, String> {
    use serde_json::{Map, Number, Value};
    match ty {
        "String" => vm
            .string_operand(0, value)
            .map(Value::String)
            .ok_or_else(|| "typed JSON response expected String".to_string()),
        "Int" => value
            .as_int()
            .map(|number| Value::Number(number.into()))
            .ok_or_else(|| "typed JSON response expected Int".to_string()),
        "Float" => value
            .as_float()
            .filter(|number| number.is_finite())
            .and_then(Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "typed JSON response expected finite Float".to_string()),
        "Bool" => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| "typed JSON response expected Bool".to_string()),
        _ => {
            let crate::web::json_records::JsonType::Record(fields) =
                crate::web::json_records::parse_json_type(ty)
                    .ok_or_else(|| "unsupported typed JSON response schema".to_string())?
            else {
                return Err("expected typed JSON record schema".into());
            };
            let values = vm
                .record_field_values(value)
                .ok_or_else(|| "typed JSON response expected a VM record".to_string())?;
            if record_names.is_empty() {
                return Err("typed JSON response missing compiled record field layout".into());
            }
            let mut object = Map::new();
            for (name, field_ty) in fields {
                let index = record_names
                    .iter()
                    .position(|field| field == &name)
                    .ok_or_else(|| {
                        format!("typed JSON response missing field layout for '{name}'")
                    })?;
                let slot = values.get(index).ok_or_else(|| {
                    format!("typed JSON response field '{name}' is out of bounds")
                })?;
                object.insert(
                    name.clone(),
                    typed_json_value(vm, *slot, record_names, &field_ty.wire_name())
                        .map_err(|_| format!("typed JSON response invalid field '{name}'"))?,
                );
            }
            Ok(Value::Object(object))
        }
    }
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
            record_fields: Some(vec![(
                "name".into(),
                crate::web::json_records::JsonBoundValue::Scalar(Constant::String("Ada".into())),
            )]),
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
