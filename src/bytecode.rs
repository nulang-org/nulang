//! Bytecode ISA, instruction encoding, and module format for the Nulang VM.

use crate::tool_schema::ToolSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Opcodes (137 total across 17 categories)
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum OpCode {
    // == Special (0x00-0x0F) ==
    Nop = 0x00,     // No operation
    Halt = 0x01,    // Stop execution
    Panic = 0x02,   // Runtime panic with message from const pool
    Const0 = 0x03,  // Load constant 0 (small int optimization)
    Const1 = 0x04,  // Load constant 1
    Const2 = 0x05,  // Load constant 2
    ConstM1 = 0x06, // Load constant -1
    ConstU = 0x07,  // Load constant from pool (idx: u16)
    ConstL = 0x08,  // Load large constant from pool (idx: u32)

    // == Stack & Locals (0x10-0x1F) ==
    Load = 0x10,  // Load from local register (src_reg, dst_reg, _)
    Store = 0x11, // Store to local register (src_reg, dst_reg, _)
    Move = 0x12,  // Register to register copy (src, dst, _)
    Pop = 0x13,   // Pop top of call stack into register
    Dup = 0x14,   // Duplicate register value
    Swap = 0x15,  // Swap two registers

    // == Arithmetic - Integer (0x20-0x2F) ==
    IAdd = 0x20,   // Integer add (r1, r2, dst)
    ISub = 0x21,   // Integer sub
    IMul = 0x22,   // Integer mul
    IDiv = 0x23,   // Integer div (checked)
    IMod = 0x24,   // Integer modulo
    INeg = 0x25,   // Integer negate
    IInc = 0x26,   // Increment register by 1
    IDec = 0x27,   // Decrement register by 1
    IPow = 0x28,   // Integer power
    Xor = 0x29,    // Bitwise xor
    Shl = 0x2A,    // Bitwise shift left
    Shr = 0x2B,    // Bitwise shift right
    BitAnd = 0x2C, // Bitwise and
    BitOr = 0x2D,  // Bitwise or

    // == Arithmetic - Float (0x30-0x3F) ==
    FAdd = 0x30, // Float add
    FSub = 0x31, // Float sub
    FMul = 0x32, // Float mul
    FDiv = 0x33, // Float div
    FNeg = 0x34, // Float negate
    FMod = 0x35, // Float modulo
    IToF = 0x36, // Int to Float conversion
    FToI = 0x37, // Float to Int (truncate)
    FToS = 0x38, // Float to String
    FPow = 0x39, // Float power

    // == Comparison & Logic (0x40-0x4F) ==
    ICmpEq = 0x40, // Int compare ==
    ICmpLt = 0x41, // Int compare <
    ICmpGt = 0x42, // Int compare >
    ICmpLe = 0x43, // Int <=
    ICmpGe = 0x44, // Int >=
    FCmpEq = 0x45, // Float ==
    FCmpLt = 0x46, // Float <
    FCmpGt = 0x47, // Float >
    SCmpEq = 0x48, // String ==
    Not = 0x49,    // Boolean not
    And = 0x4A,    // Boolean and
    Or = 0x4B,     // Boolean or

    // == Control Flow (0x50-0x5F) ==
    Jmp = 0x50,      // Unconditional jump (offset: i16)
    JmpT = 0x51,     // Jump if true (reg, offset: i16)
    JmpF = 0x52,     // Jump if false
    Switch = 0x53,   // Switch table (reg, table_idx)
    Call = 0x54,     // Call function (func_reg, argc, dst_reg)
    TailCall = 0x55, // Tail call optimization
    Ret = 0x56,      // Return from function
    RetVal = 0x57,   // Return value in register

    // == Closures (0x60-0x6F) ==
    Closure = 0x60,     // Create closure (func_idx, env_count, dst)
    CapLoad = 0x61,     // Load from capture (closure_reg, idx, dst)
    CapStore = 0x62,    // Store to capture
    FreeVar = 0x63,     // Reserved — was free variable capture; never emitted
    ClosureCall = 0x64, // Call closure (closure_reg, argc, dst)

    // == Memory & Objects (0x70-0x7F) ==
    Alloc = 0x70,    // Allocate object (size, type_id, dst)
    FieldL = 0x71,   // Load field (obj_reg, field_idx, dst)
    FieldS = 0x72,   // Store field (obj_reg, field_idx, src)
    ArrAlloc = 0x73, // Allocate array (len_reg, elem_type, dst)
    ArrLoad = 0x74,  // Array load (arr_reg, idx_reg, dst)
    ArrStore = 0x75, // Array store
    ArrLen = 0x76,   // Array length (arr_reg, dst)
    TupleMk = 0x77,  // Create tuple (count, dst)
    TupleL = 0x78,   // Tuple field load
    RecMk = 0x79,    // Create record (field_count, dst)
    RecL = 0x7A,     // Record field load by name (const_idx)
    RecS = 0x7B,     // Record field store
    IsTag = 0x7C,    // Variant tag check (val_reg, tag_id, dst)
    Unpack = 0x7D,   // Variant unpack (val_reg, dst)
    /// Shallow copy a record: allocate a new record with the same slot count
    /// and copy every field, retaining each. src_reg → dst_reg.
    RecCopy = 0x9D,
    Copy = 0x7E, // Deep copy (ref_cap, src, dst)
    Drop = 0x7F, // Drop / deallocate (rc_dec or free)

    // == Actor & Concurrency (0x80-0x8F) ==
    Spawn = 0x80,        // Spawn actor (behavior_idx, init_reg, dst_addr)
    Send = 0x81,         // Send message (addr_reg, behavior_id, args...)
    Ask = 0x82,          // Ask / request-response
    SelfOp = 0x83,       // Get self actor address (dst)
    Receive = 0x84,      // Receive / await message (timeout_reg)
    Monitor = 0x85,      // Monitor actor (target_addr, dst)
    Demon = 0x86,        // Demonitor
    Link = 0x87,         // Link actors bidirectionally
    Unlink = 0x88,       // Unlink actors
    Exit = 0x89,         // Exit / terminate actor (reason_reg)
    Yield = 0x8A,        // Yield execution (reduction quota exhausted)
    StateGet = 0x8B,     // Load current actor state field by name (field_const_idx, dst)
    StateSet = 0x8C,     // Store to current actor state field by name (val_reg, field_const_idx)
    Emit = 0x8D,         // Emit event (event_name_const_idx, arg_count)
    SignalWait = 0x8E,   // Workflow signal wait (signal_name_const_idx, dst)
    ReceiveMatch = 0x8F, // Selective receive (spec_const_idx, dst); payload lands in dst+1..

    // == Effects (0x90-0x93, 0x9C) ==
    Perform = 0x90, // Perform effect operation (eff_id, op_id, args, dst)
    Handle = 0x91,  // Install effect handler (handler_table_idx)
    Resume = 0x92,  // Resume from effect handler with value (val_reg)
    Unwind = 0x93,  // Unwind effect handler
    /// Statically-resolved effect dispatch.  `op1` = handler table index
    /// (into `code_module.handler_tables`), `op2` = binding index (into
    /// `HandlerTable.bindings`), `op3` = result register.  The VM looks up
    /// the handler offset and register mapping directly from the table —
    /// no string comparison or `handler_stack` walk.
    PerformDirect = 0x9C,

    // == Python Interop (0x94-0x9B) ==
    PyImport = 0x94,  // Import Python module (module_name_const_idx, dst_reg, _)
    PyGetAttr = 0x95, // Get attribute from Python object (obj_reg, attr_name_const_idx, dst_reg)
    PyCall = 0x96,    // Call Python callable (callable_reg, arg_count, dst_reg)
    PyCallKw = 0x97, // Call Python callable with kwargs (callable_reg, args_tuple_reg, kwargs_dict_reg, dst_reg uses op3)
    PySetAttr = 0x98, // Set attribute on Python object (obj_reg, attr_name_const_idx, val_reg)
    PyToNu = 0x99,   // Convert Python object to Nulang Value (py_val_reg, dst_reg, _)
    PyFromNu = 0x9A, // Convert Nulang Value to Python object (nu_val_reg, dst_reg, _)
    PyRelease = 0x9B, // Decrement Python object reference count (py_val_reg, _, _)

    // == Actor & Concurrency, cont. (0xA0-0xAF) ==
    // Timed selective receive: `receive { | B(x) => e ... } after ms => body`.
    //
    // ReceiveWait contract (operands; the VM/runtime side is wave 2):
    //   - op1+op2 (imm16): spec constant index. The constant is a string
    //     "max_params:id1,id2,..." — the same format as ReceiveMatch (0x8F):
    //     the candidate arm behavior ids and the number of payload registers
    //     reserved after dst.
    //   - op3: dst — base of one contiguous register run of 1 + max_params
    //     registers. On a mailbox match the VM writes the matched arm index
    //     (0-based) to dst and up to max_params payload values into dst+1..
    //     (missing -> nil, extras ignored), exactly like ReceiveMatch. On
    //     timeout expiry — or a non-positive timeout with no match — it
    //     writes the arm count (the ReceiveMatch no-match sentinel) to dst.
    //   - r0: timeout in milliseconds (Int), staged by codegen with a Move
    //     immediately before this instruction (fixed-register staging, same
    //     convention as PipelineStage reading r0..r3).
    //
    // VM semantics (wave 2 implements):
    //   1. Scan the mailbox via ActorVmCallbacks::try_receive_match(&ids).
    //      Match -> write arm index + payload, continue at the next instr.
    //   2. No match, timeout > 0, in an actor context -> suspend the actor:
    //      decrement the PC so the instruction re-executes on wake (the
    //      SignalWait pattern) and raise the "ReceiveWait:suspend" sentinel.
    //      The runtime wakes the actor when a matching message arrives
    //      (re-execution finds the match) or when the timer fires (resume
    //      with a timeout marker; the instruction then writes the arm-count
    //      sentinel to dst and continues).
    //   3. No match, timeout <= 0 (or outside an actor context) -> write the
    //      arm-count sentinel to dst and continue; fully non-blocking.
    //
    // The MIR compare chain following this instruction dispatches arm
    // indices 0..n-1 to the receive arm bodies and routes the arm-count
    // sentinel to the after-clause body (no legacy pop-any Receive
    // fallthrough in the timed form).
    ReceiveWait = 0xA0,
    /// Commit a selective receive: removes the matched ("tried") message
    /// from the skip-buffer and clears remaining "tried" flags. No operands.
    /// Emitted after a pattern+guard check succeeds.
    ReceiveCommit = 0xA1,

    // == FFI (0xB0-0xBF) ==
    FFICall = 0xB0, // Call foreign function (func_idx high, func_idx low, dst)

    // == Inference & Async Effects (0xC6) ==
    /// Generic asynchronous effect operation (e.g. "Inference.ask").
    /// Replaces the monolithic AI opcodes (LlmAsk, Pipeline*, etc.) with a
    /// single suspending dispatch: effect_op string at constant pool index
    /// (op1:op2), destination register (op3), arguments staged in r0..rN.
    PerformAsync = 0xC6,

    // == Distribution (0xD0-0xDF) ==
    NodeId = 0xD0,  // Get current node id (dst)
    Migrate = 0xD1, // Migrate actor (addr_reg, node_id_reg, dst)
    RSend = 0xD2,   // Remote send (addr_reg, behavior_id, args)
    RAsk = 0xD3,    // Remote ask
    RSpawn = 0xD4,  // Remote spawn (node_id, behavior, init)
    Gossip = 0xD5,  // Gossip cluster state

    // == String & IO (0xE0-0xEF) ==
    SConcat = 0xE0, // String concatenation
    SPrint = 0xE1,  // Print to stdout
    SRead = 0xE2,   // Read line from stdin
    FOpen = 0xE3,   // File open
    FRead = 0xE4,   // File read
    FWrite = 0xE5,  // File write
    FClose = 0xE6,  // File close
    Print = 0xE7,   // Print any value (uses debug fmt)

    // == Debug & Meta (0xF0-0xFF) ==
    DbgBreak = 0xF0, // Debugger breakpoint
    DbgPrint = 0xF1, // Debug print register state
    DbgStack = 0xF2, // Debug print call stack
    MetaType = 0xF3, // Get type of value at runtime
    MetaCap = 0xF4,  // Get capability of reference at runtime

    // == Spill (0xF5-0xF6) — register spilling for large functions ==
    /// Load a spilled local from the frame's spill vector into a register.
    /// op1:op2 = spill index (u16 big-endian), op3 = destination register.
    SpillLoad = 0xF5,
    /// Store a register into the frame's spill vector.
    /// op1 = source register, op2:op3 = spill index (u16 big-endian).
    SpillStore = 0xF6,
}

