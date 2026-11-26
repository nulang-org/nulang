//! Conservative build-time page rendering. Runtime routes remain in the
//! deployment contract but never produce an HTML artifact merely because
//! their URL has no path parameters.

use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
use crate::runtime::WebRoute;
use crate::types::{NuError, Span};
use crate::vm::VM;
use crate::web::contracts::ContractCompilation;
use crate::web::ir::{DeploymentIr, IrRoute};
use std::collections::BTreeMap;

/// Discover registered routes without executing request, I/O, or mutation
/// effects in the application entry point. Route registration is the only
/// additional operation allowed compared to page rendering.
pub(crate) fn discover_routes(module: CodeModule) -> Result<Vec<WebRoute>, String> {
    let entry = module.entry_point.unwrap_or(0);
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    vm.run_from_checked(0, entry, |module, instr| {
        if instr.opcode == OpCode::Perform
            && matches!(module.constants.get(instr.imm16() as usize),
                Some(Constant::String(name)) if name == "Web.route")
        {
            return Ok(());
        }
        check_instruction(module, instr)
            .map_err(|reason| NuError::runtime_error(reason, Span::default()))
    })
    .map_err(|error| format!("web route registration failed: {error}"))?;
    Ok(vm.take_web_routes())
}

pub(crate) fn render_pages(
    routes: &[WebRoute],
    contracts: &ContractCompilation,
    ir: &mut DeploymentIr,
) -> Result<BTreeMap<String, String>, String> {
    if routes.len() != ir.routes.len() {
        return Err("static build route/IR length mismatch".into());
    }
    let mut literal_gets = BTreeMap::new();
    for metadata in &ir.routes {
        if metadata.method != "GET" {
            continue;
        }
        if let Ok(path) = artifact_path(&metadata.path) {
            if let Some(previous) = literal_gets.insert(path.to_ascii_lowercase(), &metadata.path) {
                return Err(format!(
                    "GET {} conflicts with GET {previous} at '{path}'",
                    metadata.path
                ));
            }
        }
    }
    let mut pages = BTreeMap::new();
    let mut paths = BTreeMap::<String, String>::new();
    for (route, metadata) in routes.iter().zip(&mut ir.routes) {
        let label = format!("{} {}", metadata.method, metadata.path);
        let explicitly_static = contracts.routes.iter().any(|contract| {
            contract.method == metadata.method
                && contract.path == metadata.path
                && contract.placement.as_deref() == Some("static")
        });

        if metadata.placement != "static" {
            continue;
        }
        if let Some(reason) = ineligible(metadata, route) {
            if explicitly_static {
                return Err(format!(
                    "{label}: cannot render @placement(static): {reason}"
                ));
            }
            metadata.placement = "server".into();
            metadata.artifact = None;
            continue;
        }
        let path = artifact_path(&metadata.path)?;
        let html = match render(route)? {
            Rendered::Html(html) => html,
            Rendered::Denied(reason) => {
                if explicitly_static {
                    return Err(format!(
                        "{label}: cannot render @placement(static): {reason}"
                    ));
                }
                metadata.placement = "server".into();
                metadata.artifact = None;
                continue;
            }
        };
        // Compare portable output paths case-insensitively. Reject aliases
        // and file/directory conflicts before writing any generated page.
        let key = path.to_ascii_lowercase();
        for (other, owner) in &paths {
            if key == *other
                || key.starts_with(&format!("{other}/"))
                || other.starts_with(&format!("{key}/"))
            {
                return Err(format!(
                    "{label}: output path '{path}' conflicts with {owner}"
                ));
            }
        }
        for reserved in ["app.client.js", "app.signals.json", "nulang-app.ir.json"] {
            if key == reserved || key.starts_with(&format!("{reserved}/")) {
                return Err(format!(
                    "{label}: output path '{path}' conflicts with {reserved}"
                ));
            }
        }
        paths.insert(key, label);
        metadata.artifact = Some(path.clone());
        pages.insert(
            path,
            crate::web::reactivity::inject_client_runtime_script(&html),
        );
    }
    Ok(pages)
}

