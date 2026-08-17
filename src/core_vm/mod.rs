//! Minimal Core VM for Nulang bootstrap Stage 3.
//!
//! A pure interpreter for the frozen Core subset that loads and runs `.nbc`
//! bytecode with no JIT, no WASM, no actors, no Python, no FFI.
//! Designed for portability to new hardware without the Rust toolchain.

use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};

use crate::value_layout;

pub type Value = u64;

/// Flag bit in a `TAG_CLOSURE` payload marking an env-carrying closure (the
/// payload then indexes `closure_envs` rather than being the function index).
/// Mirrors `CLOSURE_ENV_FLAG` in `src/vm.rs`.
const CLOSURE_ENV_FLAG: u64 = 0x0000_4000_0000_0000;
const CLOSURE_ENV_IDX_MASK: u64 = CLOSURE_ENV_FLAG - 1;

/// A captured-variable environment owned by a closure value (encoded as a
/// `CLOSURE_ENV_FLAG`-tagged `TAG_CLOSURE` whose payload indexes `closure_envs`).
#[derive(Debug, Clone)]
pub struct ClosureEnv {
    pub func_idx: usize,
    pub captures: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub regs: [Value; 256],
    pub pc: usize,
    pub module_idx: usize,
    pub return_dst: u8,
    pub caller_idx: Option<usize>,
    /// The closure value this frame was invoked with, if any. `CapLoad`
    /// reads captured slots from the env it points at.
    pub closure_env: Option<Value>,
}

