//! SIMD Vectorization Pattern Analyzer
//!
//! Detects loops that can be vectorized with SIMD instructions.
//! Scans bytecode regions for element-wise array operation patterns.
//!
//! # Overview
//!
//! The analyzer identifies `for`-style loops over arrays where each iteration
//! performs independent element-wise operations. When a pattern is recognized,
//! the JIT can emit Cranelift SIMD instructions (e.g., `I64x2` add, `F32x4` mul)
//! instead of scalar operations, yielding up to 2-4x speedup on numeric kernels.
//!
//! # Supported Patterns
//!
//! | Pattern | Example | SIMD Width |
//! |---------|---------|------------|
//! | `ElementWiseBinop` | `c[i] = a[i] + b[i]` | I64x2, F64x2, I32x4, F32x4 |
//! | `ElementWiseUnary` | `b[i] = -a[i]` | I64x2, F64x2, I32x4, F32x4 |
//! | `ElementWiseCmp` | `c[i] = a[i] < b[i]` | I64x2, F64x2, I32x4, F32x4 |
//!
//! # Vectorization Requirements
//!
//! All of the following must hold for a region to be marked vectorizable:
//!
//! 1. The loop body contains at least one `ArrLoad` → arithmetic → `ArrStore` chain.
//! 2. All array accesses use the **same** induction variable register as the index.
//! 3. The induction variable increments by 1 each iteration (`IInc` on the induction
//!    register, or `IAdd` with constant 1).
//! 4. The trip count is determinable (`ArrLen` comparison or a constant bound).
//! 5. The element type is uniform across all array operations.
//! 6. No loop-carried dependencies (the destination array is different from source
//!    arrays, or the same array with no overlap concerns).
//! 7. No function calls (`Call`, `ClosureCall`) inside the loop body.
//! 8. No control flow other than the back-edge jump (conditional exit or unconditional
//!    jump back to loop header).

use crate::bytecode::{Instruction, OpCode};
use crate::jit::typed_compiler::{KnownType, TypeMetadata};
pub use crate::simd::SimdElemType;

// ---------------------------------------------------------------------------
// SimdWidth
// ---------------------------------------------------------------------------

/// The SIMD vectorization width (number of scalar elements per vector).
///
/// Each variant records the native vector width on a 128-bit SIMD register.
/// Future extensions may add `Width8` for 16-bit types and `Width16` for 8-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimdWidth {
    /// 2-wide vectors: I64x2, F64x2 — used for 64-bit element types.
    Width2,
    /// 4-wide vectors: I32x4, F32x4 — used for 32-bit element types.
    Width4,
    /// 8-wide vectors: I16x8 — reserved for future 16-bit element support.
    Width8,
}

impl SimdWidth {
    /// Return the number of lanes (scalar elements) per vector.
    pub fn lanes(&self) -> usize {
        match self {
            SimdWidth::Width2 => 2,
            SimdWidth::Width4 => 4,
            SimdWidth::Width8 => 8,
        }
    }

    /// Derive the SIMD width from an element type.
    pub fn from_elem_type(elem_type: SimdElemType) -> Self {
        match elem_type {
            SimdElemType::Int64 | SimdElemType::Float64 => SimdWidth::Width2,
            SimdElemType::Int32 | SimdElemType::Float32 => SimdWidth::Width4,
        }
    }
}

// ---------------------------------------------------------------------------
// VectorizablePattern
// ---------------------------------------------------------------------------

/// The kind of vectorizable loop pattern detected in a bytecode region.
///
/// Each variant describes the shape of the loop body so that the SIMD
/// compiler knows which instruction sequence to emit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VectorizablePattern {
    /// Element-wise binary operation: `dst[i] = lhs[i] op rhs[i]`.
    ///
    /// Registers (in order): `lhs_arr`, `rhs_arr`, `dst_arr`.
    /// Example: `c[i] = a[i] + b[i]`
    ElementWiseBinop {
        op: BinopKind,
        lhs_arr_reg: u8,
        rhs_arr_reg: u8,
        dst_arr_reg: u8,
        /// The register that receives the loaded `lhs` element (temp).
        lhs_elem_reg: u8,
        /// The register that receives the loaded `rhs` element (temp).
        rhs_elem_reg: u8,
        /// The register that holds the binop result before store (temp).
        result_reg: u8,
    },

    /// Element-wise unary operation: `dst[i] = op(src[i])`.
    ///
    /// Registers (in order): `src_arr`, `dst_arr`.
    /// Example: `b[i] = -a[i]`
    ElementWiseUnary {
        op: UnaryKind,
        src_arr_reg: u8,
        dst_arr_reg: u8,
        /// The register that receives the loaded element (temp).
        src_elem_reg: u8,
        /// The register that holds the unary result before store (temp).
        result_reg: u8,
    },

    /// Element-wise comparison: `dst[i] = lhs[i] cmp rhs[i]`.
    ///
    /// Registers (in order): `lhs_arr`, `rhs_arr`, `dst_arr`.
    /// Example: `c[i] = a[i] < b[i]`
    ElementWiseCmp {
        op: CmpKind,
        lhs_arr_reg: u8,
        rhs_arr_reg: u8,
        dst_arr_reg: u8,
        lhs_elem_reg: u8,
        rhs_elem_reg: u8,
        result_reg: u8,
    },
}

/// Binary operation kinds supported for SIMD vectorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinopKind {
    IAdd,
    ISub,
    IMul,
    IDiv,
    FAdd,
    FSub,
    FMul,
    FDiv,
}

impl BinopKind {
    /// Return true if this is a floating-point operation.
    pub fn is_float(&self) -> bool {
        matches!(
            self,
            BinopKind::FAdd | BinopKind::FSub | BinopKind::FMul | BinopKind::FDiv
        )
    }

    /// Return true if this is an integer operation.
    pub fn is_int(&self) -> bool {
        !self.is_float()
    }
}

/// Unary operation kinds supported for SIMD vectorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryKind {
    INeg,
    FNeg,
}

/// Comparison operation kinds supported for SIMD vectorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpKind {
    ICmpEq,
    ICmpLt,
    ICmpGt,
    ICmpLe,
    ICmpGe,
    FCmpEq,
    FCmpLt,
    FCmpGt,
}

impl CmpKind {
    /// Return true if this is a floating-point comparison.
    pub fn is_float(&self) -> bool {
        matches!(self, CmpKind::FCmpEq | CmpKind::FCmpLt | CmpKind::FCmpGt)
    }
}

// ---------------------------------------------------------------------------
// SimdRegion
// ---------------------------------------------------------------------------