impl OpCode {
    pub fn from_u8(v: u8) -> Option<Self> {
        use OpCode::*;
        match v {
            0x00 => Some(Nop),
            0x01 => Some(Halt),
            0x02 => Some(Panic),
            0x03 => Some(Const0),
            0x04 => Some(Const1),
            0x05 => Some(Const2),
            0x06 => Some(ConstM1),
            0x07 => Some(ConstU),
            0x08 => Some(ConstL),
            0x10 => Some(Load),
            0x11 => Some(Store),
            0x12 => Some(Move),
            0x13 => Some(Pop),
            0x14 => Some(Dup),
            0x15 => Some(Swap),
            0x20 => Some(IAdd),
            0x21 => Some(ISub),
            0x22 => Some(IMul),
            0x23 => Some(IDiv),
            0x24 => Some(IMod),
            0x25 => Some(INeg),
            0x26 => Some(IInc),
            0x27 => Some(IDec),
            0x28 => Some(IPow),
            0x29 => Some(Xor),
            0x2A => Some(Shl),
            0x2B => Some(Shr),
            0x2C => Some(BitAnd),
            0x2D => Some(BitOr),
            0x30 => Some(FAdd),
            0x31 => Some(FSub),
            0x32 => Some(FMul),
            0x33 => Some(FDiv),
            0x34 => Some(FNeg),
            0x35 => Some(FMod),
            0x36 => Some(IToF),
            0x37 => Some(FToI),
            0x38 => Some(FToS),
            0x40 => Some(ICmpEq),
            0x39 => Some(FPow),
            0x41 => Some(ICmpLt),
            0x42 => Some(ICmpGt),
            0x43 => Some(ICmpLe),
            0x44 => Some(ICmpGe),
            0x45 => Some(FCmpEq),
            0x46 => Some(FCmpLt),
            0x47 => Some(FCmpGt),
            0x48 => Some(SCmpEq),
            0x49 => Some(Not),
            0x4A => Some(And),
            0x4B => Some(Or),
            0x50 => Some(Jmp),
            0x51 => Some(JmpT),
            0x52 => Some(JmpF),
            0x53 => Some(Switch),
            0x54 => Some(Call),
            0x55 => Some(TailCall),
            0x56 => Some(Ret),
            0x57 => Some(RetVal),
            0x60 => Some(Closure),
            0x61 => Some(CapLoad),
            0x62 => Some(CapStore),
            0x63 => Some(FreeVar),
            0x64 => Some(ClosureCall),
            0x70 => Some(Alloc),
            0x71 => Some(FieldL),
            0x72 => Some(FieldS),
            0x73 => Some(ArrAlloc),
            0x74 => Some(ArrLoad),
            0x75 => Some(ArrStore),
            0x76 => Some(ArrLen),
            0x77 => Some(TupleMk),
            0x78 => Some(TupleL),
            0x79 => Some(RecMk),
            0x7A => Some(RecL),
            0x7B => Some(RecS),
            0x7C => Some(IsTag),
            0x7D => Some(Unpack),
            0x7E => Some(Copy),
            0x7F => Some(Drop),
            0x80 => Some(Spawn),
            0x81 => Some(Send),
            0x82 => Some(Ask),
            0x83 => Some(SelfOp),
            0x84 => Some(Receive),
            0x85 => Some(Monitor),
            0x86 => Some(Demon),
            0x87 => Some(Link),
            0x88 => Some(Unlink),
            0x89 => Some(Exit),
            0x8A => Some(Yield),
            0x8B => Some(StateGet),
            0x8C => Some(StateSet),
            0x8D => Some(Emit),
            0x8E => Some(SignalWait),
            0x8F => Some(ReceiveMatch),
            0x90 => Some(Perform),
            0x91 => Some(Handle),
            0x92 => Some(Resume),
            0x93 => Some(Unwind),
            0x94 => Some(PyImport),
            0x95 => Some(PyGetAttr),
            0x96 => Some(PyCall),
            0x97 => Some(PyCallKw),
            0x98 => Some(PySetAttr),
            0x99 => Some(PyToNu),
            0x9A => Some(PyFromNu),
            0x9B => Some(PyRelease),
            0x9D => Some(RecCopy),
            0x9C => Some(PerformDirect),
            0xA0 => Some(ReceiveWait),
            0xA1 => Some(ReceiveCommit),
            0xB0 => Some(FFICall),
            0xC6 => Some(PerformAsync),
            0xD0 => Some(NodeId),
            0xD1 => Some(Migrate),
            0xD2 => Some(RSend),
            0xD3 => Some(RAsk),
            0xD4 => Some(RSpawn),
            0xD5 => Some(Gossip),
            0xE0 => Some(SConcat),
            0xE1 => Some(SPrint),
            0xE2 => Some(SRead),
            0xE3 => Some(FOpen),
            0xE4 => Some(FRead),
            0xE5 => Some(FWrite),
            0xE6 => Some(FClose),
            0xE7 => Some(Print),
            0xF0 => Some(DbgBreak),
            0xF1 => Some(DbgPrint),
            0xF2 => Some(DbgStack),
            0xF3 => Some(MetaType),
            0xF4 => Some(MetaCap),
            0xF5 => Some(SpillLoad),
            0xF6 => Some(SpillStore),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

// ---------------------------------------------------------------------------
// Instruction Encoding
// ---------------------------------------------------------------------------

/// 32-bit fixed-width instruction.
/// Layout: [opcode: u8] [op1: u8] [op2: u8] [op3: u8]
/// Extended format for larger immediates uses op1+op2 as u16, or op1+op2+op3 as u24.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    pub opcode: OpCode,
    pub op1: u8,
    pub op2: u8,
    pub op3: u8,
}

impl Instruction {
    pub fn new0(opcode: OpCode) -> Self {
        Instruction {
            opcode,
            op1: 0,
            op2: 0,
            op3: 0,
        }
    }
    pub fn new1(opcode: OpCode, a: u8) -> Self {
        Instruction {
            opcode,
            op1: a,
            op2: 0,
            op3: 0,
        }
    }
    pub fn new2(opcode: OpCode, a: u8, b: u8) -> Self {
        Instruction {
            opcode,
            op1: a,
            op2: b,
            op3: 0,
        }
    }
    pub fn new3(opcode: OpCode, a: u8, b: u8, c: u8) -> Self {
        Instruction {
            opcode,
            op1: a,
            op2: b,
            op3: c,
        }
    }

    /// Encode as u32 (big-endian: opcode | op1 | op2 | op3).
    pub fn encode(&self) -> u32 {
        ((self.opcode.as_u8() as u32) << 24)
            | ((self.op1 as u32) << 16)
            | ((self.op2 as u32) << 8)
            | (self.op3 as u32)
    }

    /// Decode from u32.
    pub fn decode(encoded: u32) -> Option<Self> {
        let opcode = OpCode::from_u8((encoded >> 24) as u8)?;
        Some(Instruction {
            opcode,
            op1: ((encoded >> 16) & 0xFF) as u8,
            op2: ((encoded >> 8) & 0xFF) as u8,
            op3: (encoded & 0xFF) as u8,
        })
    }

    /// Get 16-bit immediate from op1+op2 (used by Jmp, ConstU, Call, etc.)
    pub fn imm16(&self) -> u16 {
        ((self.op1 as u16) << 8) | (self.op2 as u16)
    }

    /// Get signed 16-bit immediate from op1+op2.
    pub fn simm16(&self) -> i16 {
        self.imm16() as i16
    }

    /// Get 16-bit offset from op2+op3 (used by JmpT, JmpF which store reg in op1)
    pub fn offset16(&self) -> i16 {
        (((self.op2 as u16) << 8) | (self.op3 as u16)) as i16
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Constant {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Nil,
    Unit,
    TypeDescriptor(String), // String representation of type
    FunctionRef(usize),     // Index into function table
    BehaviorRef(usize),     // Index into behavior table
}

// ---------------------------------------------------------------------------
// Effect Handler Table
// ---------------------------------------------------------------------------

/// A single binding from effect name to handler code offset.
/// Compiled by the compiler when processing `handle eff_name -> { body }` blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandlerBinding {
    pub effect_name: String,
    /// Bytecode offset of the handler body (receives args in r0..rn).
    pub handler_offset: usize,
    /// Number of arguments the effect operation expects.
    pub arg_count: u8,
    /// Register to place the effect operation result into (for resume).
    pub result_reg: u8,
    /// Whether the continuation is consumed at most once (linear use of
    /// `resume`).  When `true` the VM may skip heap-allocating the
    /// `Continuation`.  Determined at compile time by
    /// `effect_checker::is_single_shot`.
    pub single_shot: bool,
}

/// A handler table: maps effect names to their handler implementations.
/// One table per `handle { ... }` block. Pushed onto the handler stack at
/// runtime by the Handle opcode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandlerTable {
    pub bindings: Vec<HandlerBinding>,
    /// Optional fallback: code offset to jump to if no binding matches.
    /// If None, an unhandled effect triggers a runtime error.
    pub fallback_offset: Option<usize>,
}

// ---------------------------------------------------------------------------
// Behavior Table Entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorTableEntry {
    pub name: String,
    pub param_count: usize,
    pub code_offset: usize, // Offset into bytecode
    pub local_count: usize, // Number of local registers needed
    pub effect_mask: u32,   // Which effects this behavior may perform (bitmap)
    /// Optional code offset for the saga compensation expression of this step.
    pub compensate_offset: Option<usize>,
    pub content_hash: Option<[u8; 32]>, // BLAKE3 hash of compiled bytecode body + param/return types
    /// Optional source location (file, line, column) for hash→source mapping in error messages.
    pub source_location: Option<(String, u32, u32)>,
    /// For synthetic parallel steps: the ordered names of the branches.
    /// `None` for normal sequential steps.
    pub parallel_branches: Option<Vec<String>>,
}

/// Actor metadata for durable execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActorMeta {
    pub name: String,
    pub persistent: bool,
    /// State field name -> model (Local, Durable, EventSourced, Crdt).
    pub state_models: Vec<(String, crate::ast::StateModel)>,
    /// Default values for state fields (literals only in the MVP).
    pub state_defaults: Vec<(String, Constant)>,
    /// Indices into the behavior table that belong to this actor.
    pub behavior_indices: Vec<usize>,
    /// True if this actor was generated from a `workflow` declaration.
    pub is_workflow: bool,
    /// True if this actor was generated from an `agent` declaration.
    pub is_agent: bool,
    /// True if from organization (RFC 0009).
    #[serde(default)]
    pub is_organization: bool,
    /// True if declared as `virtual entity` (RFC 0016 virtual actor
    /// auto-hydration).
    #[serde(default)]
    pub is_virtual: bool,
    /// Tool schemas exposed to this agent actor.
    pub tools: Vec<ToolSchema>,
    /// Semantic-memory vector dimensions, if configured for this agent.
    pub semantic_memory_dimensions: Option<usize>,
    /// Procedural-memory namespace, if configured for this agent.
    pub procedural_memory_namespace: Option<String>,
    /// Compile-time backend selection for this actor.
    #[serde(default)]
    pub backend: crate::ast::ActorBackendKind,
    /// Serialized fallback pipeline (JSON `Vec<AgentFallbackEntry>`).
    #[serde(default)]
    pub fallback_config: String,
    /// Serialized retry config (JSON `Option<AgentRetryConfig>`).
    #[serde(default)]
    pub retry_config: String,
    /// NTIR structural type hash.
    #[serde(default)]
    pub type_hash: Option<[u8; 32]>,
    /// Entity schema version (RFC 0008).  Defaults to 1.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Serialized migration contracts (JSON `Vec<MigrationDecl>`).  RFC 0008.
    #[serde(default)]
    pub migrations: String,
}

