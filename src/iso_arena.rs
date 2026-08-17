//! Wave D4 — iso-erasure bump arenas.
//!
//! # Motivation
//!
//! An `iso` (isolated) object that is provably created and consumed within a
//! single actor message-handler activation — it never escapes — does not need
//! ORCA reference counting at all.  It can be allocated in a per-activation
//! bump arena and reclaimed in O(1) by resetting the arena offset when the
//! handler activation completes.  This integrates *with* ORCA (which remains
//! the collector for cross-message / live data); it does not replace it.
//!
//! # What qualifies (conservative escape analysis)
//!
//! [`qualifying_alloc_sites`] performs a forward may-alias dataflow over a
//! module's flat bytecode for every `ArrAlloc` / `RecMk` / `TupleMk` site.
//! An allocation site qualifies only when the allocated value is provably
//! *dead* (every alias register overwritten or dropped) before any escape
//! point on every control-flow path.  Escape points include:
//!
//! * storing the value into another object (`ArrStore` / `FieldS` / `RecS`
//!   source operand, `CapStore`),
//! * sending it in a message (`Send` / `Ask` / `RSend` / `RAsk` / `Emit`),
//! * storing it into actor state (`StateSet`),
//! * returning it (`Ret` / `RetVal`, falling off the end with the value in
//!   `r0`, `Halt` with the value in `r0`),
//! * passing it to a call (`Call` function/argument registers; *any* live
//!   alias at `ClosureCall` / `TailCall`, which transfer the whole register
//!   file),
//! * crossing an effect `perform` boundary (`Perform` / `PerformDirect` /
//!   `PerformAsync`),
//! * any other opcode not on the verified safe whitelist (conservative
//!   catch-all: FFI, Python interop, `Spawn`, `Receive*`, `SignalWait`,
//!   `Switch`, ...).
//!
//! Reading the value is fine: arithmetic/comparison operands, `ArrLen`,
//! `ArrLoad` / `FieldL` / `RecL` container position, `Move` / `Load` /
//! `Store` / `Dup` / `Swap` (tracked as aliases), `Drop`, and conditional
//! branches.  Storing *into* the candidate object (the container operand of
//! `ArrStore` / `FieldS` / `RecS`) is fine only when the stored register is
//! provably pointer-free: arena container slots are not rc-held (both
//! barriers skip), so a pointer-suspect child — computed by the module-level
//! [`may_ptr_sets`] may-analysis — would dangle once its donor register's
//! rc is released.  This keeps arena containers scalar-only.
//!
//! # Soundness notes
//!
//! * **Capability:** Nulang does not track reference capabilities at runtime
//!   (`MetaCap` is a stub), so the `iso` requirement is discharged by the
//!   escape proof itself: a value that provably never leaves the activation
//!   is indistinguishable from a message-scoped `iso` temporary for
//!   reclamation purposes, regardless of its static capability.
//! * **NaN-boxing:** arena objects are wrapped in `Value::ptr` exactly like
//!   heap objects; every arena object carries a fully initialised
//!   [`OrcaHeader`] so `array_len` / type-tag inspection work unchanged.
//! * **ORCA:** arena objects are *not* linked into the actor heap's live
//!   list, so ORCA rc and cycle detection never trace or free them.
//!   `drop_ref` / `retain_ref` no-op on arena pointers (range check), and
//!   the write barriers skip retain/drop when the *container* is an arena
//!   object (arena slots are never rc-held, and the arena dies wholesale at
//!   activation end).  Under the compiler's existing ownership discipline
//!   (a `Drop` only ever runs on bindings the code owns) this keeps every
//!   heap object's reference count exactly as balanced as the heap path.
//! * **Suspension:** if a handler activation suspends (workflow signal wait,
//!   async effect, JIT safepoint yield) the arena is *not* reset; captured
//!   VM frames may still hold arena pointers.  The reset happens when the
//!   resumed activation finally completes.  Arenas are per actor, so other
//!   actors' activations never share the suspended actor's arena.
//!
//! # Current limitations
//!
//! * Only `ArrAlloc` / `RecMk` / `TupleMk` sites are eligible.  `RecCopy`
//!   results stay on the heap (child retains would otherwise leak), and
//!   internal string allocations (concat, `Int.to_string`, ...) are not yet
//!   routed through the arena.
//! * The analysis is deliberately syntactic/conservative: a live alias at
//!   any non-whitelisted opcode rejects the site.  Broader coverage (type
//!   directed capability proofs, inter-procedural escape) is deferred to
//!   later waves.
//! * VM interpreter only; JIT/AOT keep their existing allocation paths.
//!   The arena is an allocation strategy, not a semantics change, so
//!   cross-backend observable behaviour is identical.
//!
//! The feature is gated by `NULANG_ISO_ARENA=1` (env) / `--iso-arena` (CLI)
//! and is **off by default**.

