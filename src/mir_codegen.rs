//! MIR -> Bytecode codegen.
//!
//! Converts the Mid-level IR into the existing `CodeModule` bytecode format,
//! following the same runtime contracts as the stable AST compiler:
//!
//!   - call arguments travel in r0..rN, the callee value in r254;
//!   - closures are `Closure` objects over function-table entries, with
//!     captures stored via `CapStore` and loaded via a `CapLoad` prologue;
//!   - records use module-wide field ids (`RecMk`/`RecS`/`RecL`);
//!   - effect handlers use `Handle`/`Unwind`/`Resume` with handler tables.
//!
//! Register scheme: r0..r11 are a scratch/staging zone (call and effect
//! arguments, transient values); r12..r14 are spill scratch registers
//! (used round-robin by local_reg to avoid clobbering);
//! each MIR local gets the fixed register `LOCAL_BASE + local_id`.
//! A function whose locals exceed the register file spills excess
//! locals into the frame's spill vector via SpillLoad/SpillStore.
//!
//! Intra-actor reclamation: `compile_function` runs a conservative
//! liveness-based analysis (`plan_drops`) that emits `OpCode::Drop` when a
//! local provably holding the sole counted reference to a heap object dies —
//! overwritten by a new definition, dead after its last use, or dead at the
//! entry of a block its value flows into unused. The VM clears the register
//! on `Drop`, so duplicate drops are harmless no-ops.

use crate::bytecode::{
    CodeModule, Constant, DebugFunctionInfo, ForeignFunctionDef, HandlerBinding, HandlerTable,
    Instruction, OpCode,
};
use crate::mir;
use crate::types::{NuError, NuResult, PrimitiveType, Span, Type};
use std::collections::HashSet;

type FxHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;
const FUNC_VALUE_REG: u8 = 254;
/// First general-purpose local register. r0..(LOCAL_BASE-1) is the call/effect staging zone,
/// r12..r14 are spill scratch registers, and rLOCAL_BASE..253 hold MIR locals that are not spilled.
pub const LOCAL_BASE: u32 = 15;
const MAX_STAGED_ARGS: usize = 12;
const SCRATCH0: u8 = 0;
const SCRATCH1: u8 = 1;
const SPILL_TEMP: u8 = 12;
const SPILL_TEMP2: u8 = 13;
#[allow(dead_code)]
const SPILL_TEMP3: u8 = 14;

fn not_yet_implemented(feature: &str, span: Span) -> NuError {
    NuError::NotYetImplemented {
        feature: feature.to_string(),
        span,
    }
}

fn compile_err(msg: impl Into<String>, span: Span) -> NuError {
    NuError::VMError {
        msg: msg.into(),
        span,
    }
}

#[derive(Debug, Clone, Copy)]
enum JumpKind {
    Jmp,
    JmpF,
}

#[derive(Debug, Clone)]
struct JumpPatch {
    instr_idx: usize,
    target_block: mir::BlockId,
    kind: JumpKind,
}

pub struct MirCodegen {
    module: CodeModule,
    /// Module-wide record field ids, mirroring the stable compiler's layout.
    field_map: FxHashMap<String, u8>,
    next_field_id: u8,
    /// Constant-pool index of each `self.field` name already emitted for
    /// `StateGet`/`StateSet`, so repeated access to the same field reuses
    /// one constant instead of growing the pool with a fresh duplicate
    /// string every time (unlike record fields, `state` is string-keyed at
    /// runtime, not a positional slot `field_id` could cover).
    state_field_constants: FxHashMap<String, usize>,
    /// Per-function float-ness of MIR locals (see `float_locals`), used to
    /// pick float opcode variants for arithmetic and comparisons. Rebuilt
    /// at the start of every `compile_function`.
    float_locals: Vec<bool>,
    /// Per-function spill map: local_id → spill slot in frame's spill vector.
    /// Built at the start of compile_function.  SpillLoad/SpillStore are
    /// emitted inline during codegen via local_reg / local_dst / spill_write_done.
    spill_map: FxHashMap<u32, u16>,
    /// Round-robin counter for spilled-read temp register selection.
    /// Cycles through SPILL_TEMP (12), SPILL_TEMP2 (13), SPILL_TEMP3 (14)
    /// so that consecutive spilled reads don't clobber each other.
    spill_read_cycle: u8,
}

impl MirCodegen {
    pub fn new(module_name: impl Into<String>) -> Self {
        MirCodegen {
            module: CodeModule::new(module_name),
            field_map: FxHashMap::default(),
            next_field_id: 0,
            state_field_constants: FxHashMap::default(),
            float_locals: Vec::new(),
            spill_map: FxHashMap::default(),
            spill_read_cycle: 0,
        }
    }

    /// Whether the given local of the function currently being compiled is
    /// known to hold a Float at runtime.
    fn is_float_local(&self, id: mir::LocalId) -> bool {
        self.float_locals
            .get(id.0 as usize)
            .copied()
            .unwrap_or(false)
    }

    // -- Inline register spilling -----------------------------------------
    // Locals exceeding the register file are spilled into the frame's spill
    // vector.  These methods emit SpillLoad/SpillStore during codegen so no
    // post-processing rewrite is needed — spilled locals never occupy a
    // physical register, avoiding u8-wrapping ambiguity.

    fn is_spilled(&self, id: mir::LocalId) -> bool {
        self.spill_map.contains_key(&id.0)
    }

    /// Read a local: emits SpillLoad if spilled, returns the register.
    /// Uses a round-robin temp register (r12/r13/r14) to avoid clobbering
    /// when multiple spilled locals are read for the same instruction.
    fn local_reg(&mut self, id: mir::LocalId) -> u8 {
        if let Some(&slot) = self.spill_map.get(&id.0) {
            let temp = SPILL_TEMP + (self.spill_read_cycle % 3);
            self.spill_read_cycle = self.spill_read_cycle.wrapping_add(1);
            self.emit(Instruction::new3(
                OpCode::SpillLoad,
                (slot >> 8) as u8,
                (slot & 0xFF) as u8,
                temp,
            ));
            temp
        } else {
            (LOCAL_BASE + id.0) as u8
        }
    }

    /// Destination register for a write.  For spilled locals this is
    /// SPILL_TEMP; the caller MUST call spill_write_done after emitting
    /// the writing instruction.
    fn local_dst(&self, id: mir::LocalId) -> u8 {
        if self.spill_map.contains_key(&id.0) {
            SPILL_TEMP
        } else {
            (LOCAL_BASE + id.0) as u8
        }
    }

    /// Emit SpillStore to complete a write to a spilled local.
    fn spill_write_done(&mut self, id: mir::LocalId) {
        if let Some(&slot) = self.spill_map.get(&id.0) {
            self.emit(Instruction::new3(
                OpCode::SpillStore,
                SPILL_TEMP,
                (slot >> 8) as u8,
                (slot & 0xFF) as u8,
            ));
        }
    }

    /// For compound rvalues that write to dst then call local_reg: save dst
    /// to r11 (safe from local_reg) to prevent clobbering. Returns the
    /// register (r11 or dst) to use for subsequent construction operations.
    fn protect_dst(&mut self, dst: u8) -> u8 {
        // Only needed when dst is in the spill-temp zone (12-14) that
        // local_reg may return.  r11 is in the staging zone and is never
        // returned by local_reg or load_constant during construction loops.
        if dst == SPILL_TEMP || dst == SPILL_TEMP2 || dst == SPILL_TEMP3 {
            const SAFE_DST: u8 = 11;
            self.emit(Instruction::new2(OpCode::Move, dst, SAFE_DST));
            SAFE_DST
        } else {
            dst
        }
    }

    /// Restore dst from the safe register if protection was applied.
    fn restore_dst(&mut self, dst: u8, safe: u8) {
        if safe != dst {
            self.emit(Instruction::new2(OpCode::Move, safe, dst));
        }
    }
    /// Drop a spilled local: load → drop → store nil back.
    fn spill_drop(&mut self, id: mir::LocalId) {
        if let Some(&slot) = self.spill_map.get(&id.0) {
            self.emit(Instruction::new3(
                OpCode::SpillLoad,
                (slot >> 8) as u8,
                (slot & 0xFF) as u8,
                SPILL_TEMP2,
            ));
            self.emit(Instruction::new1(OpCode::Drop, SPILL_TEMP2));
            self.emit(Instruction::new3(
                OpCode::SpillStore,
                SPILL_TEMP2,
                (slot >> 8) as u8,
                (slot & 0xFF) as u8,
            ));
        }
    }

    /// Constant-pool index for a `self.field` name, reusing an existing
    /// entry if this field was already referenced elsewhere in the module.
    fn state_field_constant(&mut self, field: &str) -> usize {
        if let Some(&idx) = self.state_field_constants.get(field) {
            return idx;
        }
        let idx = self
            .module
            .add_constant(Constant::String(field.to_string()));
        self.state_field_constants.insert(field.to_string(), idx);
        idx
    }

