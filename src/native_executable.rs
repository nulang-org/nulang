//! Experimental, deliberately restricted native executable emission.
//!
//! This is *not* the in-process AOT/JIT backend. It emits a relocatable object
//! containing a C ABI `main`, and links that object against the host libc.
//! No Nulang VM, scheduler, compiler or JIT is linked into the result.
//!
//! Only single-basic-block entry programs using literal strings, scalar
//! integer arithmetic, and IO.print/println(string) are accepted. Everything
//! else is rejected rather than lowering with different semantics.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use cranelift::prelude::settings::Configurable;
use cranelift::prelude::*;
use cranelift_module::{DataDescription, Linkage, Module as _};
use cranelift_object::{ObjectBuilder, ObjectModule};

use crate::ast::{BinOp, UnOp};
use crate::bytecode::Constant;
use crate::mir::{self, RValue, Stmt, Terminator};
use crate::types::{NuError, NuResult, Span};

fn error(message: impl Into<String>) -> NuError {
    NuError::VMError {
        msg: format!("native executable unsupported: {}", message.into()),
        span: Span::default(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    String,
    Int,
    Unit,
}

#[derive(Clone, Copy)]
struct Slot {
    value: Value,
    kind: Kind,
    // The restricted straight-line backend has no dynamic Int inputs.
    // Keep each constant's VM-normalized value for compile-time folding.
    constant_int: Option<i64>,
}

fn wrap_int(value: i64) -> i64 {
    crate::value_layout::sext48((value as u64) & crate::value_layout::PAYLOAD_MASK)
}

fn lookup(slots: &[Option<Slot>], id: mir::LocalId) -> NuResult<Slot> {
    slots
        .get(id.0 as usize)
        .and_then(|s| *s)
        .ok_or_else(|| error(format!("uninitialized MIR local {}", id.0)))
}

fn compile_object(mir: &mir::Module) -> NuResult<Vec<u8>> {
    if !mir.behaviors.is_empty()
        || !mir.actor_metadata.is_empty()
        || !mir.compensation_of.is_empty()
        || !mir.parallel_branches_of.is_empty()
        || !mir.foreign_functions.is_empty()
    {
        return Err(error("actors, workflows and FFI require the full runtime"));
    }
    let entry = mir
        .functions
        .iter()
        .find(|f| f.name == "__main")
        .or_else(|| mir.functions.iter().find(|f| f.name == "main"))
        .ok_or_else(|| error("no __main or main entry point"))?;
    if !entry.params.is_empty()
        || !entry.captures.is_empty()
        || !entry.handler_tables.is_empty()
        || entry.blocks.len() != 1
        || entry.blocks[0].id != entry.entry
    {
        return Err(error(
            "entry must be a parameterless, straight-line function",
        ));
    }
    let block = &entry.blocks[0];
    if !matches!(block.terminator, Terminator::Return(_)) {
        return Err(error("branches, loops and non-returning entries"));
    }

    let mut flags = settings::builder();
    flags
        .set("opt_level", "speed_and_size")
        .map_err(|e| error(format!("Cranelift flags: {e}")))?;
    let isa = cranelift_native::builder()
        .map_err(|e| error(format!("native ISA: {e}")))?
        .finish(settings::Flags::new(flags))
        .map_err(|e| error(format!("native ISA: {e}")))?;
    let builder = ObjectBuilder::new(
        isa,
        "nulang_tiny",
        cranelift_module::default_libcall_names(),
    )
    .map_err(|e| error(format!("object builder: {e}")))?;
    let mut module = ObjectModule::new(builder);
    let ptr_ty = module.target_config().pointer_type();

    // Encode constant data before creating the function builder.
    // Reject NUL rather than silently changing IO.print's semantics via puts.
    let mut strings = HashMap::new();
    for stmt in &block.stmts {
        if let Stmt::Assign {
            op: RValue::Const(Constant::String(value)),
            ..
        } = stmt
        {
            if value.as_bytes().contains(&0) {
                return Err(error("embedded NUL in string literal"));
            }
            if !strings.contains_key(value) {
                let name = format!("nulang_string_{}", strings.len());
                let id = module
                    .declare_data(&name, Linkage::Local, false, false)
                    .map_err(|e| error(format!("declare data: {e}")))?;
                let mut data = DataDescription::new();
                let mut bytes = value.as_bytes().to_vec();
                bytes.push(0);
                data.define(bytes.into_boxed_slice());
                module
                    .define_data(id, &data)
                    .map_err(|e| error(format!("define data: {e}")))?;
                strings.insert(value.clone(), id);
            }
        }
    }

    let mut puts_sig = module.make_signature();
    puts_sig.params.push(AbiParam::new(ptr_ty));
    puts_sig.returns.push(AbiParam::new(types::I32));
    let puts = module
        .declare_function("puts", Linkage::Import, &puts_sig)
        .map_err(|e| error(format!("declare puts: {e}")))?;

    let mut signature = module.make_signature();
    signature.returns.push(AbiParam::new(types::I32));
    let main_id = module
        .declare_function("main", Linkage::Export, &signature)
        .map_err(|e| error(format!("declare main: {e}")))?;
    let mut ctx = module.make_context();
    ctx.func.signature = signature;
    let mut fn_ctx = FunctionBuilderContext::new();
    {
        let mut fb = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry_block = fb.create_block();
        fb.switch_to_block(entry_block);
        fb.seal_block(entry_block);
        let puts_ref = module.declare_func_in_func(puts, fb.func);
        let mut slots = vec![None; entry.locals.len()];

        for stmt in &block.stmts {
            let Stmt::Assign { dst, op } = stmt else {
                return Err(error(format!("MIR statement {stmt:?}")));
            };
            let next = match op {
                RValue::Const(Constant::String(s)) => {
                    let data_id = strings.get(s).ok_or_else(|| error("missing string data"))?;
                    let global = module.declare_data_in_func(*data_id, fb.func);
                    Slot {
                        value: fb.ins().symbol_value(ptr_ty, global),
                        kind: Kind::String,
                        constant_int: None,
                    }
                }
                RValue::Const(Constant::Int(n)) => {
                    let value = wrap_int(*n);
                    Slot {
                        value: fb.ins().iconst(types::I64, value),
                        kind: Kind::Int,
                        constant_int: Some(value),
                    }
                }
                RValue::Const(Constant::Unit) => Slot {
                    value: fb.ins().iconst(types::I64, 0),
                    kind: Kind::Unit,
                    constant_int: None,
                },
                RValue::Load(src) => lookup(&slots, *src)?,
                RValue::Unary(UnOp::Neg, src) => {
                    let x = lookup(&slots, *src)?;
                    if x.kind != Kind::Int {
                        return Err(error("unary negation of non-Int"));
                    }
                    let raw = x
                        .constant_int
                        .ok_or_else(|| error("unknown integer value"))?;
                    let result = wrap_int(raw.wrapping_neg());
                    Slot {
                        value: fb.ins().iconst(types::I64, result),
                        kind: Kind::Int,
                        constant_int: Some(result),
                    }
                }
                RValue::Binary(operator, lhs, rhs) => {
                    let a = lookup(&slots, *lhs)?;
                    let b = lookup(&slots, *rhs)?;
                    if a.kind != Kind::Int || b.kind != Kind::Int {
                        return Err(error("binary operation on non-Int"));
                    }
                    let lhs = a
                        .constant_int
                        .ok_or_else(|| error("unknown integer value"))?;
                    let rhs = b
                        .constant_int
                        .ok_or_else(|| error("unknown integer value"))?;
                    let result = wrap_int(match operator {
                        BinOp::Add => lhs.wrapping_add(rhs),
                        BinOp::Sub => lhs.wrapping_sub(rhs),
                        BinOp::Mul => lhs.wrapping_mul(rhs),
                        _ => return Err(error(format!("binary operator {operator:?}"))),
                    });
                    Slot {
                        value: fb.ins().iconst(types::I64, result),
                        kind: Kind::Int,
                        constant_int: Some(result),
                    }
                }
                RValue::Perform {
                    effect,
                    op,
                    args,
                    resolved_handler: None,
                } if effect == "IO" && (op == "print" || op == "println") && args.len() == 1 => {
                    let arg = lookup(&slots, args[0])?;
                    if arg.kind != Kind::String {
                        return Err(error("IO.print argument must be a String"));
                    }
                    fb.ins().call(puts_ref, &[arg.value]);
                    Slot {
                        value: fb.ins().iconst(types::I64, 0),
                        kind: Kind::Unit,
                        constant_int: None,
                    }
                }
                other => return Err(error(format!("MIR operation {other:?}"))),
            };
            let destination = slots
                .get_mut(dst.0 as usize)
                .ok_or_else(|| error(format!("invalid MIR destination {}", dst.0)))?;
            *destination = Some(next);
        }

        if let Terminator::Return(Some(id)) = &block.terminator {
            if lookup(&slots, *id)?.kind != Kind::Unit {
                return Err(error("returning a value requires the VM's result printer"));
            }
        }
        let zero = fb.ins().iconst(types::I32, 0);
        fb.ins().return_(&[zero]);
        fb.finalize();
    }
    module
        .define_function(main_id, &mut ctx)
        .map_err(|e| error(format!("define main: {e}")))?;
    module.clear_context(&mut ctx);
    module
        .finish()
        .emit()
        .map_err(|e| error(format!("emit object: {e}")))
}

/// Write and link an experimental native executable on Linux.
///
/// The resulting binary is one executable file but uses the target's system
/// libc: this is NOT yet a fully static/musl deployment artifact.
#[cfg(target_os = "linux")]
pub fn emit_executable(mir: &mir::Module, output: &Path) -> NuResult<()> {
    let bytes = compile_object(mir)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Resolve the parent before forming linker arguments. A relative output
    // directory such as "-native-output" would otherwise turn the staged
    // object path into an option-like argument for cc/ld.
    let parent = std::fs::canonicalize(parent)
        .map_err(|e| error(format!("resolve output directory: {e}")))?;
    // A private, exclusive directory avoids predictable .o/.tmp names and
    // symlink-following writes in attacker-controlled directories.
    let staging = tempfile::Builder::new()
        .prefix(".nulang-link-")
        .tempdir_in(&parent)
        .map_err(|e| error(format!("create staging directory: {e}")))?;
    let object_path = staging.path().join("main.o");
    let temporary_exe = staging.path().join("main");
    std::fs::write(&object_path, bytes).map_err(|e| error(format!("write object: {e}")))?;

    let result = Command::new("cc")
        .arg(&object_path)
        .arg("-Wl,--gc-sections")
        .arg("-s")
        .arg("-o")
        .arg(&temporary_exe)
        .output()
        .map_err(|e| error(format!("invoke cc: {e}")))?;
    if !result.status.success() {
        return Err(error(format!(
            "host linker failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    // Same-directory rename installs only a fully linked artifact. TempDir
    // removes both the object and failed linker outputs on every exit path.
    std::fs::rename(&temporary_exe, output).map_err(|e| error(format!("install executable: {e}")))
}

#[cfg(not(target_os = "linux"))]
pub fn emit_executable(_mir: &mir::Module, _output: &Path) -> NuResult<()> {
    Err(error("native executable linking is currently Linux-only"))
}

#[cfg(test)]
mod integer_semantics_tests {
    use super::wrap_int;
    use crate::value_layout::{INT48_MAX, INT48_MIN};

    #[test]
    fn integer_arithmetic_wraps_at_signed_48bit_boundary() {
        assert_eq!(wrap_int(INT48_MAX.wrapping_add(1)), INT48_MIN);
        assert_eq!(wrap_int(INT48_MIN.wrapping_sub(1)), INT48_MAX);
        assert_eq!(wrap_int(INT48_MIN.wrapping_neg()), INT48_MIN);
        assert_eq!(wrap_int(INT48_MAX.wrapping_mul(2)), -2);
    }
}