use crate::bytecode::{CodeModule, Instruction, OpCode};
use crate::runtime::heap::{ActorHeap, OrcaHeader, TypeTag};
use std::collections::HashSet;

/// Default arena block size (bytes).  Blocks are chained on exhaustion and
/// retained across epochs, so a steady-state handler reuses the same memory.
const BLOCK_SIZE: usize = 64 * 1024;

/// Per-actor (per-activation) bump arena for message-scoped allocations.
///
/// Objects are bump-allocated with a standard [`OrcaHeader`] prefix (so
/// `array_len` and tag inspection work) but are *not* linked into any heap
/// live list.  [`IsoArena::reset`] reclaims the whole epoch in O(1).
#[derive(Debug)]
pub struct IsoArena {
    /// Backing blocks.  `Box<[u64]>` guarantees 8-byte alignment for the
    /// embedded headers and keeps block addresses stable as the Vec grows.
    blocks: Vec<Box<[u64]>>,
    /// Index of the block currently being bumped from.
    block_idx: usize,
    /// Byte offset into the current block.
    offset: usize,
    /// Allocations served in the current epoch (since last reset).
    epoch_allocs: usize,
    /// Total allocations served over the arena's lifetime.
    total_allocs: usize,
    /// Number of epoch resets.
    resets: usize,
}

impl Default for IsoArena {
    fn default() -> Self {
        Self::new()
    }
}

impl IsoArena {
    pub fn new() -> Self {
        IsoArena {
            blocks: Vec::new(),
            block_idx: 0,
            offset: 0,
            epoch_allocs: 0,
            total_allocs: 0,
            resets: 0,
        }
    }

    fn block_cap(&self, idx: usize) -> usize {
        self.blocks[idx].len() * std::mem::size_of::<u64>()
    }

    /// Allocate `payload_size` bytes in the arena, returning a pointer to
    /// the payload (just past a freshly written [`OrcaHeader`]).
    ///
    /// Returns `None` only on usize overflow of the size computation; block
    /// growth is unbounded (a fresh block is chained when needed).
    pub fn alloc(&mut self, payload_size: usize, type_tag: TypeTag) -> Option<*mut u8> {
        let aligned = payload_size.checked_add(7)? & !7;
        let total = ActorHeap::HEADER_SIZE.checked_add(aligned)?;

        // Find a block with room, chaining a fresh one when necessary.
        loop {
            if self.block_idx == self.blocks.len() {
                let cap = BLOCK_SIZE.max(total);
                self.blocks
                    .push(vec![0u64; cap / std::mem::size_of::<u64>()].into_boxed_slice());
                self.offset = 0;
            }
            if self.offset + total <= self.block_cap(self.block_idx) {
                break;
            }
            // Current block is full (or a retained undersized block); move
            // to the next one.  Stranded space is reclaimed at the next
            // reset, when allocation restarts from block 0.
            self.block_idx += 1;
            self.offset = 0;
        }

        let base = self.blocks[self.block_idx].as_mut_ptr() as *mut u8;
        debug_assert!(base as usize % 8 == 0);
        let header_ptr = unsafe { base.add(self.offset) } as *mut OrcaHeader;
        let payload = unsafe { base.add(self.offset + ActorHeap::HEADER_SIZE) };
        unsafe {
            // SAFETY: header_ptr points into our live block with `total`
            // bytes of room; OrcaHeader fits in the first HEADER_SIZE bytes.
            std::ptr::write(header_ptr, OrcaHeader::new(0, type_tag, total, payload_size));
        }
        self.offset += total;
        self.epoch_allocs += 1;
        self.total_allocs += 1;
        debug_assert!(self.contains(payload));
        debug_assert!(payload as usize % 8 == 0);
        Some(payload)
    }

    /// True when `ptr` lies inside any arena block (including space from
    /// previous, already-reset epochs — such pointers are dead anyway, and
    /// treating them as arena-owned keeps stale rc traffic harmless).
    pub fn contains(&self, ptr: *const u8) -> bool {
        if ptr.is_null() {
            return false;
        }
        let p = ptr as usize;
        self.blocks.iter().any(|b| {
            let start = b.as_ptr() as usize;
            p >= start && p < start + b.len() * std::mem::size_of::<u64>()
        })
    }