    pub fn compile_module(&mut self, mir: &mut mir::Module) -> NuResult<&CodeModule> {
        // MIR optimization pass: constant folding, identity simplification,
        // jump threading, and dead-store elimination. Runs on every
        // function and behavior before codegen.
        let mut module_consts = Vec::new();
        for func in &mut mir.functions {
            optimize_function(func, &mut module_consts);
        }
        for func in &mut mir.behaviors {
            optimize_function(func, &mut module_consts);
        }

        // Register foreign functions first so FFICall indices line up.
        for ff in &mir.foreign_functions {
            let params = ff
                .params
                .iter()
                .map(crate::ffi::marshal::nulang_type_to_ffi_type)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    compile_err(
                        format!(
                            "unsupported parameter type in extern function {}",
                            ff.symbol
                        ),
                        Span::default(),
                    )
                })?;
            let ret = crate::ffi::marshal::nulang_type_to_ffi_type(&ff.ret).ok_or_else(|| {
                compile_err(
                    format!("unsupported return type in extern function {}", ff.symbol),
                    Span::default(),
                )
            })?;
            self.module.foreign_functions.push(ForeignFunctionDef {
                library: ff.library.clone(),
                symbol: ff.symbol.clone(),
                params,
                ret,
            });
        }

        // Reserve one function-table slot per MIR function; MIR function
        // indices are function-table indices.
        self.module.function_table.resize(mir.functions.len(), 0);
        self.module
            .function_local_counts
            .resize(mir.functions.len(), 0);

        let mut main_idx = None;
        let mut user_main_idx = None;
        for (idx, func) in mir.functions.iter().enumerate() {
            let offset = self.compile_function(func)?;
            self.module.function_table[idx] = offset;
            self.module.function_local_counts[idx] = LOCAL_BASE as usize + func.locals.len();
            if func.name == "__main" {
                main_idx = Some(idx);
            }
            if func.name == "main" {
                user_main_idx = Some(idx);
            }
        }
        // If no synthetic __main wrapper exists but user declared fn main(),
        // treat main as the entry point (matching the legacy compiler).
        let effective_main = main_idx.or(user_main_idx);

        // Actor behaviors compile through the exact same machinery as
        // ordinary functions, but land in CodeModule.behaviors instead of
        // function_table — Spawn/Send/Ask reference them by index there,
        // and (unlike functions) they are never reachable via Call.
        // mir_lower.rs computed ActorMeta.behavior_indices assuming
        // behaviors compile in this order, so this loop must not be
        // reordered or interleaved with function compilation.
        for func in &mir.behaviors {
            let offset = self.compile_function(func)?;
            let end = self.module.instructions.len();

            // Compute BLAKE3 content hash from the compiled bytecode slice +
            // param types + return type.
            let bytecode_slice = &self.module.instructions[offset..end];
            let mut hasher = blake3::Hasher::new();
            for instr in bytecode_slice {
                hasher.update(&[instr.opcode as u8, instr.op1, instr.op2, instr.op3]);
            }
            let param_count_bytes = (func.params.len() as u32).to_be_bytes();
            hasher.update(&param_count_bytes);
            // Hash return type if present
            if let Some(ref ret_ty) = func.ret {
                let ty_str = format!("{:?}", ret_ty);
                hasher.update(ty_str.as_bytes());
            }
            let hash_bytes = *hasher.finalize().as_bytes();

            self.module
                .behaviors
                .push(crate::bytecode::BehaviorTableEntry {
                    name: func.name.clone(),
                    param_count: func.params.len(),
                    code_offset: offset,
                    local_count: LOCAL_BASE as usize + func.locals.len(),
                    effect_mask: 0,
                    compensate_offset: None,
                    content_hash: Some(hash_bytes),
                    source_location: None,
                    parallel_branches: None,
                });
        }
        // Saga compensation: patch each step's compensate_offset from its
        // already-compiled compensation function's code offset.
        // compensation_of stores (abs_step_idx, comp_idx) where
        // abs_step_idx is the step's WHOLE-MODULE behavior index. The
        // cursor walks both lists in the same order (entries are pushed
        // actor-by-actor, behavior-by-behavior, matching
        // `behavior_indices`), so a match means "this actor's step owns
        // this compensation" — a plain actor before the workflow cannot
        // hijack it (SPEC2 §10 known-issue #2).
        let mut comp_cursor = 0;
        for meta in &mir.actor_metadata {
            for &abs_step_idx in &meta.behavior_indices {
                if comp_cursor < mir.compensation_of.len()
                    && mir.compensation_of[comp_cursor].0 == abs_step_idx
                {
                    let comp_idx = mir.compensation_of[comp_cursor].1;
                    let comp_offset = self
                        .module
                        .behaviors
                        .get(comp_idx)
                        .map(|b| b.code_offset)
                        .ok_or_else(|| {
                            compile_err(
                                "internal: compensation behavior index out of range",
                                Span::default(),
                            )
                        })?;
                    let entry = self.module.behaviors.get_mut(abs_step_idx).ok_or_else(|| {
                        compile_err(
                            "internal: compensated behavior index out of range",
                            Span::default(),
                        )
                    })?;
                    entry.compensate_offset = Some(comp_offset);
                    comp_cursor += 1;
                }
            }
        }
        // Parallel-branch metadata: copy branch names onto the matching
        // synthesized step's BehaviorTableEntry (see mir::Module::parallel_branches_of).
        for (behavior_idx, branches) in &mir.parallel_branches_of {
            let entry = self
                .module
                .behaviors
                .get_mut(*behavior_idx)
                .ok_or_else(|| {
                    compile_err(
                        "internal: parallel-branch behavior index out of range",
                        Span::default(),
                    )
                })?;
            entry.parallel_branches = Some(branches.clone());
        }
        self.module.actor_metadata = mir.actor_metadata.clone();

        // Collect tools from agent actors into module.tools so the runtime
        // can resolve @tool-annotated functions for agent LLM requests.
        for meta in &self.module.actor_metadata {
            if meta.is_agent {
                for tool in &meta.tools {
                    if !self.module.tools.iter().any(|t| t.name == tool.name) {
                        self.module.tools.push(tool.clone());
                    }
                }
            }
        }

        // Entry prologue: call the effective main function and halt.
        if let Some(idx) = effective_main {
            let entry = self.module.instructions.len();
            self.load_constant(SCRATCH0, &Constant::Int(idx as i64));
            self.emit(Instruction::new3(OpCode::Call, SCRATCH0, 0, 0));
            self.emit(Instruction::new0(OpCode::Halt));
            self.module.entry_point = Some(entry);
        } else {
            let entry = self.module.instructions.len();
            self.emit(Instruction::new0(OpCode::Halt));
            self.module.entry_point = Some(entry);
        }

        Ok(&self.module)
    }

    fn compile_function(&mut self, func: &mir::Function) -> NuResult<usize> {
        // Isolate this function's bytecode so block offsets are relative to
        // the function start while still allowing forward jump resolution.
        let mut saved_instructions = Vec::new();
        std::mem::swap(&mut saved_instructions, &mut self.module.instructions);
        let function_start = saved_instructions.len();
        // Build the spill map: locals whose id exceeds the register file
        // get a slot in the frame's spill vector.  Inline spilling via
        // local_reg / local_dst / spill_write_done emits SpillLoad/SpillStore
        // during codegen — no capacity limit, no wrapping ambiguity.
        self.spill_map.clear();
        let spilled_threshold = FUNC_VALUE_REG as u32 - LOCAL_BASE;
        let mut next_spill_slot: u16 = 0;
        for i in 0..func.locals.len() as u32 {
            if i >= spilled_threshold {
                self.spill_map.insert(i, next_spill_slot);
                next_spill_slot += 1;
            }
        }
        if func.params.len() > MAX_STAGED_ARGS {
            // Mirrors stage_args's call-site limit: the prologue below reads
            // incoming arguments from r0..r11 (the same staging zone callers
            // stage into), so a param count above that would alias into
            // LOCAL_BASE-mapped registers instead of erroring cleanly.
            self.module.instructions = saved_instructions;
            return Err(compile_err(format!(
                "function '{}' has {} parameters, exceeding the MIR calling convention's limit of {}",
                func.name,
                func.params.len(),
                MAX_STAGED_ARGS
            ), Span::default()));
        }

        // Type-directed opcode selection: the VM's integer handlers coerce
        // float operands to 0, so float arithmetic/comparisons must be
        // emitted as their F* variants.
        self.float_locals = float_locals(func);
        self.spill_read_cycle = 0;

        // Prologue: move incoming arguments into their local registers.
        for (i, param) in func.params.iter().enumerate() {
            let dst = self.local_dst(*param);
            let src = i as u8;
            if src != dst {
                self.emit(Instruction::new2(OpCode::Move, src, dst));
            }
            self.spill_write_done(*param);
        }
        for (i, cap) in func.captures.iter().enumerate() {
            let dst = self.local_dst(*cap);
            self.emit(Instruction::new3(OpCode::CapLoad, i as u8, dst, 0));
            self.spill_write_done(*cap);
        }

        let mut block_offsets: FxHashMap<mir::BlockId, usize> = FxHashMap::default();
        let mut patches: Vec<JumpPatch> = Vec::new();
        // Handler-param moves to inject at the start of handler body blocks.
        let mut handler_prologues: FxHashMap<mir::BlockId, Vec<mir::LocalId>> =
            FxHashMap::default();
        for table in &func.handler_tables {
            for binding in &table.bindings {
                if binding.params.len() > MAX_STAGED_ARGS {
                    // The VM delivers effect arguments in r0..r11; beyond
                    // that the prologue moves below would alias into
                    // LOCAL_BASE-mapped locals — the same corruption the
                    // function-parameter check above rejects.
                    self.module.instructions = saved_instructions;
                    return Err(compile_err(format!(
                        "handler for effect '{}' in function '{}' has {} parameters, exceeding the MIR staging limit of {}",
                        binding.effect_name,
                        func.name,
                        binding.params.len(),
                        MAX_STAGED_ARGS
                    ), Span::default()));
                }
                handler_prologues.insert(binding.body, binding.params.clone());
            }
        }
        // `Handle` instructions awaiting their table index (fn-relative idx).
        let mut handle_patches: Vec<(usize, usize)> = Vec::new();

        // Conservative liveness-based placement of `Drop` instructions (see
        // the module docs and `plan_drops`).
        let drop_plan = plan_drops(func);

        // Source-line map: `(block id, statement index) -> line`, translated
        // to bytecode PCs below so the debugger can place breakpoints and
        // step through the source. Keyed identically to `mir::Function.line_table`.
        let mut line_map: FxHashMap<(u32, usize), u32> = FxHashMap::default();
        for &((block, si), line) in &func.line_table {
            line_map.insert((block.0, si), line);
        }
        // Function-relative pcs of each source statement's first instruction.
        let mut func_lines: Vec<(usize, u32)> = Vec::new();

        for (bi, block) in func.blocks.iter().enumerate() {
            block_offsets.insert(block.id, self.module.instructions.len());
            if let Some(params) = handler_prologues.get(&block.id) {
                // The VM delivers effect arguments in r0..rN.
                for (i, p) in params.iter().enumerate() {
                    let dst = self.local_dst(*p);
                    if i as u8 != dst {
                        self.emit(Instruction::new2(OpCode::Move, i as u8, dst));
                    }
                    self.spill_write_done(*p);
                }
            }
            if let Some(ids) = drop_plan.block_entry.get(&bi) {
                for id in ids {
                    if self.is_spilled(*id) {
                        self.spill_drop(*id);
                    } else {
                        self.emit(Instruction::new1(OpCode::Drop, (LOCAL_BASE + id.0) as u8));
                    }
                }
            }
            for (si, stmt) in block.stmts.iter().enumerate() {
                if let Some(ids) = drop_plan.before_stmt.get(&(bi, si)) {
                    for id in ids {
                        if self.is_spilled(*id) {
                            self.spill_drop(*id);
                        } else {
                            self.emit(Instruction::new1(OpCode::Drop, (LOCAL_BASE + id.0) as u8));
                        }
                    }
                }
                if let Some(&line) = line_map.get(&(block.id.0, si)) {
                    func_lines.push((self.module.instructions.len(), line));
                }
                self.compile_stmt(stmt, func, &mut handle_patches)?;
                if let Some(ids) = drop_plan.after_stmt.get(&(bi, si)) {
                    for id in ids {
                        if self.is_spilled(*id) {
                            self.spill_drop(*id);
                        } else {
                            self.emit(Instruction::new1(OpCode::Drop, (LOCAL_BASE + id.0) as u8));
                        }
                    }
                }
            }
            self.compile_terminator(&block.terminator, &func.name, &block_offsets, &mut patches)?;
        }

        // (SpillLoad/SpillStore are emitted inline during codegen via
        // local_reg / local_dst / spill_write_done — no post-processing
        // rewrite pass is needed.)

        // Patch forward jumps now that all block offsets are known.
        for patch in &patches {
            let target_offset =
                block_offsets
                    .get(&patch.target_block)
                    .copied()
                    .ok_or_else(|| {
                        compile_err("internal: jump to unknown MIR block", Span::default())
                    })?;
            let diff = target_offset as i64 - patch.instr_idx as i64;
            let instr = &mut self.module.instructions[patch.instr_idx];
            match patch.kind {
                JumpKind::Jmp => {
                    instr.op1 = ((diff as i16 >> 8) & 0xFF) as u8;
                    instr.op2 = (diff as i16 & 0xFF) as u8;
                }
                JumpKind::JmpF => {
                    instr.op2 = ((diff as i16 >> 8) & 0xFF) as u8;
                    instr.op3 = (diff as i16 & 0xFF) as u8;
                }
            }
        }

        // Build handler tables: offsets become module-absolute.
        for (instr_idx, table_idx) in handle_patches {
            let def = &func.handler_tables[table_idx];
            let mut bindings = Vec::with_capacity(def.bindings.len());
            for b in &def.bindings {
                let rel = block_offsets.get(&b.body).copied().ok_or_else(|| {
                    compile_err("internal: handler body block missing", Span::default())
                })?;
                let result_reg = func
                    .blocks
                    .get(b.body.0 as usize)
                    .and_then(|blk| match blk.terminator {
                        mir::Terminator::Resume(id) => Some(self.local_reg(id)),
                        _ => None,
                    })
                    .unwrap_or(0);
                bindings.push(HandlerBinding {
                    effect_name: b.effect_name.clone(),
                    handler_offset: function_start + rel,
                    arg_count: b.params.len() as u8,
                    result_reg,
                    single_shot: b.single_shot,
                });
            }
            let global_idx = self.module.add_handler_table(HandlerTable {
                bindings,
                fallback_offset: None,
            });
            if global_idx > u8::MAX as usize {
                return Err(compile_err(
                    "too many effect handler tables in module",
                    Span::default(),
                ));
            }
            self.module.instructions[instr_idx].op1 = global_idx as u8;
        }

        let mut function_code = Vec::new();
        std::mem::swap(&mut function_code, &mut self.module.instructions);
        self.module.instructions = saved_instructions;
        let code_len = function_code.len();
        self.module.instructions.extend(function_code);

        // Publish the debugger's pc<->line map and per-function debug info.
        for (rel, line) in func_lines {
            self.module.line_table.push((function_start + rel, line));
        }
        self.module.debug_functions.push(DebugFunctionInfo {
            name: func.name.clone(),
            code_offset: function_start,
            code_len,
            params: func
                .params
                .iter()
                .map(|p| LOCAL_BASE as usize + p.0 as usize)
                .collect(),
            locals: func
                .locals
                .iter()
                .map(|l| (LOCAL_BASE as usize + l.id.0 as usize, l.name.clone()))
                .collect(),
        });

        Ok(function_start)
    }

    fn compile_stmt(
        &mut self,
        stmt: &mir::Stmt,
        func: &mir::Function,
        handle_patches: &mut Vec<(usize, usize)>,
    ) -> NuResult<()> {
        match stmt {
            mir::Stmt::Assign { dst, op } => {
                let _spill_dst = self.local_dst(*dst);
                self.compile_rvalue(_spill_dst, op)?;
                self.spill_write_done(*dst);
            }
            mir::Stmt::StoreFieldNamed { obj, field, src } => {
                let fid = self.field_id(field)?;
                let _robj = self.local_reg(*obj);
                let _rsrc = self.local_reg(*src);
                self.emit(Instruction::new3(OpCode::RecS, _robj, fid, _rsrc));
            }
            mir::Stmt::ArrayStore { arr, idx, src } => {
                let _rarr = self.local_reg(*arr);
                let _ridx = self.local_reg(*idx);
                let _rsrc = self.local_reg(*src);
                self.emit(Instruction::new3(OpCode::ArrStore, _rarr, _ridx, _rsrc));
            }
            mir::Stmt::EnterHandle { table } => {
                if *table >= func.handler_tables.len() {
                    return Err(compile_err(
                        "internal: EnterHandle references unknown table",
                        Span::default(),
                    ));
                }
                let instr_idx = self.module.instructions.len();
                self.emit(Instruction::new1(OpCode::Handle, 0));
                handle_patches.push((instr_idx, *table));
            }
            mir::Stmt::PopHandler => {
                self.emit(Instruction::new0(OpCode::Unwind));
            }
            mir::Stmt::StateSet { field, src } => {
                let field_idx = self.state_field_constant(field);
                let _rsrc = self.local_reg(*src);
                self.emit(Instruction::new3(
                    OpCode::StateSet,
                    ((field_idx >> 8) & 0xFF) as u8,
                    (field_idx & 0xFF) as u8,
                    _rsrc,
                ));
            }
            mir::Stmt::Emit { event, args } => {
                self.stage_args(args)?;
                let event_idx = self.module.add_constant(Constant::String(event.clone()));
                self.emit(Instruction::new3(
                    OpCode::Emit,
                    ((event_idx >> 8) & 0xFF) as u8,
                    (event_idx & 0xFF) as u8,
                    args.len() as u8,
                ));
            }
        }
        Ok(())
    }

    /// Move argument locals into the staging registers r0..rN.
    fn stage_args(&mut self, args: &[mir::LocalId]) -> NuResult<()> {
        if args.len() > MAX_STAGED_ARGS {
            return Err(compile_err(
                format!(
                    "call/effect with {} arguments exceeds the MIR staging limit of {}",
                    args.len(),
                    MAX_STAGED_ARGS
                ),
                Span::default(),
            ));
        }
        for (i, a) in args.iter().enumerate() {
            let src = self.local_reg(*a);
            if src != i as u8 {
                self.emit(Instruction::new2(OpCode::Move, src, i as u8));
            }
        }
        Ok(())
    }

    fn compile_rvalue(&mut self, dst: u8, rv: &mir::RValue) -> NuResult<()> {
        match rv {
            mir::RValue::Const(c) => {
                self.load_constant(dst, c);
            }
            mir::RValue::Panic(msg) => {
                // The Panic opcode reads register 0 for the message.
                self.load_constant(0, &Constant::String(msg.clone()));
                self.emit(Instruction::new0(OpCode::Panic));
            }
            mir::RValue::Load(id) => {
                let src = self.local_reg(*id);
                if src != dst {
                    self.emit(Instruction::new2(OpCode::Move, src, dst));
                }
            }
            mir::RValue::LoadFieldNamed { obj, field } => {
                let fid = self.field_id(field)?;
                let _robj = self.local_reg(*obj);
                self.emit(Instruction::new3(OpCode::RecL, _robj, fid, dst));
            }
            mir::RValue::LoadFieldPos { obj, index } => {
                let _robj = self.local_reg(*obj);
                self.emit(Instruction::new3(OpCode::FieldL, _robj, *index, dst));
            }
            mir::RValue::ArrayLoad { arr, idx } => {
                let _rarr = self.local_reg(*arr);
                let _ridx = self.local_reg(*idx);
                self.emit(Instruction::new3(OpCode::ArrLoad, _rarr, _ridx, dst));
            }
            mir::RValue::ArrayLen(arr) => {
                let _rarr = self.local_reg(*arr);
                self.emit(Instruction::new2(OpCode::ArrLen, _rarr, dst));
            }
            mir::RValue::ArrayLit(elems) => {
                // Protect dst from local_reg clobbering: save to r11
                // (r11 is in the staging zone never returned by local_reg).
                self.load_constant(SCRATCH0, &Constant::Int(elems.len() as i64));
                self.emit(Instruction::new2(OpCode::ArrAlloc, SCRATCH0, dst));
                let safe = self.protect_dst(dst);
                for (i, e) in elems.iter().enumerate() {
                    self.load_constant(SCRATCH1, &Constant::Int(i as i64));
                    let _re = self.local_reg(*e);
                    self.emit(Instruction::new3(OpCode::ArrStore, safe, SCRATCH1, _re));
                }
                self.restore_dst(dst, safe);
            }
            mir::RValue::Unary(op, id) => {
                let src = self.local_reg(*id);
                // `Deref`/`Ref` are register copies, same as the stable
                // compiler's compile_unary: Nulang's ref cells are locals
                // reassigned in place (see lower_place's Var arm), not a
                // distinct heap allocation, so `&`/`*` are no-ops at the
                // bytecode level — the type checker is what restricts
                // reassignment to Ref-typed locals.
                let opcode = match op {
                    crate::ast::UnOp::Neg => {
                        if self.is_float_local(*id) {
                            OpCode::FNeg
                        } else {
                            OpCode::INeg
                        }
                    }
                    crate::ast::UnOp::Not => OpCode::Not,
                    crate::ast::UnOp::Deref => OpCode::Load,
                    crate::ast::UnOp::Ref(_) => OpCode::Move,
                };
                if opcode == OpCode::FNeg {
                    // The interpreter reads the source from op1 and writes
                    // the destination to op3 for FNeg (unlike INeg's op2).
                    self.emit(Instruction::new3(OpCode::FNeg, src, 0, dst));
                } else {
                    self.emit(Instruction::new2(opcode, src, dst));
                }
            }
            mir::RValue::Binary(op, l, r) => {
                let lr = self.local_reg(*l);
                let rr = self.local_reg(*r);
                // The type checker rejects mixed int/float arithmetic, so
                // operands are homogeneous: one float operand means both
                // are floats and the F* opcode variants are required (the
                // integer handlers coerce float operands to 0).
                let is_float = self.is_float_local(*l) || self.is_float_local(*r);
                use crate::ast::BinOp;
                match (op, is_float) {
                    (BinOp::Ne, f) => {
                        let eq = if f { OpCode::FCmpEq } else { OpCode::ICmpEq };
                        self.emit(Instruction::new3(eq, lr, rr, SCRATCH0));
                        self.emit(Instruction::new2(OpCode::Not, SCRATCH0, dst));
                    }
                    // Float Le/Ge have no dedicated opcodes: expand to the
                    // negated inverse comparison (a <= b == !(a > b)).
                    (BinOp::Le, true) => {
                        self.emit(Instruction::new3(OpCode::FCmpGt, lr, rr, SCRATCH0));
                        self.emit(Instruction::new2(OpCode::Not, SCRATCH0, dst));
                    }
                    (BinOp::Ge, true) => {
                        self.emit(Instruction::new3(OpCode::FCmpLt, lr, rr, SCRATCH0));
                        self.emit(Instruction::new2(OpCode::Not, SCRATCH0, dst));
                    }
                    _ => {
                        let opcode = binary_opcode(op, is_float)?;
                        self.emit(Instruction::new3(opcode, lr, rr, dst));
                    }
                }
            }
            mir::RValue::StringEq(l, r) => {
                let _rl = self.local_reg(*l);
                let _rr = self.local_reg(*r);
                self.emit(Instruction::new3(OpCode::SCmpEq, _rl, _rr, dst));
            }
            mir::RValue::StrConcat(l, r) => {
                let _rl = self.local_reg(*l);
                let _rr = self.local_reg(*r);
                self.emit(Instruction::new3(OpCode::SConcat, _rl, _rr, dst));
            }
            mir::RValue::Call { func, args } => {
                // Load the callee value first (it lives above the staging
                // zone, so staging cannot clobber it).
                match func {
                    mir::FuncRef::Index(idx) => {
                        self.load_constant(FUNC_VALUE_REG, &Constant::Int(*idx as i64));
                    }
                    mir::FuncRef::Local(id) => {
                        let _rid = self.local_reg(*id);
                        self.emit(Instruction::new2(OpCode::Move, _rid, FUNC_VALUE_REG));
                    }
                }
                self.stage_args(args)?;
                self.emit(Instruction::new3(
                    OpCode::Call,
                    FUNC_VALUE_REG,
                    args.len() as u8,
                    dst,
                ));
            }
            mir::RValue::Closure { func, captures } => {
                self.emit(Instruction::new3(
                    OpCode::Closure,
                    ((*func >> 8) & 0xFF) as u8,
                    (*func & 0xFF) as u8,
                    dst,
                ));
                let safe = self.protect_dst(dst);
                for (i, cap) in captures.iter().enumerate() {
                    let _rcap = self.local_reg(*cap);
                    self.emit(Instruction::new3(OpCode::CapStore, safe, i as u8, _rcap));
                }
                self.restore_dst(dst, safe);
            }
            mir::RValue::Tuple(elems) => {
                self.emit(Instruction::new2(OpCode::TupleMk, elems.len() as u8, dst));
                let safe = self.protect_dst(dst);
                for (i, e) in elems.iter().enumerate() {
                    let _re = self.local_reg(*e);
                    self.emit(Instruction::new3(OpCode::FieldS, safe, i as u8, _re));
                }
                self.restore_dst(dst, safe);
            }
            mir::RValue::Record(fields) => {
                let mut max_field_id: u8 = 0;
                let mut field_ids = Vec::with_capacity(fields.len());
                for (name, _) in fields {
                    let fid = self.field_id(name)?;
                    max_field_id = max_field_id.max(fid);
                    field_ids.push(fid);
                }
                let slot_count = max_field_id.saturating_add(1);
                self.emit(Instruction::new2(OpCode::RecMk, slot_count, dst));
                let safe = self.protect_dst(dst);
                for ((_, e), fid) in fields.iter().zip(field_ids) {
                    let _re = self.local_reg(*e);
                    self.emit(Instruction::new3(OpCode::RecS, safe, fid, _re));
                }
                self.restore_dst(dst, safe);
            }
            mir::RValue::RecordUpdate { base, overrides } => {
                // Shallow copy the base record, then overwrite each override.
                let _rbase = self.local_reg(*base);
                self.emit(Instruction::new2(OpCode::RecCopy, _rbase, dst));
                let safe = self.protect_dst(dst);
                for (name, val_id) in overrides {
                    let fid = self.field_id(name)?;
                    let _rval = self.local_reg(*val_id);
                    self.emit(Instruction::new3(OpCode::RecS, safe, fid, _rval));
                }
                self.restore_dst(dst, safe);
            }
            mir::RValue::Resume(val) => {
                let rid = self.local_reg(*val);
                self.emit(Instruction::new1(OpCode::Resume, rid));
            }
            mir::RValue::Perform {
                effect,
                op,
                args,
                resolved_handler,
            } => {
                self.stage_args(args)?;
                if let Some(href) = resolved_handler {
                    // Statically-resolved handler — emit PerformDirect with
                    // table and binding indices, skipping the string lookup.
                    self.emit(Instruction::new3(
                        OpCode::PerformDirect,
                        href.table_index as u8,
                        href.binding_index as u8,
                        dst,
                    ));
                } else {
                    let eff_idx = self
                        .module
                        .add_constant(Constant::String(format!("{}.{}", effect, op)));
                    self.emit(Instruction::new3(
                        OpCode::Perform,
                        ((eff_idx >> 8) & 0xFF) as u8,
                        (eff_idx & 0xFF) as u8,
                        dst,
                    ));
                }
            }
            mir::RValue::PerformAsync {
                effect_op,
                args,
                resolved_handler: _,
            } => {
                self.stage_args(args)?;
                let eff_idx = self
                    .module
                    .add_constant(Constant::String(effect_op.clone()));
                self.emit(Instruction::new3(
                    OpCode::PerformAsync,
                    ((eff_idx >> 8) & 0xFF) as u8,
                    (eff_idx & 0xFF) as u8,
                    dst,
                ));
            }
            mir::RValue::SignalWait { name } => {
                let name_idx = self.module.add_constant(Constant::String(name.clone()));
                self.emit(Instruction::new3(
                    OpCode::SignalWait,
                    ((name_idx >> 8) & 0xFF) as u8,
                    (name_idx & 0xFF) as u8,
                    dst,
                ));
            }
            mir::RValue::Receive => {
                // Pops the next mailbox message via ActorVmCallbacks::try_receive;
                // writes its first payload value (or nil) to dst.
                self.emit(Instruction::new1(OpCode::Receive, dst));
            }
            mir::RValue::ReceiveMatch {
                behavior_ids,
                max_params,
            } => {
                // Selective receive: the spec constant encodes the reserved
                // payload-register count and the candidate arm behavior ids
                // as "max_params:id1,id2,...". The VM writes the matched arm
                // index (or the arm count when nothing matched) to dst and
                // payload values into the registers following dst.
                let ids = behavior_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let spec = format!("{}:{}", max_params, ids);
                let spec_idx = self.module.add_constant(Constant::String(spec));
                self.emit(Instruction::new3(
                    OpCode::ReceiveMatch,
                    ((spec_idx >> 8) & 0xFF) as u8,
                    (spec_idx & 0xFF) as u8,
                    dst,
                ));
            }
            mir::RValue::ReceiveWait {
                behavior_ids,
                max_params,
                timeout,
            } => {
                // Timed selective receive (receive-after): same spec constant
                // and dst contract as ReceiveMatch, plus the timeout in
                // milliseconds staged into r0 (fixed-register staging, like
                // the pipeline opcodes). See OpCode::ReceiveWait (0xA0) in
                // bytecode.rs for the full VM-side contract.
                let ids = behavior_ids
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let spec = format!("{}:{}", max_params, ids);
                let spec_idx = self.module.add_constant(Constant::String(spec));
                let _rtimeout = self.local_reg(*timeout);
                self.emit(Instruction::new2(OpCode::Move, _rtimeout, SCRATCH0));
                self.emit(Instruction::new3(
                    OpCode::ReceiveWait,
                    ((spec_idx >> 8) & 0xFF) as u8,
                    (spec_idx & 0xFF) as u8,
                    dst,
                ));
            }
            mir::RValue::ReceiveCommit => {
                // Commit: removes the matched message from the skip-buffer.
                self.emit(Instruction::new0(OpCode::ReceiveCommit));
            }
            mir::RValue::FFICall { idx, args } => {
                self.stage_args(args)?;
                self.emit(Instruction::new3(
                    OpCode::FFICall,
                    ((*idx >> 8) & 0xFF) as u8,
                    (*idx & 0xFF) as u8,
                    dst,
                ));
            }
            mir::RValue::Migrate { actor, node } => {
                let _ractor = self.local_reg(*actor);
                let _rnode = self.local_reg(*node);
                self.emit(Instruction::new3(OpCode::Migrate, _ractor, _rnode, dst));
            }
            mir::RValue::SelfRef => {
                self.emit(Instruction::new1(OpCode::SelfOp, dst));
            }
            mir::RValue::CapabilityCheck { val } => {
                let _ = val;
                self.emit(Instruction::new1(OpCode::Const1, dst)); // true
            }
            mir::RValue::StateGet { field } => {
                let field_idx = self.state_field_constant(field);
                self.emit(Instruction::new3(
                    OpCode::StateGet,
                    ((field_idx >> 8) & 0xFF) as u8,
                    (field_idx & 0xFF) as u8,
                    dst,
                ));
            }
            mir::RValue::Spawn {
                behavior_idx,
                init,
                target_node,
                capabilities: _,
            } => {
                if let Some(node) = target_node {
                    let node_reg = self.local_reg(*node);
                    if init.len() > MAX_STAGED_ARGS {
                        return Err(compile_err(
                            format!(
                                "spawn@node with {} init fields exceeds MIR staging limit of {}",
                                init.len(),
                                MAX_STAGED_ARGS
                            ),
                            Span::default(),
                        ));
                    }
                    let pc = self.current_offset();
                    let names: Vec<String> = init.iter().map(|(n, _)| n.clone()).collect();
                    for (i, (_, rv)) in init.iter().enumerate() {
                        self.compile_rvalue(i as u8, rv)?;
                    }
                    if !names.is_empty() {
                        self.module.remote_spawn_init_fields.push((pc, names));
                    }
                    self.emit(Instruction::new3(
                        OpCode::RSpawn,
                        node_reg,
                        ((*behavior_idx >> 8) & 0xFF) as u8,
                        (*behavior_idx & 0xFF) as u8,
                    ));
                    self.emit(Instruction::new2(OpCode::Move, node_reg, dst));
                } else {
                    let pc = self.current_offset();
                    self.emit(Instruction::new3(
                        OpCode::Spawn,
                        ((*behavior_idx >> 8) & 0xFF) as u8,
                        (*behavior_idx & 0xFF) as u8,
                        dst,
                    ));
                    if !init.is_empty() {
                        let overrides: Vec<(String, crate::bytecode::Constant)> = init
                            .iter()
                            .filter_map(|(name, rv)| match rv {
                                mir::RValue::Const(c) => Some((name.clone(), c.clone())),
                                _ => None,
                            })
                            .collect();
                        if !overrides.is_empty() {
                            self.module.spawn_init_overrides.push((pc, overrides));
                        }
                    }
                }
            }
            mir::RValue::Send {
                actor,
                behavior_idx,
                args,
                remote,
            } => {
                let _ractor = self.local_reg(*actor);
                self.emit(Instruction::new2(OpCode::Move, _ractor, FUNC_VALUE_REG));
                self.stage_args(args)?;
                let opcode = if *remote { OpCode::RSend } else { OpCode::Send };
                self.emit(Instruction::new3(
                    opcode,
                    FUNC_VALUE_REG,
                    ((*behavior_idx >> 8) & 0xFF) as u8,
                    (*behavior_idx & 0xFF) as u8,
                ));
                // Send is fire-and-forget with no VM-level result register;
                // the stable compiler yields 0 for send-as-expression.
            }
            mir::RValue::Ask {
                actor,
                behavior_idx,
                args,
                remote,
                timeout_ms,
            } => {
                let _ractor = self.local_reg(*actor);
                self.emit(Instruction::new2(OpCode::Move, _ractor, FUNC_VALUE_REG));
                self.stage_args(args)?;
                // If timeout is specified, stage it into r0 for the VM to read
                let opcode = if *remote { OpCode::RAsk } else { OpCode::Ask };
                if let Some(ms) = timeout_ms {
                    let timeout_idx = self.module.add_constant(Constant::Int(*ms as i64));
                    self.emit(Instruction::new3(
                        OpCode::ConstU,
                        12, // r12 — timeout register (past staging area r0..r11)
                        ((timeout_idx >> 8) & 0xFF) as u8,
                        (timeout_idx & 0xFF) as u8,
                    ));
                }
                self.emit(Instruction::new3(
                    opcode,
                    FUNC_VALUE_REG,
                    ((*behavior_idx >> 8) & 0xFF) as u8,
                    (*behavior_idx & 0xFF) as u8,
                ));
                // Ask writes its result back into op1 register; move to dst.
                self.emit(Instruction::new2(OpCode::Move, FUNC_VALUE_REG, dst));
            }
        }
        Ok(())
    }

    fn compile_terminator(
        &mut self,
        term: &mir::Terminator,
        func_name: &str,
        block_offsets: &FxHashMap<mir::BlockId, usize>,
        patches: &mut Vec<JumpPatch>,
    ) -> NuResult<()> {
        match term {
            mir::Terminator::Return(val) => match val {
                Some(id) => {
                    let _rid = self.local_reg(*id);
                    self.emit(Instruction::new1(OpCode::RetVal, _rid));
                }
                None => {
                    self.emit(Instruction::new1(OpCode::Const0, SCRATCH0));
                    self.emit(Instruction::new1(OpCode::RetVal, SCRATCH0));
                }
            },
            mir::Terminator::Jump(target) => {
                let idx = self.module.instructions.len();
                if let Some(&offset) = block_offsets.get(target) {
                    let diff = offset as i64 - idx as i64;
                    self.emit(Instruction::new2(
                        OpCode::Jmp,
                        ((diff as i16 >> 8) & 0xFF) as u8,
                        (diff as i16 & 0xFF) as u8,
                    ));
                } else {
                    self.emit(Instruction::new2(OpCode::Jmp, 0, 0));
                    patches.push(JumpPatch {
                        instr_idx: idx,
                        target_block: *target,
                        kind: JumpKind::Jmp,
                    });
                }
            }
            mir::Terminator::Branch { cond, then_, else_ } => {
                let cond_reg = self.local_reg(*cond);

                // JmpF to else_ when the condition is false.
                let jmpf_idx = self.module.instructions.len();
                if let Some(&else_offset) = block_offsets.get(else_) {
                    let diff = else_offset as i64 - jmpf_idx as i64;
                    self.emit(Instruction::new3(
                        OpCode::JmpF,
                        cond_reg,
                        ((diff as i16 >> 8) & 0xFF) as u8,
                        (diff as i16 & 0xFF) as u8,
                    ));
                } else {
                    self.emit(Instruction::new3(OpCode::JmpF, cond_reg, 0, 0));
                    patches.push(JumpPatch {
                        instr_idx: jmpf_idx,
                        target_block: *else_,
                        kind: JumpKind::JmpF,
                    });
                }

                // Unconditional jump to then_.
                let jmp_idx = self.module.instructions.len();
                if let Some(&then_offset) = block_offsets.get(then_) {
                    let diff = then_offset as i64 - jmp_idx as i64;
                    self.emit(Instruction::new2(
                        OpCode::Jmp,
                        ((diff as i16 >> 8) & 0xFF) as u8,
                        (diff as i16 & 0xFF) as u8,
                    ));
                } else {
                    self.emit(Instruction::new2(OpCode::Jmp, 0, 0));
                    patches.push(JumpPatch {
                        instr_idx: jmp_idx,
                        target_block: *then_,
                        kind: JumpKind::Jmp,
                    });
                }
            }
            mir::Terminator::Resume(id) => {
                let _rid = self.local_reg(*id);
                self.emit(Instruction::new1(OpCode::Resume, _rid));
            }
            mir::Terminator::Unterminated => {
                return Err(compile_err(
                    format!(
                        "internal: unterminated MIR block in function '{}'",
                        func_name
                    ),
                    Span::default(),
                ));
            }
        }
        Ok(())
    }

    fn field_id(&mut self, name: &str) -> NuResult<u8> {
        if let Some(&id) = self.field_map.get(name) {
            return Ok(id);
        }
        if self.field_map.len() > u8::MAX as usize {
            // Mirrors the stable compiler's field_id: the 256th distinct
            // field name has no free id left (a single byte encodes it), so
            // this is an honest error instead of silently aliasing two
            // unrelated fields onto the same slot.
            return Err(compile_err(format!(
                "module has more than {} distinct record/tuple field names (limit for the current u8 field-id encoding); '{}' has no id left to assign",
                u8::MAX as usize + 1,
                name
            ), Span::default()));
        }
        let id = self.next_field_id;
        self.next_field_id = self.next_field_id.saturating_add(1);
        self.field_map.insert(name.to_string(), id);
        Ok(id)
    }

    fn load_constant(&mut self, dst: u8, c: &Constant) {
        match c {
            Constant::Int(0) => self.emit(Instruction::new1(OpCode::Const0, dst)),
            Constant::Int(1) => self.emit(Instruction::new1(OpCode::Const1, dst)),
            Constant::Int(2) => self.emit(Instruction::new1(OpCode::Const2, dst)),
            Constant::Int(-1) => self.emit(Instruction::new1(OpCode::ConstM1, dst)),
            _ => {
                let idx = self.module.add_constant(c.clone());
                self.emit(Instruction::new3(
                    OpCode::ConstU,
                    ((idx >> 8) & 0xFF) as u8,
                    (idx & 0xFF) as u8,
                    dst,
                ));
            }
        }
    }

    fn emit(&mut self, instr: Instruction) {
        self.module.instructions.push(instr);
    }

    fn current_offset(&self) -> usize {
        self.module.instructions.len()
    }

    pub fn finish(self) -> CodeModule {
        self.module
    }
}