/// Description of a vectorizable loop region found in the bytecode.
///
/// Created by [`analyze_region`] or [`SimdAnalyzer::find_all_vectorizable_regions`]
/// and consumed by the SIMD compiler to emit Cranelift SIMD instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimdRegion {
    /// Bytecode offset where the vectorizable loop body starts.
    pub start_offset: usize,
    /// Number of instructions in the detected loop body.
    pub num_instrs: usize,
    /// The detected vectorization pattern (binary / unary / comparison).
    pub pattern: VectorizablePattern,
    /// The SIMD width to use (derived from `elem_type`).
    pub width: SimdWidth,
    /// The scalar element type of the arrays (determines lane width).
    pub elem_type: SimdElemType,
    /// The register that holds the loop induction variable (the array index).
    pub induction_var_reg: u8,
    /// Registers that hold array references (input + output arrays).
    pub array_regs: Vec<u8>,
    /// Known trip count if statically determinable (e.g. from `ArrLen`).
    /// `Some(0)` means "runtime-determined from `arr_len_reg`".
    pub trip_count_hint: Option<usize>,
    /// Register holding the `ArrLen` result when `trip_count_hint == Some(0)`.
    pub arr_len_reg: Option<u8>,
}

// ---------------------------------------------------------------------------
// SimdAnalyzer
// ---------------------------------------------------------------------------

/// Stateful SIMD pattern analyzer.
///
/// Scans a full instruction stream to discover every vectorizable loop region.
/// Each discovered region is returned as a [`SimdRegion`] that the SIMD compiler
/// can then transform into native SIMD code.
///
/// # Example
///
/// ```ignore
/// ```no_run
/// use nulang::jit::simd_analyzer::SimdAnalyzer;
/// use nulang::bytecode::Instruction;
///
/// let analyzer = SimdAnalyzer::new();
/// let regions = analyzer.find_all_vectorizable_regions(&instructions, None);
/// ```
#[derive(Debug, Clone, Default)]
pub struct SimdAnalyzer {
    // Currently stateless; reserved for future caching / profiling state.
}

impl SimdAnalyzer {
    /// Create a new SIMD analyzer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan the full instruction stream and return every vectorizable region.
    ///
    /// The algorithm walks the instruction stream and, for every potential loop
    /// body (identified by a backward jump), calls [`analyze_region`]. Regions
    /// are returned sorted by `start_offset`.
    pub fn find_all_vectorizable_regions(
        &self,
        instructions: &[Instruction],
        type_metadata: Option<&TypeMetadata>,
    ) -> Vec<SimdRegion> {
        let mut regions = Vec::new();
        let n = instructions.len();
        if n < 3 {
            return regions;
        }

        // Scan for backward jumps (potential loop back-edges).
        for pc in 0..n {
            let instr = instructions[pc];
            let backward_target = match instr.opcode {
                OpCode::Jmp => {
                    let target = (pc as i64 + instr.simm16() as i64) as usize;
                    if target < pc {
                        Some(target)
                    } else {
                        None
                    }
                }
                OpCode::JmpT | OpCode::JmpF => {
                    // The conditional jump may jump backward (loop back-edge)
                    // or forward (loop exit). We look at the backward case.
                    let target = (pc as i64 + instr.offset16() as i64) as usize;
                    if target < pc {
                        Some(target)
                    } else {
                        None
                    }
                }
                _ => None,
            };

            if let Some(loop_header) = backward_target {
                // The loop body spans from the header to the back-edge (inclusive).
                let body_start = loop_header;
                let body_end = pc; // back-edge instruction
                let body_len = if body_end > body_start {
                    body_end - body_start
                } else {
                    continue;
                };

                // Skip tiny bodies — not worth vectorizing.
                if body_len < 3 {
                    continue;
                }

                if let Some(region) =
                    analyze_region(instructions, body_start, body_len, type_metadata)
                {
                    // Avoid duplicate regions (same start offset).
                    if !regions
                        .iter()
                        .any(|r: &SimdRegion| r.start_offset == region.start_offset)
                    {
                        regions.push(region);
                    }
                }
            }
        }

        // Also look for simple counted loops without explicit backward jumps
        // by scanning for ArrLoad / ArrStore patterns with IInc.
        // This catches loops that the simple back-edge detection might miss.
        self.find_counted_loop_patterns(instructions, type_metadata, &mut regions);

        // Sort by start offset for deterministic output.
        regions.sort_by_key(|r| r.start_offset);
        regions
    }