    /// Reclaim the whole epoch in O(1): restart bumping from block 0.
    /// Block memory is retained for reuse by the next epoch.
    pub fn reset(&mut self) {
        self.block_idx = 0;
        self.offset = 0;
        self.epoch_allocs = 0;
        self.resets += 1;
    }

    /// Allocations served in the current epoch.
    pub fn epoch_allocs(&self) -> usize {
        self.epoch_allocs
    }

    /// Total allocations served (lifetime).
    pub fn total_allocs(&self) -> usize {
        self.total_allocs
    }

    /// Number of epoch resets performed.
    pub fn resets(&self) -> usize {
        self.resets
    }
}

// ---------------------------------------------------------------------------
// Conservative escape analysis
// ---------------------------------------------------------------------------

/// How an instruction interacts with the alias set of one candidate
/// allocation during the forward dataflow.
enum Transfer {
    /// ` regs[src] -> regs[dst] ` copy; alias propagates.
    Copy { src: usize, dst: usize },
    /// Swap the two registers' contents.
    Swap { a: usize, b: usize },
    /// Reads allowed; writes `kill` (removing any alias there). No escape.
    Safe { kill: Option<usize> },
    /// Container store: if the *stored* register aliases the candidate the
    /// value escapes into another object; if the *container* operand aliases
    /// the candidate the stored register must be provably pointer-free
    /// (arena container slots are not rc-held).
    StoreEscape { container: usize, src: usize },
    /// Reads allowed; writes nothing. No escape.
    Pure,
    /// Instruction ends the activation/path; escapes iff `reg` aliases.
    Terminal { reg: Option<usize> },
    /// Not on the verified whitelist: if *any* alias is live here, reject.
    EscapeAll,
    /// `Call`: escapes through the function register and the staged
    /// argument registers `r0..r(argc)`; the callee cannot touch any other
    /// caller register (a fresh frame copies only `r0..argc`).  The return
    /// write to `dst` kills an alias there.
    Call { func: usize, argc: usize, dst: usize },
}

fn classify(instr: &Instruction) -> Transfer {
    use OpCode::*;
    match instr.opcode {
        Load | Store | Move | Dup => Transfer::Copy {
            src: instr.op1 as usize,
            dst: instr.op2 as usize,
        },
        Swap => Transfer::Swap {
            a: instr.op1 as usize,
            b: instr.op2 as usize,
        },
        Const0 | Const1 | Const2 | ConstM1 => Transfer::Safe {
            kill: Some(instr.op1 as usize),
        },
        ConstU | ConstL => Transfer::Safe {
            kill: Some(instr.op3 as usize),
        },
        // Binary arithmetic / comparison / logic: read op1, op2; write op3.
        IAdd | ISub | IMul | IDiv | IMod | IPow | Xor | Shl | Shr | BitAnd | BitOr | FAdd
        | FSub | FMul | FDiv | FMod | FPow | ICmpEq | ICmpLt | ICmpGt | ICmpLe | ICmpGe
        | FCmpEq | FCmpLt | FCmpGt | SCmpEq | And | Or => Transfer::Safe {
            kill: Some(instr.op3 as usize),
        },
        // Unary ops never escape their operand.  We conservatively kill
        // nothing (an in-place rewrite keeps the alias tracked), which can
        // only reject, never wrongly qualify.
        INeg | FNeg | IToF | FToI | FToS | Not | IsTag | IInc | IDec => Transfer::Pure,
        ArrLen => Transfer::Safe {
            kill: Some(instr.op2 as usize),
        },
        // Loads: reading the container is fine.  Arena containers only ever
        // hold heap pointers or scalars (storing an alias is rejected), so a
        // loaded register never becomes an untracked arena alias.
        ArrLoad | FieldL | RecL => Transfer::Safe {
            kill: Some(instr.op3 as usize),
        },
        ArrStore | FieldS | RecS => Transfer::StoreEscape {
            container: instr.op1 as usize,
            src: instr.op3 as usize,
        },
        // Fresh allocation sites overwrite their destination register.
        ArrAlloc | RecMk | TupleMk => Transfer::Safe {
            kill: Some(instr.op2 as usize),
        },
        JmpT | JmpF | Jmp | Nop => Transfer::Pure,
        Drop => Transfer::Safe {
            kill: Some(instr.op1 as usize),
        },
        Halt | Ret => Transfer::Terminal { reg: Some(0) },
        RetVal => Transfer::Terminal {
            reg: Some(instr.op1 as usize),
        },
        Panic => Transfer::Terminal { reg: None },
        Call => Transfer::Call {
            func: instr.op1 as usize,
            argc: instr.op2 as usize,
            dst: instr.op3 as usize,
        },
        _ => Transfer::EscapeAll,
    }
}