// ---------------------------------------------------------------------------

fn binary_opcode(op: &crate::ast::BinOp, is_float: bool) -> NuResult<OpCode> {
    use crate::ast::BinOp;
    match (op, is_float) {
        (BinOp::Add, false) => Ok(OpCode::IAdd),
        (BinOp::Add, true) => Ok(OpCode::FAdd),
        (BinOp::Sub, false) => Ok(OpCode::ISub),
        (BinOp::Sub, true) => Ok(OpCode::FSub),
        (BinOp::Mul, false) => Ok(OpCode::IMul),
        (BinOp::Mul, true) => Ok(OpCode::FMul),
        (BinOp::Pow, false) => Ok(OpCode::IPow),
        (BinOp::Pow, true) => Ok(OpCode::FPow),
        (BinOp::Div, false) => Ok(OpCode::IDiv),
        (BinOp::Div, true) => Ok(OpCode::FDiv),
        (BinOp::Mod, false) => Ok(OpCode::IMod),
        (BinOp::Mod, true) => Ok(OpCode::FMod),
        (BinOp::Eq, false) => Ok(OpCode::ICmpEq),
        (BinOp::Eq, true) => Ok(OpCode::FCmpEq),
        (BinOp::Lt, false) => Ok(OpCode::ICmpLt),
        (BinOp::Lt, true) => Ok(OpCode::FCmpLt),
        (BinOp::Gt, false) => Ok(OpCode::ICmpGt),
        (BinOp::Gt, true) => Ok(OpCode::FCmpGt),
        (BinOp::Le, false) => Ok(OpCode::ICmpLe),
        (BinOp::Ge, false) => Ok(OpCode::ICmpGe),
        // Float Le/Ge are expanded to negated inverse comparisons by the
        // caller (there are no FCmpLe/FCmpGe opcodes).
        (BinOp::Le, true) | (BinOp::Ge, true) => Err(compile_err(
            "internal: float Le/Ge must be expanded by the caller",
            Span::default(),
        )),
        (BinOp::And, _) => Ok(OpCode::And),
        (BinOp::Or, _) => Ok(OpCode::Or),
        (BinOp::BitAnd, _) => Ok(OpCode::BitAnd),
        (BinOp::BitOr, _) => Ok(OpCode::BitOr),
        (BinOp::BitXor, _) => Ok(OpCode::Xor),
        (BinOp::Shl, _) => Ok(OpCode::Shl),
        (BinOp::Shr, _) => Ok(OpCode::Shr),
        (other, _) => Err(not_yet_implemented(
            &format!("binary operator {:?}", other),
            Span::default(),
        )),
    }
}