fn ineligible(route: &IrRoute, executable: &WebRoute) -> Option<&'static str> {
    if route.method != "GET" {
        Some("only GET handlers produce static pages")
    } else if !route.params.is_empty()
        || route.path.contains(':')
        || route.path.contains('{')
        || route.path.contains('}')
    {
        Some("parameterized routes require explicit static parameter enumeration")
    } else if route.handler.is_none() {
        Some("handler contract is unresolved")
    } else if !route.handler_params.is_empty() || !route.bindings.is_empty() {
        Some("handler depends on request arguments")
    } else if !executable
        .handler_module
        .function_table
        .get(executable.handler_func_idx)
        .is_some_and(|offset| {
            executable
                .handler_module
                .debug_functions
                .iter()
                .any(|function| function.code_offset == *offset && function.params.is_empty())
        })
    {
        Some("compiled handler is not known to accept zero arguments")
    } else if route
        .response_type
        .as_deref()
        .is_some_and(|ty| !matches!(ty, "Html" | "String"))
    {
        Some("declared response type is not HTML or text")
    } else if route.effects.iter().any(|effect| {
        !matches!(
            effect.as_str(),
            "Web" | "Render" | "Array" | "String" | "Int" | "Float"
        )
    }) {
        Some("handler has declared non-render effects")
    } else {
        None
    }
}

// A deliberately narrow, portable filesystem mapping. Percent-encoded paths,
// traversal, Windows separators/drive prefixes, and empty segments must not
// become filenames. A single trailing slash is normalized.
fn artifact_path(path: &str) -> Result<String, String> {
    let relative = path
        .strip_prefix('/')
        .ok_or_else(|| format!("invalid static route path '{path}'"))?;
    if relative.is_empty() {
        return Ok("index.html".into());
    }
    let relative = relative.strip_suffix('/').unwrap_or(relative);
    if relative.split('/').any(|segment| {
        segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.ends_with('.')
            || segment
                .bytes()
                .any(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'-' | b'_' | b'.' | b'~'))
            || matches!(
                segment
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase()
                    .as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
    }) {
        return Err(format!("invalid static route path '{path}'"));
    }
    Ok(format!("{relative}/index.html"))
}

enum Rendered {
    Html(String),
    Denied(String),
}

fn render(route: &WebRoute) -> Result<Rendered, String> {
    let label = format!("{} {}", route.method.as_str(), route.path);
    let index = route.handler_func_idx;
    if index > u16::MAX as usize || index >= route.handler_module.function_table.len() {
        return Err(format!("{label}: invalid static handler index {index}"));
    }
    let mut module = route.handler_module.clone();
    let entry = module.instructions.len();
    module.emit(Instruction::new3(
        OpCode::Closure,
        (index >> 8) as u8,
        index as u8,
        0,
    ));
    module.emit(Instruction::new3(OpCode::ClosureCall, 0, 0, 0));
    module.emit(Instruction::new0(OpCode::Ret));
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    let mut denied = None;
    let result = vm.run_from_checked(0, entry, |module, instr| {
        if let Err(reason) = check_instruction(module, instr) {
            denied = Some(reason.clone());
            return Err(NuError::runtime_error(reason, Span::default()));
        }
        Ok(())
    });
    if let Some(reason) = denied {
        return Ok(Rendered::Denied(reason));
    }
    let value = result.map_err(|error| format!("{label}: static render failed: {error}"))?;
    let html = vm
        .string_operand(0, value)
        .ok_or_else(|| format!("{label}: static render did not return a string"))?;
    Ok(Rendered::Html(html))
}