/// Control-flow successors of instruction `i`.  Returns `None` when a jump
/// target is out of range (malformed module — caller rejects the site).
fn successors(instrs: &[Instruction], i: usize) -> Option<Vec<usize>> {
    let n = instrs.len();
    let ins = &instrs[i];
    let fallthrough = if i + 1 < n { Some(i + 1) } else { None };
    match ins.opcode {
        OpCode::Jmp => {
            let t = i as i64 + ins.simm16() as i64;
            if (0..n as i64).contains(&t) {
                Some(vec![t as usize])
            } else {
                None
            }
        }
        OpCode::JmpT | OpCode::JmpF => {
            let t = i as i64 + ins.offset16() as i64;
            if !(0..=n as i64).contains(&t) {
                return None;
            }
            let mut v = Vec::with_capacity(2);
            if let Some(f) = fallthrough {
                v.push(f);
            }
            if (t as usize) < n {
                v.push(t as usize);
            }
            Some(v)
        }
        OpCode::Halt | OpCode::Ret | OpCode::RetVal | OpCode::Panic => Some(Vec::new()),
        _ => Some(fallthrough.into_iter().collect()),
    }
}

// ---------------------------------------------------------------------------
// May-hold-pointer analysis (module level, fixpoint over the flat stream)
// ---------------------------------------------------------------------------

/// Per-instruction in-set of registers that may hold a pointer value (heap
/// *or* arena — the analysis deliberately does not distinguish).  Used to
/// keep arena containers pointer-free: an arena container's slots are never
/// rc-held (the store barriers skip retain/drop), so storing a heap pointer
/// into one would dangle as soon as the donor register's rc is released.
///
/// 256 registers => four u64 words per instruction.
type RegSet = [u64; 4];

fn regset_insert(s: &mut RegSet, r: u8) {
    s[(r / 64) as usize] |= 1u64 << (r % 64);
}

fn regset_remove(s: &mut RegSet, r: u8) {
    s[(r / 64) as usize] &= !(1u64 << (r % 64));
}

fn regset_contains(s: &RegSet, r: u8) -> bool {
    s[(r / 64) as usize] & (1u64 << (r % 64)) != 0
}

/// Forward may-analysis: at each instruction, which registers may hold a
/// pointer.  Soundly over-approximates:
///  - every allocation / load / call result is pointer-suspect;
///  - string constants are pointers, scalar constants are not;
///  - function-entry registers are all pointer-suspect (unknown args);
///  - any opcode not explicitly modeled taints all three operand registers.
fn may_ptr_sets(module: &CodeModule) -> Vec<RegSet> {
    use OpCode::*;
    let instrs = &module.instructions;
    let n = instrs.len();
    let mut in_sets: Vec<RegSet> = vec![[0; 4]; n];
    let mut worklist: Vec<usize> = Vec::new();

    // Entry points: module entry takes no arguments; every function-table
    // target conservatively receives unknown (pointer-suspect) arguments.
    let entry = module.entry_point.unwrap_or(0);
    if entry < n {
        worklist.push(entry);
    }
    for &off in &module.function_table {
        let off = off as usize;
        if off < n {
            for r in 0..=255u8 {
                regset_insert(&mut in_sets[off], r);
            }
            worklist.push(off);
        }
    }

    while let Some(i) = worklist.pop() {
        let instr = &instrs[i];
        let set = in_sets[i];
        let mut out = set;
        match instr.opcode {
            Load | Store | Move | Dup => {
                if regset_contains(&set, instr.op1) {
                    regset_insert(&mut out, instr.op2);
                } else {
                    regset_remove(&mut out, instr.op2);
                }
            }
            Swap => {
                let (a, b) = (regset_contains(&set, instr.op1), regset_contains(&set, instr.op2));
                if a != b {
                    if a {
                        regset_remove(&mut out, instr.op1);
                        regset_insert(&mut out, instr.op2);
                    } else {
                        regset_remove(&mut out, instr.op2);
                        regset_insert(&mut out, instr.op1);
                    }
                }
            }
            Const0 | Const1 | Const2 | ConstM1 => regset_remove(&mut out, instr.op1),
            ConstU | ConstL => {
                let ptr_const = matches!(
                    module.constants.get(instr.imm16() as usize),
                    Some(crate::bytecode::Constant::String(_))
                        | Some(crate::bytecode::Constant::TypeDescriptor(_))
                        | Some(crate::bytecode::Constant::FunctionRef(_))
                        | Some(crate::bytecode::Constant::BehaviorRef(_))
                );
                if ptr_const {
                    regset_insert(&mut out, instr.op3);
                } else {
                    regset_remove(&mut out, instr.op3);
                }
            }
            IAdd | ISub | IMul | IDiv | IMod | IPow | Xor | Shl | Shr | BitAnd | BitOr | FAdd
            | FSub | FMul | FDiv | FMod | FPow | ICmpEq | ICmpLt | ICmpGt | ICmpLe | ICmpGe
            | FCmpEq | FCmpLt | FCmpGt | SCmpEq | And | Or => regset_remove(&mut out, instr.op3),
            FToS => regset_insert(&mut out, instr.op1),
            ArrLen => regset_remove(&mut out, instr.op2),
            ArrLoad | FieldL | RecL => regset_insert(&mut out, instr.op3),
            ArrAlloc | RecMk | TupleMk => regset_insert(&mut out, instr.op2),
            Drop => regset_remove(&mut out, instr.op1),
            ArrStore | FieldS | RecS | JmpT | JmpF | Jmp | Nop => {}
            Halt | Ret | RetVal | Panic => continue,
            Call => regset_insert(&mut out, instr.op3),
            // Unmodeled opcodes may write any operand register: taint all.
            _ => {
                regset_insert(&mut out, instr.op1);
                regset_insert(&mut out, instr.op2);
                regset_insert(&mut out, instr.op3);
            }
        }
        let Some(succs) = successors(instrs, i) else {
            continue; // malformed target: no propagation (sites reject anyway)
        };
        for s in succs {
            let mut changed = false;
            for w in 0..4 {
                let merged = in_sets[s][w] | out[w];
                if merged != in_sets[s][w] {
                    in_sets[s][w] = merged;
                    changed = true;
                }
            }
            if changed {
                worklist.push(s);
            }
        }
    }
    in_sets
}