fn default_version() -> u32 {
    1
}
impl ActorMeta {
    pub fn new(name: impl Into<String>) -> Self {
        ActorMeta {
            name: name.into(),
            persistent: false,
            state_models: Vec::new(),
            state_defaults: Vec::new(),
            behavior_indices: Vec::new(),
            is_workflow: false,
            is_agent: false,
            is_organization: false,
            is_virtual: false,
            tools: Vec::new(),
            semantic_memory_dimensions: None,
            procedural_memory_namespace: None,
            backend: crate::ast::ActorBackendKind::default(),
            fallback_config: String::new(),
            retry_config: String::new(),
            type_hash: None,
            version: 1,
            migrations: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// FFI Function Definition
// ---------------------------------------------------------------------------

/// FFI primitive types supported by the bytecode compiler and VM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FfiType {
    Int,
    Float,
    Bool,
    String,
    Unit,
    Pointer,
}

/// A foreign function declared in an `extern "lib" { ... }` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForeignFunctionDef {
    pub library: String,
    pub symbol: String,
    pub params: Vec<FfiType>,
    pub ret: FfiType,
}

// ---------------------------------------------------------------------------
// Code Module
// ---------------------------------------------------------------------------

/// Per-function debug metadata for the DAP server: how to name a stack frame
/// (by code range) and which registers hold which local variables. Register
/// index = `mir::FunctionBuilder::LOCAL_BASE` + MIR local id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DebugFunctionInfo {
    pub name: String,
    /// Byte offset of the first instruction of this function.
    pub code_offset: usize,
    /// Number of instructions in the function (for code-range lookup).
    pub code_len: usize,
    /// Register indices of the function parameters.
    pub params: Vec<usize>,
    /// `(register index, optional local name)` for every local.
    pub locals: Vec<(usize, Option<String>)>,
}

