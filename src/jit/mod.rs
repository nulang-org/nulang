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
    /// Regions compiled through the type-directed (guard-stripped) path in
    /// `typed_compiler`, i.e. where inferred register types were available.
    typed_regions: FxHashSet<(usize, usize)>,
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
            typed_regions: FxHashSet::default(),
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
        cap_caps: Option<&[u8]>,
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

        if has_known_types {
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
                cap_caps,
            ) {
                self.compiled
                    .insert((module_idx, start_offset), (ptr, num_instrs));
                self.typed_regions.insert((module_idx, start_offset));
                return Some(std::mem::transmute(ptr));
            }
            // Typed compilation failed: fall through to the scalar compiler.
        }

        self.compile_region(module_idx, start_offset, num_instrs, instructions)
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
                None,
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
                None,
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
        // Single inlined probe — the per-step interpreter cost when the JIT
        // is enabled. `is_compiled` and `record_and_check_hot` bodies are
        // inlined here (not called through `dyn`) so the common cold case is
        // a couple of bounds checks + a flat-array increment, no hash.
        if !self.compiled.is_empty() && self.compiled.contains_key(&(module_idx, pc)) {
            return true;
        }
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
            let region_len = find_compilable_region(pc, instructions);
            if region_len >= 3 {
                let meta = typed_compiler::infer_reg_types(module, pc);
                let meta_ref = if meta.is_empty() { None } else { Some(&meta) };
                let cap_caps = typed_compiler::infer_reg_caps(module, pc);
                if let Some(func) = unsafe {
                    self.compile_region_typed(module_idx, pc, region_len, instructions, meta_ref, cap_caps)
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