// Fail closed for unfamiliar operations. The actual interpreted instruction
// stream is checked across function/closure calls, not merely the handler's
// declared effect row. This is a build policy, not an untrusted-code sandbox.
fn check_instruction(module: &CodeModule, instr: &Instruction) -> Result<(), String> {
    use OpCode::*;
    match instr.opcode {
        Nop | Halt | Panic | Const0 | Const1 | Const2 | ConstM1 | ConstU | ConstL | Load
        | Store | Move | Pop | Dup | Swap | IAdd | ISub | IMul | IDiv | IMod | INeg | IInc
        | IDec | IPow | Xor | Shl | Shr | BitAnd | BitOr | FAdd | FSub | FMul | FDiv | FNeg
        | FMod | IToF | FToI | FToS | FPow | ICmpEq | ICmpLt | ICmpGt | ICmpLe | ICmpGe
        | FCmpEq | FCmpLt | FCmpGt | SCmpEq | Not | And | Or | Jmp | JmpT | JmpF | Switch
        | Call | TailCall | Ret | RetVal | Closure | CapLoad | CapStore | ClosureCall | Alloc
        | FieldL | FieldS | ArrAlloc | ArrLoad | ArrStore | ArrLen | TupleMk | TupleL | RecMk
        | RecL | RecS | IsTag | Unpack | RecCopy | Copy | Drop | SConcat | MetaType | MetaCap
        | SpillLoad | SpillStore => Ok(()),
        Perform => {
            let operation = module.constants.get(instr.imm16() as usize);
            match operation {
                Some(Constant::String(name)) if allowed_effect(name) => Ok(()),
                Some(Constant::String(name)) => {
                    Err(format!("build-time rendering disallows {name}"))
                }
                _ => Err("build-time rendering encountered an invalid effect operand".into()),
            }
        }
        other => Err(format!("build-time rendering disallows opcode {other:?}")),
    }
}

fn allowed_effect(name: &str) -> bool {
    matches!(
        name,
        "Web.html"
            | "Web.text"
            | "Web.raw"
            | "Array.length"
            | "Array.push"
            | "Array.new"
            | "Array.set"
            | "Array.slice"
            | "Int.to_string"
            | "Int.to_float"
            | "Int.to_hex"
            | "Int.to_binary"
            | "Float.to_int"
            | "Float.to_string"
            | "Float.sin"
            | "Float.cos"
            | "Float.sqrt"
            | "Float.tan"
            | "Float.log"
            | "Float.exp"
            | "Float.log2"
            | "Float.log10"
            | "Float.pow"
            | "String.to_int"
            | "String.to_float"
            | "String.length"
            | "String.charAt"
            | "String.from_char"
            | "String.concat"
            | "String.substring"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_paths_are_portable_and_contained() {
        assert_eq!(artifact_path("/").unwrap(), "index.html");
        assert_eq!(artifact_path("/docs/").unwrap(), "docs/index.html");
        for path in [
            "docs",
            "//",
            "/../outside",
            "/a/./b",
            "/a//b",
            "/a\\b",
            "/a%2fb",
            "/C:/tmp",
            "/CON",
        ] {
            assert!(artifact_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn effect_policy_denies_ambient_and_external_operations() {
        for name in [
            "Web.cookie",
            "Web.method",
            "Web.kv_get",
            "Web.kv_set",
            "Web.serve_static",
            "FS.read",
            "IO.print",
            "Http.get",
            "Time.now",
            "Rand.int",
        ] {
            let mut module = CodeModule::new("test");
            module.constants.push(Constant::String(name.into()));
            assert!(
                check_instruction(&module, &Instruction::new3(OpCode::Perform, 0, 0, 0)).is_err(),
                "{name}"
            );
        }
        for opcode in [
            OpCode::FFICall,
            OpCode::Spawn,
            OpCode::SRead,
            OpCode::PerformAsync,
        ] {
            assert!(
                check_instruction(&CodeModule::new("test"), &Instruction::new0(opcode)).is_err()
            );
        }
    }
}
