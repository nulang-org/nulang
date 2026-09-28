//! Cranelift JIT Backend for Nulang.
//!
//! Provides tiered execution: bytecode is first interpreted, and hot regions
//! are lazily compiled to native code via Cranelift.
//!
//! # Architecture
//!
//! - `JitSession`: Owns tiering/cache state and installed native pointers.
//! - `native_codegen`: Machine-code backend boundary; Cranelift owns its modules/contexts there.
//! - `region_planner`: Backend-neutral region, call-safety, recursion, and type analysis.
//! - `compiler`: Translates a planned bytecode region to Cranelift IR (CLIF).
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
mod native_codegen;
mod region_planner;
pub mod runtime;
pub mod simd_analyzer;
pub mod simd_compiler;
pub mod typed_compiler;

#[cfg(test)]
mod tests;

pub use compiler::*;

use native_codegen::{
    CraneliftCodegen, NativeCodegenBackend, NativeCompileKind, NativeCompileRequest,
};
use region_planner::RegionPlanner;
#[cfg(test)]
use region_planner::{
    compute_may_suspend, compute_recursive, direct_call_target, find_compilable_region,
};

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

/// Native compilation tier currently installed for a bytecode region.
///
/// Tier metadata is stored beside the machine-code pointer so promotion can
/// replace an existing entry instead of accidentally returning the cached
/// lower-tier function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompilationTier {
    Baseline,
    Typed,
    Simd,
}

/// Cranelift optimization policy used for the installed machine code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodegenOptimization {
    /// Minimize tier-up latency for the first native version.
    Fast,
    /// Spend more compile time on code expected to remain hot.
    Optimized,
}

#[derive(Debug, Clone, Copy)]
struct CompiledRegion {
    ptr: *const u8,
    len: usize,
    tier: CompilationTier,
    optimization: CodegenOptimization,
    /// True when this region can invoke a helper that re-enters the VM frame
    /// stack. Such regions must execute against detached register storage:
    /// pushing an interpreter frame can reallocate `VM::frames`, invalidating
    /// a raw pointer into the caller's in-place register array.
    requires_vm_reentry: bool,
    /// Wall-clock time spent in the compiler for the currently installed
    /// version of this region. This is intentionally per-region observability,
    /// not a benchmark substitute.
    compile_time_ns: u64,
}

// ---------------------------------------------------------------------------
// JIT Session
// ---------------------------------------------------------------------------

/// Manages the Cranelift JIT compilation lifecycle.
///
/// - Creates and configures the `JITModule`
/// - Compiles bytecode regions to native functions
/// - Caches compiled function pointers by `(module_idx, bytecode offset)`
pub struct JitSession {
    /// Native machine-code emitter. Tiering and cache ownership stay in this
    /// session; compiler-specific modules/contexts stay behind this boundary.
    codegen: CraneliftCodegen,
    /// Dense per-module table indexed `[module_idx][bytecode offset]`.
    ///
    /// The JIT probe runs on every JIT-enabled interpreter step. Once the
    /// first region compiles, a hash map here would impose a hash lookup on
    /// every remaining cold instruction. Bytecode PCs are already dense
    /// integers, so direct indexing is both simpler and cheaper.
    ///
    /// Each occupied slot stores the active machine-code version and its tier.
    compiled: Vec<Vec<Option<CompiledRegion>>>,
    /// Number of occupied compiled-region slots across all modules.
    compiled_count: usize,
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
    /// Backend-neutral region/safety/type analysis. Cranelift consumes the
    /// resulting plans but does not own the language-level planning rules.
    region_planner: RegionPlanner,
    /// Monotonic suffix for replacement compilations. Cranelift keeps prior
    /// function declarations alive, so every promotion needs a fresh symbol.
    promotion_serial: u64,
}

impl JitSession {
    /// Create a new JIT session with the native target ISA.
    /// Returns `None` if the host platform is not supported or ISA finalization
    /// fails, printing a warning to stderr.
    pub fn new() -> Option<Self> {
        let codegen = CraneliftCodegen::new()?;

        Some(JitSession {
            codegen,
            compiled: Vec::new(),
            compiled_count: 0,
            hot_counts: Vec::new(),
            typed_regions: FxHashSet::default(),
            region_planner: RegionPlanner::default(),
            tier2_counters: FxHashMap::default(),
            promotion_serial: 0,
        })
    }