/// Export table entry for library distribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportTableEntry {
    pub name: String,
    pub kind: String,
    pub index: usize,
    pub type_sig: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeModule {
    pub name: String,
    pub constants: Vec<Constant>,
    pub instructions: Vec<Instruction>,
    pub behaviors: Vec<BehaviorTableEntry>,
    pub function_table: Vec<usize>, // code offsets for named functions
    /// Local register count for each function in `function_table`.
    /// Parallel array: `function_local_counts[i]` = highest register index + 1
    /// used by `function_table[i]`. Used by the VM to limit register copies.
    /// `#[serde(default)]` — old `.nbc` artifacts deserialize without it.
    #[serde(default)]
    pub function_local_counts: Vec<usize>,
    pub exports: Vec<(String, usize)>, // name -> constant/function index
    /// Entry point for inline __main (None if no __main, defaults to 0 in VM)
    pub entry_point: Option<usize>,
    /// Effect handler tables: one per `handle { ... }` block.
    /// Indexed by the handler_table_idx operand of the Handle opcode.
    pub handler_tables: Vec<HandlerTable>,
    /// Actor metadata for durable execution (v0.7).
    pub actor_metadata: Vec<ActorMeta>,
    /// Foreign function definitions from `extern` blocks.
    pub foreign_functions: Vec<ForeignFunctionDef>,
    /// Tool schemas for functions annotated with `@tool(description: "...")`.
    pub tools: Vec<ToolSchema>,
    /// Spawn-site init field overrides.  Maps `Spawn` instruction byte-offset
    /// to per-field constant values that should override declared state defaults.
    /// Populated by MIR codegen; consumed by the VM's `step_spawn`.
    #[serde(default)]
    pub spawn_init_overrides: Vec<(usize, Vec<(String, Constant)>)>,
    #[serde(default)]
    pub remote_spawn_init_fields: Vec<(usize, Vec<String>)>,
    /// Sorted (bytecode pc -> 1-indexed source line) for the DAP server's
    /// breakpoint resolution and stepping. One entry per source statement
    /// (the pc of its first instruction).
    #[serde(default)]
    pub line_table: Vec<(usize, u32)>,
    /// Per-function debug info (name / code-range / locals) for the DAP
    /// server. Includes actor behaviors (compiled as functions).
    #[serde(default)]
    pub debug_functions: Vec<DebugFunctionInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub export_table: Vec<ExportTableEntry>,
}