impl Frame {
    pub fn new(caller_idx: Option<usize>, module_idx: usize) -> Self {
        Frame {
            regs: [value_layout::TAG_UNIT; 256],
            pc: 0,
            module_idx,
            return_dst: 0,
            caller_idx,
            closure_env: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HandlerFrame {
    pub handler_table_idx: usize,
    pub module_idx: usize,
    pub resume_pc: usize,
    pub resume_dst: u8,
    pub saved_regs: Option<[Value; 256]>,
    pub saved_pc: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinEffect {
    IOPrint,
    IORead,
}

impl BuiltinEffect {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "IO.print" => Some(BuiltinEffect::IOPrint),
            "IO.read" => Some(BuiltinEffect::IORead),
            _ => None,
        }
    }
}

pub struct CoreVM {
    pub modules: Vec<CodeModule>,
    pub frames: Vec<Frame>,
    pub handler_stack: Vec<HandlerFrame>,
    pub closure_envs: Vec<ClosureEnv>,
    pub strings: Vec<String>,
    pub halted: bool,
    pub exit_code: i64,
    /// Optional capture sink for `IO.print` output. When set (e.g. by the
    /// browser playground), printed lines are appended to the buffer
    /// *instead of* going to process stdout.
    pub output_sink: Option<std::rc::Rc<std::cell::RefCell<String>>>,
}

impl CoreVM {
    pub fn new() -> Self {
        CoreVM {
            modules: Vec::new(),
            frames: Vec::new(),
            handler_stack: Vec::new(),
            closure_envs: Vec::new(),
            strings: Vec::new(),
            halted: false,
            exit_code: 0,
            output_sink: None,
        }
    }

    /// Emit one line of `IO.print` output: to the capture sink when one is
    /// installed (browser playground), otherwise to process stdout.
    fn emit_print(&self, line: &str) {
        if let Some(sink) = &self.output_sink {
            let mut buf = sink.borrow_mut();
            buf.push_str(line);
            buf.push('\n');
        } else {
            println!("{line}");
        }
    }

    pub fn load_nbc(&mut self, data: &[u8]) -> Result<usize, String> {
        let artifact = <crate::bytecode::CodeModule>::from_nbc(data)
            .map_err(|e| format!("Failed to load .nbc: {e}"))?;
        let module_idx = self.modules.len();
        self.modules.push(artifact.module);
        Ok(module_idx)
    }

    pub fn load_module_from_code(&mut self, module: &CodeModule) -> Result<usize, String> {
        let module_idx = self.modules.len();
        self.modules.push(module.clone());
        Ok(module_idx)
    }

    /// Intern a string into the VM's runtime pool, returning its pool index
    /// as a `TAG_STRING` value.
    fn intern_string(&mut self, s: &str) -> Value {
        let idx = self
            .strings
            .iter()
            .position(|existing| existing == s)
            .unwrap_or_else(|| {
                self.strings.push(s.to_string());
                self.strings.len() - 1
            });
        value_layout::TAG_STRING | (idx as u64 & value_layout::PAYLOAD_MASK)
    }

    /// Resolve a `TAG_STRING` value to its pool contents, if present.
    fn resolve_string(&self, value: Value) -> Option<&str> {
        if (value & value_layout::TAG_MASK) != value_layout::TAG_STRING {
            return None;
        }
        let idx = (value & value_layout::PAYLOAD_MASK) as usize;
        self.strings.get(idx).map(|s| s.as_str())
    }

    /// Resolve a tagged value to its printable string representation, for
    /// top-level result display. Returns `None` for non-string values.
    pub fn resolve_display_string(&self, value: Value) -> Option<String> {
        self.resolve_string(value).map(|s| s.to_string())
    }

    pub fn run(&mut self, module_idx: usize, entry_pc: usize) -> Result<Value, String> {
        self.run_inner(module_idx, entry_pc)
    }
    fn run_inner(&mut self, module_idx: usize, entry_pc: usize) -> Result<Value, String> {
        let frame = Frame::new(None, module_idx);
        self.frames.push(frame);
        let frame_idx = self.frames.len() - 1;
        self.frames[frame_idx].pc = entry_pc;
        self.halted = false;

        loop {
            if self.halted || self.frames.is_empty() {
                break;
            }
            // Always use the topmost frame for PC/instr fetch
            let top = self.frames.len() - 1;
            let module_idx = self.frames[top].module_idx;
            let pc = self.frames[top].pc;

            let instr = self.modules[module_idx]
                .instructions
                .get(pc)
                .copied()
                .unwrap_or(Instruction::new0(OpCode::Halt));

            let keep_going = self.step(top, instr)?;
            if !keep_going {
                self.halted = true;
                break;
            }

            // Advance PC on the frame that was active during step
            // (step_call may have pushed new frames, so use `top` not a fresh lookup)
            if top < self.frames.len() {
                self.frames[top].pc += 1;
            }
        }

        let result = self
            .frames
            .last()
            .map(|f| f.regs[0])
            .unwrap_or(value_layout::TAG_UNIT);
        if value_layout::is_int_raw(result) {
            self.exit_code = value_layout::as_int_raw(result);
        }
        Ok(result)
    }

    fn step(&mut self, frame_idx: usize, instr: Instruction) -> Result<bool, String> {
        self.step_inner(frame_idx, instr)
    }
    fn step_inner(&mut self, frame_idx: usize, instr: Instruction) -> Result<bool, String> {
        let opcode = instr.opcode;
        let op1 = instr.op1;
        let op2 = instr.op2;
        let op3 = instr.op3;
        let imm16 = instr.imm16();
        let offset16 = instr.offset16();

        match opcode {
            OpCode::Nop => {}

            OpCode::Halt => return Ok(false),

            OpCode::Const0 => self.frames[frame_idx].regs[op1 as usize] = value_layout::tag_int(0),
            OpCode::Const1 => self.frames[frame_idx].regs[op1 as usize] = value_layout::tag_int(1),
            OpCode::Const2 => self.frames[frame_idx].regs[op1 as usize] = value_layout::tag_int(2),
            OpCode::ConstM1 => {
                self.frames[frame_idx].regs[op1 as usize] = value_layout::tag_int(-1)
            }

            OpCode::ConstU => {
                let idx = imm16 as usize;
                let module_idx = self.frames[frame_idx].module_idx;
                let string_slot = match self.modules[module_idx].constants.get(idx) {
                    Some(Constant::String(s)) => Some(s.clone()),
                    _ => None,
                };
                let val = match string_slot {
                    Some(s) => self.intern_string(&s),
                    None => match self.modules[module_idx].constants.get(idx) {
                        Some(Constant::Int(n)) => value_layout::tag_int(*n),
                        Some(Constant::Bool(b)) => value_layout::tag_bool(*b),
                        Some(Constant::Nil) => value_layout::TAG_NIL,
                        Some(Constant::Unit) => value_layout::TAG_UNIT,
                        _ => value_layout::TAG_NIL,
                    },
                };
                self.frames[frame_idx].regs[op3 as usize] = val;
            }

            OpCode::Move | OpCode::Load | OpCode::Store | OpCode::Dup => {
                let src = self.frames[frame_idx].regs[op1 as usize];
                self.frames[frame_idx].regs[op2 as usize] = src;
            }
            OpCode::Swap => {
                let a = op1 as usize;
                let b = op2 as usize;
                let tmp = self.frames[frame_idx].regs[a];
                self.frames[frame_idx].regs[a] = self.frames[frame_idx].regs[b];
                self.frames[frame_idx].regs[b] = tmp;
            }

            OpCode::IAdd => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] =
                    value_layout::tag_int(a.wrapping_add(b));
            }
            OpCode::ISub => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] =
                    value_layout::tag_int(a.wrapping_sub(b));
            }
            OpCode::IMul => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] =
                    value_layout::tag_int(a.wrapping_mul(b));
            }
            OpCode::IDiv => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] = if b == 0 {
                    value_layout::TAG_NIL
                } else {
                    value_layout::tag_int(a / b)
                };
            }
            OpCode::IMod => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] = if b == 0 {
                    value_layout::TAG_NIL
                } else {
                    value_layout::tag_int(a % b)
                };
            }
            OpCode::INeg => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                self.frames[frame_idx].regs[op2 as usize] = value_layout::tag_int(-a);
            }
            OpCode::IInc => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                self.frames[frame_idx].regs[op1 as usize] = value_layout::tag_int(a + 1);
            }
            OpCode::IDec => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                self.frames[frame_idx].regs[op1 as usize] = value_layout::tag_int(a - 1);
            }
            OpCode::Shl => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] =
                    value_layout::tag_int(a << (b as u32).min(63));
            }
            OpCode::Shr => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] =
                    value_layout::tag_int(a >> (b as u32).min(63));
            }

            OpCode::ICmpEq => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] = value_layout::tag_bool(a == b);
            }
            OpCode::ICmpLt => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] = value_layout::tag_bool(a < b);
            }
            OpCode::ICmpGt => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] = value_layout::tag_bool(a > b);
            }
            OpCode::ICmpLe => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] = value_layout::tag_bool(a <= b);
            }
            OpCode::ICmpGe => {
                let a = value_layout::as_int_raw(self.frames[frame_idx].regs[op1 as usize]);
                let b = value_layout::as_int_raw(self.frames[frame_idx].regs[op2 as usize]);
                self.frames[frame_idx].regs[op3 as usize] = value_layout::tag_bool(a >= b);
            }
            OpCode::Not => {
                let val = self.frames[frame_idx].regs[op1 as usize];
                let truthy = (val & value_layout::PAYLOAD_MASK) != 0;
                self.frames[frame_idx].regs[op2 as usize] = value_layout::tag_bool(!truthy);
            }
            OpCode::And => {
                let a =
                    (self.frames[frame_idx].regs[op1 as usize] & value_layout::PAYLOAD_MASK) != 0;
                let b =
                    (self.frames[frame_idx].regs[op2 as usize] & value_layout::PAYLOAD_MASK) != 0;
                self.frames[frame_idx].regs[op3 as usize] = value_layout::tag_bool(a && b);
            }
            OpCode::Or => {
                let a =
                    (self.frames[frame_idx].regs[op1 as usize] & value_layout::PAYLOAD_MASK) != 0;
                let b =
                    (self.frames[frame_idx].regs[op2 as usize] & value_layout::PAYLOAD_MASK) != 0;
                self.frames[frame_idx].regs[op3 as usize] = value_layout::tag_bool(a || b);
            }

            OpCode::Jmp => {
                self.frames[frame_idx].pc =
                    (self.frames[frame_idx].pc as i64 + imm16 as i16 as i64 - 1) as usize;
            }
            OpCode::JmpT => {
                if (self.frames[frame_idx].regs[op1 as usize] & value_layout::PAYLOAD_MASK) != 0 {
                    self.frames[frame_idx].pc =
                        (self.frames[frame_idx].pc as i64 + offset16 as i64 - 1) as usize;
                }
            }
            OpCode::JmpF => {
                if (self.frames[frame_idx].regs[op1 as usize] & value_layout::PAYLOAD_MASK) == 0 {
                    self.frames[frame_idx].pc =
                        (self.frames[frame_idx].pc as i64 + offset16 as i64 - 1) as usize;
                }
            }

            OpCode::Call => self.step_call(frame_idx, instr)?,
            OpCode::RetVal | OpCode::Ret => {
                let ret_val = if opcode == OpCode::RetVal {
                    self.frames[frame_idx].regs[op1 as usize]
                } else {
                    self.frames[frame_idx].regs[0]
                };
                if let Some(caller_idx) = self.frames[frame_idx].caller_idx {
                    let dst = self.frames[frame_idx].return_dst;
                    self.frames[caller_idx].regs[dst as usize] = ret_val;
                    self.frames.pop();
                } else {
                    self.frames[frame_idx].regs[0] = ret_val;
                    return Ok(false);
                }
            }

            OpCode::Closure => {
                // Immediate closure: payload is the function-table index.
                self.frames[frame_idx].regs[op3 as usize] = value_layout::tag_closure(imm16 as u64);
            }
            OpCode::CapStore => {
                let closure_val = self.frames[frame_idx].regs[op1 as usize];
                let slot = op2 as usize;
                let src = self.frames[frame_idx].regs[op3 as usize];
                if (closure_val & value_layout::TAG_MASK) != value_layout::TAG_CLOSURE {
                    return Err("CapStore target is not a closure".to_string());
                }
                let payload = closure_val & value_layout::PAYLOAD_MASK;
                let env_idx = if payload & CLOSURE_ENV_FLAG != 0 {
                    (payload & CLOSURE_ENV_IDX_MASK) as usize
                } else {
                    // First capture on this closure: allocate an env and point
                    // the closure value at it.
                    let idx = self.closure_envs.len();
                    self.closure_envs.push(ClosureEnv {
                        func_idx: payload as usize,
                        captures: Vec::new(),
                    });
                    self.frames[frame_idx].regs[op1 as usize] = value_layout::tag_closure(
                        CLOSURE_ENV_FLAG | (idx as u64 & CLOSURE_ENV_IDX_MASK),
                    );
                    idx
                };
                let env = &mut self.closure_envs[env_idx];
                if env.captures.len() <= slot {
                    env.captures.resize(slot + 1, value_layout::TAG_NIL);
                }
                env.captures[slot] = src;
            }
            OpCode::CapLoad => {
                let slot = op1 as usize;
                let dst = op2 as usize;
                let env_val = self.frames[frame_idx]
                    .closure_env
                    .ok_or_else(|| "CapLoad outside a closure call".to_string())?;
                let payload = env_val & value_layout::PAYLOAD_MASK;
                if payload & CLOSURE_ENV_FLAG == 0 {
                    return Err("CapLoad in a closure without captures".to_string());
                }
                let env_idx = (payload & CLOSURE_ENV_IDX_MASK) as usize;
                let value = self
                    .closure_envs
                    .get(env_idx)
                    .and_then(|env| env.captures.get(slot))
                    .copied()
                    .ok_or_else(|| format!("CapLoad of missing capture slot {slot}"))?;
                self.frames[frame_idx].regs[dst] = value;
            }
            OpCode::ClosureCall => {
                let closure_val = self.frames[frame_idx].regs[op1 as usize];
                let argc = op2;
                let dst = op3;
                let (func_idx, closure_env) = self.resolve_function(closure_val)?;
                let module_idx = self.frames[frame_idx].module_idx;
                let code_offset = self.modules[module_idx]
                    .function_table
                    .get(func_idx)
                    .copied()
                    .ok_or_else(|| format!("Function {} not found", func_idx))?;
                let mut new_frame = Frame::new(Some(frame_idx), module_idx);
                new_frame.pc = code_offset;
                for i in 0..(argc as usize).min(256) {
                    new_frame.regs[i] = self.frames[frame_idx].regs[i];
                }
                new_frame.return_dst = dst;
                new_frame.closure_env = closure_env;
                self.frames.push(new_frame);
            }

            OpCode::Handle => {
                self.handler_stack.push(HandlerFrame {
                    handler_table_idx: op1 as usize,
                    module_idx: self.frames[frame_idx].module_idx,
                    resume_pc: self.frames[frame_idx].pc,
                    resume_dst: op2,
                    saved_regs: None,
                    saved_pc: None,
                });
            }

            OpCode::Perform | OpCode::PerformDirect => self.step_perform(frame_idx, instr)?,

            OpCode::Resume => {
                let val = self.frames[frame_idx].regs[op1 as usize];
                if let Some(hf) = self
                    .handler_stack
                    .iter_mut()
                    .rev()
                    .find(|h| h.saved_regs.is_some())
                {
                    if let Some(regs) = hf.saved_regs.take() {
                        self.frames[frame_idx].regs = regs;
                        self.frames[frame_idx].regs[hf.resume_dst as usize] = val;
                        self.frames[frame_idx].pc = hf.saved_pc.unwrap_or(0);
                        return Ok(true);
                    }
                }
                return Err("resume called without a captured continuation".to_string());
            }

            OpCode::SConcat => {
                let a = self.frames[frame_idx].regs[op1 as usize];
                let b = self.frames[frame_idx].regs[op2 as usize];
                let sa = self.resolve_string(a).unwrap_or("").to_string();
                let sb = self.resolve_string(b).unwrap_or("").to_string();
                let result = format!("{sa}{sb}");
                self.frames[frame_idx].regs[op3 as usize] = self.intern_string(&result);
            }

            _ => {} // unsupported opcodes are no-ops in core VM
        }
        Ok(true)
    }

    /// Resolve a function value to a `(function_table_index, closure_env)`.
    fn resolve_function(&self, func_val: Value) -> Result<(usize, Option<Value>), String> {
        if value_layout::is_int_raw(func_val) {
            Ok((value_layout::as_int_raw(func_val) as usize, None))
        } else if (func_val & value_layout::TAG_MASK) == value_layout::TAG_CLOSURE {
            let payload = func_val & value_layout::PAYLOAD_MASK;
            if payload & CLOSURE_ENV_FLAG != 0 {
                // Env-carrying closure: the function index lives in the env.
                let env_idx = (payload & CLOSURE_ENV_IDX_MASK) as usize;
                let func_idx = self
                    .closure_envs
                    .get(env_idx)
                    .map(|env| env.func_idx)
                    .ok_or_else(|| format!("Dangling closure environment {env_idx}"))?;
                Ok((func_idx, Some(func_val)))
            } else {
                // Immediate closure: the payload is the function index.
                Ok((payload as usize, Some(func_val)))
            }
        } else {
            Err(format!("Not a function: {func_val:#x}"))
        }
    }

    fn step_call(&mut self, frame_idx: usize, instr: Instruction) -> Result<(), String> {
        let func_val = self.frames[frame_idx].regs[instr.op1 as usize];
        let argc = instr.op2;
        let dst = instr.op3;
        let (func_idx, closure_env) = self.resolve_function(func_val)?;
        let module_idx = self.frames[frame_idx].module_idx;
        let code_offset = self.modules[module_idx]
            .function_table
            .get(func_idx)
            .copied()
            .ok_or_else(|| format!("Function {func_idx} not found"))?;
        let mut new_frame = Frame::new(Some(frame_idx), module_idx);
        new_frame.pc = code_offset;
        for i in 0..(argc as usize).min(256) {
            new_frame.regs[i] = self.frames[frame_idx].regs[i];
        }
        new_frame.return_dst = dst;
        new_frame.closure_env = closure_env;
        self.frames.push(new_frame);
        Ok(())
    }

    fn step_perform(&mut self, frame_idx: usize, instr: Instruction) -> Result<(), String> {
        let module_idx = self.frames[frame_idx].module_idx;
        let eff_name = if instr.opcode == OpCode::PerformDirect {
            let table_idx = instr.op1 as usize;
            let binding_idx = instr.op2 as usize;
            let module = &self.modules[module_idx];
            let table = module
                .handler_tables
                .get(table_idx)
                .ok_or_else(|| format!("Handler table {table_idx} not found"))?;
            let binding = table
                .bindings
                .get(binding_idx)
                .ok_or_else(|| format!("Binding {binding_idx} not found"))?;
            binding.effect_name.clone()
        } else {
            let const_idx = instr.imm16() as usize;
            match self.modules[module_idx].constants.get(const_idx) {
                Some(Constant::String(s)) => s.clone(),
                _ => return Err("Effect name not found in constants".to_string()),
            }
        };

        if let Some(builtin) = BuiltinEffect::from_name(&eff_name) {
            match builtin {
                BuiltinEffect::IOPrint => {
                    let val = self.frames[frame_idx].regs[0];
                    if value_layout::is_int_raw(val) {
                        self.emit_print(&value_layout::as_int_raw(val).to_string());
                    } else if val == value_layout::TAG_NIL {
                        self.emit_print("nil");
                    } else if (val & value_layout::TAG_MASK) == value_layout::TAG_BOOL {
                        self.emit_print(if (val & 1) != 0 { "true" } else { "false" });
                    } else if let Some(s) = self.resolve_string(val) {
                        self.emit_print(s);
                    } else {
                        self.emit_print(&format!("<value:{val:#x}>"));
                    }
                    self.frames[frame_idx].regs[instr.op3 as usize] = value_layout::TAG_UNIT;
                }
                BuiltinEffect::IORead => {
                    use std::io::BufRead;
                    let stdin = std::io::stdin();
                    let mut line = String::new();
                    stdin
                        .lock()
                        .read_line(&mut line)
                        .map_err(|e| format!("IO.read: {e}"))?;
                    if line.ends_with('\n') {
                        line.pop();
                    }
                    if line.ends_with('\r') {
                        line.pop();
                    }
                    self.frames[frame_idx].regs[instr.op3 as usize] = self.intern_string(&line);
                }
            }
            return Ok(());
        }

        match eff_name.as_str() {
            "String.charAt" => {
                let s_val = self.frames[frame_idx].regs[0];
                let idx_val = self.frames[frame_idx].regs[1];
                let char_idx = value_layout::as_int_raw(idx_val) as usize;
                let s = self
                    .resolve_string(s_val)
                    .ok_or("String.charAt: string not found")?;
                let ch = s.chars().nth(char_idx).unwrap_or('\0');
                self.frames[frame_idx].regs[instr.op3 as usize] = value_layout::tag_int(ch as i64);
            }
            "String.length" => {
                let s_val = self.frames[frame_idx].regs[0];
                let s = self
                    .resolve_string(s_val)
                    .ok_or("String.length: string not found")?;
                self.frames[frame_idx].regs[instr.op3 as usize] =
                    value_layout::tag_int(s.len() as i64);
            }
            "Int.to_string" => {
                let val = value_layout::as_int_raw(self.frames[frame_idx].regs[0]);
                self.frames[frame_idx].regs[instr.op3 as usize] =
                    self.intern_string(&val.to_string());
            }
            "String.from_char" => {
                let ch = value_layout::as_int_raw(self.frames[frame_idx].regs[0]);
                let s = char::from_u32(ch as u32)
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                self.frames[frame_idx].regs[instr.op3 as usize] = self.intern_string(&s);
            }
            _ => return Err(format!("Unhandled effect: {eff_name}")),
        }
        Ok(())
    }
}

