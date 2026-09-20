//! Static mechanical-cost analysis for compiled Nulang bytecode.
//!
//! The VM deliberately keeps many high-level operations compact, which can
//! hide expensive work (allocation, copying, suspension, FFI, or distribution)
//! behind a single opcode. This module makes those costs inspectable without
//! coupling diagnostics to a particular VM implementation.
//!
//! The analysis is conservative and structural: it counts bytecode sites, not
//! dynamic execution frequency. A loop containing one allocation opcode has one
//! allocation *site* even if it allocates millions of times at runtime.

use crate::bytecode::{CodeModule, OpCode};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MechanicalCostSummary {
    pub instructions: usize,
    pub allocation_sites: usize,
    pub heap_allocation_sites: usize,
    pub string_materialization_sites: usize,
    pub copy_sites: usize,
    pub call_sites: usize,
    pub branch_sites: usize,
    pub effect_boundaries: usize,
    pub suspension_points: usize,
    pub ffi_boundaries: usize,
    pub actor_operations: usize,
    pub distributed_operations: usize,
    pub io_operations: usize,
}

impl MechanicalCostSummary {
    fn observe(&mut self, opcode: OpCode) {
        use OpCode::*;

        self.instructions += 1;

        // Explicit VM heap allocations. RecCopy is both an allocation and a
        // shallow copy. Closure itself is an immediate tagged value in the
        // current bytecode VM, so it is intentionally not counted here.
        if matches!(opcode, Alloc | ArrAlloc | TupleMk | RecMk | RecCopy) {
            self.heap_allocation_sites += 1;
            self.allocation_sites += 1;
        }

        // These materialize owned strings in the current interpreters. Keep
        // them separate from VM object allocations so callers can distinguish
        // object-heap pressure from host/string allocation pressure.
        if matches!(opcode, FToS | SConcat) {
            self.string_materialization_sites += 1;
            self.allocation_sites += 1;
        }

        if matches!(opcode, Copy | RecCopy) {
            self.copy_sites += 1;
        }

        if matches!(opcode, Call | TailCall | ClosureCall) {
            self.call_sites += 1;
        }

        if matches!(opcode, Jmp | JmpT | JmpF | Switch) {
            self.branch_sites += 1;
        }

        if matches!(
            opcode,
            Perform | PerformDirect | PerformAsync | Handle | Resume | Unwind
        ) {
            self.effect_boundaries += 1;
        }

        if matches!(
            opcode,
            Receive | ReceiveWait | SignalWait | PerformAsync | Ask | RAsk
        ) {
            self.suspension_points += 1;
        }

        if matches!(
            opcode,
            FFICall | PyImport | PyGetAttr | PyCall | PyCallKw | PySetAttr | PyToNu
                | PyFromNu | PyRelease
        ) {
            self.ffi_boundaries += 1;
        }

        if matches!(
            opcode,
            Spawn
                | Send
                | Ask
                | SelfOp
                | Receive
                | Monitor
                | Demon
                | Link
                | Unlink
                | Exit
                | Yield
                | StateGet
                | StateSet
                | Emit
                | SignalWait
                | ReceiveMatch
                | ReceiveWait
                | ReceiveCommit
        ) {
            self.actor_operations += 1;
        }

        if matches!(opcode, Migrate | RSend | RAsk | RSpawn | Gossip) {
            self.distributed_operations += 1;
        }

        if matches!(opcode, SPrint | SRead | FOpen | FRead | FWrite | FClose | Print) {
            self.io_operations += 1;
        }
    }