impl CodeModule {
    pub fn new(name: impl Into<String>) -> Self {
        CodeModule {
            name: name.into(),
            constants: Vec::new(),
            instructions: Vec::new(),
            behaviors: Vec::new(),
            function_table: Vec::new(),
            function_local_counts: Vec::new(),
            exports: Vec::new(),
            entry_point: None,
            spawn_init_overrides: Vec::new(),
            remote_spawn_init_fields: Vec::new(),
            handler_tables: Vec::new(),
            actor_metadata: Vec::new(),
            foreign_functions: Vec::new(),
            tools: Vec::new(),
            line_table: Vec::new(),
            debug_functions: Vec::new(),
            export_table: Vec::new(),
        }
    }

    pub fn add_actor_meta(&mut self, meta: ActorMeta) -> usize {
        let idx = self.actor_metadata.len();
        self.actor_metadata.push(meta);
        idx
    }

    pub fn emit(&mut self, instr: Instruction) -> usize {
        let idx = self.instructions.len();
        self.instructions.push(instr);
        idx
    }

    pub fn patch_jump(&mut self, instr_idx: usize, target_offset: i16) {
        if let Some(instr) = self.instructions.get_mut(instr_idx) {
            let abs_offset = (instr_idx as i64 + target_offset as i64) as u16;
            instr.op1 = (abs_offset >> 8) as u8;
            instr.op2 = (abs_offset & 0xFF) as u8;
        }
    }

    /// Source line for a bytecode pc (greatest line-table pc <= `pc`), or
    /// `None` if `pc` precedes every recorded statement. `line_table` is
    /// sorted by pc and non-decreasing in line.
    pub fn line_at(&self, pc: usize) -> Option<u32> {
        match self.line_table.binary_search_by_key(&pc, |&(p, _)| p) {
            Ok(i) => Some(self.line_table[i].1),
            Err(0) => None,
            Err(i) => Some(self.line_table[i - 1].1),
        }
    }

