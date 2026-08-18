//! Cranelift JIT Backend for Nulang.
//!
//! Provides tiered execution: bytecode is first interpreted, and hot regions
//! are lazily compiled to native code via Cranelift.
//!
//! # Architecture
//!
//! - `JitSession`: Owns the Cranelift JIT module, tracks hot counters, and
//!   manages compiled function pointers.
//! - `compiler`: Translates a bytecode region to Cranelift IR (CLIF).
//! - `typed_compiler`: Type-aware JIT that strips NaN-tag guards when types
//!   are known from the typechecker.
//! - `simd_analyzer`: Detects loops that can be vectorized with SIMD.
//! - `simd_compiler`: Emits SIMD CLIF for vectorized array operations.
//! - `runtime.rs`: Runtime helper functions callable from JIT code for
//!   NaN-tag-aware operations.
//!
//! # JIT Function Signature
//!
//! All JIT-compiled functions have the same C ABI signature:
//! ```c
//! void nulang_jit_func(uint64_t* regs, const uint64_t* constants);
//! ```
//! - `regs`: pointer to 256 u64 register file (read/write)
//! - `constants`: pointer to the constants pool (read-only)
//!
//! The function reads operands from `regs`, writes results back, and
//! returns via native `ret`. Control flow (jumps) is compiled to native
//! branches.

mod compiler;
pub mod helpers;
pub mod runtime;
pub mod simd_analyzer;
pub mod simd_compiler;
pub mod typed_compiler;

#[cfg(test)]
mod tests;

pub use compiler::*;

use cranelift::prelude::*;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::Module;
use rustc_hash::{FxHashMap, FxHashSet};

// ---------------------------------------------------------------------------
// Hot Counter
// ---------------------------------------------------------------------------

/// Threshold: how many times a bytecode region must be interpreted
/// before it becomes eligible for JIT compilation.
pub const HOT_THRESHOLD: u64 = 1000;

/// Threshold for tier-2 recompilation: after an already-compiled region
/// has been executed this many additional times, a more aggressive
/// compilation strategy is attempted (typed path if not already typed,
/// or SIMD if the region is amenable).
pub const TIER2_THRESHOLD: u64 = 10_000;

/// Minimum length for a STRAIGHT-LINE region (no internal loop back-edge) to
/// be worth JIT-compiling. Such a region is re-entered by the interpreter
/// every iteration of an enclosing loop, so the JIT enter/exit + probe cost
/// is paid per iteration — compiling a small fragment is slower than
/// interpreting it (a call-heavy loop benchmarked ~4x slower when its
/// fragments were compiled). Genuine loops (internal back-edge) are always
/// compiled regardless of length; only straight-line fragments below this
/// threshold are rejected.
pub const STRAIGHT_LINE_MIN: usize = 8;

// ---------------------------------------------------------------------------
// JIT Session
// ---------------------------------------------------------------------------

/// Manages the Cranelift JIT compilation lifecycle.
///
/// - Creates and configures the `JITModule`
/// - Compiles bytecode regions to native functions
/// - Caches compiled function pointers by `(module_idx, bytecode offset)`
pub struct JitSession {
    /// The Cranelift JIT module that owns compiled code memory.
    module: JITModule,
    /// Map from `(module_idx, bytecode offset)` → (compiled function
    /// pointer, region length in instructions). The length is recorded at
    /// compile time so the VM can advance pc after a JIT run without
    /// re-scanning the instruction stream.
    compiled: FxHashMap<(usize, usize), (*const u8, usize)>,
    /// Per-region execution counters for already-compiled code. When a
    /// region crosses TIER2_THRESHOLD, a more aggressive compilation is
    /// attempted. Reset after each promotion attempt.
    tier2_counters: FxHashMap<(usize, usize), u64>,
    /// Hot counters, flat `Vec<Vec<u32>>` indexed `[module_idx][offset]` so
    /// identical offsets in different modules keep independent counts.
    /// A flat array (not an `FxHashMap`) because `record_and_check_hot` runs
    /// on EVERY interpreted step of a JIT-enabled VM — even cold code that
    /// never tiers up — so the per-step cost must be a bounds-check + array
    /// increment, not a hash insert. Rows grow lazily on first touch. `u32`
    /// is ample: a region crosses HOT_THRESHOLD (1000) and compiles long
    /// before a counter could wrap.
    hot_counts: Vec<Vec<u32>>,
    /// Cache of the last compiled PC we probed, to avoid repeated HashMap lookups
    /// for sequential execution in hot loops.
    last_compiled_probe: Option<(usize, usize)>,
    /// Regions compiled through the type-directed (guard-stripped) path in
    /// `typed_compiler`, i.e. where inferred register types were available.
    typed_regions: FxHashSet<(usize, usize)>,
    /// Per-module "may suspend" vectors (indexed by function-table index),
    /// computed lazily from each module's bytecode: true if the function
    /// transitively performs an effect that can suspend (or calls one).
    /// JIT-compiled native calls are only emitted for functions with
    /// `false` here — running a suspending callee from native code would
    /// double-execute its pre-suspend side effects on fallback.
    may_suspend: FxHashMap<usize, Vec<bool>>,
    /// Per module, per function: is the function part of a direct-call
    /// recursion cycle (so it must NOT go through the re-entrant direct-call
    /// helper, which consumes native stack per recursion level).
    recursive: FxHashMap<usize, Vec<bool>>,
    /// Reusable function builder context.
    builder_context: FunctionBuilderContext,
    /// Reusable codegen context.
    ctx: codegen::Context,
}