/// Compute which locals of a function may hold a Float at runtime, so
/// binary/unary opcode emission can pick the float opcode variants.
///
/// Seeds: locals declared with a Float type and locals assigned a float
/// constant. Propagates to a fixpoint through register copies, float
/// arithmetic, and unary negation. Best-effort: MIR temp types are
/// unreliable (see hir_lower's fallbacks), so float values arriving via
/// paths with no Float-typed origin — unannotated function parameters,
/// call results, array/record loads — are not tracked, and operations on
/// them keep the legacy integer opcodes. That is a limitation, not a
/// regression: pre-fix behavior for those cases was the same.
fn float_locals(func: &mir::Function) -> Vec<bool> {
    let mut is_float = vec![false; func.locals.len()];
    for local in &func.locals {
        if local.ty == Type::Primitive(PrimitiveType::Float) {
            is_float[local.id.0 as usize] = true;
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for block in &func.blocks {
            for stmt in &block.stmts {
                let mir::Stmt::Assign { dst, op } = stmt else {
                    continue;
                };
                let result = match op {
                    mir::RValue::Const(Constant::Float(_)) => true,
                    mir::RValue::Load(src) => is_float[src.0 as usize],
                    mir::RValue::Unary(crate::ast::UnOp::Neg, src) => is_float[src.0 as usize],
                    mir::RValue::Binary(op, l, r)
                        if matches!(
                            op,
                            crate::ast::BinOp::Add
                                | crate::ast::BinOp::Sub
                                | crate::ast::BinOp::Mul
                                | crate::ast::BinOp::Div
                                | crate::ast::BinOp::Mod
                                | crate::ast::BinOp::Pow
                        ) =>
                    {
                        is_float[l.0 as usize] || is_float[r.0 as usize]
                    }
                    _ => false,
                };
                if result && !is_float[dst.0 as usize] {
                    is_float[dst.0 as usize] = true;
                    changed = true;
                }
            }
        }
    }
    is_float
}

// ===========================================================================
// MIR optimization pass
// ===========================================================================
//
// A lightweight, conservative MIR→MIR optimizer that runs on every function
// and behavior before bytecode emission. Four transforms in one fixpoint
// loop (capped at MAX_OPT_ITERATIONS rounds):
//
//   1. constant folding     — arithmetic/comparison on Const operands
//                             (int, float, bool, string concat) and Unary;
//   2. identity folding     — x+0, x*1, x|0, x&&true, x*0, ... collapses;
//   3. jump threading       — trampoline blocks (0 stmts + Jump) are
//                             bypassed and marked unreachable;
//   4. dead-store elimination — stores whose dst is never read anywhere in
//                             the function are dropped (function-wide read
//                             set — block-local liveness alone would be
//                             unsound across loop back-edges and joins).
//
// Everything here is semantics-preserving with respect to the bytecode VM.
// Notable VM quirks the folding accounts for:
//   - float Eq/Ne is NOT folded: the VM's ICmpEq/FCmpEq use epsilon
//     equality (|a-b| < f64::EPSILON), not exact ==;
//   - identity/zero-propagation rules are skipped when the non-const
//     operand is a float local: the VM's F* handlers coerce the int
//     constant to a float, and IEEE semantics differ at ±0.0/NaN
//     (e.g. x * 0.0 is NaN for infinite x, and -0.0 + 0.0 = +0.0);
//   - int arithmetic folds with i64 wrapping ops — the VM computes in i64
//     and truncates to the 48-bit payload on `Value::int`, so a folded
//     `Const::Int` reloaded through the constant pool truncates to the
//     identical 48-bit result;
//   - And/Or/Not fold only for Bool constants: the typechecker requires
//     Bool operands, and the VM's as_bool().unwrap_or(false) would
//     disagree with any truthiness interpretation of non-bool values.

const MAX_OPT_ITERATIONS: usize = 10;

/// Optimize one MIR function in place. `_module_consts` reserves space for
/// module-level constant pooling; unused by the current transforms.
fn optimize_function(func: &mut mir::Function, _module_consts: &mut Vec<mir::RValue>) {
    for _ in 0..MAX_OPT_ITERATIONS {
        let const_locals = collect_const_locals(func);
        let is_float = float_locals(func);
        let folded = fold_function(func, &const_locals, &is_float);
        let threaded = thread_jumps(func);
        let dce = dead_store_elim(func);
        if !folded && !threaded && !dce {
            break;
        }
    }
}

/// Collect locals that hold a single, never-reassigned constant value.
/// Params and closure captures never qualify (their initial values come
/// from outside the function). Only single-definition, single-constant
/// locals are recorded — a local assigned different constants in different
/// blocks is disqualified even though both are const.
fn collect_const_locals(func: &mir::Function) -> std::collections::HashMap<mir::LocalId, Constant> {
    let mut assign_count: std::collections::HashMap<mir::LocalId, usize> =
        std::collections::HashMap::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let mir::Stmt::Assign { dst, .. } = stmt {
                *assign_count.entry(*dst).or_insert(0) += 1;
            }
        }
    }
    let mut disallowed: HashSet<mir::LocalId> = HashSet::new();
    disallowed.extend(func.params.iter().copied());
    disallowed.extend(func.captures.iter().copied());

    let mut const_locals: std::collections::HashMap<mir::LocalId, Constant> =
        std::collections::HashMap::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let mir::Stmt::Assign {
                dst,
                op: mir::RValue::Const(c),
            } = stmt
            {
                if !disallowed.contains(dst) && assign_count.get(dst) == Some(&1) {
                    const_locals.insert(*dst, c.clone());
                }
            }
        }
    }
    const_locals
}

/// Fold and simplify every `Stmt::Assign` RValue. Returns true if anything
/// changed.
fn fold_function(
    func: &mut mir::Function,
    const_locals: &std::collections::HashMap<mir::LocalId, Constant>,
    is_float: &[bool],
) -> bool {
    let mut changed = false;
    for block in &mut func.blocks {
        for stmt in &mut block.stmts {
            if let mir::Stmt::Assign { op, .. } = stmt {
                let mut original = mir::RValue::Const(Constant::Nil);
                std::mem::swap(op, &mut original);
                let folded = fold_rvalue(original, const_locals, is_float);
                if folded != *op {
                    *op = folded;
                    changed = true;
                }
            }
        }
    }
    changed
}

fn fold_rvalue(
    op: mir::RValue,
    const_locals: &std::collections::HashMap<mir::LocalId, Constant>,
    is_float: &[bool],
) -> mir::RValue {
    use crate::ast::{BinOp, UnOp};
    use mir::RValue;
    use Constant::{Bool, Float, Int, Nil, String as CString};

    match op {
        RValue::Load(local) => match const_locals.get(&local) {
            Some(c) => RValue::Const(c.clone()),
            None => RValue::Load(local),
        },
        RValue::Unary(uop, local) => match const_locals.get(&local) {
            Some(c) => match (uop, c) {
                (UnOp::Neg, Int(n)) => RValue::Const(Int(n.wrapping_neg())),
                (UnOp::Neg, Float(f)) => RValue::Const(Float(-f)),
                (UnOp::Not, Bool(b)) => RValue::Const(Bool(!b)),
                // The remaining Not rules never fire on well-typed MIR
                // (the typechecker requires Bool operands), but they mirror
                // the VM's as_bool().unwrap_or(false) semantics for the
                // falsy values nil and 0.
                (UnOp::Not, Nil) => RValue::Const(Bool(true)),
                (UnOp::Not, Int(0)) => RValue::Const(Bool(true)),
                (UnOp::Not, Int(_)) => RValue::Const(Bool(false)),
                _ => RValue::Unary(uop, local),
            },
            None => RValue::Unary(uop, local),
        },
        RValue::Binary(bop, a, b) => {
            let a_is_float = is_float.get(a.0 as usize).copied().unwrap_or(false);
            let b_is_float = is_float.get(b.0 as usize).copied().unwrap_or(false);
            let a_const = const_locals.get(&a);
            let b_const = const_locals.get(&b);
            match (a_const, b_const) {
                (Some(ca), Some(cb)) => match fold_binary_consts(bop, ca, cb) {
                    Some(rv) => rv,
                    None => RValue::Binary(bop, a, b),
                },
                (Some(ca), None) => match fold_one_const(bop, ca, b, b_is_float, true) {
                    Some(rv) => rv,
                    None => RValue::Binary(bop, a, b),
                },
                (None, Some(cb)) => match fold_one_const(bop, cb, a, a_is_float, false) {
                    Some(rv) => rv,
                    None => RValue::Binary(bop, a, b),
                },
                (None, None) => {
                    // x == x is always true and x != x always false — for
                    // every value except a NaN float (the VM's epsilon
                    // equality gives NaN == NaN = false), so skip floats.
                    if a == b && !a_is_float {
                        match bop {
                            BinOp::Eq => RValue::Const(Bool(true)),
                            BinOp::Ne => RValue::Const(Bool(false)),
                            _ => RValue::Binary(bop, a, b),
                        }
                    } else {
                        RValue::Binary(bop, a, b)
                    }
                }
            }
        }
        RValue::StrConcat(a, b) => match (const_locals.get(&a), const_locals.get(&b)) {
            (Some(CString(sa)), Some(CString(sb))) => RValue::Const(CString(sa.clone() + sb)),
            _ => RValue::StrConcat(a, b),
        },
        other => other,
    }
}