    /// Resolve a requested source `line` to a bytecode pc for breakpoint
    /// placement: exact match when the line is executable, otherwise the
    /// next executable line after it (the common "snap to next statement"
    /// behaviour). Returns `(pc, actual_line)` or `None` when no executable
    /// line at or after `line` exists.
    pub fn resolve_line(&self, line: u32) -> Option<(usize, u32)> {
        self.line_table
            .iter()
            .find(|&&(_, l)| l >= line)
            .map(|&(pc, l)| (pc, l))
    }

    pub fn add_constant(&mut self, c: Constant) -> usize {
        let idx = self.constants.len();
        self.constants.push(c);
        idx
    }

    pub fn add_behavior(&mut self, b: BehaviorTableEntry) -> usize {
        let idx = self.behaviors.len();
        self.behaviors.push(b);
        idx
    }

    pub fn add_handler_table(&mut self, ht: HandlerTable) -> usize {
        let idx = self.handler_tables.len();
        self.handler_tables.push(ht);
        idx
    }

    pub fn current_offset(&self) -> usize {
        self.instructions.len()
    }

    /// Build a CodeModule from bootstrap emitter JSON format.
    /// See `bootstrap/FORMAT.md` for the JSON schema.
    pub fn from_bootstrap_json(json: &str) -> Result<Self, String> {
        #[derive(serde::Deserialize)]
        struct BJson {
            #[serde(default)]
            name: String,
            instructions: Vec<String>,
            #[serde(default)]
            constants: Vec<serde_json::Value>,
            #[serde(default)]
            entry_point: Option<usize>,
        }
        let input: BJson = serde_json::from_str(json).map_err(|e| format!("JSON: {e}"))?;
        let mut m = CodeModule::new(if input.name.is_empty() {
            "bootstrap"
        } else {
            &input.name
        });
        for hex in &input.instructions {
            let w = u32::from_str_radix(hex, 16).map_err(|e| format!("hex '{hex}': {e}"))?;
            let i = Instruction::decode(w).ok_or_else(|| format!("bad opcode: {hex}"))?;
            m.instructions.push(i);
        }
        for c in &input.constants {
            let ty = c.get("type").and_then(|v| v.as_str()).unwrap_or("Int");
            let val = c.get("value");
            let constant = match ty {
                "Int" => Constant::Int(
                    val.and_then(|v| v.as_i64())
                        .ok_or("Int value not integer")?,
                ),
                "Float" => Constant::Float(
                    val.and_then(|v| v.as_f64())
                        .ok_or("Float value not number")?,
                ),
                "Bool" => Constant::Int(if val.and_then(|v| v.as_bool()).unwrap_or(false) {
                    1
                } else {
                    0
                }),
                "String" => {
                    Constant::String(val.and_then(|v| v.as_str()).unwrap_or("").to_string())
                }
                t => return Err(format!("unknown constant type: {t}")),
            };
            m.constants.push(constant);
        }
        m.entry_point = input.entry_point;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Instruction encoding / decoding round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn test_instruction_encode_decode_roundtrip() {
        // new0: no operands
        let i0 = Instruction::new0(OpCode::Halt);
        let enc0 = i0.encode();
        let dec0 = Instruction::decode(enc0).unwrap();
        assert_eq!(i0, dec0);

        // new1: one operand
        let i1 = Instruction::new1(OpCode::Load, 0x42);
        let enc1 = i1.encode();
        let dec1 = Instruction::decode(enc1).unwrap();
        assert_eq!(i1, dec1);

        // new2: two operands
        let i2 = Instruction::new2(OpCode::IAdd, 0x12, 0x34);
        let enc2 = i2.encode();
        let dec2 = Instruction::decode(enc2).unwrap();
        assert_eq!(i2, dec2);

        // new3: three operands
        let i3 = Instruction::new3(OpCode::Call, 0xAA, 0xBB, 0xCC);
        let enc3 = i3.encode();
        let dec3 = Instruction::decode(enc3).unwrap();
        assert_eq!(i3, dec3);
    }

    #[test]
    fn test_instruction_imm16() {
        let instr = Instruction::new2(OpCode::ConstU, 0x12, 0x34);
        assert_eq!(instr.imm16(), 0x1234);
    }

    #[test]
    fn test_instruction_simm16() {
        // op1=0xFF, op2=0x00 -> imm16 = 0xFF00 = 65280, sign-extended = -256
        let instr = Instruction::new2(OpCode::Jmp, 0xFF, 0x00);
        assert_eq!(instr.simm16(), -256i16);

        // positive: op1=0x00, op2=0x7F -> imm16 = 0x007F = 127
        let instr2 = Instruction::new2(OpCode::Jmp, 0x00, 0x7F);
        assert_eq!(instr2.simm16(), 127i16);
    }

    #[test]
    fn test_instruction_offset16() {
        // offset16 uses op2+op3: op2=0xAB, op3=0xCD -> 0xABCD
        let instr = Instruction::new3(OpCode::JmpT, 0x01, 0xAB, 0xCD);
        assert_eq!(instr.offset16(), 0xABCDu16 as i16);

        // negative offset: op2=0xFF, op3=0x00 -> 0xFF00 = -256
        let instr2 = Instruction::new3(OpCode::JmpF, 0x01, 0xFF, 0x00);
        assert_eq!(instr2.offset16(), -256i16);
    }

    // -----------------------------------------------------------------------
    // OpCode from_u8 / as_u8 round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn test_opcode_from_u8_all() {
        // Known opcodes exist in 0x00..=0xF6; gaps return None.
        // Build a set of all known byte values for verification.
        let known: Vec<u8> = (0x00..=0x08)
            .chain(0x10..=0x15)
            .chain(0x20..=0x2D)
            .chain(0x30..=0x39)
            .chain(0x40..=0x4B)
            .chain(0x50..=0x57)
            .chain(0x60..=0x64)
            .chain(0x70..=0x7F)
            .chain(0x80..=0x8F)
            .chain(0x90..=0x93)
            .chain(0x94..=0x9B)
            .chain(0x9C..=0x9D)
            .chain(0xA0..=0xA1)
            .chain(0xB0..=0xB0)
            .chain(0xC6..=0xC6)
            .chain(0xD0..=0xD5)
            .chain(0xE0..=0xE7)
            .chain(0xF0..=0xF6)
            .collect();

        for byte in 0..=0xF6u8 {
            let result = OpCode::from_u8(byte);
            if known.contains(&byte) {
                assert!(result.is_some(), "expected Some(OpCode) for 0x{byte:02X}");
                assert_eq!(
                    result.unwrap().as_u8(),
                    byte,
                    "round-trip failed for 0x{byte:02X}"
                );
            } else {
                assert!(
                    result.is_none(),
                    "expected None for gap byte 0x{byte:02X}, got {result:?}"
                );
            }
        }
    }

