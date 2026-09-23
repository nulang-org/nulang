//! Concurrency Intermediate Representation (CIR) for WasmFX backend.
//!
//! CIR is a structured CFG extracted from MIR that makes suspension boundaries
//! explicit. Only functions containing at least one suspension point get a CIR
//! representation; non-suspending functions stay on the existing `mir_wasm.rs`
//! path.
//!
//! All types are gated behind `#[cfg(feature = "wasmfx-backend")]`.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VarId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

// ---------------------------------------------------------------------------
// CIR Function
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CirFunction {
    pub name: String,
    pub locals: Vec<CirLocal>,
    pub blocks: Vec<CirBlock>,
    pub entry_block: BlockId,
}

#[derive(Debug, Clone)]
pub struct CirLocal {
    pub id: VarId,
}

// ---------------------------------------------------------------------------
// CIR Block
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CirBlock {
    pub id: BlockId,
    pub stmts: Vec<CirStmt>,
    pub terminator: CirTerminator,
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum CirStmt {
    /// Pure computation (no side effects): dst = src
    Assign { dst: VarId, src: CirExpr },
    /// Fire-and-forget effect (non-suspending): ActorSend, IO.print, log
    Emit {
        effect: EffectKind,
        args: Vec<CirExpr>,
    },
    /// Allocate continuation frame for upcoming SuspendAndYield.
    /// Emitted at the end of the block before SuspendAndYield.
    SaveFrame {
        vars: Vec<VarId>,
        offsets: Vec<usize>,
        frame_ptr: VarId,
    },
    /// Restore variables from frame on resume path.
    /// Emitted at the top of the resume block.
    RestoreFrame {
        vars: Vec<VarId>,
        offsets: Vec<usize>,
        frame_ptr: VarId,
    },
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum CirExpr {
    Var(VarId),
    ConstI64(i64),
    ConstF64(f64),
    ConstBool(bool),
    ConstNil,
    ConstUnit,
    /// String constant; interned into the data segment at codegen time.
    /// The Wasm value is `TAG_STRING | offset`.
    ConstString(String),
    BinaryOp {
        op: BinaryOp,
        lhs: Box<CirExpr>,
        rhs: Box<CirExpr>,
    },
    UnaryOp {
        op: UnaryOp,
        operand: Box<CirExpr>,
    },
    Call {
        func_idx: u32,
        args: Vec<CirExpr>,
    },
    ArrayLen {
        arr: Box<CirExpr>,
    },
    ArrayLoad {
        arr: Box<CirExpr>,
        idx: Box<CirExpr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

// ---------------------------------------------------------------------------
// Terminators
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum CirTerminator {
    Return(Option<CirExpr>),
    Jump(BlockId),
    Branch {
        cond: CirExpr,
        then_block: BlockId,
        else_block: BlockId,
    },
    /// Yield to host via WasmFX suspend. Resumes at resume_block with
    /// resume_var holding the host-provided result (tagged i64).
    SuspendAndYield {
        effect: EffectKind,
        args: Vec<CirExpr>,
        resume_block: BlockId,
        resume_var: VarId,
        live_vars: Vec<VarId>,
    },
    /// User-defined effect handler resume (Terminator::Resume in MIR).
    Resume(CirExpr),
}

// ---------------------------------------------------------------------------
// Effect kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectKind {
    /// Non-blocking send to actor mailbox: RValue::Send
    ActorSend,
    /// Mailbox host boundary used by ReceiveWait and, in the current WasmFX
    /// restricted profile, legacy ReceiveMatch lowering.
    MailboxDequeue,
    /// RValue::SignalWait
    SignalWait,
    /// RValue::Perform with effect "LLM.ask"
    LlmAsk,
    /// Stmt::PerformAsync — async effect emission
    PerformAsync,
    /// Built-in or user-defined effect dispatched to host
    HostEffect { module: String, name: String },
}

// ---------------------------------------------------------------------------
// Frame layout (shared between cir_frame and cir_analysis)
// ---------------------------------------------------------------------------

/// Layout of a continuation frame in Wasm linear memory.
///
/// ```text
/// Offset 0..4:   state_id (u32) — BlockId of the resume block
/// Offset 4..8:   frame_size (u32) — total frame size in bytes
/// Offset 8..12:  parent_ptr (u32) — pointer to parent frame (0 if none)
/// Offset 12..16: [padding to 8-byte boundary]
/// Offset 16..:   live_vars[0] (i64), live_vars[1] (i64), ...
/// ```
pub struct FrameLayout {
    pub total_size: usize,
    /// Offset for each live variable in the frame.
    pub var_offsets: HashMap<VarId, usize>,
}

/// Fixed header size before live variables (16 bytes with alignment padding).
pub const FRAME_HEADER_SIZE: usize = 16;

/// Compute frame layout for a set of live variables.
///
/// All values are i64 (8 bytes, 8-byte aligned). The header is 16 bytes
/// (12-byte real header + 4 bytes padding to align variables at 8 bytes).
pub fn compute_frame_layout(live_vars: &[VarId]) -> FrameLayout {
    let mut var_offsets = HashMap::new();
    for (i, &var) in live_vars.iter().enumerate() {
        var_offsets.insert(var, FRAME_HEADER_SIZE + i * 8);
    }
    FrameLayout {
        total_size: FRAME_HEADER_SIZE + live_vars.len() * 8,
        var_offsets,
    }
}