/// Constant-fold `Const op Const` per the VM's integer/float handler
/// semantics. Returns None when no rule applies (mixed int/float, float
/// Eq/Ne — the VM uses epsilon equality there — nil comparisons, ...).
fn fold_binary_consts(bop: crate::ast::BinOp, a: &Constant, b: &Constant) -> Option<mir::RValue> {
    use crate::ast::BinOp;
    use mir::RValue;
    use Constant::{Bool, Float, Int, Nil, String as CString};

    Some(match (bop, a, b) {
        // -- Int arithmetic (i64 wrapping; the constant pool truncates to
        //    the same 48-bit payload the VM's Value::int produces) --
        (BinOp::Add, Int(x), Int(y)) => RValue::Const(Int(x.wrapping_add(*y))),
        (BinOp::Sub, Int(x), Int(y)) => RValue::Const(Int(x.wrapping_sub(*y))),
        (BinOp::Mul, Int(x), Int(y)) => RValue::Const(Int(x.wrapping_mul(*y))),
        (BinOp::Div, Int(_), Int(0)) => RValue::Const(Nil),
        (BinOp::Div, Int(x), Int(y)) => RValue::Const(Int(x.wrapping_div(*y))),
        (BinOp::Mod, Int(_), Int(0)) => RValue::Const(Nil),
        (BinOp::Mod, Int(x), Int(y)) => RValue::Const(Int(x.wrapping_rem(*y))),
        // -- Float arithmetic (standard f64 ops; div/mod by 0.0 → nil,
        //    matching step_idiv/step_imod's `bf != 0.0` check) --
        (BinOp::Add, Float(x), Float(y)) => RValue::Const(Float(x + y)),
        (BinOp::Sub, Float(x), Float(y)) => RValue::Const(Float(x - y)),
        (BinOp::Mul, Float(x), Float(y)) => RValue::Const(Float(x * y)),
        (BinOp::Div, Float(_), Float(y)) if *y == 0.0 => RValue::Const(Nil),
        (BinOp::Div, Float(x), Float(y)) => RValue::Const(Float(x / y)),
        (BinOp::Mod, Float(_), Float(y)) if *y == 0.0 => RValue::Const(Nil),
        (BinOp::Mod, Float(x), Float(y)) => RValue::Const(Float(x % y)),
        // -- Int comparisons (VM ICmp* int-int paths are exact) --
        (BinOp::Eq, Int(x), Int(y)) => RValue::Const(Bool(x == y)),
        (BinOp::Ne, Int(x), Int(y)) => RValue::Const(Bool(x != y)),
        (BinOp::Lt, Int(x), Int(y)) => RValue::Const(Bool(x < y)),
        (BinOp::Le, Int(x), Int(y)) => RValue::Const(Bool(x <= y)),
        (BinOp::Gt, Int(x), Int(y)) => RValue::Const(Bool(x > y)),
        (BinOp::Ge, Int(x), Int(y)) => RValue::Const(Bool(x >= y)),
        // -- Float comparisons: Lt/Le/Gt/Ge are standard in both the
        //    ICmp*/FCmp* handlers and the caller's Le/Ge expansion.
        //    Eq/Ne are deliberately excluded (epsilon equality) —
        //    the plan's contingency for a semantic mismatch. --
        (BinOp::Lt, Float(x), Float(y)) => RValue::Const(Bool(x < y)),
        (BinOp::Le, Float(x), Float(y)) => RValue::Const(Bool(x <= y)),
        (BinOp::Gt, Float(x), Float(y)) => RValue::Const(Bool(x > y)),
        (BinOp::Ge, Float(x), Float(y)) => RValue::Const(Bool(x >= y)),
        // -- Bool comparisons (VM ICmpEq falls through to raw ==) --
        (BinOp::Eq, Bool(x), Bool(y)) => RValue::Const(Bool(x == y)),
        (BinOp::Ne, Bool(x), Bool(y)) => RValue::Const(Bool(x != y)),
        // -- String comparisons (VM ICmpEq string path: content) --
        (BinOp::Eq, CString(x), CString(y)) => RValue::Const(Bool(x == y)),
        (BinOp::Ne, CString(x), CString(y)) => RValue::Const(Bool(x != y)),
        // -- Logical (Bool operands are guaranteed by the typechecker) --
        (BinOp::And, Bool(x), Bool(y)) => RValue::Const(Bool(*x && *y)),
        (BinOp::Or, Bool(x), Bool(y)) => RValue::Const(Bool(*x || *y)),
        // -- Bitwise / shifts (VM masks shift counts to 6 bits) --
        (BinOp::BitAnd, Int(x), Int(y)) => RValue::Const(Int(x & y)),
        (BinOp::BitOr, Int(x), Int(y)) => RValue::Const(Int(x | y)),
        (BinOp::BitXor, Int(x), Int(y)) => RValue::Const(Int(x ^ y)),
        (BinOp::Shl, Int(x), Int(y)) => RValue::Const(Int(x.wrapping_shl((*y & 0x3f) as u32))),
        (BinOp::Shr, Int(x), Int(y)) => RValue::Const(Int(x.wrapping_shr((*y & 0x3f) as u32))),
        _ => return None,
    })
}

/// Identity / zero-one-propagation rules for `Const op Local` /
/// `Local op Const`. `const_on_left` selects the mirror rule set (Sub/Div/
/// Shl/Shr have no left-const identities: `0 - x` and `1 / x` are not x).
/// Returns `None` when no rule applies — the caller keeps the original
/// RValue (a substituted const cannot be inlined: `Binary` holds LocalIds).
fn fold_one_const(
    bop: crate::ast::BinOp,
    c: &Constant,
    other: mir::LocalId,
    other_is_float: bool,
    const_on_left: bool,
) -> Option<mir::RValue> {
    use crate::ast::BinOp;
    use mir::RValue;
    use Constant::{Bool, Int, Nil};

    // Skip when the live operand may hold a float: the VM's F* handlers
    // coerce the int constant, and IEEE semantics differ at ±0.0/NaN.
    let guard = !other_is_float;
    match (const_on_left, bop, c) {
        (true, BinOp::Add | BinOp::BitOr | BinOp::BitXor, Int(0)) if guard => {
            Some(RValue::Load(other))
        }
        (true, BinOp::Mul, Int(1)) if guard => Some(RValue::Load(other)),
        (true, BinOp::Mul, Int(0)) if guard => Some(RValue::Const(Int(0))),
        (true, BinOp::And, Bool(true)) if guard => Some(RValue::Load(other)),
        (true, BinOp::And, Bool(false)) if guard => Some(RValue::Const(Bool(false))),
        (true, BinOp::And, Nil) if guard => Some(RValue::Const(Bool(false))),
        (true, BinOp::Or, Bool(true)) if guard => Some(RValue::Const(Bool(true))),
        (true, BinOp::Or, Bool(false)) if guard => Some(RValue::Load(other)),
        (true, BinOp::Or, Nil) if guard => Some(RValue::Load(other)),
        (
            false,
            BinOp::Add | BinOp::Sub | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr,
            Int(0),
        ) if guard => Some(RValue::Load(other)),
        (false, BinOp::Mul | BinOp::Div, Int(1)) if guard => Some(RValue::Load(other)),
        (false, BinOp::Mul, Int(0)) if guard => Some(RValue::Const(Int(0))),
        (false, BinOp::And, Bool(true)) if guard => Some(RValue::Load(other)),
        (false, BinOp::And, Bool(false)) if guard => Some(RValue::Const(Bool(false))),
        (false, BinOp::And, Nil) if guard => Some(RValue::Const(Bool(false))),
        (false, BinOp::Or, Bool(true)) if guard => Some(RValue::Const(Bool(true))),
        (false, BinOp::Or, Bool(false)) if guard => Some(RValue::Load(other)),
        (false, BinOp::Or, Nil) if guard => Some(RValue::Load(other)),
        _ => None,
    }
}

/// Thread jumps through trampoline blocks: a block with zero statements and
/// a `Jump` terminator (that isn't the function entry) is bypassed by
/// rewriting every reference to it (terminator targets and handler-table
/// `body` fields) to its final non-trampoline target, then marked
/// unreachable (`Return`). The dead block stays in `func.blocks` (a single
/// `Ret` instruction) so BlockId→index alignment in `compile_function` is
/// preserved. Returns true if anything changed.
fn thread_jumps(func: &mut mir::Function) -> bool {
    let is_trampoline: Vec<bool> = func
        .blocks
        .iter()
        .map(|b| {
            b.stmts.is_empty()
                && b.id != func.entry
                && matches!(b.terminator, mir::Terminator::Jump(t) if t != b.id)
        })
        .collect();
    let is_tramp =
        |id: mir::BlockId| -> bool { is_trampoline.get(id.0 as usize).copied().unwrap_or(false) };

    // Follow each trampoline's Jump chain to its final non-trampoline
    // target. A cycle (or a chain into the entry block) yields no entry —
    // those trampolines are left untouched.
    let mut final_target: std::collections::HashMap<mir::BlockId, mir::BlockId> =
        std::collections::HashMap::new();
    for (bi, block) in func.blocks.iter().enumerate() {
        if !is_trampoline[bi] {
            continue;
        }
        let mut cur = block.id;
        let mut seen: HashSet<mir::BlockId> = HashSet::new();
        let mut target = None;
        while let Some(t) = match &func.blocks[cur.0 as usize].terminator {
            mir::Terminator::Jump(t) => Some(*t),
            _ => None,
        } {
            if !seen.insert(cur) {
                target = None;
                break;
            }
            if is_tramp(t) {
                cur = t;
            } else {
                target = Some(t);
                break;
            }
        }
        if let Some(t) = target {
            final_target.insert(block.id, t);
        }
    }
    if final_target.is_empty() {
        return false;
    }

    let mut changed = false;
    for block in &mut func.blocks {
        match &mut block.terminator {
            mir::Terminator::Jump(t) => {
                if let Some(&ft) = final_target.get(t) {
                    *t = ft;
                    changed = true;
                }
            }
            mir::Terminator::Branch { then_, else_, .. } => {
                if let Some(&ft) = final_target.get(then_) {
                    *then_ = ft;
                    changed = true;
                }
                if let Some(&ft) = final_target.get(else_) {
                    *else_ = ft;
                    changed = true;
                }
            }
            _ => {}
        }
    }
    for table in &mut func.handler_tables {
        for binding in &mut table.bindings {
            if let Some(&ft) = final_target.get(&binding.body) {
                binding.body = ft;
                changed = true;
            }
        }
    }
    for block in &mut func.blocks {
        if is_tramp(block.id) && final_target.contains_key(&block.id) {
            block.terminator = mir::Terminator::Return(None);
            changed = true;
        }
    }
    changed
}

/// Drop stores whose destination is never read anywhere in the function.
///
/// Block-local liveness alone would be unsound: a "dead" store can feed a
/// loop back-edge or a join point in a different block (e.g. a
/// loop-carried variable whose next-iteration read sits earlier in the same
/// block). The conservative condition is a function-wide read set: the
/// store is removable only when its dst is read nowhere at all. Captured
/// locals are excluded too — a closure reads them through `CapLoad` outside
/// this function's MIR.
///
/// Self-moves (`x = Load(x)`) are removed unconditionally: the register
/// already holds the value, so the statement is a no-op.
fn dead_store_elim(func: &mut mir::Function) -> bool {
    let mut reads: HashSet<mir::LocalId> = HashSet::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            stmt_reads(stmt, &mut reads);
        }
        terminator_reads(&block.terminator, &mut reads);
    }

    let mut changed = false;
    for block in &mut func.blocks {
        let block_id = block.id;
        let mut removed: Vec<usize> = Vec::new();
        let mut kept: Vec<mir::Stmt> = Vec::with_capacity(block.stmts.len());
        for (si, stmt) in std::mem::take(&mut block.stmts).into_iter().enumerate() {
            let removable = match &stmt {
                mir::Stmt::Assign { dst, op } => {
                    let self_move = matches!(op, mir::RValue::Load(src) if src == dst);
                    // Source-named locals stay visible to the debugger at
                    // breakpoints: constant propagation can make their
                    // definition look dead (the folded RValue no longer
                    // references them), but removing the store would make
                    // the paused frame report nil. Only anonymous temps and
                    // compiler-generated names (hir_lower's `__tmpN`) are
                    // safe to drop.
                    let named = func
                        .locals
                        .get(dst.0 as usize)
                        .and_then(|l| l.name.as_deref())
                        .map(|n| !n.starts_with("__"))
                        .unwrap_or(false);
                    self_move
                        || (!named
                            && !func.captures.contains(dst)
                            && !reads.contains(dst)
                            && !rvalue_side_effecting(op))
                }
                _ => false,
            };
            if removable {
                removed.push(si);
                changed = true;
            } else {
                kept.push(stmt);
            }
        }
        // Fix up source-line indices for surviving statements: entries for
        // removed statements are dropped, entries after them shift down.
        if !removed.is_empty() {
            let mut new_line_table = Vec::with_capacity(func.line_table.len());
            for &((b, si), line) in &func.line_table {
                if b == block_id {
                    if removed.contains(&si) {
                        continue;
                    }
                    let shifted = removed.iter().filter(|&&r| r < si).count();
                    new_line_table.push(((b, si - shifted), line));
                } else {
                    new_line_table.push(((b, si), line));
                }
            }
            func.line_table = new_line_table;
        }
        block.stmts = kept;
    }
    changed
}

/// Whether evaluating `rv` has an observable side effect. Only
/// side-effect-free RValues may be dropped by dead-store elimination.
fn rvalue_side_effecting(rv: &mir::RValue) -> bool {
    matches!(
        rv,
        mir::RValue::Call { .. }
            | mir::RValue::Perform { .. }
            | mir::RValue::PerformAsync { .. }
            | mir::RValue::SignalWait { .. }
            | mir::RValue::Receive
            | mir::RValue::ReceiveMatch { .. }
            | mir::RValue::ReceiveWait { .. }
            | mir::RValue::ReceiveCommit
            | mir::RValue::FFICall { .. }
            | mir::RValue::Migrate { .. }
            | mir::RValue::Spawn { .. }
            | mir::RValue::Send { .. }
            | mir::RValue::Resume(..)
            | mir::RValue::Ask { .. }
    )
}

/// Collect every local read by a statement.
fn stmt_reads(stmt: &mir::Stmt, out: &mut HashSet<mir::LocalId>) {
    use mir::Stmt;
    match stmt {
        Stmt::Assign { op, .. } => rvalue_reads(op, out),
        Stmt::StoreFieldNamed { obj, src, .. } => {
            out.insert(*obj);
            out.insert(*src);
        }
        Stmt::ArrayStore { arr, idx, src } => {
            out.insert(*arr);
            out.insert(*idx);
            out.insert(*src);
        }
        Stmt::Emit { args, .. } => {
            for a in args {
                out.insert(*a);
            }
        }
        Stmt::StateSet { src, .. } => {
            out.insert(*src);
        }
        Stmt::EnterHandle { .. } | Stmt::PopHandler => {}
    }
}

/// Collect every local read by a terminator.
fn terminator_reads(term: &mir::Terminator, out: &mut HashSet<mir::LocalId>) {
    use mir::Terminator;
    match term {
        Terminator::Return(Some(x)) | Terminator::Resume(x) => {
            out.insert(*x);
        }
        Terminator::Branch { cond, .. } => {
            out.insert(*cond);
        }
        Terminator::Return(None) | Terminator::Jump(_) | Terminator::Unterminated => {}
    }
}

/// Collect every local read by an RValue.
fn rvalue_reads(rv: &mir::RValue, out: &mut HashSet<mir::LocalId>) {
    use mir::RValue;
    match rv {
        RValue::Const(_)
        | RValue::SignalWait { .. }
        | RValue::Receive
        | RValue::ReceiveMatch { .. }
        | RValue::ReceiveCommit
        | RValue::SelfRef
        | RValue::Panic(_)
        | RValue::StateGet { .. } => {}
        RValue::Load(x)
        | RValue::ArrayLen(x)
        | RValue::Unary(_, x)
        | RValue::Resume(x)
        | RValue::CapabilityCheck { val: x } => {
            out.insert(*x);
        }
        RValue::LoadFieldNamed { obj, .. } | RValue::LoadFieldPos { obj, .. } => {
            out.insert(*obj);
        }
        RValue::ArrayLoad { arr, idx }
        | RValue::Binary(_, arr, idx)
        | RValue::StringEq(arr, idx)
        | RValue::StrConcat(arr, idx)
        | RValue::Migrate {
            actor: arr,
            node: idx,
        } => {
            out.insert(*arr);
            out.insert(*idx);
        }
        RValue::ArrayLit(xs) | RValue::Tuple(xs) => {
            for x in xs {
                out.insert(*x);
            }
        }
        RValue::Closure { captures, .. } => {
            for x in captures {
                out.insert(*x);
            }
        }
        RValue::Call { func, args, .. } => {
            if let mir::FuncRef::Local(x) = func {
                out.insert(*x);
            }
            for x in args {
                out.insert(*x);
            }
        }
        RValue::FFICall { args, .. } => {
            for x in args {
                out.insert(*x);
            }
        }
        RValue::Perform { args, .. } | RValue::PerformAsync { args, .. } => {
            for x in args {
                out.insert(*x);
            }
        }
        RValue::Record(pairs) => {
            for (_, x) in pairs {
                out.insert(*x);
            }
        }
        RValue::RecordUpdate { base, overrides } => {
            out.insert(*base);
            for (_, x) in overrides {
                out.insert(*x);
            }
        }
        RValue::ReceiveWait { timeout, .. } => {
            out.insert(*timeout);
        }
        RValue::Spawn {
            init, target_node, ..
        } => {
            if let Some(n) = target_node {
                out.insert(*n);
            }
            for (_, init_rv) in init {
                rvalue_reads(init_rv, out);
            }
        }
        RValue::Send { actor, args, .. } | RValue::Ask { actor, args, .. } => {
            out.insert(*actor);
            for x in args {
                out.insert(*x);
            }
        }
    }
}