impl JitSession {
    /// Create a new JIT session with the native target ISA.
    /// Returns `None` if the host platform is not supported or ISA finalization
    /// fails, printing a warning to stderr.
    pub fn new() -> Option<Self> {
        let mut flag_builder = settings::builder();
        // Enable baseline SIMD support (SSE2 on x86_64, NEON on aarch64)
        let _ = flag_builder.set("enable_simd", "true");
        let _ = flag_builder.set("opt_level", "speed");
        let isa_builder = match cranelift_native::builder() {
            Ok(b) => b,
            Err(msg) => {
                eprintln!("JIT: host machine is not supported: {} — JIT disabled", msg);
                return None;
            }
        };
        let isa = match isa_builder.finish(settings::Flags::new(flag_builder)) {
            Ok(isa) => isa,
            Err(e) => {
                eprintln!(
                    "JIT: failed to finalize Cranelift ISA: {} — JIT disabled",
                    e
                );
                return None;
            }
        };

        let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());

        // Register NaN-tag-aware runtime helpers so compiled code can call them.
        // Single source of truth: src/jit/helpers.rs define_helpers! macro.
        crate::jit::helpers::register_with_builder(&mut builder);

        let module = JITModule::new(builder);
        let ctx = module.make_context();

        Some(JitSession {
            module,
            compiled: FxHashMap::default(),
            hot_counts: Vec::new(),
            last_compiled_probe: None,
            typed_regions: FxHashSet::default(),
            may_suspend: FxHashMap::default(),
            recursive: FxHashMap::default(),
            builder_context: FunctionBuilderContext::new(),
            tier2_counters: FxHashMap::default(),
            ctx,
        })
    }

    /// Record one interpreted execution of the region at
    /// `(module_idx, offset)`. Returns true once the region has been
    /// interpreted at least `HOT_THRESHOLD` times, making it eligible for
    /// JIT compilation.
    pub fn record_and_check_hot(&mut self, module_idx: usize, offset: usize) -> bool {
        if module_idx >= self.hot_counts.len() {
            self.hot_counts.resize(module_idx + 1, Vec::new());
        }
        let row = &mut self.hot_counts[module_idx];
        if offset >= row.len() {
            // Grow geometrically so first-touch allocation across a whole
            // module is linear overall, not O(offset) per distinct pc.
            let new_len = (offset + 1).max(row.len().max(1) * 2);
            row.resize(new_len, 0);
        }
        let count = &mut row[offset];
        *count += 1;
        u64::from(*count) >= HOT_THRESHOLD
    }

    /// Reset all hot counters (used by tests that re-heat a region on an
    /// existing session).
    pub fn reset_hot_counters(&mut self) {
        self.hot_counts.clear();
    }

    /// Lazily computed per-module "may suspend" vector (indexed by
    /// function-table index), computed from the module's bytecode. A function
    /// with `false` here is safe to call from JIT-compiled code (no suspending
    /// effect in its transitive call graph). Returns an empty slice when the
    /// module index is out of range (callers treat empty as "unsafe").
    ///
    /// Foundation for JIT-compiling direct calls (the next slice): currently
    /// exercised by `compute_may_suspend` and its test.
    #[allow(dead_code)]
    fn may_suspend_for(
        &mut self,
        module_idx: usize,
        module: &crate::bytecode::CodeModule,
    ) -> &[bool] {
        if !self.may_suspend.contains_key(&module_idx) {
            let v = compute_may_suspend(module);
            self.may_suspend.insert(module_idx, v);
        }
        &self.may_suspend[&module_idx]
    }

    fn recursive_for(
        &mut self,
        module_idx: usize,
        module: &crate::bytecode::CodeModule,
    ) -> &[bool] {
        if !self.recursive.contains_key(&module_idx) {
            let v = compute_recursive(module);
            self.recursive.insert(module_idx, v);
        }
        &self.recursive[&module_idx]
    }

    /// Record one execution of an already-compiled region and attempt
    /// tier-2 promotion when the threshold is crossed.
    ///
    /// Tier-2 attempts more aggressive compilation: typed path for regions
    /// that were compiled untyped, or SIMD for typed regions.  Promotion is
    /// best-effort — a failed attempt just resets the counter so we retry
    /// later.
    pub fn record_tier2_and_maybe_promote(
        &mut self,
        module_idx: usize,
        pc: usize,
        instructions: &[crate::bytecode::Instruction],
    ) {
        let count = self.tier2_counters.entry((module_idx, pc)).or_insert(0);
        *count += 1;
        if *count < TIER2_THRESHOLD {
            return;
        }

        let region_len = match self.compiled.get(&(module_idx, pc)) {
            Some(&(_, len)) if len >= 3 => len,
            _ => return,
        };

        let was_typed = self.typed_regions.contains(&(module_idx, pc));

        if !was_typed {
            // Try typed compilation with the benefit of profile data.
            // We don't have a CodeModule here, so infer_reg_types needs
            // one — skip for now, promotion will retry later.
            // Reset counter to allow future retries.
            self.tier2_counters.insert((module_idx, pc), 0);
        } else {
            // Try SIMD compilation for hot typed regions.
            if let Some(_func) =
                unsafe { self.compile_region_simd(module_idx, pc, region_len, instructions, None) }
            {
                // SIMD compilation succeeded; the compiled cache was
                // updated inside compile_region_simd.
            }
            self.tier2_counters.insert((module_idx, pc), 0);
        }
    }

    /// Reset tier-2 counters (used by tests).
    pub fn reset_tier2_counters(&mut self) {
        self.tier2_counters.clear();
    }

    /// Compile a bytecode region starting at `start_offset` with `num_instrs`
    /// instructions. Returns the compiled function pointer, or None if the
    /// region contains unsupported opcodes.
    ///
    /// # Safety
    /// The returned function pointer is valid for the lifetime of this
    /// `JitSession`. The bytecode must not be modified while JIT code is
    /// executing.
    pub unsafe fn compile_region(
        &mut self,
        module_idx: usize,
        start_offset: usize,
        num_instrs: usize,
        instructions: &[crate::bytecode::Instruction],
        native_calls: &std::collections::HashMap<usize, usize>,
    ) -> Option<JitFunctionPtr> {
        // Check if already compiled
        if let Some(&(ptr, _)) = self.compiled.get(&(module_idx, start_offset)) {
            return Some(std::mem::transmute(ptr));
        }

        // Build the function
        let func_name = format!("nulang_jit_{}_{}", module_idx, start_offset);

        match compiler::compile_bytecode_region(
            &mut self.module,
            &mut self.builder_context,
            &mut self.ctx,
            &func_name,
            start_offset,
            num_instrs,
            instructions,
            native_calls,
        ) {
            Ok(ptr) => {
                self.compiled
                    .insert((module_idx, start_offset), (ptr, num_instrs));
                Some(std::mem::transmute(ptr))
            }
            Err(_) => None,
        }
    }

    /// Compile a bytecode region with optional type-directed guard stripping.
    ///
    /// When `type_metadata` proves at least one register's type, the region
    /// goes through `typed_compiler::compile_bytecode_region_typed`, which
    /// emits direct CLIF for statically typed operations instead of
    /// NaN-tag-aware runtime helper calls. Absent/empty metadata — or any
    /// typed-compilation failure — falls back to the scalar
    /// [`JitSession::compile_region`], so this never compiles *less* code
    /// than the untyped path.
    ///
    /// # Safety
    /// Same safety requirements as `compile_region`.
    pub unsafe fn compile_region_typed(
        &mut self,
        module_idx: usize,
        start_offset: usize,
        num_instrs: usize,
        instructions: &[crate::bytecode::Instruction],
        type_metadata: Option<&crate::jit::typed_compiler::TypeMetadata>,
        native_calls: &std::collections::HashMap<usize, usize>,
    ) -> Option<JitFunctionPtr> {
        // Check if already compiled
        if let Some(&(ptr, _)) = self.compiled.get(&(module_idx, start_offset)) {
            return Some(std::mem::transmute(ptr));
        }

        let has_known_types = type_metadata
            .map(|m| {
                m.regs
                    .iter()
                    .any(|&t| t != crate::jit::typed_compiler::KnownType::Unknown)
            })
            .unwrap_or(false);

        if has_known_types && native_calls.is_empty() {
            // The typed compiler does not understand `Call`; a region
            // containing a native direct call (non-empty map) must go through
            // the scalar compiler, which handles `nulang_jit_direct_call`.
            let func_name = format!("nulang_tjit_{}_{}", module_idx, start_offset);
            if let Ok(ptr) = typed_compiler::compile_bytecode_region_typed(
                &mut self.module,
                &mut self.builder_context,
                &mut self.ctx,
                &func_name,
                start_offset,
                num_instrs,
                instructions,
                type_metadata,
            ) {
                self.compiled
                    .insert((module_idx, start_offset), (ptr, num_instrs));
                self.typed_regions.insert((module_idx, start_offset));
                return Some(std::mem::transmute(ptr));
            }
            // Typed compilation failed: fall through to the scalar compiler.
        }

        self.compile_region(module_idx, start_offset, num_instrs, instructions, native_calls)
    }

    /// Return the number of regions compiled through the type-directed path.
    pub fn typed_compiled_count(&self) -> usize {
        self.typed_regions.len()
    }

    /// Check whether a `(module_idx, offset)` region was compiled with
    /// type-directed guard stripping.
    pub fn is_typed_compiled(&self, module_idx: usize, offset: usize) -> bool {
        self.typed_regions.contains(&(module_idx, offset))
    }

    /// Check if a `(module_idx, offset)` region has already been compiled.
    pub fn is_compiled(&self, module_idx: usize, offset: usize) -> bool {
        // Fast path: before any region is compiled — the common case for a
        // cold program, which is exactly when the probe runs on every step —
        // skip the hash entirely.
        if self.compiled.is_empty() {
            return false;
        }
        self.compiled.contains_key(&(module_idx, offset))
    }

    /// Get the compiled function pointer for `(module_idx, offset)` (if compiled).
    ///
    /// # Safety
    /// The returned function pointer is valid only while this `JitSession` is
    /// alive and the original bytecode has not been modified.
    pub unsafe fn get_compiled(&self, module_idx: usize, offset: usize) -> Option<JitFunctionPtr> {
        self.compiled
            .get(&(module_idx, offset))
            .map(|&(ptr, _)| std::mem::transmute(ptr))
    }

    /// Number of bytecode instructions covered by the compiled region at
    /// `(module_idx, offset)`, recorded at compile time. The VM uses this
    /// to advance pc after a JIT run instead of re-scanning the
    /// instruction stream.
    pub fn compiled_region_len(&self, module_idx: usize, offset: usize) -> Option<usize> {
        self.compiled
            .get(&(module_idx, offset))
            .map(|&(_, len)| len)
    }

    /// Return the number of compiled regions.
    pub fn compiled_count(&self) -> usize {
        self.compiled.len()
    }

    /// Compile a SIMD-vectorizable bytecode region.
    /// First analyzes the region for vectorizable array loop patterns. If found,
    /// emits SIMD CLIF (I64x2/F64x2/I32x4/F32x4), falling back to the
    /// type-directed scalar compiler if SIMD emission fails. Returns `None`
    /// when the region has no vectorizable pattern at all.
    ///
    /// Wired into tier-2 promotion: when a typed region exceeds
    /// `TIER2_THRESHOLD` executions, SIMD compilation is attempted.
    /// Falls back to typed/scalar on any failure.  Element-wise array
    /// ops store results to memory (no register write-back needed);
    /// trip count must be a runtime `ArrLen` register (baked hints
    /// are unsafe and rejected by the analyzer).
    ///
    /// # Safety
    /// Same safety requirements as `compile_region`.
    pub unsafe fn compile_region_simd(
        &mut self,
        module_idx: usize,
        start_offset: usize,
        num_instrs: usize,
        instructions: &[crate::bytecode::Instruction],
        type_metadata: Option<&crate::jit::typed_compiler::TypeMetadata>,
    ) -> Option<JitFunctionPtr> {
        use crate::jit::simd_analyzer::analyze_region;
        use crate::jit::simd_compiler::{compile_simd_region, is_simd_supported};

        // Check if already compiled
        if let Some(&(ptr, _)) = self.compiled.get(&(module_idx, start_offset)) {
            return Some(std::mem::transmute(ptr));
        }

        // Only attempt SIMD if host CPU supports it
        if !is_simd_supported() {
            return self.compile_region_typed(
                module_idx,
                start_offset,
                num_instrs,
                instructions,
                type_metadata,
                &std::collections::HashMap::new(),
            );
        }

        // Analyze for vectorizable patterns
        let simd_region = analyze_region(instructions, start_offset, num_instrs, type_metadata)?;

        let func_name = format!("nulang_simd_{}_{}", module_idx, start_offset);

        match compile_simd_region(
            &mut self.module,
            &mut self.builder_context,
            &mut self.ctx,
            &func_name,
            instructions,
            &simd_region,
        ) {
            Ok(ptr) => {
                self.compiled
                    .insert((module_idx, start_offset), (ptr, num_instrs));
                Some(std::mem::transmute(ptr))
            }
            Err(_) => self.compile_region_typed(
                module_idx,
                start_offset,
                num_instrs,
                instructions,
                type_metadata,
                &std::collections::HashMap::new(),
            ),
        }
    }
}

