//! Runtime execution support for compiler-produced web route bindings.
//!
//! The compiler owns route syntax and produces [`RouteBindingContract`] values.
//! This module is deliberately transport-agnostic: it accepts already-captured
//! request values and stages them into the VM call ABI (`r0..rN`, `argc`) without
//! re-parsing route patterns or consulting ambient request state.

use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
use crate::vm::VM;
use crate::web::bindings::{RouteBindingContract, RouteBindingSource};
use std::collections::HashMap;

/// One VM argument produced from a compiler binding.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundRouteArgument {
    pub handler_index: usize,
    pub value: Constant,
}

/// Convert captured path values into a complete handler argument vector.
///
/// Every handler slot must have exactly one compiler-produced binding. This is
/// intentionally strict: silently filling an unbound slot with `nil` would turn
/// a compiler contract violation into request-time behavior.
pub fn bind_path_arguments(
    bindings: &[RouteBindingContract],
    handler_param_count: usize,
    path_params: &HashMap<String, String>,
) -> Result<Vec<BoundRouteArgument>, String> {
    if handler_param_count > u8::MAX as usize {
        return Err(format!(
            "route handler has {handler_param_count} parameters; VM call ABI supports at most {}",
            u8::MAX
        ));
    }

    let mut slots: Vec<Option<BoundRouteArgument>> = vec![None; handler_param_count];
    for binding in bindings {
        if binding.source != RouteBindingSource::Path {
            return Err(format!(
                "unsupported route binding source for handler parameter '{}'",
                binding.handler_param
            ));
        }
        if binding.handler_index >= handler_param_count {
            return Err(format!(
                "binding for '{}' targets handler slot {} but handler has {} parameters",
                binding.handler_param, binding.handler_index, handler_param_count
            ));
        }
        if slots[binding.handler_index].is_some() {
            return Err(format!(
                "multiple route bindings target handler slot {}",
                binding.handler_index
            ));
        }

        let raw = path_params.get(&binding.source_name).ok_or_else(|| {
            format!(
                "missing captured path parameter '{}' for handler parameter '{}'",
                binding.source_name, binding.handler_param
            )
        })?;
        let value = decode_path_constant(raw, binding.ty.as_deref()).map_err(|message| {
            format!(
                "path parameter '{}' for handler parameter '{}': {message}",
                binding.source_name, binding.handler_param
            )
        })?;
        slots[binding.handler_index] = Some(BoundRouteArgument {
            handler_index: binding.handler_index,
            value,
        });
    }

    slots
        .into_iter()
        .enumerate()
        .map(|(index, slot)| {
            slot.ok_or_else(|| format!("handler parameter slot {index} has no request binding"))
        })
        .collect()
}

/// Execute a handler with compiler-produced path bindings.
///
/// Arguments are materialized as constants into `r0..rN`, a non-capturing
/// closure for the handler is placed immediately after the argument bank, and
/// `ClosureCall` receives the real argument count. The returned VM value is
/// rendered with the module's normal string representation.
pub fn render_bound_route_handler(
    module: &CodeModule,
    func_idx: usize,
    bindings: &[RouteBindingContract],
    handler_param_count: usize,
    path_params: &HashMap<String, String>,
) -> Result<String, String> {
    let args = bind_path_arguments(bindings, handler_param_count, path_params)?;
    if handler_param_count >= u8::MAX as usize {
        // One additional register is required for the closure itself.
        return Err(format!(
            "route handler has {handler_param_count} parameters; no VM register remains for the call target"
        ));
    }

    let mut vm = VM::new();
    let mut executable = module.clone();
    let entry_offset = executable.instructions.len();

    for arg in args {
        let constant_index = executable.add_constant(arg.value);
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
    Ok(vm.value_to_string(0, result))
}

fn decode_path_constant(raw: &str, ty: Option<&str>) -> Result<Constant, String> {
    match ty.map(str::trim) {
        Some("Int") => raw
            .parse::<i64>()
            .map(Constant::Int)
            .map_err(|_| format!("expected Int, got '{raw}'")),
        Some("Float") => raw
            .parse::<f64>()
            .map(Constant::Float)
            .map_err(|_| format!("expected Float, got '{raw}'")),
        Some("Bool") => match raw {
            "true" => Ok(Constant::Bool(true)),
            "false" => Ok(Constant::Bool(false)),
            _ => Err(format!("expected Bool ('true' or 'false'), got '{raw}'")),
        },
        // `String`, untyped legacy parameters, and source-level aliases/opaque
        // identifier types are string-backed until the typed frontend exposes
        // their runtime representation in the Web Contract IR.
        _ => Ok(Constant::String(raw.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(name: &str, index: usize, ty: Option<&str>) -> RouteBindingContract {
        RouteBindingContract {
            source: RouteBindingSource::Path,
            source_name: name.to_string(),
            handler_param: name.to_string(),
            handler_index: index,
            ty: ty.map(str::to_string),
        }
    }

    #[test]
    fn stages_arguments_by_handler_slot_not_path_order() {
        let bindings = vec![
            binding("org", 1, Some("String")),
            binding("user", 0, Some("Int")),
        ];
        let params = HashMap::from([
            ("org".to_string(), "acme".to_string()),
            ("user".to_string(), "42".to_string()),
        ]);

        let args = bind_path_arguments(&bindings, 2, &params).unwrap();
        assert_eq!(args[0].handler_index, 0);
        assert_eq!(args[0].value, Constant::Int(42));
        assert_eq!(args[1].handler_index, 1);
        assert_eq!(args[1].value, Constant::String("acme".to_string()));
    }

    #[test]
    fn rejects_missing_handler_slots() {
        let bindings = vec![binding("id", 1, Some("String"))];
        let params = HashMap::from([("id".to_string(), "abc".to_string())]);
        let error = bind_path_arguments(&bindings, 2, &params).unwrap_err();
        assert!(error.contains("slot 0"));
    }

    #[test]
    fn rejects_missing_captured_path_values() {
        let bindings = vec![binding("id", 0, Some("String"))];
        let error = bind_path_arguments(&bindings, 1, &HashMap::new()).unwrap_err();
        assert!(error.contains("missing captured path parameter 'id'"));
    }

    #[test]
    fn decodes_supported_primitive_types() {
        assert_eq!(decode_path_constant("7", Some("Int")).unwrap(), Constant::Int(7));
        assert_eq!(
            decode_path_constant("3.5", Some("Float")).unwrap(),
            Constant::Float(3.5)
        );
        assert_eq!(
            decode_path_constant("true", Some("Bool")).unwrap(),
            Constant::Bool(true)
        );
    }

    #[test]
    fn keeps_custom_identifier_types_string_backed() {
        assert_eq!(
            decode_path_constant("usr_123", Some("UserId")).unwrap(),
            Constant::String("usr_123".to_string())
        );
    }

    #[test]
    fn reports_primitive_decode_failures() {
        assert!(decode_path_constant("not-an-int", Some("Int"))
            .unwrap_err()
            .contains("expected Int"));
        assert!(decode_path_constant("1", Some("Bool"))
            .unwrap_err()
            .contains("expected Bool"));
    }
}