    #[inline(always)]
    fn compiled_entry(&self, module_idx: usize, offset: usize) -> Option<CompiledRegion> {
        self.compiled
            .get(module_idx)
            .and_then(|row| row.get(offset))
            .copied()
            .flatten()
    }

    #[cfg(test)]
    fn store_compiled(
        &mut self,
        module_idx: usize,
        offset: usize,
        ptr: *const u8,
        region_len: usize,
    ) {
        self.store_compiled_with_metadata(
            module_idx,
            offset,
            ptr,
            region_len,
            CompilationTier::Baseline,
            CodegenOptimization::Fast,
            false,
            0,
        );
    }

    fn store_compiled_with_metadata(
        &mut self,
        module_idx: usize,
        offset: usize,
        ptr: *const u8,
        region_len: usize,
        tier: CompilationTier,
        optimization: CodegenOptimization,
        requires_vm_reentry: bool,
        compile_time_ns: u64,
    ) {
        if module_idx >= self.compiled.len() {
            self.compiled.resize(module_idx + 1, Vec::new());
        }
        let row = &mut self.compiled[module_idx];
        if offset >= row.len() {
            let new_len = (offset + 1).max(row.len().max(1) * 2);
            row.resize(new_len, None);
        }
        if row[offset].is_none() {
            self.compiled_count += 1;
        }
        row[offset] = Some(CompiledRegion {
            ptr,
            len: region_len,
            tier,
            optimization,
            requires_vm_reentry,
            compile_time_ns,
        });
    }

    fn next_promotion_name(&mut self, prefix: &str, module_idx: usize, offset: usize) -> String {
        let serial = self.promotion_serial;
        self.promotion_serial = self.promotion_serial.wrapping_add(1);
        format!("{prefix}_{module_idx}_{offset}_{serial}")
    }

    fn elapsed_ns(started: std::time::Instant) -> u64 {
        started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
    }

    /// Tier currently installed for a compiled region.
    pub fn compiled_tier(&self, module_idx: usize, offset: usize) -> Option<CompilationTier> {
        self.compiled_entry(module_idx, offset)
            .map(|region| region.tier)
    }

    /// Optimization policy used for the currently installed version.
    pub fn compiled_optimization(
        &self,
        module_idx: usize,
        offset: usize,
    ) -> Option<CodegenOptimization> {
        self.compiled_entry(module_idx, offset)
            .map(|region| region.optimization)
    }