impl Default for JitSession {
    fn default() -> Self {
        Self::new().expect("JIT must be available for Default::default()")
    }
}

// ---------------------------------------------------------------------------
// JIT Function Type
// ---------------------------------------------------------------------------

/// Type of a JIT-compiled Nulang function.
///
/// Signature: `fn(regs: *mut u64, constants: *const u64)`
///
/// The function reads from `regs` (256 elements), performs operations,
/// writes results back to `regs`, and returns. Control flow is entirely
/// within the native code.
pub type JitFunctionPtr = extern "C" fn(*mut u64, *const u64);

// ---------------------------------------------------------------------------
// Tiered Execution
// ---------------------------------------------------------------------------

/// Find a contiguous region of compilable instructions starting at `offset`.
/// Returns the number of instructions in the region.
///
/// Regions normally stop BEFORE the first branch (straight-line only): after
/// a region runs the VM unconditionally advances pc by the region length, so
/// a compiled branch to an outside target would resume at the wrong place.
/// HOWEVER, when the scan encounters a BACKWARD edge (a branch whose target
/// precedes its own pc — the hallmark of a loop), the region is extended to
/// include the branches so the hot loop compiles natively: the loop head,
/// body and back-jump run in one VM entry, and any branch to a target outside
/// the region yields the interpreter there via the branch-exit slot. This
/// makes tight loops far faster (the straight-line body-slice approach cost a
/// 256-register snapshot/restore per iteration). Forward-only branchy code
/// (e.g. an `if`/recursion that leads to a `Call`) keeps the straight-line
/// boundary, which is why recursion does not regress.
/// A direct call's callee value is staged into `FUNC_VALUE_REG` (254) by
/// `mir_codegen` before the `Call` (a `load_constant` then the arg staging).
/// Recover the statically-known callee of a `Call` at `pc` by scanning
/// backward to the nearest prior write to reg 254:
/// - a `ConstU`/`Const0/1/2` into 254 → direct call to that function index;
/// - a `Move` (or anything else) into 254 → an indirect call (closure /
///   runtime value) — the target is not statically known.
/// `func_start` bounds the scan to the calling function's code. This is a
/// hint only: compiled direct calls re-check the live value in reg 254 at
/// run time and fall back to the interpreter on mismatch, so a stale
/// recovery here never calls the wrong function.
///
/// Foundation for JIT-compiling direct calls (the next slice): currently
/// exercised by the `may_suspend` analysis and its test.
#[allow(dead_code)]
pub(crate) fn direct_call_target(
    module: &crate::bytecode::CodeModule,
    pc: usize,
    func_start: usize,
) -> Option<usize> {
    use crate::bytecode::{Constant, OpCode};
    const FUNC_VALUE_REG: u8 = 254;
    let mut p = pc;
    while p > func_start {
        p -= 1;
        let instr = module.instructions[p];
        match instr.opcode {
            OpCode::Const0 | OpCode::Const1 | OpCode::Const2 if instr.op1 == FUNC_VALUE_REG => {
                let idx = match instr.opcode {
                    OpCode::Const0 => 0,
                    OpCode::Const1 => 1,
                    _ => 2,
                };
                return Some(idx);
            }
            OpCode::ConstM1 if instr.op1 == FUNC_VALUE_REG => return None, // -1 is not a function
            OpCode::ConstU if instr.op3 == FUNC_VALUE_REG => {
                let pool = instr.imm16() as usize;
                return match module.constants.get(pool) {
                    Some(Constant::Int(i)) if *i >= 0 => Some(*i as usize),
                    _ => None,
                };
            }
            OpCode::Move if instr.op2 == FUNC_VALUE_REG => return None, // indirect
            _ => {}
        }
    }
    None
}