    #[test]
    fn test_opcode_from_u8_invalid() {
        for byte in 0xF7..=0xFFu8 {
            assert_eq!(
                OpCode::from_u8(byte),
                None,
                "byte 0x{byte:02X} should return None"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Constant variants
    // -----------------------------------------------------------------------

    #[test]
    fn test_constant_variants() {
        let c_int = Constant::Int(42);
        assert_eq!(c_int, Constant::Int(42));
        assert_ne!(c_int, Constant::Int(0));

        let c_float = Constant::Float(1.5);
        assert_eq!(c_float, Constant::Float(1.5));

        let c_str = Constant::String("hello".into());
        assert_eq!(c_str, Constant::String("hello".into()));

        let c_true = Constant::Bool(true);
        assert_eq!(c_true, Constant::Bool(true));

        let c_false = Constant::Bool(false);
        assert_eq!(c_false, Constant::Bool(false));
        assert_ne!(c_true, c_false);

        assert_eq!(Constant::Nil, Constant::Nil);
        assert_eq!(Constant::Unit, Constant::Unit);
        assert_ne!(Constant::Nil, Constant::Unit);

        let c_type = Constant::TypeDescriptor("Int".into());
        assert_eq!(c_type, Constant::TypeDescriptor("Int".into()));

        let c_fn = Constant::FunctionRef(7);
        assert_eq!(c_fn, Constant::FunctionRef(7));

        let c_beh = Constant::BehaviorRef(3);
        assert_eq!(c_beh, Constant::BehaviorRef(3));
    }

    // -----------------------------------------------------------------------
    // CodeModule operations
    // -----------------------------------------------------------------------

    #[test]
    fn test_code_module_emit_and_patch() {
        let mut modl = CodeModule::new("test");
        assert!(modl.instructions.is_empty());

        // Emit a Jmp (placeholder with zeros)
        let idx = modl.emit(Instruction::new2(OpCode::Jmp, 0, 0));
        assert_eq!(idx, 0);
        assert_eq!(modl.instructions.len(), 1);

        // Emit a second instruction so offsets make sense
        modl.emit(Instruction::new0(OpCode::Nop));
        assert_eq!(modl.instructions.len(), 2);

        // Patch the jump at idx 0 to skip one instruction (offset +2 = from idx 0 to idx 2)
        modl.patch_jump(0, 2);
        assert_eq!(modl.instructions[0].op1, 0);
        assert_eq!(modl.instructions[0].op2, 2);

        // Decode round-trip
        let enc = modl.instructions[0].encode();
        let dec = Instruction::decode(enc).unwrap();
        assert_eq!(dec.opcode, OpCode::Jmp);
        assert_eq!(dec.op1, 0);
        assert_eq!(dec.op2, 2);
    }

    #[test]
    fn test_code_module_add_constant() {
        let mut modl = CodeModule::new("test_const");
        assert!(modl.constants.is_empty());

        let idx0 = modl.add_constant(Constant::Int(100));
        assert_eq!(idx0, 0);

        let idx1 = modl.add_constant(Constant::String("foo".into()));
        assert_eq!(idx1, 1);

        let idx2 = modl.add_constant(Constant::Nil);
        assert_eq!(idx2, 2);

        assert_eq!(modl.constants.len(), 3);
        assert_eq!(modl.constants[0], Constant::Int(100));
        assert_eq!(modl.constants[1], Constant::String("foo".into()));
        assert_eq!(modl.constants[2], Constant::Nil);
    }

    #[test]
    fn test_code_module_add_behavior() {
        let mut modl = CodeModule::new("test_beh");

        let entry = BehaviorTableEntry {
            name: "step1".into(),
            param_count: 2,
            code_offset: 10,
            local_count: 8,
            effect_mask: 0b0011,
            compensate_offset: None,
            content_hash: None,
            source_location: None,
            parallel_branches: None,
        };
        let _idx = modl.add_behavior(entry);
        let entry2 = BehaviorTableEntry {
            name: "step2".into(),
            param_count: 1,
            code_offset: 20,
            local_count: 4,
            effect_mask: 0,
            compensate_offset: Some(30),
            content_hash: None,
            source_location: None,
            parallel_branches: Some(vec!["a".into(), "b".into()]),
        };
        let idx2 = modl.add_behavior(entry2);
        assert_eq!(idx2, 1);

        assert_eq!(modl.behaviors.len(), 2);
        assert_eq!(modl.behaviors[0].name, "step1");
        assert_eq!(modl.behaviors[1].name, "step2");
        assert_eq!(modl.behaviors[1].compensate_offset, Some(30));
    }

    #[test]
    fn test_code_module_add_handler_table() {
        let mut modl = CodeModule::new("test_ht");

        // Table with fallback
        let ht_with = HandlerTable {
            bindings: vec![HandlerBinding {
                effect_name: "io.read".into(),
                handler_offset: 100,
                arg_count: 1,
                result_reg: 0,
                single_shot: false,
            }],
            fallback_offset: Some(200),
        };
        let idx0 = modl.add_handler_table(ht_with);
        assert_eq!(idx0, 0);

        // Table without fallback
        let ht_without = HandlerTable {
            bindings: vec![],
            fallback_offset: None,
        };
        let idx1 = modl.add_handler_table(ht_without);
        assert_eq!(idx1, 1);

        assert_eq!(modl.handler_tables.len(), 2);
        assert_eq!(modl.handler_tables[0].bindings.len(), 1);
        assert_eq!(modl.handler_tables[0].fallback_offset, Some(200));
        assert!(modl.handler_tables[1].bindings.is_empty());
        assert_eq!(modl.handler_tables[1].fallback_offset, None);
    }

    #[test]
    fn test_code_module_current_offset() {
        let mut modl = CodeModule::new("test_off");
        assert_eq!(modl.current_offset(), 0);

        modl.emit(Instruction::new0(OpCode::Nop));
        assert_eq!(modl.current_offset(), 1);

        modl.emit(Instruction::new0(OpCode::Nop));
        modl.emit(Instruction::new0(OpCode::Nop));
        assert_eq!(modl.current_offset(), 3);
    }

    // -----------------------------------------------------------------------
    // ActorMeta
    // -----------------------------------------------------------------------

    #[test]
    fn test_actor_meta_new() {
        let meta = ActorMeta::new("my_actor");
        assert_eq!(meta.name, "my_actor");
        assert!(!meta.persistent);
        assert!(meta.state_models.is_empty());
        assert!(meta.state_defaults.is_empty());
        assert!(meta.behavior_indices.is_empty());
        assert!(!meta.is_workflow);
        assert!(!meta.is_agent);
        assert!(meta.tools.is_empty());
        assert_eq!(meta.semantic_memory_dimensions, None);
        assert_eq!(meta.procedural_memory_namespace, None);
    }

    // -----------------------------------------------------------------------
    // HandlerTable fallback
    // -----------------------------------------------------------------------

    #[test]
    fn test_handler_table_fallback() {
        // With fallback
        let ht = HandlerTable {
            bindings: vec![],
            fallback_offset: Some(42),
        };
        assert!(ht.fallback_offset.is_some());
        assert_eq!(ht.fallback_offset.unwrap(), 42);
        assert!(ht.bindings.is_empty());

        // Without fallback
        let ht2 = HandlerTable {
            bindings: vec![HandlerBinding {
                effect_name: "test".into(),
                handler_offset: 10,
                arg_count: 2,
                result_reg: 1,
                single_shot: false,
            }],
            fallback_offset: None,
        };
        assert!(ht2.fallback_offset.is_none());
        assert_eq!(ht2.bindings.len(), 1);
        assert_eq!(ht2.bindings[0].effect_name, "test");
    }

    // -----------------------------------------------------------------------
    // ForeignFunctionDef
    // -----------------------------------------------------------------------

    #[test]
    fn test_foreign_function_def() {
        let def = ForeignFunctionDef {
            library: "mylib.so".into(),
            symbol: "my_func".into(),
            params: vec![FfiType::Int, FfiType::String],
            ret: FfiType::Float,
        };
        assert_eq!(def.library, "mylib.so");
        assert_eq!(def.symbol, "my_func");
        assert_eq!(def.params.len(), 2);
        assert_eq!(def.params[0], FfiType::Int);
        assert_eq!(def.params[1], FfiType::String);
        assert_eq!(def.ret, FfiType::Float);

        // All FfiType variants
        assert_eq!(FfiType::Int, FfiType::Int);
        assert_eq!(FfiType::Float, FfiType::Float);
        assert_eq!(FfiType::Bool, FfiType::Bool);
        assert_eq!(FfiType::String, FfiType::String);
        assert_eq!(FfiType::Unit, FfiType::Unit);
        assert_eq!(FfiType::Pointer, FfiType::Pointer);
        assert_ne!(FfiType::Int, FfiType::Float);
    }

    #[test]
    fn test_from_bootstrap_json_lit42() {
        let json =
            r#"{"instructions":["07000000","57000000"],"constants":[{"type":"Int","value":42}]}"#;
        let m = CodeModule::from_bootstrap_json(json).expect("parse");
        assert_eq!(m.instructions.len(), 2);
        assert_eq!(m.constants.len(), 1);
        let nbc = m.to_nbc(None).expect("to_nbc");
        let a = CodeModule::from_nbc(&nbc).expect("from_nbc");
        let mut vm = crate::vm::VM::new();
        vm.load_module(a.module);
        assert_eq!(vm.run().unwrap().as_int(), Some(42));
    }

    #[test]
    fn test_recursive_factorial() {
        // fn fact(n) { if n<=1 then 1 else n*fact(n-1) }; main() { fact(3) }
        // entry_point = offset of main (instruction 11)
        let instrs = [
            0x04010000, 0x43000102, 0x52020003, // fact: r1=1; cmp; jmpf
            0x04000000, 0x57000000, // base: return 1
            0x12000300, 0x21000100, 0x03040000, // r3=n; r0=n-1; r4=0
            0x54040100, 0x22030000, 0x57000000, // call; mul; ret
            0x07000000, 0x03010000, 0x54010100, 0x57000000, // main
        ];
        let mut m = CodeModule::new("fact3");
        m.constants.push(Constant::Int(3));
        for &w in &instrs {
            m.instructions.push(Instruction::decode(w).unwrap());
        }
        m.function_table = vec![0, 11];
        m.entry_point = Some(11); // DIRECT OFFSET of main, not function table index!
        let a = CodeModule::from_nbc(&m.to_nbc(None).unwrap()).unwrap();
        let mut vm = crate::vm::VM::new();
        vm.load_module(a.module);
        assert_eq!(vm.run().unwrap().as_int(), Some(6));
    }

    #[test]
    fn test_recursive_decrement() {
        // fn dec(n) { if n==0 then 0 else dec(n-1) }; main() { dec(1) }
        let instrs = [
            0x03010000, 0x40000102, 0x52020003, 0x03000000, 0x57000000, 0x04030000, 0x21000300,
            0x03040000, 0x54040100, 0x57000000, 0x04000000, 0x03010000, 0x54010100, 0x57000000,
        ];
        let mut m = CodeModule::new("dec");
        for &w in &instrs {
            m.instructions.push(Instruction::decode(w).unwrap());
        }
        m.function_table = vec![0, 10];
        m.entry_point = Some(10); // direct offset of main
        let a = CodeModule::from_nbc(&m.to_nbc(None).unwrap()).unwrap();
        let mut vm = crate::vm::VM::new();
        vm.load_module(a.module);
        assert_eq!(vm.run().unwrap().as_int(), Some(0));
    }
}
