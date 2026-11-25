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
    if result.is_nil() {
        return Err("route handler returned nil instead of a renderable response".into());
    }
    Ok(vm.value_to_string(0, result))
}

/// A VM materialization plan for a *trusted, already validated* host value.
/// The argument type must be checked against a compiler binding contract
/// before using this low-level trampoline.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedWebValue {
    Scalar(crate::bytecode::Constant),
    /// None is a nullary variant string; Some(payload) is a compiler-layout
    /// record. Neither can be replaced with a JSON or empty-string sentinel.
    OptionScalar(Option<crate::bytecode::Constant>),
    /// A real VM-owned array of scalar constants, in submitted order.
    ArrayScalars(Vec<crate::bytecode::Constant>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypedBoundRouteArgument {
    pub handler_index: usize,
    pub value: TypedWebValue,
}

/// Execute a typed handler with true variant and heap-array representations.
///
/// The record-field layout must be obtained from *the same compiler run* as
/// the bytecode. This does not accept old, separately deserialized modules;
/// the frozen bytecode format is unchanged. Route registration must separately
/// validate every declared handler type and data shape.
pub fn render_typed_bound_handler(
    compiled: &crate::mir_codegen::CompiledWebModule,
    func_idx: usize,
    handler_param_count: usize,
    args: &[TypedBoundRouteArgument],
) -> Result<String, String> {
    // The MIR compiler reserves r0..r11 for call arguments.
    if handler_param_count > 12 || args.len() != handler_param_count {
        return Err(
            "typed web handler argument bank exceeds the 12-register staging ABI or is incomplete"
                .into(),
        );
    }
    if func_idx >= compiled.module().function_table.len() || func_idx > u16::MAX as usize {
        return Err("typed web handler function index is invalid".into());
    }
    let mut seen = vec![false; handler_param_count];
    for arg in args {
        if arg.handler_index >= handler_param_count || seen[arg.handler_index] {
            return Err("typed web handler has duplicate or out-of-range argument slot".into());
        }
        seen[arg.handler_index] = true;
    }
    if seen.iter().any(|occupied| !occupied) {
        return Err("typed web handler has an unbound argument slot".into());
    }

    let mut executable = compiled.module().clone();
    let entry_offset = executable.instructions.len();
    const INDEX_REG: u8 = 250;
    const ELEM_REG: u8 = 251;
    const TAG_REG: u8 = 252;
    const LENGTH_REG: u8 = 253;

    // All argument slots are constructed before the VM handler call. Check
    // against the compiler's actual MIR signature, not an untrusted form
    // annotation or an assumption that all elements share a type.
    for arg in args {
        let parameter_type = compiled
            .handler_param_type(func_idx, arg.handler_index)
            .ok_or("compiler typed web ABI is missing a handler parameter type")?;
        if !typed_value_conforms(&arg.value, parameter_type) {
            return Err("typed web value does not match compiled handler parameter type".into());
        }
        let dst = arg.handler_index as u8;
        match &arg.value {
            TypedWebValue::Scalar(value) => emit_typed_const(&mut executable, value, dst)?,
            TypedWebValue::OptionScalar(None) => {
                emit_typed_const(
                    &mut executable,
                    &crate::bytecode::Constant::String("None".into()),
                    dst,
                )?;
            }
            TypedWebValue::OptionScalar(Some(value)) => {
                let ctor = compiled
                    .record_field_id("ctor")
                    .ok_or("compiler field layout is missing Option ctor")?;
                let payload = compiled
                    .record_field_id("payload")
                    .ok_or("compiler field layout is missing Option payload")?;
                if ctor == payload {
                    return Err("compiler Option record field IDs collide".into());
                }
                let max_id = ctor.max(payload);
                let slots = max_id
                    .checked_add(1)
                    .ok_or("compiler Option record slot count exceeds bytecode u8 range")?;
                validate_typed_constant(value)?;
                executable.emit(Instruction::new2(OpCode::RecMk, slots, dst));
                emit_typed_const(
                    &mut executable,
                    &crate::bytecode::Constant::String("Some".into()),
                    TAG_REG,
                )?;
                executable.emit(Instruction::new3(OpCode::RecS, dst, ctor, TAG_REG));
                emit_typed_const(&mut executable, value, ELEM_REG)?;
                executable.emit(Instruction::new3(OpCode::RecS, dst, payload, ELEM_REG));
            }
            TypedWebValue::ArrayScalars(values) => {
                if values.len() > 64 {
                    return Err("typed web array exceeds 64 element form limit".into());
                }
                for value in values {
                    validate_typed_constant(value)?;
                }
                emit_typed_const(
                    &mut executable,
                    &crate::bytecode::Constant::Int(values.len() as i64),
                    LENGTH_REG,
                )?;
                executable.emit(Instruction::new2(OpCode::ArrAlloc, LENGTH_REG, dst));
                for (index, value) in values.iter().enumerate() {
                    emit_typed_const(
                        &mut executable,
                        &crate::bytecode::Constant::Int(index as i64),
                        INDEX_REG,
                    )?;
                    emit_typed_const(&mut executable, value, ELEM_REG)?;
                    executable.emit(Instruction::new3(
                        OpCode::ArrStore,
                        dst,
                        INDEX_REG,
                        ELEM_REG,
                    ));
                }
            }
        }
    }
    let closure_reg = handler_param_count as u8;
    executable.emit(Instruction::new3(
        OpCode::Closure,
        ((func_idx >> 8) & 0xff) as u8,
        (func_idx & 0xff) as u8,
        closure_reg,
    ));
    executable.emit(Instruction::new3(
        OpCode::ClosureCall,
        closure_reg,
        handler_param_count as u8,
        0,
    ));
    executable.emit(Instruction::new0(OpCode::Ret));
    let mut vm = VM::new();
    vm.load_module(executable);
    let result = vm
        .run_from(0, entry_offset)
        .map_err(|error| format!("typed web handler execution failed: {error}"))?;
    Ok(vm.value_to_string(0, result))
}

/// Check an argument against the compiler-owned MIR parameter type, not
/// solely against HTTP form metadata. Reject unresolved types and nested
/// structures until there is a verified runtime materializer for them.
fn typed_value_conforms(value: &TypedWebValue, expected: &crate::types::Type) -> bool {
    use crate::types::Type;
    match (value, expected) {
        (TypedWebValue::Scalar(value), expected) => scalar_conforms(value, expected),
        (TypedWebValue::OptionScalar(value), Type::Variant(constructors)) => {
            if constructors.len() != 2 {
                return false;
            }
            let some = constructors.iter().find(|(name, _)| name == "Some");
            let none = constructors.iter().find(|(name, _)| name == "None");
            match (some, none) {
                (Some((_, Some(payload_type))), Some((_, None))) => {
                    primitive_type_supported(payload_type)
                        && value
                            .as_ref()
                            .is_none_or(|value| scalar_conforms(value, payload_type))
                }
                _ => false,
            }
        }
        (TypedWebValue::ArrayScalars(values), Type::Array(element_type)) => {
            primitive_type_supported(element_type)
                && values
                    .iter()
                    .all(|value| scalar_conforms(value, element_type))
        }
        _ => false,
    }
}

fn primitive_type_supported(expected: &crate::types::Type) -> bool {
    use crate::types::{PrimitiveType as P, Type};
    matches!(
        expected,
        Type::Primitive(P::String | P::Int | P::Float | P::Bool)
    )
}

fn scalar_conforms(value: &crate::bytecode::Constant, expected: &crate::types::Type) -> bool {
    use crate::bytecode::Constant;
    use crate::types::{PrimitiveType as P, Type};
    matches!(
        (value, expected),
        (Constant::String(_), Type::Primitive(P::String))
            | (Constant::Int(_), Type::Primitive(P::Int))
            | (Constant::Float(_), Type::Primitive(P::Float))
            | (Constant::Bool(_), Type::Primitive(P::Bool))
    )
}

fn validate_typed_constant(value: &crate::bytecode::Constant) -> Result<(), String> {
    use crate::bytecode::Constant;
    match value {
        Constant::String(_) | Constant::Bool(_) => Ok(()),
        Constant::Float(v) if v.is_finite() => Ok(()),
        Constant::Int(v) if crate::value_layout::int48_in_range(*v) => Ok(()),
        _ => Err("typed web arguments require a representable scalar value".into()),
    }
}

fn emit_typed_const(
    module: &mut CodeModule,
    value: &crate::bytecode::Constant,
    dst: u8,
) -> Result<(), String> {
    validate_typed_constant(value)?;
    let index = module.add_constant(value.clone());
    if index > u16::MAX as usize {
        return Err("typed web constant pool exceeds u16 operand range".into());
    }
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((index >> 8) & 0xff) as u8,
        (index & 0xff) as u8,
        dst,
    ));
    Ok(())
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
        }
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