pub fn compile_mir(mir: &mut mir::Module, module_name: impl Into<String>) -> NuResult<CodeModule> {
    let mut codegen = MirCodegen::new(module_name);
    codegen.compile_module(mir)?;
    Ok(codegen.finish())
}

// ---------------------------------------------------------------------------
// Liveness-based Drop planning
// ---------------------------------------------------------------------------
//
// The VM's `Drop` opcode releases a register's local reference to a heap
// object and clears the register to nil (so duplicate drops are no-ops).
// This pass decides where to emit it. The goal is conservative correctness:
// when in doubt, no drop is emitted and the value lives until actor exit.
//
// A local is a *candidate* when the analysis can prove its register always
// holds the value's only counted reference (besides references taken by the
// retaining store barriers, which keep the object alive independently):
//
//   - its type may hold a NaN-boxed heap pointer (MIR temp types are
//     unreliable, so only definitely-scalar types are excluded);
//   - it is not a parameter, closure capture, or effect-handler parameter
//     (those arrive through plain, uncounted register copies);
//   - it has at least one definition (never-assigned registers may hold
//     VM-written values such as ReceiveMatch payloads, which follow the
//     foreign-ref protocol and must not be dropped locally);
//   - every definition is an owning rvalue — Tuple/Record/ArrayLit (fresh
//     allocation) or Const (never a heap pointer) — that does not read the
//     local itself;
//   - no use copies the value through an uncounted channel: Move/Load,
//     `&`/`*`, call or effect arguments, closure captures, sends/asks,
//     returns/resumes, `StateSet`, or the AI builtins' staging moves.
//
// Uses through the retaining barriers (container element stores) and
// read-only uses (container base/length, operands, branch conditions) do
// not disqualify: after a retaining store the slot owns its own reference,
// so releasing the register's duplicate is sound.
//
// Escapees: a local defined by a field/element load (`RecL`/`FieldL`/
// `ArrLoad`) from a candidate aliases that container's slots *without* a
// counted reference, so a candidate is never dropped at a point where one
// of its (transitive) escapees is still live.
//
// Drop points per candidate: before every redefinition (release the old
// value; always sound — the register is nil after any earlier drop), after
// a definition whose value is immediately dead, after a last read-only or
// retaining use, and at the entry of blocks the value flows into but never
// uses (branch-edge death). A value whose last use is a branch condition
// cannot be dropped there (the terminator must run first) and simply lives
// until actor exit.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UseKind {
    /// Read without copying the value bits (container base, length, operand).
    ReadOnly,
    /// Copied into a heap container through a retaining barrier — the
    /// register's own reference may be released afterwards.
    Retaining,
    /// Copied through a channel that takes no counted reference (Move/Load,
    /// call staging, send, capture, return, actor state).
    Copy,
}

/// Locals of these types can hold NaN-boxed heap pointers at runtime. MIR
/// temps are often typed `Type::unit()` while carrying pointers, so only
/// definitely-scalar types are excluded.
fn may_hold_heap_ptr(ty: &Type) -> bool {
    match ty {
        Type::Primitive(p) => matches!(p, PrimitiveType::String | PrimitiveType::Unit),
        Type::Tuple(_)
        | Type::Record(_)
        | Type::Array(_)
        | Type::App { .. }
        | Type::Var(_)
        | Type::Variant(_)
        | Type::Function { .. }
        | Type::Actor { .. }
        | Type::Scheme { .. }
        | Type::Reference { .. } => true,
        Type::Skolem(_) => false,
        Type::Nominal { underlying, .. } => may_hold_heap_ptr(underlying),
    }
}

/// The rvalue forms whose result is a freshly allocated heap object (or a
/// non-pointer constant) owned solely by the destination register.
fn rvalue_is_owning(op: &mir::RValue) -> bool {
    matches!(
        op,
        mir::RValue::Tuple(_)
            | mir::RValue::Record(_)
            | mir::RValue::RecordUpdate { .. }
            | mir::RValue::ArrayLit(_)
            | mir::RValue::Const(_)
    )
}

/// Every occurrence of a local inside an rvalue, with how the value is used.
fn rvalue_uses(op: &mir::RValue) -> Vec<(usize, UseKind)> {
    use mir::RValue::*;
    let mut out = Vec::new();
    let ro = |out: &mut Vec<(usize, UseKind)>, id: mir::LocalId| {
        out.push((id.0 as usize, UseKind::ReadOnly))
    };
    let ret = |out: &mut Vec<(usize, UseKind)>, id: mir::LocalId| {
        out.push((id.0 as usize, UseKind::Retaining))
    };
    let cp = |out: &mut Vec<(usize, UseKind)>, id: mir::LocalId| {
        out.push((id.0 as usize, UseKind::Copy))
    };
    match op {
        Const(_)
        | SignalWait { .. }
        | Receive
        | ReceiveMatch { .. }
        | ReceiveCommit
        | Spawn { .. }
        | SelfRef
        | Panic(_)
        | StateGet { .. }
        | mir::RValue::Resume(..) => {}
        // The timeout value is staged into r0 with a plain Move — an
        // uncounted copy channel like call/effect argument staging.
        ReceiveWait { timeout, .. } => cp(&mut out, *timeout),
        Load(x) => cp(&mut out, *x),
        LoadFieldNamed { obj, .. } | LoadFieldPos { obj, .. } => ro(&mut out, *obj),
        ArrayLoad { arr, idx } => {
            ro(&mut out, *arr);
            ro(&mut out, *idx);
        }
        ArrayLen(x) => ro(&mut out, *x),
        ArrayLit(elems) => {
            for e in elems {
                ret(&mut out, *e);
            }
        }
        Unary(_, x) => cp(&mut out, *x),
        Binary(_, l, r) => {
            ro(&mut out, *l);
            ro(&mut out, *r);
        }
        StringEq(l, r) => {
            ro(&mut out, *l);
            ro(&mut out, *r);
        }
        StrConcat(l, r) => {
            ro(&mut out, *l);
            ro(&mut out, *r);
        }
        Call { func, args } => {
            if let mir::FuncRef::Local(f) = func {
                cp(&mut out, *f);
            }
            for a in args {
                cp(&mut out, *a);
            }
        }
        Closure { captures, .. } => {
            for c in captures {
                cp(&mut out, *c);
            }
        }
        Tuple(elems) => {
            for e in elems {
                ret(&mut out, *e);
            }
        }
        Record(fields) => {
            for (_, e) in fields {
                ret(&mut out, *e);
            }
        }
        RecordUpdate { base, overrides } => {
            ro(&mut out, *base);
            for (_, e) in overrides {
                ret(&mut out, *e);
            }
        }
        Perform { args, .. } | PerformAsync { args, .. } | FFICall { args, .. } => {
            for a in args {
                cp(&mut out, *a);
            }
        }
        Migrate { actor, node } => {
            cp(&mut out, *actor);
            cp(&mut out, *node);
        }
        CapabilityCheck { val } => cp(&mut out, *val),
        Send { actor, args, .. } | Ask { actor, args, .. } => {
            cp(&mut out, *actor);
            for a in args {
                cp(&mut out, *a);
            }
        }
    }
    out
}

/// Every occurrence of a local inside a statement (an assignment's
/// destination is a definition, not a use).
fn stmt_uses(stmt: &mir::Stmt) -> Vec<(usize, UseKind)> {
    match stmt {
        mir::Stmt::Assign { op, .. } => rvalue_uses(op),
        mir::Stmt::StoreFieldNamed { obj, src, .. } => vec![
            (obj.0 as usize, UseKind::ReadOnly),
            (src.0 as usize, UseKind::Retaining),
        ],
        mir::Stmt::ArrayStore { arr, idx, src } => vec![
            (arr.0 as usize, UseKind::ReadOnly),
            (idx.0 as usize, UseKind::ReadOnly),
            (src.0 as usize, UseKind::Retaining),
        ],
        mir::Stmt::EnterHandle { .. } | mir::Stmt::PopHandler => Vec::new(),
        mir::Stmt::Emit { args, .. } => {
            args.iter().map(|a| (a.0 as usize, UseKind::Copy)).collect()
        }
        // StateSet stores into actor state without retaining, so the stored
        // value must keep its register reference: treat it as a copy.
        mir::Stmt::StateSet { src, .. } => vec![(src.0 as usize, UseKind::Copy)],
    }
}

fn terminator_uses(term: &mir::Terminator) -> Vec<(usize, UseKind)> {
    match term {
        mir::Terminator::Return(Some(v)) | mir::Terminator::Resume(v) => {
            vec![(v.0 as usize, UseKind::Copy)]
        }
        mir::Terminator::Branch { cond, .. } => vec![(cond.0 as usize, UseKind::ReadOnly)],
        _ => Vec::new(),
    }
}

/// Successor block indices of a terminator (block ids are dense indices
/// into `Function::blocks`).
fn terminator_successors(term: &mir::Terminator) -> Vec<usize> {
    match term {
        mir::Terminator::Jump(t) => vec![t.0 as usize],
        mir::Terminator::Branch { then_, else_, .. } => {
            vec![then_.0 as usize, else_.0 as usize]
        }
        _ => Vec::new(),
    }
}

/// Where to emit `Drop` instructions for one function.
#[derive(Default)]
struct DropPlan {
    /// Before the block's first statement (after any handler prologue).
    block_entry: FxHashMap<usize, Vec<mir::LocalId>>,
    before_stmt: FxHashMap<(usize, usize), Vec<mir::LocalId>>,
    after_stmt: FxHashMap<(usize, usize), Vec<mir::LocalId>>,
}

