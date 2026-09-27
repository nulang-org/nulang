//! Portable proof engine for the source-level `@noalloc` contract.
//!
//! This module intentionally has no dependency on actor-heap or IsoArena
//! implementation details so the same proof runs in the native compiler and
//! the browser playground. Allocation reporting lives separately in
//! `cost_model`.

use crate::bytecode::{CodeModule, OpCode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoAllocViolation {
    pub function: String,
    pub reason: String,
}

fn noalloc_forbidden_opcode(opcode: OpCode) -> Option<&'static str> {
    use OpCode::*;
    match opcode {
        Alloc | ArrAlloc | TupleMk | RecMk | RecCopy => Some("heap object allocation"),
        FToS | SConcat => Some("string materialization"),
        SCmpEq => Some("string comparison may allocate while the VM materializes owned strings"),
        Panic => Some("panic/error formatting may allocate"),
        // Copy is specified as a deep/capability-aware copy even though parts
        // of the current VM path are conservative/reserved. A no-allocation
        // contract must remain valid when that opcode gains its full semantics.
        Copy => Some("deep copy may allocate"),
        // Capturing closures materialize VM-side environment storage through
        // CapStore. Plain non-capturing Closure values remain immediate.
        CapStore => Some("capturing closure environment"),
        Perform | PerformDirect | PerformAsync | Handle | Resume | Unwind => {
            Some("effect/continuation boundary may allocate")
        }
        Receive | ReceiveWait | SignalWait | Ask | Spawn | Send | Monitor | Demon | Link
        | Unlink | Exit | Yield | StateGet | StateSet | Emit | ReceiveMatch | ReceiveCommit => {
            Some("actor/suspension boundary may allocate")
        }
        FFICall | PyImport | PyGetAttr | PyCall | PyCallKw | PySetAttr | PyToNu | PyFromNu
        | PyRelease => Some("foreign-runtime boundary is not allocation-provable"),
        Migrate | RSend | RAsk | RSpawn | Gossip => {
            Some("distributed-runtime boundary may allocate")
        }
        SPrint | SRead | FOpen | FRead | FWrite | FClose | Print => {
            Some("I/O boundary is not allocation-provable")
        }
        DbgPrint | DbgStack | MetaType | MetaCap => {
            Some("debug/meta operation may materialize runtime data")
        }
        // Spilled locals use frame-side dynamic storage. Treat this as an
        // allocation risk rather than silently weakening @noalloc for very
        // large functions.
        SpillLoad | SpillStore => Some("register spill requires dynamic frame storage"),
        _ => None,
    }
}

fn direct_calls(func: &crate::mir::Function) -> (Vec<usize>, bool) {
    let mut calls = Vec::new();
    let mut has_indirect = false;
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let crate::mir::Stmt::Assign {
                op: crate::mir::RValue::Call { func, .. },
                ..
            } = stmt
            {
                match func {
                    crate::mir::FuncRef::Index(idx) => calls.push(*idx),
                    crate::mir::FuncRef::Local(_) => has_indirect = true,
                }
            }
        }
    }
    (calls, has_indirect)
}

fn function_bytecode_range(module: &CodeModule, function_index: usize) -> Option<(usize, usize)> {
    let start = *module.function_table.get(function_index)?;
    let info = module
        .debug_functions
        .iter()
        .find(|info| info.code_offset == start)?;
    Some((start, info.code_len))
}

fn is_proven_numeric_type(ty: &crate::types::Type) -> bool {
    *ty == crate::types::Type::int() || *ty == crate::types::Type::float()
}

fn mir_hidden_allocation_reason(func: &crate::mir::Function) -> Option<String> {
    for block in &func.blocks {
        for stmt in &block.stmts {
            let crate::mir::Stmt::Assign { op, .. } = stmt else {
                continue;
            };
            if let crate::mir::RValue::Binary(crate::ast::BinOp::Add, left, right) = op {
                let left_ty = func.locals.get(left.0 as usize).map(|local| &local.ty);
                let right_ty = func.locals.get(right.0 as usize).map(|local| &local.ty);
                if !matches!(
                    (left_ty, right_ty),
                    (Some(left_ty), Some(right_ty))
                        if is_proven_numeric_type(left_ty) && is_proven_numeric_type(right_ty)
                ) {
                    return Some(
                        "dynamic addition may allocate a string because operand types are not proven numeric"
                            .to_string(),
                    );
                }
            }
        }
    }
    None
}