/// Opcodes that can never suspend (a pure function's safe set). A function
/// whose body contains only these, plus direct calls to other safe functions,
/// is non-suspending and safe to call from JIT-compiled code. Everything
/// else — effects (`Perform`/`PerformDirect`/`Handle`/`Resume`/`Unwind`),
/// actor ops, async effects, `SignalWait`/`Receive*`, foreign calls,
/// `SConcat`/record/closure ops — is conservatively treated as suspending.
#[allow(dead_code)]
fn is_non_suspending_op(op: crate::bytecode::OpCode) -> bool {
    use crate::bytecode::OpCode;
    matches!(
        op,
        OpCode::Nop
            | OpCode::Halt
            | OpCode::Const0
            | OpCode::Const1
            | OpCode::Const2
            | OpCode::ConstM1
            | OpCode::ConstU
            | OpCode::Load
            | OpCode::Store
            | OpCode::Move
            | OpCode::Swap
            | OpCode::Dup
            | OpCode::IAdd
            | OpCode::ISub
            | OpCode::IMul
            | OpCode::IDiv
            | OpCode::IMod
            | OpCode::INeg
            | OpCode::IInc
            | OpCode::IDec
            | OpCode::IPow
            | OpCode::FPow
            | OpCode::Xor
            | OpCode::Shl
            | OpCode::Shr
            | OpCode::BitAnd
            | OpCode::BitOr
            | OpCode::FAdd
            | OpCode::FSub
            | OpCode::FMul
            | OpCode::FDiv
            | OpCode::FNeg
            | OpCode::ICmpEq
            | OpCode::ICmpLt
            | OpCode::ICmpGt
            | OpCode::ICmpLe
            | OpCode::ICmpGe
            | OpCode::FCmpEq
            | OpCode::FCmpLt
            | OpCode::FCmpGt
            | OpCode::Not
            | OpCode::And
            | OpCode::Or
            | OpCode::Jmp
            | OpCode::JmpT
            | OpCode::JmpF
            | OpCode::IToF
            | OpCode::FToI
            | OpCode::DbgPrint
            | OpCode::Ret
            | OpCode::RetVal
            | OpCode::ArrLoad
            | OpCode::ArrStore
            | OpCode::ArrLen
            | OpCode::FieldL
    )
}