    pub fn is_allocation_free(&self) -> bool {
        self.allocation_sites == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AllocationSite {
    pub pc: usize,
    pub opcode: String,
    pub function: Option<String>,
    pub source_line: Option<u32>,
    pub class: AllocationClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationClass {
    HeapObject,
    StringMaterialization,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FunctionCost {
    pub name: String,
    pub code_offset: usize,
    pub code_len: usize,
    pub summary: MechanicalCostSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MechanicalCostReport {
    pub module: String,
    pub total: MechanicalCostSummary,
    pub functions: Vec<FunctionCost>,
    pub unattributed: MechanicalCostSummary,
    pub allocation_sites: Vec<AllocationSite>,
}

impl MechanicalCostReport {
    pub fn is_allocation_free(&self) -> bool {
        self.total.is_allocation_free()
    }

    /// Human-readable report intended for the CLI and CI logs.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("mechanical costs: {}\n", self.module));
        out.push_str(&format_summary("total", &self.total));

        if !self.functions.is_empty() {
            out.push_str("\nfunctions:\n");
            for function in &self.functions {
                out.push_str(&format_summary(&function.name, &function.summary));
            }
        }

        if self.unattributed.instructions > 0 {
            out.push_str("\nunattributed bytecode:\n");
            out.push_str(&format_summary("module", &self.unattributed));
        }

        if !self.allocation_sites.is_empty() {
            out.push_str("\nallocation sites:\n");
            for site in &self.allocation_sites {
                let owner = site.function.as_deref().unwrap_or("<module>");
                let line = site
                    .source_line
                    .map(|line| format!(" line {line}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  pc {:>5}  {:<12} {:<22} {}{}\n",
                    site.pc,
                    site.opcode,
                    format!("{:?}", site.class),
                    owner,
                    line
                ));
            }
        }

        out
    }
}

fn format_summary(name: &str, summary: &MechanicalCostSummary) -> String {
    format!(
        "  {:<24} instr={:<5} alloc={:<4} heap={:<4} strings={:<4} copies={:<4} calls={:<4} branches={:<4} effects={:<4} suspend={:<4} ffi={:<4} actor={:<4} dist={:<4} io={}\n",
        name,
        summary.instructions,
        summary.allocation_sites,
        summary.heap_allocation_sites,
        summary.string_materialization_sites,
        summary.copy_sites,
        summary.call_sites,
        summary.branch_sites,
        summary.effect_boundaries,
        summary.suspension_points,
        summary.ffi_boundaries,
        summary.actor_operations,
        summary.distributed_operations,
        summary.io_operations,
    )
}

fn summary_for_range(module: &CodeModule, start: usize, len: usize) -> MechanicalCostSummary {
    let mut summary = MechanicalCostSummary::default();
    let end = start.saturating_add(len).min(module.instructions.len());
    for instruction in &module.instructions[start.min(end)..end] {
        summary.observe(instruction.opcode);
    }
    summary
}

fn function_name_at(module: &CodeModule, pc: usize) -> Option<String> {
    module
        .debug_functions
        .iter()
        .find(|function| {
            pc >= function.code_offset
                && pc < function.code_offset.saturating_add(function.code_len)
        })
        .map(|function| function.name.clone())
}

fn allocation_class(opcode: OpCode) -> Option<AllocationClass> {
    use OpCode::*;
    if matches!(opcode, Alloc | ArrAlloc | TupleMk | RecMk | RecCopy) {
        Some(AllocationClass::HeapObject)
    } else if matches!(opcode, FToS | SConcat) {
        Some(AllocationClass::StringMaterialization)
    } else {
        None
    }
}

/// Analyze a compiled bytecode module.
///
/// Function attribution uses `debug_functions`. Instructions outside all
/// debug ranges remain visible under `unattributed`; this commonly includes
/// module-level `__main` scaffolding in older artifacts.
pub fn analyze_module(module: &CodeModule) -> MechanicalCostReport {
    let total = summary_for_range(module, 0, module.instructions.len());

    let functions = module
        .debug_functions
        .iter()
        .map(|function| FunctionCost {
            name: function.name.clone(),
            code_offset: function.code_offset,
            code_len: function.code_len,
            summary: summary_for_range(module, function.code_offset, function.code_len),
        })
        .collect::<Vec<_>>();

    let mut covered = vec![false; module.instructions.len()];
    for function in &module.debug_functions {
        let end = function
            .code_offset
            .saturating_add(function.code_len)
            .min(covered.len());
        for slot in covered
            .iter_mut()
            .take(end)
            .skip(function.code_offset.min(end))
        {
            *slot = true;
        }
    }

    let mut unattributed = MechanicalCostSummary::default();
    for (pc, instruction) in module.instructions.iter().enumerate() {
        if !covered[pc] {
            unattributed.observe(instruction.opcode);
        }
    }

    let allocation_sites = module
        .instructions
        .iter()
        .enumerate()
        .filter_map(|(pc, instruction)| {
            allocation_class(instruction.opcode).map(|class| AllocationSite {
                pc,
                opcode: format!("{:?}", instruction.opcode),
                function: function_name_at(module, pc),
                source_line: module.line_at(pc),
                class,
            })
        })
        .collect();

    MechanicalCostReport {
        module: module.name.clone(),
        total,
        functions,
        unattributed,
        allocation_sites,
    }
}


#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NoAllocViolation {
    pub function: String,
    pub reason: String,
}

fn noalloc_forbidden_opcode(opcode: OpCode) -> Option<&'static str> {
    use OpCode::*;
    match opcode {
        Alloc | ArrAlloc | TupleMk | RecMk | RecCopy => Some("heap object allocation"),
        FToS | SConcat => Some("string materialization"),
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
        Receive
        | ReceiveWait
        | SignalWait
        | Ask
        | Spawn
        | Send
        | Monitor
        | Demon
        | Link
        | Unlink
        | Exit
        | Yield
        | StateGet
        | StateSet
        | Emit
        | ReceiveMatch
        | ReceiveCommit => Some("actor/suspension boundary may allocate"),
        FFICall
        | PyImport
        | PyGetAttr
        | PyCall
        | PyCallKw
        | PySetAttr
        | PyToNu
        | PyFromNu
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

fn function_bytecode_range(
    module: &CodeModule,
    function_index: usize,
) -> Option<(usize, usize)> {
    let start = *module.function_table.get(function_index)?;
    let info = module
        .debug_functions
        .iter()
        .find(|info| info.code_offset == start)?;
    Some((start, info.code_len))
}

fn direct_noalloc_reason(
    module: &CodeModule,
    function_index: usize,
) -> Option<String> {
    let (start, len) = function_bytecode_range(module, function_index)?;
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
        direct_reason[idx] = direct_noalloc_reason(module, idx);
        let (targets, has_indirect) = direct_calls(func);
        calls[idx] = targets;
        if has_indirect && direct_reason[idx].is_none() {
            direct_reason[idx] = Some(
                "indirect/closure call target cannot be proven allocation-free".to_string(),
            );
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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{CodeModule, DebugFunctionInfo, Instruction};

    #[test]
    fn classifies_allocations_copies_and_boundaries() {
        let mut module = CodeModule::new("cost-test");
        module.emit(Instruction::new2(OpCode::ArrAlloc, 1, 2));
        module.emit(Instruction::new2(OpCode::RecMk, 2, 3));
        module.emit(Instruction::new2(OpCode::RecCopy, 3, 4));
        module.emit(Instruction::new3(OpCode::SConcat, 4, 5, 6));
        module.emit(Instruction::new3(OpCode::Call, 7, 0, 8));
        module.emit(Instruction::new0(OpCode::PerformAsync));
        module.emit(Instruction::new0(OpCode::FFICall));
        module.emit(Instruction::new0(OpCode::RSend));

        let report = analyze_module(&module);
        assert_eq!(report.total.instructions, 8);
        assert_eq!(report.total.heap_allocation_sites, 3);
        assert_eq!(report.total.string_materialization_sites, 1);
        assert_eq!(report.total.allocation_sites, 4);
        assert_eq!(report.total.copy_sites, 1);
        assert_eq!(report.total.call_sites, 1);
        assert_eq!(report.total.effect_boundaries, 1);
        assert_eq!(report.total.suspension_points, 1);
        assert_eq!(report.total.ffi_boundaries, 1);
        assert_eq!(report.total.distributed_operations, 1);
        assert!(!report.is_allocation_free());
        assert_eq!(report.allocation_sites.len(), 4);
    }

    #[test]
    fn attributes_costs_to_debug_function_ranges() {
        let mut module = CodeModule::new("ranges");
        module.emit(Instruction::new2(OpCode::ArrAlloc, 1, 2));
        module.emit(Instruction::new0(OpCode::Ret));
        module.emit(Instruction::new3(OpCode::IAdd, 1, 2, 3));
        module.emit(Instruction::new0(OpCode::Ret));
        module.debug_functions.push(DebugFunctionInfo {
            name: "allocating".into(),
            code_offset: 0,
            code_len: 2,
            params: Vec::new(),
            locals: Vec::new(),
        });
        module.debug_functions.push(DebugFunctionInfo {
            name: "pure".into(),
            code_offset: 2,
            code_len: 2,
            params: Vec::new(),
            locals: Vec::new(),
        });

        let report = analyze_module(&module);
        assert_eq!(report.functions.len(), 2);
        assert_eq!(report.functions[0].summary.allocation_sites, 1);
        assert!(report.functions[1].summary.is_allocation_free());
        assert_eq!(report.unattributed.instructions, 0);
        assert_eq!(
            report.allocation_sites[0].function.as_deref(),
            Some("allocating")
        );
    }

    #[test]
    fn immediate_closure_is_not_reported_as_an_allocation() {
        let mut module = CodeModule::new("closure");
        module.emit(Instruction::new3(OpCode::Closure, 0, 0, 1));
        let report = analyze_module(&module);
        assert!(report.is_allocation_free());
    }
}