    /// Compiler wall time for the currently installed version of a region.
    pub fn compiled_region_compile_time_ns(&self, module_idx: usize, offset: usize) -> Option<u64> {
        self.compiled_entry(module_idx, offset)
            .map(|region| region.compile_time_ns)
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
    /// higher-tier replacement when the threshold is crossed.
    ///
    /// First-tier regions are compiled by a low-latency Cranelift module
    /// (`opt_level=none`, `regalloc_algorithm=single_pass`). Once hot enough
    /// they are replaced by code from the speed/backtracking module.
    /// Type-directed specialization is preserved when
    /// available; an already-optimized typed region is subsequently eligible
    /// for SIMD replacement. Promotion is best-effort: failures leave the
    /// current machine code installed.
    pub fn record_tier2_and_maybe_promote(
        &mut self,
        module_idx: usize,
        pc: usize,
        module: &crate::bytecode::CodeModule,
    ) {
        let should_promote = {
            let count = self.tier2_counters.entry((module_idx, pc)).or_insert(0);
            *count += 1;
            *count >= TIER2_THRESHOLD
        };
        if !should_promote {
            return;
        }

        let Some(region) = self.compiled_entry(module_idx, pc) else {
            self.tier2_counters.insert((module_idx, pc), 0);
            return;
        };
        if region.len < 3 {
            self.tier2_counters.insert((module_idx, pc), 0);
            return;
        }

        let instructions = &module.instructions;
        match (region.optimization, region.tier) {
            (CodegenOptimization::Fast, CompilationTier::Baseline) => {
                let plan = self.region_planner.plan(module_idx, pc, module);
                if let Some(meta) = plan.type_metadata() {
                    if plan.native_calls.is_empty() {
                        let _ = unsafe {
                            self.promote_region_typed(
                                module_idx,
                                pc,
                                region.len,
                                instructions,
                                meta,
                            )
                        };
                    } else {
                        let _ = unsafe {
                            self.promote_region_baseline(
                                module_idx,
                                pc,
                                region.len,
                                instructions,
                                &plan.native_calls,
                            )
                        };
                    }
                } else {
                    let _ = unsafe {
                        self.promote_region_baseline(
                            module_idx,
                            pc,
                            region.len,
                            instructions,
                            &plan.native_calls,
                        )
                    };
                }
            }
            (CodegenOptimization::Fast, CompilationTier::Typed) => {
                let plan = self.region_planner.plan(module_idx, pc, module);
                if let Some(meta) = plan.type_metadata() {
                    let _ = unsafe {
                        self.promote_region_typed(module_idx, pc, region.len, instructions, meta)
                    };
                }
            }
            (CodegenOptimization::Fast, CompilationTier::Simd) => {
                let plan = self.region_planner.plan(module_idx, pc, module);
                let _ = unsafe {
                    self.promote_region_simd(
                        module_idx,
                        pc,
                        region.len,
                        instructions,
                        plan.type_metadata(),
                    )
                };
            }
            (CodegenOptimization::Optimized, CompilationTier::Typed) => {
                let plan = self.region_planner.plan(module_idx, pc, module);
                let _ = unsafe {
                    self.promote_region_simd(
                        module_idx,
                        pc,
                        region.len,
                        instructions,
                        plan.type_metadata(),
                    )
                };
            }
            (CodegenOptimization::Optimized, CompilationTier::Baseline)
            | (CodegenOptimization::Optimized, CompilationTier::Simd) => {}
        }

        self.tier2_counters.insert((module_idx, pc), 0);
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
        if let Some(region) = self.compiled_entry(module_idx, start_offset) {
            return Some(std::mem::transmute(region.ptr));
        }

        // Build the function
        let func_name = format!("nulang_jit_{}_{}", module_idx, start_offset);
        let started = std::time::Instant::now();

        match self.codegen.compile(NativeCompileRequest {
            symbol: &func_name,
            start_offset,
            num_instrs,
            instructions,
            optimization: CodegenOptimization::Fast,
            kind: NativeCompileKind::Scalar { native_calls },
        }) {
            Ok(ptr) => {
                self.store_compiled_with_metadata(
                    module_idx,
                    start_offset,
                    ptr,
                    num_instrs,
                    CompilationTier::Baseline,
                    CodegenOptimization::Fast,
                    !native_calls.is_empty(),
                    Self::elapsed_ns(started),
                );
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
        if let Some(region) = self.compiled_entry(module_idx, start_offset) {
            return Some(std::mem::transmute(region.ptr));
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
            let started = std::time::Instant::now();
            if let Ok(ptr) = self.codegen.compile(NativeCompileRequest {
                symbol: &func_name,
                start_offset,
                num_instrs,
                instructions,
                optimization: CodegenOptimization::Fast,
                kind: NativeCompileKind::Typed { type_metadata },
            }) {
                self.store_compiled_with_metadata(
                    module_idx,
                    start_offset,
                    ptr,
                    num_instrs,
                    CompilationTier::Typed,
                    CodegenOptimization::Fast,
                    false,
                    Self::elapsed_ns(started),
                );
                self.typed_regions.insert((module_idx, start_offset));
                return Some(std::mem::transmute(ptr));
            }
            // Typed compilation failed: fall through to the scalar compiler.
        }

        self.compile_region(
            module_idx,
            start_offset,
            num_instrs,
            instructions,
            native_calls,
        )
    }

    unsafe fn promote_region_baseline(
        &mut self,
        module_idx: usize,
        start_offset: usize,
        num_instrs: usize,
        instructions: &[crate::bytecode::Instruction],
        native_calls: &std::collections::HashMap<usize, usize>,
    ) -> Option<JitFunctionPtr> {
        let func_name = self.next_promotion_name("nulang_jit_opt", module_idx, start_offset);
        let started = std::time::Instant::now();
        match self.codegen.compile(NativeCompileRequest {
            symbol: &func_name,
            start_offset,
            num_instrs,
            instructions,
            optimization: CodegenOptimization::Optimized,
            kind: NativeCompileKind::Scalar { native_calls },
        }) {
            Ok(ptr) => {
                self.store_compiled_with_metadata(
                    module_idx,
                    start_offset,
                    ptr,
                    num_instrs,
                    CompilationTier::Baseline,
                    CodegenOptimization::Optimized,
                    !native_calls.is_empty(),
                    Self::elapsed_ns(started),
                );
                Some(std::mem::transmute(ptr))
            }
            Err(_) => None,
        }
    }

    unsafe fn promote_region_typed(
        &mut self,
        module_idx: usize,
        start_offset: usize,
        num_instrs: usize,
        instructions: &[crate::bytecode::Instruction],
        type_metadata: &crate::jit::typed_compiler::TypeMetadata,
    ) -> Option<JitFunctionPtr> {
        if type_metadata.is_empty() {
            return None;
        }

        let func_name = self.next_promotion_name("nulang_tjit_promote", module_idx, start_offset);
        let started = std::time::Instant::now();
        match self.codegen.compile(NativeCompileRequest {
            symbol: &func_name,
            start_offset,
            num_instrs,
            instructions,
            optimization: CodegenOptimization::Optimized,
            kind: NativeCompileKind::Typed {
                type_metadata: Some(type_metadata),
            },
        }) {
            Ok(ptr) => {
                self.store_compiled_with_metadata(
                    module_idx,
                    start_offset,
                    ptr,
                    num_instrs,
                    CompilationTier::Typed,
                    CodegenOptimization::Optimized,
                    false,
                    Self::elapsed_ns(started),
                );
                self.typed_regions.insert((module_idx, start_offset));
                Some(std::mem::transmute(ptr))
            }
            Err(_) => None,
        }
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
        self.compiled_entry(module_idx, offset).is_some()
    }

    /// Get the compiled function pointer for `(module_idx, offset)` (if compiled).
    ///
    /// # Safety
    /// The returned function pointer is valid only while this `JitSession` is
    /// alive and the original bytecode has not been modified.
    pub unsafe fn get_compiled(&self, module_idx: usize, offset: usize) -> Option<JitFunctionPtr> {
        self.compiled_entry(module_idx, offset)
            .map(|region| std::mem::transmute(region.ptr))
    }

    /// Number of bytecode instructions covered by the compiled region at
    /// `(module_idx, offset)`, recorded at compile time. The VM uses this
    /// to advance pc after a JIT run instead of re-scanning the
    /// instruction stream.
    pub fn compiled_region_len(&self, module_idx: usize, offset: usize) -> Option<usize> {
        self.compiled_entry(module_idx, offset)
            .map(|region| region.len)
    }

    /// Return the number of compiled regions.
    pub fn compiled_count(&self) -> usize {
        self.compiled_count
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
        use crate::jit::simd_compiler::is_simd_supported;

        // Check if already compiled
        if let Some(region) = self.compiled_entry(module_idx, start_offset) {
            return Some(std::mem::transmute(region.ptr));
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

        // Analyze for vectorizable patterns. The SIMD compiler falls back to
        // scalar when it cannot determine a trip count; keep tier metadata
        // honest by taking the typed/scalar path explicitly in that case.
        let simd_region = analyze_region(instructions, start_offset, num_instrs, type_metadata)?;
        if simd_region.trip_count_hint.is_none() {
            return self.compile_region_typed(
                module_idx,
                start_offset,
                num_instrs,
                instructions,
                type_metadata,
                &std::collections::HashMap::new(),
            );
        }

        let func_name = format!("nulang_simd_{}_{}", module_idx, start_offset);
        let started = std::time::Instant::now();

        match self.codegen.compile(NativeCompileRequest {
            symbol: &func_name,
            start_offset,
            num_instrs,
            instructions,
            optimization: CodegenOptimization::Optimized,
            kind: NativeCompileKind::Simd {
                region: &simd_region,
            },
        }) {
            Ok(ptr) => {
                self.store_compiled_with_metadata(
                    module_idx,
                    start_offset,
                    ptr,
                    num_instrs,
                    CompilationTier::Simd,
                    CodegenOptimization::Optimized,
                    false,
                    Self::elapsed_ns(started),
                );
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

    unsafe fn promote_region_simd(
        &mut self,
        module_idx: usize,
        start_offset: usize,
        num_instrs: usize,
        instructions: &[crate::bytecode::Instruction],
        type_metadata: Option<&crate::jit::typed_compiler::TypeMetadata>,
    ) -> Option<JitFunctionPtr> {
        use crate::jit::simd_analyzer::analyze_region;
        use crate::jit::simd_compiler::is_simd_supported;

        if !is_simd_supported() {
            return None;
        }

        let started = std::time::Instant::now();
        let simd_region = analyze_region(instructions, start_offset, num_instrs, type_metadata)?;
        if simd_region.trip_count_hint.is_none() {
            return None;
        }
        let func_name = self.next_promotion_name("nulang_simd_promote", module_idx, start_offset);

        match self.codegen.compile(NativeCompileRequest {
            symbol: &func_name,
            start_offset,
            num_instrs,
            instructions,
            optimization: CodegenOptimization::Optimized,
            kind: NativeCompileKind::Simd {
                region: &simd_region,
            },
        }) {
            Ok(ptr) => {
                self.store_compiled_with_metadata(
                    module_idx,
                    start_offset,
                    ptr,
                    num_instrs,
                    CompilationTier::Simd,
                    CodegenOptimization::Optimized,
                    false,
                    Self::elapsed_ns(started),
                );
                Some(std::mem::transmute(ptr))
            }
            Err(_) => None,
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

// TieredAction is defined in `crate::backends` so the VM can reference it
// without importing the JIT module. Re-export for backward compatibility.
pub use crate::backends::TieredAction;

// ---------------------------------------------------------------------------
// JitBackend trait impl — adapts the Cranelift JIT to the backend trait
// ---------------------------------------------------------------------------

impl crate::backends::JitBackend for JitSession {
    fn is_compiled(&self, module_idx: usize, pc: usize) -> bool {
        self.compiled_entry(module_idx, pc).is_some()
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
        // Bytecode PCs are dense, so checking for compiled code is a pair of
        // bounds checks plus an Option load rather than a hash-table probe.
        if self.compiled_entry(module_idx, pc).is_some() {
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
        self.compiled_entry(module_idx, pc).map(|region| region.len)
    }

    fn compiled_region_requires_vm_reentry(&self, module_idx: usize, pc: usize) -> bool {
        self.compiled_entry(module_idx, pc)
            .map(|region| region.requires_vm_reentry)
            .unwrap_or(true)
    }

    fn compiled_count(&self) -> usize {
        self.compiled_count
    }

    fn typed_compiled_count(&self) -> usize {
        self.typed_regions.len()
    }

    fn reset_hot_counters(&mut self) {
        self.hot_counts.clear();
    }

    fn prepare_tiered_step(
        &mut self,
        module_idx: usize,
        pc: usize,
        module: &crate::bytecode::CodeModule,
    ) -> bool {
        let instructions = &module.instructions;

        // The caller already probed this PC. Existing compiled code performs
        // Tier-2 bookkeeping while the module borrow is still available, before
        // native execution can re-enter the VM.
        if self.compiled_entry(module_idx, pc).is_some() {
            self.record_tier2_and_maybe_promote(module_idx, pc, module);
            return true;
        }

        let plan = self.region_planner.plan(module_idx, pc, module);
        if plan.len >= 3 {
            if unsafe {
                self.compile_region_typed(
                    module_idx,
                    pc,
                    plan.len,
                    instructions,
                    plan.type_metadata(),
                    &plan.native_calls,
                )
            }
            .is_some()
            {
                return true;
            }
        }

        // Rejected (too small / fragmented) or compile failed. Reset the hot
        // counter so the next interpreted execution does not rescan every step.
        if module_idx < self.hot_counts.len() && pc < self.hot_counts[module_idx].len() {
            self.hot_counts[module_idx][pc] = 0;
        }
        false
    }

    fn execute_compiled(
        &mut self,
        module_idx: usize,
        pc: usize,
        regs: &mut [u64; 256],
        constants: &[u64],
    ) -> crate::backends::TieredAction {
        let Some(func) = (unsafe { self.get_compiled(module_idx, pc) }) else {
            return crate::backends::TieredAction::Interpret;
        };
        func(regs.as_mut_ptr(), constants.as_ptr());
        crate::backends::TieredAction::RanJit
    }
}