/// Dataflow for a single allocation site: returns true when the value
/// allocated at `pc` (destination register `dst`) is provably dead before
/// any escape point on every path.
///
/// `may_ptr` is the module-level pointer-suspect analysis: when the
/// candidate is the *container* operand of a store, the stored register
/// must be provably pointer-free, because arena containers' slots are not
/// rc-held (a heap child would dangle once its donor register drops).
fn site_qualifies(instrs: &[Instruction], may_ptr: &[RegSet], pc: usize, dst: u8) -> bool {
    let n = instrs.len();
    let mut in_sets: Vec<HashSet<u8>> = vec![HashSet::new(); n];
    let mut worklist: Vec<usize> = Vec::new();
    if pc + 1 < n {
        in_sets[pc + 1].insert(dst);
        worklist.push(pc + 1);
    } else {
        // Allocation is the last instruction: control falls off the end,
        // which returns r0.  Qualifies only if the value isn't in r0.
        return dst != 0;
    }

    while let Some(i) = worklist.pop() {
        let set = in_sets[i].clone();
        if set.is_empty() {
            continue;
        }
        let instr = &instrs[i];
        let mut out = set.clone();
        match classify(instr) {
            Transfer::Copy { src, dst: d } => {
                if set.contains(&(src as u8)) {
                    out.insert(d as u8);
                } else {
                    out.remove(&(d as u8));
                }
            }
            Transfer::Swap { a, b } => {
                let (a, b) = (a as u8, b as u8);
                match (set.contains(&a), set.contains(&b)) {
                    (true, false) => {
                        out.remove(&a);
                        out.insert(b);
                    }
                    (false, true) => {
                        out.remove(&b);
                        out.insert(a);
                    }
                    _ => {}
                }
            }
            Transfer::Safe { kill } => {
                if let Some(k) = kill {
                    out.remove(&(k as u8));
                }
            }
            Transfer::StoreEscape { container, src } => {
                if set.contains(&(src as u8)) {
                    return false; // stored into another object
                }
                if set.contains(&(container as u8))
                    && regset_contains(&may_ptr[i], src as u8)
                {
                    // Arena container receiving a pointer-suspect child: the
                    // child's rc would never be retained for this slot, so
                    // the slot could dangle.  Keep the container on the heap.
                    return false;
                }
            }
            Transfer::Pure => {}
            Transfer::Terminal { reg } => {
                if let Some(r) = reg {
                    if set.contains(&(r as u8)) {
                        return false; // returned / observed after activation
                    }
                }
                continue; // no successors
            }
            Transfer::EscapeAll => {
                // Any live alias at an unverified opcode: reject.  This
                // covers message sends, actor-state stores, effect performs,
                // closure capture, FFI, suspension points, etc.
                return false;
            }
            Transfer::Call { func, argc, dst: d } => {
                if set.contains(&(func as u8)) {
                    return false;
                }
                for r in 0..argc.min(256) {
                    if set.contains(&(r as u8)) {
                        return false; // passed to another function
                    }
                }
                out.remove(&(d as u8));
            }
        }
        let Some(succs) = successors(instrs, i) else {
            return false; // malformed jump target: not provable
        };
        for s in succs {
            let before = in_sets[s].len();
            in_sets[s].extend(out.iter().copied());
            if in_sets[s].len() != before {
                worklist.push(s);
            }
        }
    }
    true
}