/// Compute conservative `Drop` placements for one function; see the section
/// docs above for the soundness argument.
fn plan_drops(func: &mir::Function) -> DropPlan {
    let mut plan = DropPlan::default();
    let nlocals = func.locals.len();
    let nblocks = func.blocks.len();
    if nlocals == 0 || nblocks == 0 {
        return plan;
    }

    let ptr_ty: Vec<bool> = func
        .locals
        .iter()
        .map(|l| may_hold_heap_ptr(&l.ty))
        .collect();

    // Locals that receive their value outside MIR assignments can never be
    // proven solely owned.
    let mut excluded = vec![false; nlocals];
    for id in func.params.iter().chain(&func.captures) {
        excluded[id.0 as usize] = true;
    }
    for table in &func.handler_tables {
        for binding in &table.bindings {
            for id in &binding.params {
                excluded[id.0 as usize] = true;
            }
        }
    }

    // Scan defs and uses for the whole function.
    let mut has_def = vec![false; nlocals];
    let mut defs_owning = vec![true; nlocals];
    let mut no_copy_use = vec![true; nlocals];
    let mut block_defs: Vec<HashSet<usize>> = (0..nblocks).map(|_| HashSet::new()).collect();
    let mut block_uses: Vec<HashSet<usize>> = (0..nblocks).map(|_| HashSet::new()).collect();
    // (dst, base) pairs of field/element loads, for escapee tracking.
    let mut loads: Vec<(usize, usize)> = Vec::new();

    for (bi, block) in func.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            for (u, kind) in stmt_uses(stmt) {
                block_uses[bi].insert(u);
                if kind == UseKind::Copy {
                    no_copy_use[u] = false;
                }
            }
            if let mir::Stmt::Assign { dst, op } = stmt {
                let d = dst.0 as usize;
                has_def[d] = true;
                block_defs[bi].insert(d);
                if !rvalue_is_owning(op) || rvalue_uses(op).iter().any(|(u, _)| *u == d) {
                    defs_owning[d] = false;
                }
                match op {
                    mir::RValue::LoadFieldNamed { obj, .. }
                    | mir::RValue::LoadFieldPos { obj, .. } => loads.push((d, obj.0 as usize)),
                    mir::RValue::ArrayLoad { arr, .. } => loads.push((d, arr.0 as usize)),
                    _ => {}
                }
            }
        }
        for (u, kind) in terminator_uses(&block.terminator) {
            block_uses[bi].insert(u);
            if kind == UseKind::Copy {
                no_copy_use[u] = false;
            }
        }
    }

    let candidate: Vec<bool> = (0..nlocals)
        .map(|i| ptr_ty[i] && !excluded[i] && has_def[i] && defs_owning[i] && no_copy_use[i])
        .collect();

    // Escapees: locals defined by field/element loads from a candidate or
    // another escapee (transitively).
    let mut escapees: Vec<Vec<usize>> = (0..nlocals).map(|_| Vec::new()).collect();
    for c in 0..nlocals {
        if !candidate[c] {
            continue;
        }
        let mut seen = HashSet::new();
        let mut frontier = vec![c];
        while let Some(x) = frontier.pop() {
            for &(dst, base) in &loads {
                if base == x && ptr_ty[dst] && seen.insert(dst) {
                    escapees[c].push(dst);
                    frontier.push(dst);
                }
            }
        }
    }
    let esc_clear = |c: usize, live: &HashSet<usize>| escapees[c].iter().all(|e| !live.contains(e));

    // Backward may-liveness over all locals.
    let mut live_in: Vec<HashSet<usize>> = (0..nblocks).map(|_| HashSet::new()).collect();
    let mut live_out: Vec<HashSet<usize>> = (0..nblocks).map(|_| HashSet::new()).collect();
    loop {
        let mut changed = false;
        for bi in (0..nblocks).rev() {
            let mut out: HashSet<usize> = HashSet::new();
            for succ in terminator_successors(&func.blocks[bi].terminator) {
                for l in &live_in[succ] {
                    out.insert(*l);
                }
            }
            let mut inset = out.clone();
            for d in &block_defs[bi] {
                inset.remove(d);
            }
            for u in &block_uses[bi] {
                inset.insert(*u);
            }
            if inset != live_in[bi] || out != live_out[bi] {
                live_in[bi] = inset;
                live_out[bi] = out;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Walk each block backward, emitting drops where a candidate's value
    // dies.
    for (bi, block) in func.blocks.iter().enumerate() {
        let mut live: HashSet<usize> = live_out[bi].clone();
        for (u, _) in terminator_uses(&block.terminator) {
            live.insert(u);
        }
        for (si, stmt) in block.stmts.iter().enumerate().rev() {
            let uses = stmt_uses(stmt);
            // Last-use drops for candidates this statement reads.
            for (u, _) in &uses {
                if candidate[*u] && !live.contains(u) && esc_clear(*u, &live) {
                    plan.after_stmt
                        .entry((bi, si))
                        .or_default()
                        .push(func.locals[*u].id);
                }
            }
            for (u, _) in &uses {
                live.insert(*u);
            }
            if let mir::Stmt::Assign { dst, .. } = stmt {
                let d = dst.0 as usize;
                if candidate[d] {
                    // The new value is dead on arrival: release it right
                    // after the statement.
                    if !live.contains(&d) && esc_clear(d, &live) {
                        plan.after_stmt.entry((bi, si)).or_default().push(*dst);
                    }
                    // Release the overwritten old value before the
                    // statement. Always sound for a candidate: the register
                    // holds the previous definition's product (or nil after
                    // an earlier drop), never an alias.
                    if esc_clear(d, &live) {
                        plan.before_stmt.entry((bi, si)).or_default().push(*dst);
                    }
                }
                live.remove(&d);
            }
        }
        // Entry drops: candidates held on some incoming edge but dead at
        // this block's entry (their value died at the branch that led here).
        let mut held_in: HashSet<usize> = HashSet::new();
        for (pj, pred) in func.blocks.iter().enumerate() {
            if terminator_successors(&pred.terminator).contains(&bi) {
                for l in &live_out[pj] {
                    held_in.insert(*l);
                }
            }
        }
        for c in 0..nlocals {
            if candidate[c]
                && held_in.contains(&c)
                && !live_in[bi].contains(&c)
                && esc_clear(c, &live_in[bi])
            {
                plan.block_entry
                    .entry(bi)
                    .or_default()
                    .push(func.locals[c].id);
            }
        }
    }

    for ids in plan
        .block_entry
        .values_mut()
        .chain(plan.before_stmt.values_mut())
        .chain(plan.after_stmt.values_mut())
    {
        ids.sort();
        ids.dedup();
    }
    plan
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;
    use crate::vm::VM;

    fn compile_mir_source(source: &str) -> NuResult<CodeModule> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module()?;

        let mut type_checker = TypeChecker::new();
        type_checker.check_module(&ast)?;

        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir)?;
        compile_mir(&mut mir, "test")
    }

    fn run_mir_source(source: &str) -> NuResult<crate::vm::Value> {
        let module = compile_mir_source(source)?;
        let mut vm = VM::new();
        vm.load_module(module);
        vm.run()
    }

    #[test]
    fn test_mir_codegen_simple_arithmetic() {
        let value = run_mir_source("1 + 2 * 3").unwrap();
        assert_eq!(value.as_int(), Some(7));
    }

    #[test]
    fn test_mir_codegen_bitwise_or() {
        let value = run_mir_source("6 ||| 3").unwrap();
        assert_eq!(value.as_int(), Some(7));
    }

    #[test]
    fn test_mir_codegen_if_expression_position() {
        // Statements after an expression-position if must run after it.
        let value = run_mir_source("let x = if true then 1 else 2 in x + 10").unwrap();
        assert_eq!(value.as_int(), Some(11));
    }

    #[test]
    fn test_mir_codegen_recursive_closure() {
        let value = run_mir_source(
            "let fib = fn(n) { if n <= 1 then n else fib(n - 1) + fib(n - 2) } in fib(10)",
        )
        .unwrap();
        assert_eq!(value.as_int(), Some(55));
    }

    #[test]
    fn test_mir_codegen_closure_capture() {
        let value = run_mir_source("let a = 40 in let add = fn(x) { x + a } in add(2)").unwrap();
        assert_eq!(value.as_int(), Some(42));
    }

    /// Only a bare `ident = v` parses as the dedicated Expr::Assign AST node;
    /// `arr[i] = v` and `record.f = v` are ordinary-looking BinOp::Assign
    /// binary expressions instead. Both must route through place-based
    /// lowering (regression: the stable compiler does NOT do this for
    /// non-self targets — see test_legacy_index_assign_is_a_noop_bug below).
    #[test]
    fn test_mir_codegen_index_and_field_assign() {
        let value = run_mir_source("let arr = [1, 2, 3] in { arr[0] = 99 arr[0] }").unwrap();
        assert_eq!(
            value.as_int(),
            Some(99),
            "arr[0] = 99 should actually mutate the array"
        );

        let value = run_mir_source("let r = { x: 1, y: 2 } in { r.x = 99 r.x + r.y }").unwrap();
        assert_eq!(
            value.as_int(),
            Some(101),
            "r.x = 99 should actually mutate the record"
        );
    }

    #[test]
    fn test_mir_codegen_assign_expression_yields_assigned_value() {
        // Mirrors the stable compiler's compile_assign, which returns the
        // assigned value rather than unit — an assignment used as a block's
        // trailing expression must yield that value, not unit.
        let value = run_mir_source("let x = &1 in { x = 2 }").unwrap();
        assert_eq!(
            value.as_int(),
            Some(2),
            "`x = 2` as an expression should yield 2"
        );

        let value = run_mir_source("let r = { x: 1 } in { r.x = 5 }").unwrap();
        assert_eq!(
            value.as_int(),
            Some(5),
            "`r.x = 5` as an expression should yield 5"
        );

        let value = run_mir_source("let arr = [1, 2] in { arr[0] = 7 }").unwrap();
        assert_eq!(
            value.as_int(),
            Some(7),
            "`arr[0] = 7` as an expression should yield 7"
        );
    }

    #[test]
    fn test_mir_codegen_ref_cell_deref_and_assign() {
        // Mirrors src/integration_tests.rs's test_local_assignment (legacy
        // pipeline): `&` creates a ref cell, `*` dereferences it, and
        // assignment mutates it in place.
        let value = run_mir_source("let x = &10 in { x = 3; *x }").unwrap();
        assert_eq!(value.as_int(), Some(3));
    }

    #[test]
    fn test_mir_codegen_over_limit_params_is_honest_error_not_corruption() {
        // A function with more than MAX_STAGED_ARGS (12) parameters used to
        // compile "successfully" with a prologue that reads incoming
        // arguments from registers overlapping LOCAL_BASE-mapped locals —
        // corrupt bytecode nothing could ever validly call (every call site
        // is bounded by the same 12-arg staging limit), but a compile error
        // is the honest outcome, matching this pipeline's "no silent
        // misbehavior" guarantee.
        let params = (0..13)
            .map(|i| format!("a{}: Int", i))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!("fn f({}) -> Int {{ a0 }}\n0", params);
        let result = compile_mir_source(&source);
        assert!(
            matches!(result, Err(NuError::VMError { .. })),
            "a 13-parameter function should be an honest compile error, got {:?}",
            result
        );
    }

    #[test]
    fn test_mir_codegen_field_id_errors_past_256_distinct_field_names() {
        // Mirrors the same regression in the stable compiler: the 257th
        // distinct record field name has no free u8 id, and must be an
        // honest error rather than silently aliasing onto an existing id.
        //
        // Each field name lives in its own top-level function's own tiny
        // record literal, not a single 257-field record — a single record
        // (or a chain of 257 `let`s) hits MIR's unrelated per-function local
        // count cap first, which would mask the field_id check this test is
        // actually targeting.
        let fns: Vec<String> = (0..257)
            .map(|i| format!("fn g{i}() -> Int {{ {{ f{i}: {i} }}.f{i} }}"))
            .collect();
        let source = format!("{}\ng0()", fns.join("\n"));
        let result = compile_mir_source(&source);
        assert!(
            result.is_err(),
            "the 257th distinct field name should be an honest error, not silent aliasing"
        );
    }

    #[test]
    fn test_mir_codegen_effect_handler() {
        let value =
            run_mir_source("handle perform Math.getAnswer() { | Math.getAnswer() => 42 }").unwrap();
        assert_eq!(value.as_int(), Some(42));
    }

    #[test]
    fn test_mir_codegen_float_arithmetic() {
        // Binary/unary opcode emission is type-directed: float operands
        // must compile to FAdd/FSub/FMul/FDiv/FNeg — the integer handlers
        // coerce float operands to 0.
        let value = run_mir_source("1.5 + 2.5").unwrap();
        assert_eq!(value.as_float(), Some(4.0));
        let value = run_mir_source("5.5 - 2.0").unwrap();
        assert_eq!(value.as_float(), Some(3.5));
        let value = run_mir_source("1.5 * 2.0").unwrap();
        assert_eq!(value.as_float(), Some(3.0));
        let value = run_mir_source("7.0 / 2.0").unwrap();
        assert_eq!(value.as_float(), Some(3.5));
        let value = run_mir_source("-1.5").unwrap();
        assert_eq!(value.as_float(), Some(-1.5));
    }

    #[test]
    fn test_mir_codegen_float_arithmetic_through_locals() {
        // Float-ness propagates through let bindings and intermediate
        // temps: `y` holds a float even though hir_lower types binary
        // results as Int.
        let value = run_mir_source("let x = 1.5 in let y = x + 2.5 in y * 2.0").unwrap();
        assert_eq!(value.as_float(), Some(8.0));
        let value = run_mir_source("let a = 6.0 in let b = a / 4.0 in b").unwrap();
        assert_eq!(value.as_float(), Some(1.5));
    }

    #[test]
    fn test_mir_codegen_float_comparisons() {
        // Integer comparisons on float operands coerce both sides to 0,
        // making `2.0 == 3.0` true and every ordering comparison false
        // (or always-true for Le/Ge/Ne); floats need FCmp*.
        let value = run_mir_source("1.5 < 2.5").unwrap();
        assert_eq!(value.as_bool(), Some(true));
        let value = run_mir_source("2.5 > 1.5").unwrap();
        assert_eq!(value.as_bool(), Some(true));
        let value = run_mir_source("2.5 <= 1.5").unwrap();
        assert_eq!(value.as_bool(), Some(false));
        let value = run_mir_source("1.5 >= 2.5").unwrap();
        assert_eq!(value.as_bool(), Some(false));
        let value = run_mir_source("2.0 == 3.0").unwrap();
        assert_eq!(value.as_bool(), Some(false));
        let value = run_mir_source("2.0 != 3.0").unwrap();
        assert_eq!(value.as_bool(), Some(true));
        let value = run_mir_source("2.0 == 2.0").unwrap();
        assert_eq!(value.as_bool(), Some(true));
    }

    #[test]
    fn test_mir_codegen_float_div_by_zero_yields_nil() {
        // Matches the interpreter's FDiv semantics: a zero float divisor
        // yields nil, not a trap or inf.
        let value = run_mir_source("7.0 / 0.0").unwrap();
        assert_eq!(value.as_raw(), crate::vm::Value::nil().as_raw());
    }

    #[test]
    fn test_mir_codegen_float_modulo() {
        // Float `%` compiles to the FMod opcode (0x35), which the
        // interpreter implements with f64 % f64 semantics and a nil
        // result on a zero divisor, mirroring FDiv.
        let value = run_mir_source("7.5 % 2.0").unwrap();
        assert_eq!(value.as_float(), Some(1.5));
        let value = run_mir_source("7.0 % 0.0").unwrap();
        assert_eq!(value.as_raw(), crate::vm::Value::nil().as_raw());
        // Integer modulo is unaffected.
        let value = run_mir_source("7 % 2").unwrap();
        assert_eq!(value.as_int(), Some(1));
    }

    #[test]
    fn test_mir_codegen_over_limit_handler_params_is_honest_error() {
        // A handler binding with more than MAX_STAGED_ARGS (12) parameters
        // used to compile a prologue moving r12.. into LOCAL_BASE-mapped
        // registers, silently aliasing the enclosing function's locals —
        // the VM only ever stages effect arguments in r0..r11. Like the
        // 13-parameter function check, this must be a compile error.
        let params = (0..13)
            .map(|i| format!("p{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!("handle 0 {{ | E.op({}) => p0 }}", params);
        let result = compile_mir_source(&source);
        assert!(
            matches!(result, Err(NuError::VMError { .. })),
            "a 13-parameter handler binding should be an honest compile error, got {:?}",
            result
        );
        // A 12-parameter binding stays legal.
        let params = (0..12)
            .map(|i| format!("p{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let source = format!("handle 0 {{ | E.op({}) => p0 }}", params);
        assert!(
            compile_mir_source(&source).is_ok(),
            "a 12-parameter handler binding should compile"
        );
    }

    #[test]
    fn test_mir_codegen_actor_spawn_returns_actor_ref() {
        // Actors are now lowered by the HIR/MIR pipeline. Without a real
        // Runtime attached, spawn_actor's default stub always returns
        // actor_ref(0); real behavior semantics (state, ask) are covered by
        // src/integration_tests.rs's MIR-vs-legacy actor tests, which attach
        // a Runtime.
        let value =
            run_mir_source("actor A { state x = 0 behavior get() { self.x } }\nspawn A { x = 0 }")
                .unwrap();
        assert!(
            value.as_actor_id().is_some(),
            "spawn should yield an actor reference"
        );
    }

    #[test]
    fn test_mir_codegen_spill_const_retval() {
        // Verify that Const*/RetVal instructions are correctly rewritten
        // when local registers exceed the file capacity.  Even without
        // actual spilling (40 let bindings fit in 239 registers), this
        // exercises the rewrite pass's handling of these opcodes.
        let locals = (0..40)
            .map(|i| format!("    let a{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let source = format!("fn f() -> Int {{\n{locals}\n    a0 + a39\n}}\nf()");
        let value = run_mir_source(&source).unwrap();
        assert_eq!(value.as_int(), Some(39));
    }

    #[test]
    fn test_mir_codegen_state_field_access_reuses_one_constant() {
        // Every `self.x` read/write used to add a fresh, duplicate string
        // constant to the module's constant pool. A behavior referencing the
        // same field several times should only cost one "x" constant.
        let module = compile_mir_source(
            "actor A { state x = 0 behavior bump() { (self.x = self.x + 1, self.x = self.x + 1, self.x) } }\nspawn A { x = 0 }",
        )
        .unwrap();
        let x_constants = module
            .constants
            .iter()
            .filter(|c| matches!(c, crate::bytecode::Constant::String(s) if s == "x"))
            .count();
        assert_eq!(
            x_constants, 1,
            "repeated self.x access should reuse one constant-pool entry, found {}",
            x_constants
        );
    }

    #[test]
    fn test_mir_codegen_plain_workflow_and_agent_compile() {
        // Sequential workflows and tool-less agents desugar to actors and
        // compile like any other actor declaration.
        let result = compile_mir_source("workflow W { step a { 1 } }");
        assert!(
            result.is_ok(),
            "plain sequential workflow should compile: {:?}",
            result
        );

        let result = compile_mir_source(r#"agent Ag = { model: "gpt-4o" }"#);
        assert!(
            result.is_ok(),
            "tool-less agent should compile: {:?}",
            result
        );
    }

    #[test]
    fn test_mir_codegen_parallel_workflow_compiles() {
        let result = compile_mir_source("workflow W { parallel { step a { 1 } step b { 2 } } }");
        assert!(
            result.is_ok(),
            "parallel workflow should compile: {:?}",
            result
        );
        let module = result.unwrap();
        assert_eq!(module.behaviors.len(), 1);
        assert_eq!(
            module.behaviors[0].parallel_branches,
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn test_mir_codegen_agent_with_resolved_tool_compiles() {
        let result = compile_mir_source(
            r#"
            @tool(description: "Search the web.")
            fn search(query: String) -> String { query }

            agent Ag = { model: "gpt-4o", tools: [search] }
            "#,
        );
        assert!(
            result.is_ok(),
            "agent with a resolvable tool should compile: {:?}",
            result
        );
        let module = result.unwrap();
        assert_eq!(module.actor_metadata.len(), 1);
        assert_eq!(module.actor_metadata[0].tools.len(), 1);
        assert_eq!(module.actor_metadata[0].tools[0].name, "search");
    }

    #[test]
    fn test_mir_codegen_agent_with_unknown_tool_compiles_to_actor() {
        // No @tool-annotated `search` function exists; HIR lowering still
        // produces a well-formed actor (with an empty tool list). The
        // "unknown tool" error is caught at runtime when the agent is
        // spawned, not at compile time.
        let result = compile_mir_source(r#"agent Ag = { model: "gpt-4o", tools: [search] }"#);
        assert!(
            result.is_ok(),
            "agent with an unresolvable tool should still compile to a valid actor, got {:?}",
            result
        );
    }

    #[test]
    fn test_mir_codegen_unknown_call_is_error_not_zero() {
        // Regression: unknown callees used to silently compile to Const0.
        let hir = crate::hir::Module {
            name: "t".into(),
            decls: vec![crate::hir::Decl::Function(crate::hir::FunctionDef {
                name: "__main".into(),
                type_params: vec![],
                params: vec![],
                dict_params: vec![],
                ret: crate::types::Type::unit(),
                effect: crate::types::EffectRow::empty(),
                cap: crate::types::Capability::Ref,
                placement: None,
                body: {
                    let mut b = crate::hir::Body::new();
                    b.push(crate::hir::Stmt::Let {
                        name: "r".into(),
                        ty: crate::types::Type::unit(),
                        value: crate::hir::RValue::Call {
                            func: crate::hir::Operand::Var(
                                "nope".into(),
                                crate::types::Type::unit(),
                            ),
                            args: vec![],
                            ty: crate::types::Type::unit(),
                        },
                        span: Span::default(),
                    });
                    b
                },
                public: false,
                span: Span::default(),
            })],
        };
        let result = crate::mir_lower::lower_module(&hir);
        assert!(result.is_err(), "unknown callee must be a compile error");
    }

    #[test]
    fn test_mir_nested_module_declarations_are_flattened() {
        // Nested `module Name { ... }` blocks are a pure namespacing
        // construct: the stable compiler's compile_decl flattens them by
        // recursing over their inner decls in place, and mir_lower.rs now
        // does the same instead of erroring. Constructed directly against
        // HIR (rather than via source + the type checker) because nested
        // modules don't yet export bindings into the enclosing scope at the
        // type-checker level — a separate, pre-existing gap in both
        // pipelines, unrelated to this mir_lower.rs fix.
        let square_fn = crate::hir::FunctionDef {
            name: "square".into(),
            type_params: vec![],
            params: vec![("x".into(), crate::types::Type::int())],
            dict_params: vec![],
            ret: crate::types::Type::int(),
            effect: crate::types::EffectRow::empty(),
            cap: crate::types::Capability::Ref,
            placement: None,
            body: {
                let mut b = crate::hir::Body::new();
                b.set_terminator(crate::hir::Terminator::Yield(crate::hir::Operand::Var(
                    "__result".into(),
                    crate::types::Type::int(),
                )));
                b.push(crate::hir::Stmt::Let {
                    name: "__result".into(),
                    ty: crate::types::Type::int(),
                    value: crate::hir::RValue::Binary(
                        crate::ast::BinOp::Mul,
                        crate::hir::Operand::Var("x".into(), crate::types::Type::int()),
                        crate::hir::Operand::Var("x".into(), crate::types::Type::int()),
                        crate::types::Type::int(),
                    ),
                    span: Span::default(),
                });
                b
            },
            public: false,
            span: Span::default(),
        };
        let main_fn = crate::hir::FunctionDef {
            name: "__main".into(),
            type_params: vec![],
            params: vec![],
            dict_params: vec![],
            ret: crate::types::Type::int(),
            effect: crate::types::EffectRow::empty(),
            cap: crate::types::Capability::Ref,
            placement: None,
            body: {
                let mut b = crate::hir::Body::new();
                b.set_terminator(crate::hir::Terminator::Yield(crate::hir::Operand::Var(
                    "r".into(),
                    crate::types::Type::int(),
                )));
                b.push(crate::hir::Stmt::Let {
                    name: "r".into(),
                    ty: crate::types::Type::int(),
                    value: crate::hir::RValue::Call {
                        func: crate::hir::Operand::Var("square".into(), crate::types::Type::unit()),
                        args: vec![crate::hir::Operand::Literal(
                            crate::ast::Literal::Int(6),
                            crate::types::Type::int(),
                        )],
                        ty: crate::types::Type::int(),
                    },
                    span: Span::default(),
                });
                b
            },
            public: false,
            span: Span::default(),
        };
        let hir = crate::hir::Module {
            name: "t".into(),
            decls: vec![
                crate::hir::Decl::Module {
                    name: "Math".into(),
                    exports: vec![],
                    decls: vec![crate::hir::Decl::Function(square_fn)],
                    span: Span::default(),
                },
                crate::hir::Decl::Function(main_fn),
            ],
        };
        let mut mir = crate::mir_lower::lower_module(&hir).unwrap();
        let module = crate::mir_codegen::compile_mir(&mut mir, "t").unwrap();
        let mut vm = VM::new();
        vm.load_module(module);
        let value = vm.run().unwrap();
        assert_eq!(
            value.as_int(),
            Some(36),
            "nested module's function should be reachable unqualified"
        );
    }

    #[test]
    fn test_receive_after_emits_receivewait_with_staged_timeout() {
        // receive-after codegen: a Move stages the timeout (ms) into r0,
        // then ReceiveWait (0xA0) carries the candidate-ids spec constant in
        // op1+op2 and the arm-index/payload base register in op3, exactly
        // like ReceiveMatch. (Compiling only — the VM handler is wave 2.)
        let module = compile_mir_source("receive { | Msg(x) => x } after 100 => 0").unwrap();
        let pos = module
            .instructions
            .iter()
            .position(|i| i.opcode == OpCode::ReceiveWait)
            .unwrap_or_else(|| {
                panic!(
                    "receive-after must emit ReceiveWait: {:?}",
                    module.instructions
                )
            });
        let instr = module.instructions[pos];
        // The spec constant is "max_params:id1,id2,..." like ReceiveMatch.
        let spec_idx = instr.imm16() as usize;
        match &module.constants[spec_idx] {
            Constant::String(s) => {
                assert_eq!(
                    s.split(':').next(),
                    Some("1"),
                    "one arm with one param reserves one payload register: {}",
                    s
                );
            }
            other => panic!("spec constant must be a string, got {:?}", other),
        }
        // Immediately before: Move timeout_reg -> r0.
        let prev = module.instructions[pos - 1];
        assert_eq!(prev.opcode, OpCode::Move, "timeout staging move");
        assert_eq!(prev.op2, 0, "timeout must be staged into r0");
        // No ReceiveMatch and no legacy pop-any Receive in the timed form.
        assert!(
            !module
                .instructions
                .iter()
                .any(|i| i.opcode == OpCode::ReceiveMatch),
            "timed receive must not emit ReceiveMatch"
        );
        assert!(
            !module
                .instructions
                .iter()
                .any(|i| i.opcode == OpCode::Receive),
            "receive-after must not emit the legacy pop-any Receive"
        );
    }

    #[test]
    fn test_receive_without_after_emits_receivematch_not_receivewait() {
        let module = compile_mir_source("receive { | Msg(x) => x }").unwrap();
        assert!(
            module
                .instructions
                .iter()
                .any(|i| i.opcode == OpCode::ReceiveMatch),
            "plain receive must emit ReceiveMatch"
        );
        assert!(
            module
                .instructions
                .iter()
                .any(|i| i.opcode == OpCode::Receive),
            "plain receive must keep the legacy fallback"
        );
        assert!(
            !module
                .instructions
                .iter()
                .any(|i| i.opcode == OpCode::ReceiveWait),
            "plain receive must not emit ReceiveWait"
        );
    }

    #[test]
    fn test_debug_line_table_and_functions() {
        let source = "let a = 1 in {\n  let b = a + 1 in {\n    let c = b + 2 in c\n  }\n}";
        crate::types::set_source_map(source);
        let module = compile_mir_source(source).unwrap();
        // The line table maps the innermost statement to source line 3.
        assert!(
            !module.line_table.is_empty(),
            "expected a non-empty line table"
        );
        assert!(
            module.line_table.iter().any(|&(_, l)| l == 3),
            "line table should include line 3, got {:?}",
            module.line_table
        );
        // Breakpoint resolution: line 3 resolves to a verified pc.
        let resolved = module.resolve_line(3);
        assert!(resolved.is_some(), "line 3 should resolve to a pc");
        let (pc, line) = resolved.unwrap();
        assert_eq!(line, 3);
        assert_eq!(module.line_at(pc), Some(3));
        // Debug functions carry a code range and named locals.
        assert!(
            !module.debug_functions.is_empty(),
            "expected debug functions"
        );
        let main = module
            .debug_functions
            .iter()
            .find(|df| df.name == "__main")
            .expect("expected an __main debug entry");
        assert!(main.code_len > 0, "expected a non-empty __main code range");
        assert!(
            main.locals.iter().any(|(_, n)| n.as_deref() == Some("a")),
            "expected local 'a' in __main, got {:?}",
            main.locals
        );
        crate::types::clear_source_map();
    }

    #[test]
    fn test_function_local_counts_populated() {
        // function_local_counts must be parallel to function_table, populated
        // with LOCAL_BASE + locals.len() per function (mirroring
        // BehaviorTableEntry.local_count).
        let source = "fn add(a: Int, b: Int) -> Int { let s = a + b; s }";
        let module = compile_mir_source(source).unwrap();
        assert_eq!(
            module.function_local_counts.len(),
            module.function_table.len(),
            "function_local_counts must be parallel to function_table"
        );
        for &count in &module.function_local_counts {
            assert!(
                count >= LOCAL_BASE as usize && count <= 256,
                "local count {} out of range [LOCAL_BASE, 256]",
                count
            );
        }
        // The `add` function (2 params + 1 local = 3 locals) gets
        // LOCAL_BASE + 3, matching its debug_functions locals entry.
        let add = module
            .debug_functions
            .iter()
            .find(|df| df.name == "add")
            .expect("expected an add debug entry");
        let add_idx = module
            .function_table
            .iter()
            .position(|&off| off == add.code_offset)
            .expect("add code offset must be in function_table");
        assert_eq!(
            module.function_local_counts[add_idx],
            LOCAL_BASE as usize + add.locals.len(),
            "function_local_counts must mirror debug locals"
        );
    }

    #[test]
    fn test_par_block_runs_as_sequential_block() {
        // `par { .. }` is an independence annotation: sequential block
        // semantics (last expression wins).
        let value = run_mir_source("par { 1 + 2; 3 * 4 }").unwrap();
        assert_eq!(value.as_int(), Some(12));
    }
}

// ===========================================================================
// MIR optimization pass tests
// ===========================================================================

#[cfg(test)]
mod optimize_tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;
    use crate::vm::VM;

    fn compile_source(source: &str) -> NuResult<CodeModule> {
        let tokens = Lexer::new(source).lex()?;
        let ast = Parser::new(tokens).parse_module()?;
        let mut type_checker = TypeChecker::new();
        type_checker.check_module(&ast)?;
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir)?;
        compile_mir(&mut mir, "test")
    }

    fn run_source(source: &str) -> NuResult<crate::vm::Value> {
        let module = compile_source(source)?;
        let mut vm = VM::new();
        vm.load_module(module);
        vm.run()
    }

    fn has_opcode(module: &CodeModule, op: OpCode) -> bool {
        module.instructions.iter().any(|i| i.opcode == op)
    }

    #[test]
    fn test_fold_const_add() {
        // `1 + 2` folds to a single constant; no IAdd survives.
        let value = run_source("1 + 2").unwrap();
        assert_eq!(value.as_int(), Some(3));
        let module = compile_source("1 + 2").unwrap();
        assert!(
            !has_opcode(&module, OpCode::IAdd),
            "constant addition must not emit IAdd"
        );
    }

    #[test]
    fn test_fold_const_mul_add() {
        // `1 + 2 * 3` folds to 7; neither opcode survives.
        let value = run_source("1 + 2 * 3").unwrap();
        assert_eq!(value.as_int(), Some(7));
        let module = compile_source("1 + 2 * 3").unwrap();
        assert!(!has_opcode(&module, OpCode::IAdd), "no IAdd after folding");
        assert!(!has_opcode(&module, OpCode::IMul), "no IMul after folding");
    }

    #[test]
    fn test_fold_identity_add_zero() {
        // `x + 0` is a no-op move, not an IAdd.
        let value = run_source("let f = fn(x) { x + 0 } in f(5)").unwrap();
        assert_eq!(value.as_int(), Some(5));
        let module = compile_source("let f = fn(x) { x + 0 } in f(5)").unwrap();
        assert!(
            !has_opcode(&module, OpCode::IAdd),
            "x + 0 must fold to a plain load, not IAdd"
        );
    }

    #[test]
    fn test_fold_zero_propagates() {
        // `x * 0` collapses to 0 — even with a constant x.
        let value = run_source("let x = 42 in x * 0").unwrap();
        assert_eq!(value.as_int(), Some(0));
        let module = compile_source("let x = 42 in x * 0").unwrap();
        assert!(
            !has_opcode(&module, OpCode::IMul),
            "x * 0 must fold away the multiply"
        );
    }

    #[test]
    fn test_fold_double_neg() {
        // `--5` folds through two Unary(Neg) applications to Const(5).
        let value = run_source("--5").unwrap();
        assert_eq!(value.as_int(), Some(5));
        let module = compile_source("--5").unwrap();
        assert!(
            !has_opcode(&module, OpCode::INeg),
            "double negation must fold, not emit INeg"
        );
    }

    #[test]
    fn test_dce_dead_store() {
        // `let x = 1 + 2` folds to `x = Const(3)`, then DCE drops the dead
        // store (x is never read). The block's value is 42.
        let value = run_source("let x = 1 + 2 in 42").unwrap();
        assert_eq!(value.as_int(), Some(42));
        let module = compile_source("let x = 1 + 2 in 42").unwrap();
        assert!(
            !has_opcode(&module, OpCode::IAdd),
            "dead folded store must not emit IAdd"
        );
    }

    #[test]
    fn test_no_dce_side_effect() {
        // `let _ = perform IO.print(...)` must keep the perform even though
        // its result local is never read: Perform is side-effecting.
        let module = compile_source(
            r#"handle let _ = perform IO.print("hi") in 42 { | IO.print(msg) => 0 }"#,
        )
        .unwrap();
        assert!(
            module
                .instructions
                .iter()
                .any(|i| matches!(i.opcode, OpCode::Perform | OpCode::PerformDirect)),
            "the perform must survive dead-store elimination"
        );
        let mut vm = VM::new();
        vm.load_module(module);
        let value = vm.run().unwrap();
        // The abortive handler `=> 0` replaces the body's value: 0 is the
        // result only because the perform executed and jumped into the
        // handler. If DCE had dropped the perform, the body would have
        // yielded 42 instead.
        assert_eq!(value.as_int(), Some(0));
    }

    #[test]
    fn test_fold_string_concat() {
        // `"hello" + " " + "world"` folds to a single string constant.
        let module = compile_source(r#""hello" + " " + "world""#).unwrap();
        assert!(
            module
                .constants
                .iter()
                .any(|c| matches!(c, Constant::String(s) if s == "hello world")),
            "constant pool must contain the folded string, got {:?}",
            module.constants
        );
        assert!(
            !module
                .constants
                .iter()
                .any(|c| matches!(c, Constant::String(s) if s == "hello ")),
            "intermediate concat constant must not survive"
        );
    }

    #[test]
    fn test_jump_thread() {
        // Manually build block0: Jump(1), block1: Jump(2), block2:
        // Const(42); Return. After threading, block0 jumps straight to
        // block2 and the trampoline block1 becomes an unreachable Return.
        let mut b = mir::FunctionBuilder::new("t", Some(crate::types::Type::int()));
        let b1 = b.create_block();
        let b2 = b.create_block();
        b.terminate(mir::Terminator::Jump(b1));
        b.switch_to(b1);
        b.terminate(mir::Terminator::Jump(b2));
        b.switch_to(b2);
        let tmp = b.add_temp(crate::types::Type::int());
        b.assign(tmp, mir::RValue::Const(Constant::Int(42)));
        b.terminate(mir::Terminator::Return(Some(tmp)));
        let mut func = b.build();
        let mut consts = Vec::new();
        optimize_function(&mut func, &mut consts);
        assert_eq!(
            func.blocks[0].terminator,
            mir::Terminator::Jump(b2),
            "entry must jump directly to the real target"
        );
        assert_eq!(
            func.blocks[1].terminator,
            mir::Terminator::Return(None),
            "trampoline block must be marked unreachable"
        );
        assert_eq!(
            func.blocks[2].terminator,
            mir::Terminator::Return(Some(tmp)),
            "real block must be untouched"
        );
    }

    #[test]
    fn test_self_move_eliminated() {
        // `x = Load(x)` is a no-op and must be removed even though x is
        // live (the surrounding load/store remain).
        let mut b = mir::FunctionBuilder::new("t", Some(crate::types::Type::int()));
        let a = b.add_temp(crate::types::Type::int());
        let x = b.add_temp(crate::types::Type::int());
        b.assign(x, mir::RValue::Load(a));
        b.assign(x, mir::RValue::Load(x)); // self-move
        b.terminate(mir::Terminator::Return(Some(x)));
        let mut func = b.build();
        let mut consts = Vec::new();
        optimize_function(&mut func, &mut consts);
        let stmts = &func.blocks[0].stmts;
        assert_eq!(
            stmts.len(),
            1,
            "the self-move must be removed, leaving one store, got {:?}",
            stmts
        );
        assert_eq!(
            stmts[0],
            mir::Stmt::Assign {
                dst: x,
                op: mir::RValue::Load(a)
            }
        );
    }
}