/// Compute the transitive "may suspend" vector for a module (indexed by
/// function-table index). A function may suspend if its body contains a
/// suspending opcode (or any opcode outside the pure whitelist), or an
/// indirect call (unknown target), or a direct call to a may-suspend
/// function. Fixed point over the direct-call graph recovered by
/// `direct_call_target`.
#[allow(dead_code)]
fn compute_may_suspend(module: &crate::bytecode::CodeModule) -> Vec<bool> {
    use crate::bytecode::OpCode;
    let n = module.function_table.len();
    let mut result = vec![false; n];
    // Directly unsafe: contains a non-whitelisted opcode (effect/actor/
    // foreign/suspending) or an indirect call (Call/ClosureCall whose target
    // is not a statically-recovered direct callee).
    for i in 0..n {
        let start = module.function_table[i];
        let end = if i + 1 < n {
            module.function_table[i + 1]
        } else {
            module.instructions.len()
        };
        for pc in start..end {
            let op = module.instructions[pc].opcode;
            if matches!(op, OpCode::Call | OpCode::ClosureCall) {
                if direct_call_target(module, pc, start).is_none() {
                    result[i] = true; // indirect call: unknown target
                }
                // direct call: leave for the fixed-point propagation
            } else if !is_non_suspending_op(op) {
                result[i] = true;
                break;
            }
        }
    }
    // Propagate through the direct-call graph until stable.
    loop {
        let mut changed = false;
        for i in 0..n {
            if result[i] {
                continue;
            }
            let start = module.function_table[i];
            let end = if i + 1 < n {
                module.function_table[i + 1]
            } else {
                module.instructions.len()
            };
            for pc in start..end {
                if matches!(
                    module.instructions[pc].opcode,
                    OpCode::Call | OpCode::ClosureCall
                ) {
                    if let Some(callee) = direct_call_target(module, pc, start) {
                        if callee < n && result[callee] {
                            result[i] = true;
                            changed = true;
                            break;
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    result
}

/// Per function: can it transitively reach itself via direct calls (i.e. is
/// it part of a direct-call recursion cycle)? A recursive function must NOT
/// be run through the re-entrant direct-call helper: each helper invocation
/// consumes native stack (compiled region -> helper -> interpreter step ->
/// nested region -> ...), so unbounded recursion would overflow the stack.
/// The interpreter handles recursion on heap-allocated frames; a recursive
/// callee stays there. Computed via transitive closure over the direct-call
/// graph (n is small — one per function).
fn compute_recursive(module: &crate::bytecode::CodeModule) -> Vec<bool> {
    use crate::bytecode::OpCode;
    let n = module.function_table.len();
    let mut reach = vec![vec![false; n]; n];
    for i in 0..n {
        let start = module.function_table[i];
        let end = if i + 1 < n {
            module.function_table[i + 1]
        } else {
            module.instructions.len()
        };
        for pc in start..end {
            if matches!(
                module.instructions[pc].opcode,
                OpCode::Call | OpCode::ClosureCall
            ) {
                if let Some(callee) = direct_call_target(module, pc, start) {
                    if callee < n {
                        reach[i][callee] = true;
                    }
                }
            }
        }
    }
    // Floyd-Warshall transitive closure.
    for k in 0..n {
        for i in 0..n {
            if reach[i][k] {
                for j in 0..n {
                    if reach[k][j] {
                        reach[i][j] = true;
                    }
                }
            }
        }
    }
    (0..n).map(|i| reach[i][i]).collect()
}

/// Region-length scanner WITHOUT direct-call folding; used by the unit tests.
/// The runtime path uses [`find_compilable_region_with_calls`] so direct
/// non-suspending calls fold into regions.
#[allow(dead_code)]
pub(crate) fn find_compilable_region(
    offset: usize,
    instructions: &[crate::bytecode::Instruction],
) -> usize {
    let mut len = 0;
    let mut first_branch: Option<usize> = None;
    let mut has_back_edge = false;
    for i in offset..instructions.len().min(offset + 500) {
        if !compiler::is_opcode_compilable(instructions[i].opcode) {
            break;
        }
        let op = instructions[i].opcode;
        // Stop *before* return/halt instructions so the VM still executes the
        // return (frame pop) / halt itself after the JIT region.
        if matches!(
            op,
            crate::bytecode::OpCode::Ret
                | crate::bytecode::OpCode::RetVal
                | crate::bytecode::OpCode::Halt
        ) {
            break;
        }
        let is_branch = matches!(
            op,
            crate::bytecode::OpCode::Jmp
                | crate::bytecode::OpCode::JmpT
                | crate::bytecode::OpCode::JmpF
        );
        if is_branch {
            if first_branch.is_none() {
                first_branch = Some(len);
            }
            let target = match op {
                crate::bytecode::OpCode::Jmp => {
                    (i as i64 + instructions[i].simm16() as i64) as usize
                }
                _ => (i as i64 + instructions[i].offset16() as i64) as usize,
            };
            // A genuine loop back-edge lands WITHIN the region (target >= offset):
            // the loop head is the region start or an earlier in-region pc, and
            // re-entering it continues the loop. A backward jump to BEFORE the
            // region start (target < offset) is an EXIT (e.g. a return path's
            // jump back to a RetVal), not a loop — don't treat it as a back-edge.
            if target >= offset && target < i {
                has_back_edge = true;
            }
        }
        len += 1;
    }
    if has_back_edge {
        // A genuine loop loops INTERNALLY in the compiled code, so the JIT
        // enter/exit + probe cost is amortized across all its iterations —
        // always worth compiling.
        len
    } else {
        let straight = first_branch.unwrap_or(len);
        // A small straight-line region — whether a function body or a loop
        // fragment — is re-entered by the interpreter once per call (a body)
        // or per enclosing-loop iteration (a fragment), so the JIT
        // enter/exit + probe cost is paid EVERY time. That exceeds the cost of
        // just interpreting `straight` instructions below ~STRAIGHT_LINE_MIN,
        // so compiling a small non-looping region is a regression (a
        // call-heavy loop benchmarked ~4.6x SLOWER when its 2-instruction
        // callee was compiled). Genuine loops (internal back-edge) loop
        // natively and amortize the cost, so they are always compiled; only
        // small non-looping regions are rejected.
        if straight < STRAIGHT_LINE_MIN {
            0
        } else {
            straight
        }
    }
}

/// The code offset of the function containing `pc` (largest
/// `function_table[i] <= pc`), bounding `direct_call_target`'s backward walk.
pub(crate) fn func_start_for(module: &crate::bytecode::CodeModule, pc: usize) -> usize {
    module
        .function_table
        .iter()
        .copied()
        .filter(|&o| o <= pc)
        .next_back()
        .unwrap_or(0)
}

/// If the instruction at `pc` is a `Call` of a provably-non-suspending direct
/// callee (recoverable via `direct_call_target` and gated on `may_suspend`
/// and on not being in a direct-call recursion cycle), return the callee's
/// function-table index. Such a call is safe to compile into the region as a
/// `nulang_jit_direct_call` helper invocation. Returns None for indirect
/// calls, suspending callees, recursive callees, and every other opcode.
pub(crate) fn native_direct_call(
    module: &crate::bytecode::CodeModule,
    pc: usize,
    may_suspend: Option<&[bool]>,
    recursive: Option<&[bool]>,
) -> Option<usize> {
    use crate::bytecode::OpCode;
    let instr = module.instructions.get(pc)?;
    if instr.opcode != OpCode::Call {
        return None;
    }
    let idx = direct_call_target(module, pc, func_start_for(module, pc))?;
    // A suspending callee must not be run re-entrantly from a compiled
    // region: it could suspend mid-run and be re-entered from its call start,
    // double-executing pre-suspend side effects. Stay on the interpreter.
    if may_suspend.is_some_and(|v| v.get(idx) == Some(&true)) {
        return None;
    }
    // A recursive callee must not go through the re-entrant helper either:
    // each helper call consumes native stack, so unbounded recursion would
    // overflow it. The interpreter uses heap-allocated frames instead.
    if recursive.is_some_and(|v| v.get(idx) == Some(&true)) {
        return None;
    }
    Some(idx)
}

/// Like [`find_compilable_region`], but additionally continues past `Call`
/// instructions whose direct callee is provably non-suspending, returning the
/// region length and the map of (absolute pc -> direct callee func index) for
/// the calls that were folded into the region. The caller passes this map to
/// the scalar compiler so it can emit `nulang_jit_direct_call` at those pcs.
pub(crate) fn find_compilable_region_with_calls(
    offset: usize,
    instructions: &[crate::bytecode::Instruction],
    module: &crate::bytecode::CodeModule,
    may_suspend: Option<&[bool]>,
    recursive: Option<&[bool]>,
) -> (usize, std::collections::HashMap<usize, usize>) {
    let mut native_calls = std::collections::HashMap::new();
    let mut len = 0;
    let mut first_branch: Option<usize> = None;
    let mut has_back_edge = false;
    for i in offset..instructions.len().min(offset + 500) {
        let op = instructions[i].opcode;
        if op == crate::bytecode::OpCode::Call {
            match native_direct_call(module, i, may_suspend, recursive) {
                Some(idx) => {
                    native_calls.insert(i, idx);
                }
                None => break, // indirect / suspending / recursive call — stop
            }
        } else if !compiler::is_opcode_compilable(op) {
            break;
        }
        // Stop *before* return/halt so the VM still executes the return (frame
        // pop) / halt itself after the JIT region.
        if matches!(
            op,
            crate::bytecode::OpCode::Ret
                | crate::bytecode::OpCode::RetVal
                | crate::bytecode::OpCode::Halt
        ) {
            break;
        }
        let is_branch = matches!(
            op,
            crate::bytecode::OpCode::Jmp
                | crate::bytecode::OpCode::JmpT
                | crate::bytecode::OpCode::JmpF
        );
        if is_branch {
            if first_branch.is_none() {
                first_branch = Some(len);
            }
            let target = match op {
                crate::bytecode::OpCode::Jmp => {
                    (i as i64 + instructions[i].simm16() as i64) as usize
                }
                _ => (i as i64 + instructions[i].offset16() as i64) as usize,
            };
            if target >= offset && target < i {
                has_back_edge = true;
            }
        }
        len += 1;
    }
    if !has_back_edge && first_branch.unwrap_or(len) < STRAIGHT_LINE_MIN {
        (0, std::collections::HashMap::new())
    } else {
        (len, native_calls)
    }
}

// TieredAction is defined in `crate::backends` so the VM can reference it
// without importing the JIT module. Re-export for backward compatibility.
pub use crate::backends::TieredAction;

// ---------------------------------------------------------------------------
// JitBackend trait impl — adapts the Cranelift JIT to the backend trait
// ---------------------------------------------------------------------------

impl crate::backends::JitBackend for JitSession {
    fn is_compiled(&self, module_idx: usize, pc: usize) -> bool {
        // Fast path: skip the hash while nothing is compiled (the common
        // per-step probe on a cold program).
        if self.compiled.is_empty() {
            return false;
        }
        self.compiled.contains_key(&(module_idx, pc))
    }

    fn record_and_check_hot(&mut self, module_idx: usize, pc: usize) -> bool {
        if module_idx >= self.hot_counts.len() {
            self.hot_counts.resize(module_idx + 1, Vec::new());
        }
        let row = &mut self.hot_counts[module_idx];
        if pc >= row.len() {
            let new_len = (pc + 1).max(row.len().max(1) * 2);
            row.resize(new_len, 0);
        }
        let count = &mut row[pc];
        *count += 1;
        u64::from(*count) >= HOT_THRESHOLD
    }

    fn probe_and_maybe_hot(&mut self, module_idx: usize, pc: usize) -> bool {
        // Fast path: check if this is the last compiled PC we saw
        // This avoids HashMap lookups for sequential execution in hot loops
        if self.last_compiled_probe == Some((module_idx, pc)) {
            return true;
        }

        // Check compiled map
        if !self.compiled.is_empty() && self.compiled.contains_key(&(module_idx, pc)) {
            self.last_compiled_probe = Some((module_idx, pc));
            return true;
        }

        // Increment counter (cheap operation)
        if module_idx >= self.hot_counts.len() {
            self.hot_counts.resize(module_idx + 1, Vec::new());
        }
        let row = &mut self.hot_counts[module_idx];
        if pc >= row.len() {
            let new_len = (pc + 1).max(row.len().max(1) * 2);
            row.resize(new_len, 0);
        }
        let count = &mut row[pc];
        *count += 1;

        // Return true if just became hot (will trigger compilation)
        u64::from(*count) >= HOT_THRESHOLD
    }

    fn compiled_region_len(&self, module_idx: usize, pc: usize) -> Option<usize> {
        self.compiled.get(&(module_idx, pc)).map(|&(_, len)| len)
    }

    fn compiled_count(&self) -> usize {
        self.compiled.len()
    }

    fn typed_compiled_count(&self) -> usize {
        self.typed_regions.len()
    }

    fn reset_hot_counters(&mut self) {
        self.hot_counts.clear();
    }

    fn tiered_execute_step_typed(
        &mut self,
        module_idx: usize,
        pc: usize,
        module: &crate::bytecode::CodeModule,
        regs: &mut [u64; 256],
        constants: &[u64],
    ) -> crate::backends::TieredAction {
        let instructions = &module.instructions;

        // Check if already compiled
        if let Some(func) = unsafe { self.get_compiled(module_idx, pc) } {
            func(regs.as_mut_ptr(), constants.as_ptr());
            // Track post-compilation hotness for tier-2 promotion.
            self.record_tier2_and_maybe_promote(module_idx, pc, instructions);
            return crate::backends::TieredAction::RanJit;
        }

        // Record execution for hotness
        if self.record_and_check_hot(module_idx, pc) {
            let ms = self.may_suspend_for(module_idx, module).to_vec();
            let rc = self.recursive_for(module_idx, module).to_vec();
            let (region_len, native_calls) = find_compilable_region_with_calls(
                pc,
                instructions,
                module,
                Some(&ms),
                Some(&rc),
            );
            if region_len >= 3 {
                let meta = typed_compiler::infer_reg_types(module, pc);
                let meta_ref = if meta.is_empty() { None } else { Some(&meta) };
                if let Some(func) = unsafe {
                    self.compile_region_typed(
                        module_idx,
                        pc,
                        region_len,
                        instructions,
                        meta_ref,
                        &native_calls,
                    )
                } {
                    func(regs.as_mut_ptr(), constants.as_ptr());
                    return crate::backends::TieredAction::RanJit;
                }
            }
            // Rejected (too small / fragmented) or compile failed. Reset the
            // hot counter so the per-step `record_and_check_hot` doesn't keep
            // returning true and re-scanning every step — a rejected pc would
            // otherwise call `find_compilable_region` on every execution,
            // regressing call-heavy loops ~5x.
            if module_idx < self.hot_counts.len() && pc < self.hot_counts[module_idx].len() {
                self.hot_counts[module_idx][pc] = 0;
            }
        }

        crate::backends::TieredAction::Interpret
    }
}