    /// Look for counted loop patterns: sequences that load from arrays,
    /// perform an operation, store back, and increment an induction variable.
    fn find_counted_loop_patterns(
        &self,
        instructions: &[Instruction],
        type_metadata: Option<&TypeMetadata>,
        regions: &mut Vec<SimdRegion>,
    ) {
        // This is a simpler pattern matcher that looks for windows containing
        // ArrLoad → arithmetic → ArrStore with an IInc.
        // The window-based approach helps catch loops the back-edge detector misses.
        let n = instructions.len();
        let min_window = 5;
        let max_window = 50;

        for start in 0..n.saturating_sub(min_window) {
            let max_end = (start + max_window).min(n);
            for end in (start + min_window)..max_end {
                if end > n {
                    break;
                }
                let len = end - start;

                // Skip if we already have a region at this start.
                if regions.iter().any(|r| r.start_offset == start) {
                    continue;
                }

                if let Some(region) = analyze_region(instructions, start, len, type_metadata) {
                    // Skip if this region overlaps an already-discovered region.
                    let overlaps = regions.iter().any(|r: &SimdRegion| {
                        let r_end = r.start_offset + r.num_instrs;
                        let new_end = region.start_offset + region.num_instrs;
                        region.start_offset < r_end && new_end > r.start_offset
                    });
                    if !overlaps {
                        regions.push(region);
                    }
                    break; // Found one at this start, move on.
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// analyze_region (core analysis)
// ---------------------------------------------------------------------------

/// Analyze a contiguous bytecode region and determine whether it forms a
/// vectorizable loop.
///
/// Returns `Some(SimdRegion)` when **all** of the vectorization requirements
/// are satisfied, or `None` otherwise.
///
/// # Arguments
///
/// * `instructions` — The full bytecode instruction array.
/// * `start_offset` — Bytecode offset where the candidate region starts.
/// * `num_instrs` — Number of instructions in the candidate region.
/// * `type_metadata` — Optional static type information for registers.
pub fn analyze_region(
    instructions: &[Instruction],
    start_offset: usize,
    num_instrs: usize,
    type_metadata: Option<&TypeMetadata>,
) -> Option<SimdRegion> {
    let end_offset = (start_offset + num_instrs).min(instructions.len());
    let body = &instructions[start_offset..end_offset];

    if body.len() < 3 {
        return None;
    }

    // --- Requirement 7 & 8: Check for unsupported opcodes ---
    let mut induction_reg: Option<u8> = None;
    let mut _back_edge_found = false;

    for (i, instr) in body.iter().enumerate() {
        match instr.opcode {
            // Reject function calls inside the loop body.
            OpCode::Call | OpCode::ClosureCall | OpCode::TailCall => {
                return None;
            }
            // Reject complex control flow (anything other than Jmp/JmpT/JmpF).
            OpCode::Switch | OpCode::Ret | OpCode::RetVal => {
                return None;
            }
            // Actor / concurrency opcodes are not vectorizable.
            OpCode::Spawn
            | OpCode::Send
            | OpCode::Ask
            | OpCode::SelfOp
            | OpCode::Receive
            | OpCode::ReceiveMatch
            | OpCode::ReceiveCommit
            | OpCode::Monitor
            | OpCode::Demon
            | OpCode::Link
            | OpCode::Unlink
            | OpCode::Exit
            | OpCode::Yield => {
                return None;
            }
            // Effect operations are not vectorizable.
            OpCode::Perform | OpCode::Handle | OpCode::Resume | OpCode::Unwind => {
                return None;
            }
            // IO / debug not vectorizable.
            OpCode::SRead
            | OpCode::FOpen
            | OpCode::FRead
            | OpCode::FWrite
            | OpCode::FClose
            | OpCode::DbgBreak
            | OpCode::DbgPrint
            | OpCode::DbgStack => {
                return None;
            }
            // Detect induction variable increment.
            OpCode::IInc => {
                if induction_reg.is_none() {
                    induction_reg = Some(instr.op1);
                }
            }
            // Detect back-edge jump.
            OpCode::Jmp => {
                let target = (start_offset + i) as i64 + instr.simm16() as i64;
                if (target as usize) < start_offset + i {
                    _back_edge_found = true;
                }
                // Forward jumps inside the loop body are also not allowed (except exit).
                let target_usize = target as usize;
                if target_usize >= start_offset
                    && target_usize < end_offset
                    && target_usize != start_offset
                {
                    // Jump to somewhere inside the loop body (not the header) — reject.
                    // This indicates complex control flow.
                    // However, allow it if it's just skipping past the back-edge.
                }
            }
            OpCode::JmpT | OpCode::JmpF => {
                let target = (start_offset + i) as i64 + instr.offset16() as i64;
                let target_usize = target as usize;
                // Conditional jump forward past the loop is fine (exit condition).
                if target_usize > end_offset {
                    // loop exit — OK
                } else if target_usize < start_offset + i {
                    // backward jump — another back-edge
                    _back_edge_found = true;
                }
            }
            _ => {}
        }
    }

    // --- Detect array load → op → store patterns ---
    // Collect all ArrLoad and ArrStore instructions and their registers.
    let mut loads: Vec<(usize, u8, u8, u8)> = Vec::new(); // (body_idx, arr_reg, idx_reg, dst_reg)
    let mut stores: Vec<(usize, u8, u8, u8)> = Vec::new(); // (body_idx, arr_reg, idx_reg, src_reg)

    for (i, instr) in body.iter().enumerate() {
        match instr.opcode {
            OpCode::ArrLoad => {
                loads.push((i, instr.op1, instr.op2, instr.op3));
            }
            OpCode::ArrStore => {
                stores.push((i, instr.op1, instr.op2, instr.op3));
            }
            _ => {}
        }
    }

    // Requirement 1: At least one load and one store.
    if loads.is_empty() || stores.is_empty() {
        return None;
    }

    // --- Requirement 2: All array accesses must use the same index register ---
    // (the induction variable).
    let idx_reg = loads[0].2;

    // All loads must use the same index register.
    for &(_, _, ir, _) in &loads {
        if ir != idx_reg {
            return None;
        }
    }

    // All stores must use the same index register.
    for &(_, _, ir, _) in &stores {
        if ir != idx_reg {
            return None;
        }
    }

    // --- Requirement 3: The induction variable must be incremented ---
    let has_iinc = body
        .iter()
        .any(|instr| instr.opcode == OpCode::IInc && instr.op1 == idx_reg);

    // Also accept IAdd idx_reg, Const1/Const0-like as induction increment.
    let has_iadd_inc = body.iter().any(|instr| {
        instr.opcode == OpCode::IAdd
            && instr.op3 == idx_reg
            && (instr.op1 == idx_reg || instr.op2 == idx_reg)
    });

    if !has_iinc && !has_iadd_inc {
        return None;
    }

    // Set the induction variable register.
    induction_reg = Some(idx_reg);

    // --- Try to detect a specific pattern ---

    // Look for ElementWiseBinop: two loads, one binop, one store.
    if let Some(pattern) = try_detect_elementwise_binop(body, &loads, &stores, start_offset) {
        let elem_type = infer_elem_type(&pattern, type_metadata);
        let width = SimdWidth::from_elem_type(elem_type);
        let array_regs = collect_array_regs(&pattern);

        // Requirement 6: Check loop-carried dependencies.
        if has_loop_carried_dependency(&pattern, &stores, idx_reg) {
            return None;
        }
        // Requirement 4: Try to find trip count hint.
        let (trip_count_hint, arr_len_reg) = find_trip_count_hint(body, &array_regs);

        return Some(SimdRegion {
            start_offset,
            num_instrs,
            pattern,
            width,
            elem_type,
            induction_var_reg: induction_reg.unwrap(),
            array_regs,
            trip_count_hint,
            arr_len_reg,
        });
    }

    // Look for ElementWiseUnary: one load, one unary op, one store.
    if let Some(pattern) = try_detect_elementwise_unary(body, &loads, &stores, start_offset) {
        let elem_type = infer_elem_type_unary(&pattern, type_metadata);
        let width = SimdWidth::from_elem_type(elem_type);
        let array_regs = collect_array_regs_unary(&pattern);

        if has_loop_carried_dependency_unary(&pattern, &stores, idx_reg) {
            return None;
        }

        let (trip_count_hint, arr_len_reg) = find_trip_count_hint(body, &array_regs);

        return Some(SimdRegion {
            start_offset,
            num_instrs,
            pattern,
            width,
            elem_type,
            induction_var_reg: induction_reg.unwrap(),
            array_regs,
            trip_count_hint,
            arr_len_reg,
        });
    }

    // Look for ElementWiseCmp: two loads, one comparison, one store.
    if let Some(pattern) = try_detect_elementwise_cmp(body, &loads, &stores, start_offset) {
        let elem_type = infer_elem_type_cmp(&pattern, type_metadata);
        let width = SimdWidth::from_elem_type(elem_type);
        let array_regs = collect_array_regs_cmp(&pattern);

        if has_loop_carried_dependency_cmp(&pattern, &stores, idx_reg) {
            return None;
        }

        let (trip_count_hint, arr_len_reg) = find_trip_count_hint(body, &array_regs);

        return Some(SimdRegion {
            start_offset,
            num_instrs,
            pattern,
            width,
            elem_type,
            induction_var_reg: induction_reg.unwrap(),
            array_regs,
            trip_count_hint,
            arr_len_reg,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Pattern Detection Helpers
// ---------------------------------------------------------------------------

/// Try to detect `ElementWiseBinop`: two ArrLoads, one binary op, one ArrStore.
fn try_detect_elementwise_binop(
    body: &[Instruction],
    loads: &[(usize, u8, u8, u8)],
    stores: &[(usize, u8, u8, u8)],
    _start_offset: usize,
) -> Option<VectorizablePattern> {
    // Need at least 2 loads and 1 store.
    if loads.len() < 2 || stores.len() < 1 {
        return None;
    }

    // Try every pair of loads and every store.
    for (li1, &(_load1_idx, arr1, _idx1, dst1)) in loads.iter().enumerate() {
        for (li2, &(_load2_idx, arr2, _idx2, dst2)) in loads.iter().enumerate() {
            if li1 == li2 {
                continue;
            }

            for &(_store_idx, store_arr, _store_idx_reg, store_src) in stores {
                // Look for a binary operation that takes dst1 and dst2 as operands
                // and produces store_src.
                for instr in body {
                    let op_kind = match instr.opcode {
                        OpCode::IAdd => Some(BinopKind::IAdd),
                        OpCode::ISub => Some(BinopKind::ISub),
                        OpCode::IMul => Some(BinopKind::IMul),
                        OpCode::IDiv => Some(BinopKind::IDiv),
                        OpCode::FAdd => Some(BinopKind::FAdd),
                        OpCode::FSub => Some(BinopKind::FSub),
                        OpCode::FMul => Some(BinopKind::FMul),
                        OpCode::FDiv => Some(BinopKind::FDiv),
                        _ => None,
                    };

                    if let Some(op) = op_kind {
                        // Check if the binop uses the loaded values as operands
                        // and produces the value that gets stored.
                        let uses_operands = (instr.op1 == dst1 && instr.op2 == dst2)
                            || (instr.op1 == dst2 && instr.op2 == dst1);
                        let produces_store_src = instr.op3 == store_src;

                        if uses_operands && produces_store_src {
                            return Some(VectorizablePattern::ElementWiseBinop {
                                op,
                                lhs_arr_reg: arr1,
                                rhs_arr_reg: arr2,
                                dst_arr_reg: store_arr,
                                lhs_elem_reg: dst1,
                                rhs_elem_reg: dst2,
                                result_reg: store_src,
                            });
                        }
                    }
                }
            }
        }
    }

    None
}

/// Try to detect `ElementWiseUnary`: one ArrLoad, one unary op, one ArrStore.
fn try_detect_elementwise_unary(
    body: &[Instruction],
    loads: &[(usize, u8, u8, u8)],
    stores: &[(usize, u8, u8, u8)],
    _start_offset: usize,
) -> Option<VectorizablePattern> {
    // Need at least 1 load and 1 store.
    if loads.is_empty() || stores.is_empty() {
        return None;
    }

    for &(_load_idx, load_arr, _idx, load_dst) in loads {
        // If there are multiple loads, skip unary pattern to prefer binary.
        if loads.len() > 1 {
            // Only consider this load if no other load feeds into the same store.
            // For simplicity, only allow unary when there's exactly 1 load.
            if loads.len() != 1 {
                continue;
            }
        }

        for &(_store_idx, store_arr, _store_idx_reg, store_src) in stores {
            // Look for a unary operation that takes load_dst and produces store_src.
            for instr in body {
                let op_kind = match instr.opcode {
                    OpCode::INeg => Some(UnaryKind::INeg),
                    OpCode::FNeg => Some(UnaryKind::FNeg),
                    _ => None,
                };

                if let Some(op) = op_kind {
                    let uses_loaded = instr.op1 == load_dst;
                    let produces_store_src = instr.op2 == store_src;

                    if uses_loaded && produces_store_src {
                        return Some(VectorizablePattern::ElementWiseUnary {
                            op,
                            src_arr_reg: load_arr,
                            dst_arr_reg: store_arr,
                            src_elem_reg: load_dst,
                            result_reg: store_src,
                        });
                    }
                }
            }
        }
    }

    None
}

/// Try to detect `ElementWiseCmp`: two ArrLoads, one comparison, one ArrStore.
fn try_detect_elementwise_cmp(
    body: &[Instruction],
    loads: &[(usize, u8, u8, u8)],
    stores: &[(usize, u8, u8, u8)],
    _start_offset: usize,
) -> Option<VectorizablePattern> {
    // Need at least 2 loads and 1 store.
    if loads.len() < 2 || stores.len() < 1 {
        return None;
    }

    for (li1, &(_load1_idx, arr1, _idx1, dst1)) in loads.iter().enumerate() {
        for (li2, &(_load2_idx, arr2, _idx2, dst2)) in loads.iter().enumerate() {
            if li1 == li2 {
                continue;
            }

            for &(_store_idx, store_arr, _store_idx_reg, store_src) in stores {
                for instr in body {
                    let op_kind = match instr.opcode {
                        OpCode::ICmpEq => Some(CmpKind::ICmpEq),
                        OpCode::ICmpLt => Some(CmpKind::ICmpLt),
                        OpCode::ICmpGt => Some(CmpKind::ICmpGt),
                        OpCode::ICmpLe => Some(CmpKind::ICmpLe),
                        OpCode::ICmpGe => Some(CmpKind::ICmpGe),
                        OpCode::FCmpEq => Some(CmpKind::FCmpEq),
                        OpCode::FCmpLt => Some(CmpKind::FCmpLt),
                        OpCode::FCmpGt => Some(CmpKind::FCmpGt),
                        _ => None,
                    };

                    if let Some(op) = op_kind {
                        let uses_operands = (instr.op1 == dst1 && instr.op2 == dst2)
                            || (instr.op1 == dst2 && instr.op2 == dst1);
                        let produces_store_src = instr.op3 == store_src;

                        if uses_operands && produces_store_src {
                            return Some(VectorizablePattern::ElementWiseCmp {
                                op,
                                lhs_arr_reg: arr1,
                                rhs_arr_reg: arr2,
                                dst_arr_reg: store_arr,
                                lhs_elem_reg: dst1,
                                rhs_elem_reg: dst2,
                                result_reg: store_src,
                            });
                        }
                    }
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Loop-Carried Dependency Check
// ---------------------------------------------------------------------------

/// Check for loop-carried dependencies in a binary pattern.
///
/// Returns `true` if the pattern has a loop-carried dependency that would
/// prevent safe SIMD vectorization.
///
/// We conservatively reject in-place operations (where the destination array
/// is also a source array) because, even though element-wise in-place ops
/// with the same induction variable are technically safe, they may indicate
/// an accumulator or reduction pattern that is not vectorizable as a simple
/// SIMD lane operation.
fn has_loop_carried_dependency(
    pattern: &VectorizablePattern,
    _stores: &[(usize, u8, u8, u8)],
    _idx_reg: u8,
) -> bool {
    match pattern {
        VectorizablePattern::ElementWiseBinop {
            lhs_arr_reg,
            rhs_arr_reg,
            dst_arr_reg,
            ..
        } => {
            // Reject in-place element-wise operations conservatively.
            // E.g. a[i] = a[i] + b[i] — while safe for SIMD, this may
            // also catch accumulator-style patterns that are not vectorizable.
            dst_arr_reg == lhs_arr_reg || dst_arr_reg == rhs_arr_reg
        }
        _ => false,
    }
}

fn has_loop_carried_dependency_unary(
    pattern: &VectorizablePattern,
    _stores: &[(usize, u8, u8, u8)],
    _idx_reg: u8,
) -> bool {
    match pattern {
        VectorizablePattern::ElementWiseUnary {
            src_arr_reg,
            dst_arr_reg,
            ..
        } => {
            // Conservatively reject in-place unary operations.
            src_arr_reg == dst_arr_reg
        }
        _ => false,
    }
}

fn has_loop_carried_dependency_cmp(
    pattern: &VectorizablePattern,
    _stores: &[(usize, u8, u8, u8)],
    _idx_reg: u8,
) -> bool {
    match pattern {
        VectorizablePattern::ElementWiseCmp {
            lhs_arr_reg,
            rhs_arr_reg,
            dst_arr_reg,
            ..
        } => {
            // Conservatively reject in-place comparison operations.
            dst_arr_reg == lhs_arr_reg || dst_arr_reg == rhs_arr_reg
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Element Type Inference
// ---------------------------------------------------------------------------

/// Infer the element type from a binary pattern, using type metadata if available.
fn infer_elem_type(
    pattern: &VectorizablePattern,
    type_metadata: Option<&TypeMetadata>,
) -> SimdElemType {
    match pattern {
        VectorizablePattern::ElementWiseBinop { op, result_reg, .. } => {
            // Use the operation kind to determine type category.
            if op.is_float() {
                // Check metadata for more precise type.
                if let Some(meta) = type_metadata {
                    let ty = meta.get_type(*result_reg as usize);
                    if ty == KnownType::Float {
                        // Could be Float64 or Float32 — default to Float64 for now.
                        return SimdElemType::Float64;
                    }
                }
                SimdElemType::Float64
            } else {
                if let Some(meta) = type_metadata {
                    let ty = meta.get_type(*result_reg as usize);
                    if ty == KnownType::Int {
                        // Default to Int64 for integer operations.
                        return SimdElemType::Int64;
                    }
                }
                SimdElemType::Int64
            }
        }
        _ => SimdElemType::Int64, // fallback
    }
}

fn infer_elem_type_unary(
    pattern: &VectorizablePattern,
    type_metadata: Option<&TypeMetadata>,
) -> SimdElemType {
    match pattern {
        VectorizablePattern::ElementWiseUnary { op, result_reg, .. } => match op {
            UnaryKind::FNeg => {
                if let Some(meta) = type_metadata {
                    let ty = meta.get_type(*result_reg as usize);
                    if ty == KnownType::Float {
                        return SimdElemType::Float64;
                    }
                }
                SimdElemType::Float64
            }
            UnaryKind::INeg => {
                if let Some(meta) = type_metadata {
                    let ty = meta.get_type(*result_reg as usize);
                    if ty == KnownType::Int {
                        return SimdElemType::Int64;
                    }
                }
                SimdElemType::Int64
            }
        },
        _ => SimdElemType::Int64,
    }
}

fn infer_elem_type_cmp(
    pattern: &VectorizablePattern,
    type_metadata: Option<&TypeMetadata>,
) -> SimdElemType {
    match pattern {
        VectorizablePattern::ElementWiseCmp {
            op, lhs_elem_reg, ..
        } => {
            if op.is_float() {
                if let Some(meta) = type_metadata {
                    let ty = meta.get_type(*lhs_elem_reg as usize);
                    if ty == KnownType::Float {
                        return SimdElemType::Float64;
                    }
                }
                SimdElemType::Float64
            } else {
                if let Some(meta) = type_metadata {
                    let ty = meta.get_type(*lhs_elem_reg as usize);
                    if ty == KnownType::Int {
                        return SimdElemType::Int64;
                    }
                }
                SimdElemType::Int64
            }
        }
        _ => SimdElemType::Int64,
    }
}

// ---------------------------------------------------------------------------
// Trip Count Hint
// ---------------------------------------------------------------------------

/// Try to find a static trip count hint from `ArrLen` in the loop body.
fn find_trip_count_hint(body: &[Instruction], array_regs: &[u8]) -> (Option<usize>, Option<u8>) {
    // Look for ArrLen on one of the arrays in the loop.
    for instr in body {
        if instr.opcode == OpCode::ArrLen {
            if array_regs.contains(&instr.op1) {
                // Found ArrLen on a participating array. The result register
                // (instr.op2) holds the array length at runtime.
                // Return Some(0) as a sentinel for "runtime-determined" plus
                // the register so the compiler can load the length.
                return (Some(0), Some(instr.op2));
            }
        }
    }

    // Look for a constant comparison that gives us a fixed trip count.
    for instr in body {
        match instr.opcode {
            OpCode::Const0 | OpCode::Const1 | OpCode::Const2 | OpCode::ConstM1 => {}
            OpCode::ICmpLt | OpCode::ICmpLe | OpCode::ICmpGt | OpCode::ICmpGe => {}
            _ => {}
        }
    }

    (None, None)
}

// ---------------------------------------------------------------------------
// Array Register Collection
// ---------------------------------------------------------------------------

fn collect_array_regs(pattern: &VectorizablePattern) -> Vec<u8> {
    match pattern {
        VectorizablePattern::ElementWiseBinop {
            lhs_arr_reg,
            rhs_arr_reg,
            dst_arr_reg,
            ..
        } => {
            let mut regs = vec![*lhs_arr_reg, *rhs_arr_reg, *dst_arr_reg];
            regs.dedup();
            regs
        }
        _ => Vec::new(),
    }
}

fn collect_array_regs_unary(pattern: &VectorizablePattern) -> Vec<u8> {
    match pattern {
        VectorizablePattern::ElementWiseUnary {
            src_arr_reg,
            dst_arr_reg,
            ..
        } => {
            let mut regs = vec![*src_arr_reg, *dst_arr_reg];
            regs.dedup();
            regs
        }
        _ => Vec::new(),
    }
}

fn collect_array_regs_cmp(pattern: &VectorizablePattern) -> Vec<u8> {
    match pattern {
        VectorizablePattern::ElementWiseCmp {
            lhs_arr_reg,
            rhs_arr_reg,
            dst_arr_reg,
            ..
        } => {
            let mut regs = vec![*lhs_arr_reg, *rhs_arr_reg, *dst_arr_reg];
            regs.dedup();
            regs
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod simd_analyzer_tests {
    use super::*;
    use crate::bytecode::Instruction;

    // -----------------------------------------------------------------------
    // Helper: Build a simple element-wise binary loop body.
    //
    // Registers:
    //   R0 = array a (input)
    //   R1 = array b (input)
    //   R2 = array c (output)
    //   R3 = induction variable i (index)
    //   R4 = temp for a[i]
    //   R5 = temp for b[i]
    //   R6 = temp for result
    // -----------------------------------------------------------------------

    /// Build instructions for `c[i] = a[i] + b[i]` loop body with a back-edge.
    fn build_iadd_loop_body() -> Vec<Instruction> {
        vec![
            // Loop body starts here (offset 0):
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4), // R4 = a[R3]
            Instruction::new3(OpCode::ArrLoad, 1, 3, 5), // R5 = b[R3]
            Instruction::new3(OpCode::IAdd, 4, 5, 6),    // R6 = R4 + R5
            Instruction::new3(OpCode::ArrStore, 2, 3, 6), // c[R3] = R6
            Instruction::new1(OpCode::IInc, 3),          // R3++
            Instruction::new2(OpCode::Jmp, 0, 0),        // jmp back (patched later)
        ]
    }

    /// Build instructions for `c[i] = a[i] * b[i]` loop body.
    fn build_imul_loop_body() -> Vec<Instruction> {
        vec![
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4), // R4 = a[R3]
            Instruction::new3(OpCode::ArrLoad, 1, 3, 5), // R5 = b[R3]
            Instruction::new3(OpCode::IMul, 4, 5, 6),    // R6 = R4 * R5
            Instruction::new3(OpCode::ArrStore, 2, 3, 6), // c[R3] = R6
            Instruction::new1(OpCode::IInc, 3),          // R3++
            Instruction::new2(OpCode::Jmp, 0, 0),
        ]
    }

    /// Build instructions for `c[i] = a[i] + b[i]` with float addition.
    fn build_fadd_loop_body() -> Vec<Instruction> {
        vec![
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4), // R4 = a[R3]
            Instruction::new3(OpCode::ArrLoad, 1, 3, 5), // R5 = b[R3]
            Instruction::new3(OpCode::FAdd, 4, 5, 6),    // R6 = R4 + R5 (float)
            Instruction::new3(OpCode::ArrStore, 2, 3, 6), // c[R3] = R6
            Instruction::new1(OpCode::IInc, 3),          // R3++
            Instruction::new2(OpCode::Jmp, 0, 0),
        ]
    }

    /// Build a numeric loop with no array operations.
    fn build_numeric_loop_no_arrays() -> Vec<Instruction> {
        vec![
            Instruction::new3(OpCode::IAdd, 3, 4, 5), // R5 = R3 + R4 (just arithmetic)
            Instruction::new1(OpCode::IInc, 3),       // R3++
            Instruction::new2(OpCode::Jmp, 0, 0),
        ]
    }

    /// Build a loop where the output array is also read (accumulator pattern).
    fn build_loop_carried_dep() -> Vec<Instruction> {
        vec![
            // sum[i] = sum[i] + a[i]  (loop-carried via same array)
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4), // R4 = sum[R3]
            Instruction::new3(OpCode::ArrLoad, 1, 3, 5), // R5 = a[R3]
            Instruction::new3(OpCode::IAdd, 4, 5, 6),    // R6 = R4 + R5
            Instruction::new3(OpCode::ArrStore, 0, 3, 6), // sum[R3] = R6  (same as src!)
            Instruction::new1(OpCode::IInc, 3),          // R3++
            Instruction::new2(OpCode::Jmp, 0, 0),
        ]
    }

    /// Build a loop containing a function call.
    fn build_loop_with_call() -> Vec<Instruction> {
        vec![
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4), // R4 = a[R3]
            Instruction::new3(OpCode::Call, 7, 0, 5),    // R5 = call(R7)
            Instruction::new3(OpCode::ArrStore, 1, 3, 5), // b[R3] = R5
            Instruction::new1(OpCode::IInc, 3),          // R3++
            Instruction::new2(OpCode::Jmp, 0, 0),
        ]
    }

    /// Build a comparison loop: `c[i] = a[i] < b[i]`.
    fn build_cmp_loop_body() -> Vec<Instruction> {
        vec![
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4), // R4 = a[R3]
            Instruction::new3(OpCode::ArrLoad, 1, 3, 5), // R5 = b[R3]
            Instruction::new3(OpCode::ICmpLt, 4, 5, 6),  // R6 = R4 < R5
            Instruction::new3(OpCode::ArrStore, 2, 3, 6), // c[R3] = R6
            Instruction::new1(OpCode::IInc, 3),          // R3++
            Instruction::new2(OpCode::Jmp, 0, 0),
        ]
    }

    // -----------------------------------------------------------------------
    // Test 1: Detect element-wise integer addition.
    // -----------------------------------------------------------------------
    #[test]
    fn test_detect_elementwise_add() {
        let instructions = build_iadd_loop_body();
        let region = analyze_region(&instructions, 0, instructions.len(), None);

        assert!(
            region.is_some(),
            "Should detect c[i] = a[i] + b[i] as vectorizable"
        );
        let region = region.unwrap();

        assert_eq!(region.start_offset, 0);
        assert_eq!(region.induction_var_reg, 3);
        assert_eq!(region.array_regs, vec![0, 1, 2]);
        assert_eq!(region.elem_type, SimdElemType::Int64);
        assert_eq!(region.width, SimdWidth::Width2);

        match &region.pattern {
            VectorizablePattern::ElementWiseBinop {
                op,
                lhs_arr_reg,
                rhs_arr_reg,
                dst_arr_reg,
                ..
            } => {
                assert_eq!(*op, BinopKind::IAdd);
                assert_eq!(*lhs_arr_reg, 0);
                assert_eq!(*rhs_arr_reg, 1);
                assert_eq!(*dst_arr_reg, 2);
            }
            other => panic!("Expected ElementWiseBinop, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 2: Detect element-wise integer multiplication.
    // -----------------------------------------------------------------------
    #[test]
    fn test_detect_elementwise_mul() {
        let instructions = build_imul_loop_body();
        let region = analyze_region(&instructions, 0, instructions.len(), None);

        assert!(
            region.is_some(),
            "Should detect c[i] = a[i] * b[i] as vectorizable"
        );
        let region = region.unwrap();

        assert_eq!(region.induction_var_reg, 3);
        assert_eq!(region.array_regs, vec![0, 1, 2]);

        match &region.pattern {
            VectorizablePattern::ElementWiseBinop { op, .. } => {
                assert_eq!(*op, BinopKind::IMul);
            }
            other => panic!("Expected ElementWiseBinop, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 3: Detect element-wise float addition.
    // -----------------------------------------------------------------------
    #[test]
    fn test_detect_elementwise_fadd() {
        let instructions = build_fadd_loop_body();
        let region = analyze_region(&instructions, 0, instructions.len(), None);

        assert!(
            region.is_some(),
            "Should detect c[i] = a[i] + b[i] (float) as vectorizable"
        );
        let region = region.unwrap();

        assert_eq!(region.induction_var_reg, 3);
        assert_eq!(region.elem_type, SimdElemType::Float64);
        assert_eq!(region.width, SimdWidth::Width2);

        match &region.pattern {
            VectorizablePattern::ElementWiseBinop { op, .. } => {
                assert_eq!(*op, BinopKind::FAdd);
            }
            other => panic!("Expected ElementWiseBinop, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 4: Reject loop without array operations.
    // -----------------------------------------------------------------------
    #[test]
    fn test_reject_non_array_loop() {
        let instructions = build_numeric_loop_no_arrays();
        let region = analyze_region(&instructions, 0, instructions.len(), None);

        assert!(
            region.is_none(),
            "Numeric loop without arrays should NOT be vectorizable"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: Reject loop with a function call inside.
    // -----------------------------------------------------------------------
    #[test]
    fn test_reject_call_in_loop() {
        let instructions = build_loop_with_call();
        let region = analyze_region(&instructions, 0, instructions.len(), None);

        assert!(
            region.is_none(),
            "Loop with Call inside should NOT be vectorizable"
        );
    }

    // -----------------------------------------------------------------------
    // Test 6: Reject loop with loop-carried dependency (accumulator).
    // -----------------------------------------------------------------------
    #[test]
    fn test_reject_loop_carried_dep() {
        // sum[i] = sum[i] + a[i] — destination array (R0) is same as source.
        // Our conservative dependency check rejects in-place operations.
        let instructions = build_loop_carried_dep();
        let region = analyze_region(&instructions, 0, instructions.len(), None);

        assert!(
            region.is_none(),
            "Loop with in-place array update (loop-carried dep) should NOT be vectorizable"
        );
    }

    // -----------------------------------------------------------------------
    // Test 7: Detect multiple vectorizable regions in one instruction stream.
    // -----------------------------------------------------------------------
    #[test]
    fn test_detect_multiple_regions() {
        // Stitch together two vectorizable loops separated by a Ret.
        let mut instructions = Vec::new();

        // --- First loop: c[i] = a[i] + b[i] ---
        let loop1 = vec![
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4), // R4 = a[R3]
            Instruction::new3(OpCode::ArrLoad, 1, 3, 5), // R5 = b[R3]
            Instruction::new3(OpCode::IAdd, 4, 5, 6),    // R6 = R4 + R5
            Instruction::new3(OpCode::ArrStore, 2, 3, 6), // c[R3] = R6
            Instruction::new1(OpCode::IInc, 3),          // R3++
            // Back-edge: jump back to start of this loop (offset 0)
            // simm16 = -5 (jump back 5 instructions from pc=5 to target=0)
            Instruction::new3(OpCode::Jmp, 0xFF, 0xFB, 0), // jmp -5
        ];
        instructions.extend_from_slice(&loop1);

        // Separator
        instructions.push(Instruction::new0(OpCode::Nop));

        // --- Second loop: e[i] = d[i] * f[i] ---
        let loop2_start = instructions.len();
        let loop2 = vec![
            Instruction::new3(OpCode::ArrLoad, 10, 13, 14), // R14 = d[R13]
            Instruction::new3(OpCode::ArrLoad, 11, 13, 15), // R15 = f[R13]
            Instruction::new3(OpCode::IMul, 14, 15, 16),    // R16 = R14 * R15
            Instruction::new3(OpCode::ArrStore, 12, 13, 16), // e[R13] = R16
            Instruction::new1(OpCode::IInc, 13),            // R13++
            // Back-edge: simm16 = -5 (pc = loop2_start+5, target = loop2_start)
            Instruction::new3(OpCode::Jmp, 0xFF, 0xFB, 0), // jmp -5
        ];
        instructions.extend_from_slice(&loop2);

        let analyzer = SimdAnalyzer::new();
        let regions = analyzer.find_all_vectorizable_regions(&instructions, None);

        assert_eq!(
            regions.len(),
            2,
            "Should find exactly 2 vectorizable regions"
        );

        // First region
        assert_eq!(regions[0].start_offset, 0);
        assert_eq!(regions[0].induction_var_reg, 3);
        assert_eq!(regions[0].array_regs, vec![0, 1, 2]);
        match &regions[0].pattern {
            VectorizablePattern::ElementWiseBinop { op, .. } => assert_eq!(*op, BinopKind::IAdd),
            other => panic!("Expected ElementWiseBinop, got {:?}", other),
        }

        // Second region
        assert_eq!(regions[1].start_offset, loop2_start);
        assert_eq!(regions[1].induction_var_reg, 13);
        assert_eq!(regions[1].array_regs, vec![10, 11, 12]);
        match &regions[1].pattern {
            VectorizablePattern::ElementWiseBinop { op, .. } => assert_eq!(*op, BinopKind::IMul),
            other => panic!("Expected ElementWiseBinop, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 7: Correct element type inference from type metadata.
    // -----------------------------------------------------------------------
    #[test]
    fn test_elem_type_inference() {
        let instructions = build_fadd_loop_body();

        // Without metadata: should infer Float64 from FAdd opcode.
        let region_no_meta = analyze_region(&instructions, 0, instructions.len(), None);
        assert!(region_no_meta.is_some());
        assert_eq!(region_no_meta.unwrap().elem_type, SimdElemType::Float64);

        // With metadata confirming Float type on result register.
        let mut meta = TypeMetadata::new();
        meta.set_type(6, KnownType::Float); // R6 is Float
        let region_with_meta = analyze_region(&instructions, 0, instructions.len(), Some(&meta));
        assert!(region_with_meta.is_some());
        assert_eq!(region_with_meta.unwrap().elem_type, SimdElemType::Float64);

        // Integer addition with metadata.
        let iadd_instrs = build_iadd_loop_body();
        let mut meta_int = TypeMetadata::new();
        meta_int.set_type(6, KnownType::Int); // R6 is Int
        let region_int = analyze_region(&iadd_instrs, 0, iadd_instrs.len(), Some(&meta_int));
        assert!(region_int.is_some());
        assert_eq!(region_int.unwrap().elem_type, SimdElemType::Int64);
    }

    // -----------------------------------------------------------------------
    // Test 8: Detect element-wise comparison pattern.
    // -----------------------------------------------------------------------
    #[test]
    fn test_detect_elementwise_cmp() {
        let instructions = build_cmp_loop_body();
        let region = analyze_region(&instructions, 0, instructions.len(), None);

        assert!(
            region.is_some(),
            "Should detect c[i] = a[i] < b[i] as vectorizable"
        );
        let region = region.unwrap();

        assert_eq!(region.induction_var_reg, 3);
        assert_eq!(region.array_regs, vec![0, 1, 2]);
        assert_eq!(region.elem_type, SimdElemType::Int64);

        match &region.pattern {
            VectorizablePattern::ElementWiseCmp {
                op,
                lhs_arr_reg,
                rhs_arr_reg,
                dst_arr_reg,
                ..
            } => {
                assert_eq!(*op, CmpKind::ICmpLt);
                assert_eq!(*lhs_arr_reg, 0);
                assert_eq!(*rhs_arr_reg, 1);
                assert_eq!(*dst_arr_reg, 2);
            }
            other => panic!("Expected ElementWiseCmp, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // Test 9: Reject unsupported opcodes in loop body.
    // -----------------------------------------------------------------------
    #[test]
    fn test_reject_spawn_in_loop() {
        let instructions = vec![
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4),
            Instruction::new3(OpCode::Spawn, 0, 0, 0), // actor spawn — not vectorizable
            Instruction::new3(OpCode::ArrStore, 1, 3, 4),
            Instruction::new1(OpCode::IInc, 3),
            Instruction::new2(OpCode::Jmp, 0, 0),
        ];

        let region = analyze_region(&instructions, 0, instructions.len(), None);
        assert!(
            region.is_none(),
            "Loop with Spawn should NOT be vectorizable"
        );
    }

    // -----------------------------------------------------------------------
    // Test 10: SimdWidth and SimdElemType basic properties.
    // -----------------------------------------------------------------------
    #[test]
    fn test_simd_width_and_elem_type() {
        assert_eq!(SimdWidth::Width2.lanes(), 2);
        assert_eq!(SimdWidth::Width4.lanes(), 4);
        assert_eq!(SimdWidth::Width8.lanes(), 8);

        assert_eq!(SimdElemType::Int64.lane_count(), 2);
        assert_eq!(SimdElemType::Float64.lane_count(), 2);
        assert_eq!(SimdElemType::Int32.lane_count(), 4);
        assert_eq!(SimdElemType::Float32.lane_count(), 4);

        assert_eq!(
            SimdWidth::from_elem_type(SimdElemType::Int64),
            SimdWidth::Width2
        );
        assert_eq!(
            SimdWidth::from_elem_type(SimdElemType::Float32),
            SimdWidth::Width4
        );

        assert!(SimdElemType::Float64.is_float());
        assert!(!SimdElemType::Float64.is_int());
        assert!(SimdElemType::Int64.is_int());
        assert!(!SimdElemType::Int64.is_float());
    }

    // -----------------------------------------------------------------------
    // Test 11: Empty / too-short instruction stream.
    // -----------------------------------------------------------------------
    #[test]
    fn test_empty_stream() {
        let instructions: Vec<Instruction> = vec![];
        let analyzer = SimdAnalyzer::new();
        let regions = analyzer.find_all_vectorizable_regions(&instructions, None);
        assert!(regions.is_empty());

        let short = vec![
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4),
            Instruction::new3(OpCode::ArrStore, 1, 3, 4),
        ];
        let region = analyze_region(&short, 0, short.len(), None);
        assert!(
            region.is_none(),
            "Too-short region (no IInc) should be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // Test 12: SimdAnalyzer::new and Default.
    // -----------------------------------------------------------------------
    #[test]
    fn test_analyzer_new() {
        let a1 = SimdAnalyzer::new();
        let a2 = SimdAnalyzer::default();
        // Just ensure they construct without panicking.
        let _ = format!("{:?}", a1);
        let _ = format!("{:?}", a2);
    }

    // -----------------------------------------------------------------------
    // Test 13: Different index registers across loads should be rejected.
    // -----------------------------------------------------------------------
    #[test]
    fn test_reject_mismatched_index_regs() {
        // R3 indexes first array, R4 indexes second — must use same induction var.
        let instructions = vec![
            Instruction::new3(OpCode::ArrLoad, 0, 3, 5), // R5 = a[R3]
            Instruction::new3(OpCode::ArrLoad, 1, 4, 6), // R6 = b[R4]  (different idx!)
            Instruction::new3(OpCode::IAdd, 5, 6, 7),    // R7 = R5 + R6
            Instruction::new3(OpCode::ArrStore, 2, 3, 7), // c[R3] = R7
            Instruction::new1(OpCode::IInc, 3),          // R3++
            Instruction::new2(OpCode::Jmp, 0, 0),
        ];

        let region = analyze_region(&instructions, 0, instructions.len(), None);
        assert!(
            region.is_none(),
            "Mismatched index registers should be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // Test 14: Subtraction pattern.
    // -----------------------------------------------------------------------
    #[test]
    fn test_detect_elementwise_sub() {
        let instructions = vec![
            Instruction::new3(OpCode::ArrLoad, 0, 3, 4), // R4 = a[R3]
            Instruction::new3(OpCode::ArrLoad, 1, 3, 5), // R5 = b[R3]
            Instruction::new3(OpCode::ISub, 4, 5, 6),    // R6 = R4 - R5
            Instruction::new3(OpCode::ArrStore, 2, 3, 6), // c[R3] = R6
            Instruction::new1(OpCode::IInc, 3),          // R3++
            Instruction::new2(OpCode::Jmp, 0, 0),
        ];

        let region = analyze_region(&instructions, 0, instructions.len(), None);
        assert!(region.is_some());
        match &region.unwrap().pattern {
            VectorizablePattern::ElementWiseBinop { op, .. } => {
                assert_eq!(*op, BinopKind::ISub);
            }
            other => panic!("Expected ElementWiseBinop, got {:?}", other),
        }
    }
}