fn direct_noalloc_reason(
    func: &crate::mir::Function,
    module: &CodeModule,
    function_index: usize,
) -> Option<String> {
    if let Some(reason) = mir_hidden_allocation_reason(func) {
        return Some(reason);
    }

    let Some((start, len)) = function_bytecode_range(module, function_index) else {
        return Some(format!(
            "missing bytecode range metadata for function-table index {}",
            function_index
        ));
    };
    let end = start.saturating_add(len).min(module.instructions.len());
    for (pc, instruction) in module.instructions[start..end].iter().enumerate() {
        if let Some(reason) = noalloc_forbidden_opcode(instruction.opcode) {
            return Some(format!(
                "{}: {:?} at bytecode pc {}",
                reason,
                instruction.opcode,
                start + pc
            ));
        }
    }
    None
}

/// Run the canonical bytecode mechanical proof for any backend that consumes
/// MIR directly. Backends call this only when a source-level `@noalloc`
/// contract is present, so ordinary compilation pays no duplicate-codegen
/// cost.
///
/// The bytecode proof is intentionally the single source of truth for the
/// language-level contract. Backend-specific code generators may lower the
/// accepted operations differently, but they must not silently weaken what
/// `@noalloc` means.
pub fn prove_noalloc_contracts(mir: &crate::mir::Module) -> crate::types::NuResult<()> {
    if !mir
        .functions
        .iter()
        .chain(mir.behaviors.iter())
        .any(|function| function.no_alloc)
    {
        return Ok(());
    }

    let mut proof_mir = mir.clone();
    crate::mir_codegen::compile_mir(&mut proof_mir, "<noalloc-proof>").map(|_| ())
}

/// Validate source-level `@noalloc` contracts against optimized MIR and the
/// bytecode actually emitted from it.
///
/// The guarantee is intentionally stronger than the standalone
/// `--deny-allocations` report: it rejects semantic operations whose runtime
/// implementation can allocate indirectly (effects, FFI/Python, actor and
/// distributed boundaries, I/O, capturing closure environments, and spills).
/// Direct calls are checked transitively. Indirect/closure calls fail closed
/// because their target cannot be proven allocation-free statically.
///
/// VM call-stack capacity itself is treated as stack machinery rather than a
/// semantic heap allocation; this contract is about program-visible/runtime
/// data allocation, not whether an internal Vec ever grows its reserved
/// capacity.
pub fn validate_noalloc_contracts(
    mir: &crate::mir::Module,
    module: &CodeModule,
) -> Result<(), Vec<NoAllocViolation>> {
    let n = mir.functions.len();
    let mut direct_reason = vec![None::<String>; n];
    let mut calls = vec![Vec::<usize>::new(); n];

    for (idx, func) in mir.functions.iter().enumerate() {
        direct_reason[idx] = direct_noalloc_reason(func, module, idx);
        let (targets, has_indirect) = direct_calls(func);
        calls[idx] = targets;
        if has_indirect && direct_reason[idx].is_none() {
            direct_reason[idx] =
                Some("indirect/closure call target cannot be proven allocation-free".to_string());
        }
    }

    // Least fixed point over the direct call graph. Pure recursion and mutual
    // recursion remain valid when every body is otherwise allocation-free.
    let mut violates = direct_reason
        .iter()
        .map(Option::is_some)
        .collect::<Vec<_>>();
    loop {
        let mut changed = false;
        for idx in 0..n {
            if violates[idx] {
                continue;
            }
            if calls[idx]
                .iter()
                .any(|callee| *callee >= n || violates[*callee])
            {
                violates[idx] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let mut violations = Vec::new();
    for (idx, func) in mir.functions.iter().enumerate() {
        if !func.no_alloc || !violates[idx] {
            continue;
        }

        let reason = direct_reason[idx].clone().unwrap_or_else(|| {
            if let Some(callee) = calls[idx]
                .iter()
                .copied()
                .find(|callee| *callee >= n || violates[*callee])
            {
                if let Some(target) = mir.functions.get(callee) {
                    format!(
                        "calls '{}', whose transitive body is not allocation-free",
                        target.name
                    )
                } else {
                    format!("calls unknown function-table index {}", callee)
                }
            } else {
                "transitive call graph is not allocation-free".to_string()
            }
        });
        violations.push(NoAllocViolation {
            function: func.name.clone(),
            reason,
        });
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}