impl Default for CoreVM {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir_lower;
    use crate::lexer::Lexer;
    use crate::mir_codegen;
    use crate::mir_lower;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;

    /// Compile a source string through the full frontend pipeline and run it
    /// on the Core VM, returning the VM (for string-pool resolution) and the
    /// result value.
    fn run_core_vm(source: &str) -> Result<(CoreVM, Value), String> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().map_err(|e| e.to_string())?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().map_err(|e| e.to_string())?;
        let mut tc = TypeChecker::new();
        tc.check_module(&ast).map_err(|e| e.to_string())?;
        let hir = hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir = mir_lower::lower_module(&hir).map_err(|e| e.to_string())?;
        let module = mir_codegen::compile_mir(&mut mir, "test").map_err(|e| e.to_string())?;
        let mut vm = CoreVM::new();
        let idx = vm.load_module_from_code(&module)?;
        let entry = module.entry_point.unwrap_or(0);
        let value = vm.run(idx, entry)?;
        Ok((vm, value))
    }

    fn run_core(source: &str) -> Result<Value, String> {
        run_core_vm(source).map(|(_vm, v)| v)
    }

    fn assert_int(source: &str, expected: i64) {
        let v = run_core(source).unwrap_or_else(|e| panic!("{source}: {e}"));
        assert_eq!(value_layout::as_int_raw(v), expected, "source: {source}");
    }

    #[test]
    fn test_core_arith() {
        assert_int("1 + 2 * 3", 7);
        assert_int("(1 + 2) * 3", 9);
        assert_int("10 - 3", 7);
        assert_int("10 / 3", 3);
        assert_int("10 % 3", 1);
        assert_int("-5", -5);
    }

    #[test]
    fn test_core_control_flow() {
        assert_int("if 3 > 2 then 100 else 200", 100);
        assert_int("if 1 < 2 then 100 else 200", 100);
        assert_int("let x = 5 in x * 2", 10);
        assert_int("let x = 5 in let y = 10 in x + y", 15);
        assert_int("let x = 5 in let y = x + 1 in let z = y * 2 in z", 12);
    }

    #[test]
    fn test_core_bool() {
        for (src, exp) in [
            ("7 > 3", true),
            ("7 <= 7", true),
            ("1 == 2", false),
            ("not false", true),
            ("not true", false),
            ("true and false", false),
            ("true or false", true),
        ] {
            let v = run_core(src).unwrap_or_else(|e| panic!("{src}: {e}"));
            assert_eq!(value_layout::tag_bool(exp), v, "source: {src}");
        }
    }

    #[test]
    fn test_core_recursion() {
        assert_int(
            "let rec fact = fn(n) if n <= 1 then 1 else n * fact(n - 1) in fact(5)",
            120,
        );
        assert_int(
            "let rec fib = fn(n) if n < 2 then n else fib(n - 1) + fib(n - 2) in fib(10)",
            55,
        );
        assert_int(
            "let rec count = fn(n) if n == 0 then 0 else count(n - 1) + 1 in count(5)",
            5,
        );
    }

    #[test]
    fn test_core_closures() {
        assert_int("(fn(x) x + 1)(41)", 42);
        assert_int("let f = fn(x) x * x in f(6)", 36);
        assert_int("let add = fn(x) fn(y) x + y in add(3)(4)", 7);
        assert_int("let x = 10 in let f = fn(y) x + y in f(5)", 15);
        assert_int(
            "let x = 10 in let f = fn(y) x + y in let g = fn(y) x * y in f(1) + g(2)",
            31,
        );
        assert_int(
            "let sum = fn(n) let rec loop = fn(i, acc) if i > n then acc else loop(i + 1, acc + i) in loop(0, 0) in sum(100)",
            5050,
        );
    }

    #[test]
    fn test_core_string_effects() {
        let (vm, v) = run_core_vm("\"hello\" + \" world\"").unwrap();
        assert_eq!(vm.resolve_display_string(v).as_deref(), Some("hello world"));
    }

    #[test]
    fn test_core_nbc_roundtrip() {
        // Build a module, serialize to .nbc, load it back through load_nbc,
        // and confirm the Core VM executes the deserialized artifact.
        let (mut vm, v) = run_core_vm("1 + 2 * 3").unwrap();
        assert_eq!(value_layout::as_int_raw(v), 7);

        let module = vm.modules.pop().unwrap();
        let bytes = module.to_nbc(None).unwrap();
        let mut vm2 = CoreVM::new();
        let idx = vm2.load_nbc(&bytes).unwrap();
        let entry = vm2.modules[idx].entry_point.unwrap_or(0);
        let v2 = vm2.run(idx, entry).unwrap();
        assert_eq!(value_layout::as_int_raw(v2), 7);
    }
}