/// Compute the set of instruction PCs in `module` whose allocation result
/// is provably activation-local and may therefore be served from the
/// iso arena.  Only `ArrAlloc` / `RecMk` / `TupleMk` sites are considered
/// (`RecCopy` keeps heap allocation: its child retains would otherwise leak,
/// and internal string allocations are not yet routed through the arena).
pub fn qualifying_alloc_sites(module: &CodeModule) -> HashSet<usize> {
    let instrs = &module.instructions;
    let may_ptr = may_ptr_sets(module);
    let mut out = HashSet::new();
    for (pc, ins) in instrs.iter().enumerate() {
        let dst = match ins.opcode {
            OpCode::ArrAlloc | OpCode::RecMk | OpCode::TupleMk => ins.op2,
            _ => continue,
        };
        if site_qualifies(instrs, &may_ptr, pc, dst) {
            out.insert(pc);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{CodeModule, Instruction};

    fn arr_alloc(len_reg: u8, dst: u8) -> Instruction {
        Instruction::new2(OpCode::ArrAlloc, len_reg, dst)
    }

    fn send_module(instrs: Vec<Instruction>, _param_count: usize) -> CodeModule {
        let mut m = CodeModule::new("test");
        for i in instrs {
            m.emit(i);
        }
        m
    }

    /// JmpT/JmpF with a relative offset, using the VM's offset16 (op2,op3)
    /// encoding: target = pc + offset.
    fn jmp_t(cond: u8, offset: i16) -> Instruction {
        Instruction {
            opcode: OpCode::JmpT,
            op1: cond,
            op2: ((offset as u16) >> 8) as u8,
            op3: ((offset as u16) & 0xff) as u8,
        }
    }

    fn jmp_f(cond: u8, offset: i16) -> Instruction {
        Instruction {
            opcode: OpCode::JmpF,
            op1: cond,
            op2: ((offset as u16) >> 8) as u8,
            op3: ((offset as u16) & 0xff) as u8,
        }
    }

    // -- Arena mechanics ----------------------------------------------------

    #[test]
    fn arena_alloc_write_header_and_len() {
        let mut a = IsoArena::new();
        let p = a
            .alloc(3 * std::mem::size_of::<u64>(), TypeTag::Array)
            .unwrap();
        assert!(a.contains(p));
        assert_eq!(p as usize % 8, 0);
        unsafe {
            let h = &*ActorHeap::header_of(p);
            assert_eq!(h.type_tag, TypeTag::Array);
            assert_eq!(
                h.size.saturating_sub(ActorHeap::HEADER_SIZE)
                    / std::mem::size_of::<u64>(),
                3
            );
        }
    }

    #[test]
    fn arena_reset_reclaims_and_reuses() {
        let mut a = IsoArena::new();
        let p1 = a.alloc(64, TypeTag::Array).unwrap();
        assert_eq!(a.epoch_allocs(), 1);
        a.reset();
        assert_eq!(a.resets(), 1);
        assert_eq!(a.epoch_allocs(), 0);
        let p2 = a.alloc(64, TypeTag::Array).unwrap();
        // First allocation of the new epoch reuses the start of block 0.
        assert_eq!(p1, p2);
        assert_eq!(a.total_allocs(), 2);
    }

    #[test]
    fn arena_grows_beyond_block_size() {
        let mut a = IsoArena::new();
        // Larger than one default block: chains dedicated blocks.
        let big = a.alloc(BLOCK_SIZE * 3, TypeTag::Array).unwrap();
        assert!(a.contains(big));
        a.reset();
        let big2 = a.alloc(BLOCK_SIZE * 3, TypeTag::Array).unwrap();
        assert!(a.contains(big2));
    }

    #[test]
    fn arena_contains_rejects_foreign() {
        let mut a = IsoArena::new();
        let _ = a.alloc(8, TypeTag::Array).unwrap();
        let mut stack_byte = 0u8;
        assert!(!a.contains((&mut stack_byte) as *mut u8));
        assert!(!a.contains(std::ptr::null()));
    }

    // -- Escape analysis ----------------------------------------------------

    #[test]
    fn qualifies_when_dropped_before_halt() {
        // r1 = array(r0-len); read its length; drop it; halt. r0 is an int.
        let m = send_module(
            vec![
                arr_alloc(0, 1),            // 0: r1 = array
                Instruction::new2(OpCode::ArrLen, 1, 2), // 1: r2 = len(r1)
                Instruction::new1(OpCode::Drop, 1),      // 2: drop r1
                Instruction::new0(OpCode::Halt),         // 3
            ],
            0,
        );
        let q = qualifying_alloc_sites(&m);
        assert!(q.contains(&0), "expected site 0 to qualify");
    }

    #[test]
    fn qualifies_when_overwritten_before_use() {
        let m = send_module(
            vec![
                arr_alloc(0, 1),
                Instruction::new1(OpCode::Const0, 1), // overwrite r1
                Instruction::new0(OpCode::RetVal),    // returns r0 (op1=0)
            ],
            0,
        );
        assert!(qualifying_alloc_sites(&m).contains(&0));
    }

    #[test]
    fn rejects_return() {
        let m = send_module(
            vec![arr_alloc(0, 1), Instruction::new1(OpCode::RetVal, 1)],
            0,
        );
        assert!(qualifying_alloc_sites(&m).is_empty());
    }

    #[test]
    fn rejects_halt_in_r0() {
        let m = send_module(vec![arr_alloc(0, 0), Instruction::new0(OpCode::Halt)], 0);
        assert!(qualifying_alloc_sites(&m).is_empty());
    }

    #[test]
    fn rejects_send() {
        // r1 = array; move to r0 (staged send arg); send.
        let m = send_module(
            vec![
                arr_alloc(0, 1),
                Instruction::new2(OpCode::Move, 1, 0),
                Instruction::new3(OpCode::Send, 2, 0, 0),
                Instruction::new0(OpCode::Halt),
            ],
            1,
        );
        assert!(qualifying_alloc_sites(&m).is_empty());
    }

    #[test]
    fn rejects_state_set() {
        let m = send_module(
            vec![
                arr_alloc(0, 1),
                Instruction::new3(OpCode::StateSet, 0, 0, 1),
                Instruction::new0(OpCode::Halt),
            ],
            0,
        );
        assert!(qualifying_alloc_sites(&m).is_empty());
    }

    #[test]
    fn rejects_store_into_container() {
        // r2 (array) stored into r1 (array) — r2 escapes into r1.
        let m = send_module(
            vec![
                arr_alloc(0, 1),
                arr_alloc(0, 2),
                Instruction::new3(OpCode::ArrStore, 1, 0, 2),
                Instruction::new1(OpCode::Drop, 1),
                Instruction::new0(OpCode::Halt),
            ],
            0,
        );
        let q = qualifying_alloc_sites(&m);
        // r2 escapes into r1, so its site stays on the heap.  r1 must also
        // stay on the heap: arena container slots are not rc-held, so an
        // arena r1 would dangle its (heap) child once r2's register drops.
        assert!(!q.contains(&1), "storing site must not qualify");
        assert!(
            !q.contains(&0),
            "container receiving a pointer-suspect child must not qualify"
        );
    }

    #[test]
    fn rejects_nested_array_literal_container() {
        // Regression for the differential-fuzz divergence on
        // `let m = [[1, 2], [3]]; m[0][1] + m[1][0]` (interpreter returned 2,
        // correct is 5): the outer array qualified for the arena while its
        // inner-array children stayed on the heap with their retains skipped,
        // so ORCA freed a child before the reads.
        let m = send_module(
            vec![
                arr_alloc(2, 1),                        // r1 = inner [..]
                arr_alloc(1, 2),                        // r2 = inner [..]
                arr_alloc(2, 3),                        // r3 = outer
                Instruction::new3(OpCode::ArrStore, 3, 0, 1), // outer[0] = r1
                Instruction::new3(OpCode::ArrStore, 3, 0, 2), // outer[1] = r2
                Instruction::new3(OpCode::ArrLoad, 3, 0, 4),  // r4 = outer[0]
                Instruction::new3(OpCode::ArrLoad, 4, 1, 5),  // r5 = r4[1]
                Instruction::new1(OpCode::Drop, 3),
                Instruction::new1(OpCode::Drop, 4),
                Instruction::new1(OpCode::Drop, 5),
                Instruction::new0(OpCode::Halt),
            ],
            0,
        );
        let q = qualifying_alloc_sites(&m);
        assert!(
            !q.contains(&2),
            "outer container with pointer-suspect children must stay on the heap"
        );
        assert!(!q.contains(&0) && !q.contains(&1), "stored children escape");
    }

    #[test]
    fn qualifies_container_storing_scalars() {
        // The target pattern: a scratch array built from scalar constants
        // and consumed within the activation.  Scalar stores are rc-no-ops,
        // so the arena container is sound.
        let mut m = CodeModule::new("test");
        m.constants.push(crate::bytecode::Constant::Int(7));
        m.emit(arr_alloc(0, 1)); // r1 = array(..)
        m.emit(Instruction::new3(OpCode::ConstU, 0, 0, 2)); // r2 = const int 7
        m.emit(Instruction::new3(OpCode::ArrStore, 1, 0, 2)); // r1[0] = 7
        m.emit(Instruction::new3(OpCode::ArrLoad, 1, 0, 3)); // r3 = r1[0]
        m.emit(Instruction::new1(OpCode::Drop, 1));
        m.emit(Instruction::new1(OpCode::Drop, 3));
        m.emit(Instruction::new0(OpCode::Halt));
        assert!(
            qualifying_alloc_sites(&m).contains(&0),
            "scalar-only scratch array should still qualify"
        );
    }

    #[test]
    fn rejects_alias_in_call_args() {
        let m = send_module(
            vec![
                arr_alloc(0, 1),
                Instruction::new2(OpCode::Move, 1, 0), // stage into r0
                Instruction::new3(OpCode::Call, 5, 1, 3), // call f(r0)
                Instruction::new0(OpCode::Halt),
            ],
            0,
        );
        assert!(qualifying_alloc_sites(&m).is_empty());
    }

    #[test]
    fn qualifies_when_alias_not_passed_to_call() {
        // Scratch array in r7 stays live across a 1-arg call in r0: the
        // callee cannot touch r7.
        let m = send_module(
            vec![
                arr_alloc(0, 7),
                Instruction::new1(OpCode::Const0, 0),
                Instruction::new3(OpCode::Call, 5, 1, 3),
                Instruction::new1(OpCode::Drop, 7),
                Instruction::new0(OpCode::Halt),
            ],
            0,
        );
        assert!(qualifying_alloc_sites(&m).contains(&0));
    }

    #[test]
    fn rejects_closure_call_while_live() {
        let m = send_module(
            vec![
                arr_alloc(0, 7),
                Instruction::new3(OpCode::ClosureCall, 5, 0, 3),
                Instruction::new1(OpCode::Drop, 7),
                Instruction::new0(OpCode::Halt),
            ],
            0,
        );
        assert!(qualifying_alloc_sites(&m).is_empty());
    }

    #[test]
    fn rejects_perform_while_live() {
        let m = send_module(
            vec![
                arr_alloc(0, 7),
                Instruction::new3(OpCode::Perform, 0, 0, 1),
                Instruction::new1(OpCode::Drop, 7),
                Instruction::new0(OpCode::Halt),
            ],
            0,
        );
        assert!(qualifying_alloc_sites(&m).is_empty());
    }

    #[test]
    fn rejects_escape_inside_loop() {
        // Loop body returns the scratch array on one branch.
        let m = send_module(
            vec![
                arr_alloc(0, 1),                         // 0
                Instruction::new1(OpCode::Const1, 2),    // 1: r2 = true
                jmp_f(2, 2),                           // 2: if !r2 jmp to 4
                Instruction::new1(OpCode::RetVal, 1),    // 3: return r1 (escape)
                Instruction::new1(OpCode::Drop, 1),      // 4
                Instruction::new0(OpCode::Halt),         // 5
            ],
            0,
        );
        assert!(qualifying_alloc_sites(&m).is_empty());
    }

    #[test]
    fn qualifies_across_backward_loop_when_dead() {
        // Scratch array read inside a loop, dropped after; no escapes.
        let m = send_module(
            vec![
                arr_alloc(0, 1),                         // 0: r1 = array
                Instruction::new1(OpCode::Const0, 2),    // 1: r2 = 0 (counter)
                Instruction::new2(OpCode::ArrLen, 1, 3), // 2: r3 = len(r1)
                Instruction::new1(OpCode::IInc, 2),      // 3: r2++
                Instruction::new3(OpCode::ICmpLt, 2, 3, 4), // 4: r4 = r2<r3
                jmp_t(4, -3),                          // 5: if r4 jmp to 2
                Instruction::new1(OpCode::Drop, 1),      // 6
                Instruction::new0(OpCode::Halt),         // 7
            ],
            0,
        );
        assert!(qualifying_alloc_sites(&m).contains(&0));
    }
}
