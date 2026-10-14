//! Opt-in scalar-only bytecode staging for the admitted durable actor path.
//!
//! Safety boundary: scan a *straight-line* behavior before executing any of
//! its bytecode, run it in a private non-JIT VM with detached primitive state,
//! then hand the staged snapshot to the existing epoch-fenced atomic result
//! commit. Unsupported instructions never enter the interpreter.
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Error, ErrorKind};
use std::rc::Rc;

use crate::bytecode::{CodeModule, Constant, OpCode};
use crate::runtime::heap::TypeTag as HeapTypeTag;
use crate::vm::{ActorVmCallbacks, Frame, Value, VM};

use super::{behavior_ownership, PersistedValue, Runtime, StateModel};

const MAX_PURE_INSTRUCTIONS: usize = 4_096;

fn pure_scalar_value(value: &PersistedValue) -> Option<Value> {
    match value {
        PersistedValue::Int(n) => {
            let value = Value::int(*n);
            (value.as_int() == Some(*n)).then_some(value)
        }
        PersistedValue::Float(n) => Some(Value::float(*n)),
        PersistedValue::Bool(b) => Some(Value::bool(*b)),
        PersistedValue::Nil => Some(Value::nil()),
        PersistedValue::Unit => Some(Value::unit()),
        PersistedValue::String(_) | PersistedValue::Actor(_) => None,
    }
}

fn preserve_scalar_type(expected: &PersistedValue, actual: Value) -> Option<PersistedValue> {
    match expected {
        PersistedValue::Int(_) => actual.as_int().map(PersistedValue::Int),
        PersistedValue::Float(_) => actual.as_float().map(PersistedValue::Float),
        PersistedValue::Bool(_) => actual.as_bool().map(PersistedValue::Bool),
        PersistedValue::Nil => actual.is_nil().then_some(PersistedValue::Nil),
        PersistedValue::Unit => actual.is_unit().then_some(PersistedValue::Unit),
        PersistedValue::String(_) | PersistedValue::Actor(_) => None,
    }
}

fn validate_pure_body(
    module: &CodeModule,
    behavior_idx: usize,
    state: &HashMap<String, PersistedValue>,
) -> io::Result<usize> {
    let offset = module
        .behaviors
        .get(behavior_idx)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "compiled behavior missing"))?
        .code_offset;
    let next_boundary = module
        .behaviors
        .iter()
        .map(|behavior| behavior.code_offset)
        .chain(module.function_table.iter().copied())
        .filter(|pc| *pc > offset)
        .min()
        .unwrap_or(module.instructions.len());
    let body = module.instructions.get(offset..next_boundary).ok_or_else(|| {
        Error::new(ErrorKind::InvalidData, "compiled behavior region is invalid")
    })?;
    if body.len() > MAX_PURE_INSTRUCTIONS {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "pure handler exceeds the bounded straight-line execution limit",
        ));
    }
    let mut terminated = false;
    for instr in body {
        match instr.opcode {
            OpCode::Nop
            | OpCode::Const0
            | OpCode::Const1
            | OpCode::Const2
            | OpCode::ConstM1
            | OpCode::Load
            | OpCode::Store
            | OpCode::Move
            | OpCode::IAdd
            | OpCode::ISub
            | OpCode::IMul
            | OpCode::IDiv
            | OpCode::IMod
            | OpCode::INeg
            | OpCode::IInc
            | OpCode::IDec
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
            | OpCode::Or => {}
            OpCode::ConstU => {
                let literal = module.constants.get(instr.imm16() as usize).ok_or_else(|| {
                    Error::new(ErrorKind::InvalidData, "pure turn references missing constant")
                })?;
                let compatible = match literal {
                    Constant::Int(n) => Value::int(*n).as_int() == Some(*n),
                    Constant::Float(_) | Constant::Bool(_) | Constant::Nil | Constant::Unit => true,
                    _ => false,
                };
                if !compatible {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        "pure turn may only load representable scalar constants",
                    ));
                }
            }
            OpCode::StateGet | OpCode::StateSet => {
                let field = module.constants.get(instr.imm16() as usize);
                let Some(Constant::String(field)) = field else {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "pure turn state access lacks a compiler-owned field name",
                    ));
                };
                if !state.contains_key(field) {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("pure turn references undeclared or non-durable field '{field}'"),
                    ));
                }
            }
            OpCode::Ret | OpCode::RetVal | OpCode::Halt => {
                terminated = true;
                break;
            }
            other => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    format!("opcode {other:?} is not eligible for isolated pure execution"),
                ));
            }
        }
    }
    if !terminated {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "pure handler has no return within bounded straight-line region",
        ));
    }
    Ok(offset)
}

#[derive(Debug)]
struct StageState {
    fields: HashMap<String, PersistedValue>,
    error: Option<String>,
}

#[derive(Debug)]
struct PureVmCallbacks {
    state: Rc<RefCell<StageState>>,
}

impl PureVmCallbacks {
    fn violation(&self, reason: &str) {
        self.state.borrow_mut().error = Some(reason.to_string());
    }
}

impl ActorVmCallbacks for PureVmCallbacks {
    fn alloc(&mut self, _size: usize, _tag: HeapTypeTag) -> Option<*mut u8> {
        self.violation("allocation is not allowed in a pure staged turn");
        None
    }

    fn drop_ref(&mut self, _ptr: *mut u8) {
        self.violation("heap reference mutation is not allowed");
    }

    fn retain_ref(&mut self, _ptr: *mut u8) {
        self.violation("heap reference retention is not allowed");
    }

    fn array_len(&self, _ptr: *mut u8) -> Option<usize> {
        None
    }

    fn spawn_actor(
        &mut self,
        _module: &CodeModule,
        _spawn_pc: usize,
        _behavior_idx: usize,
        _init: Vec<(String, Value)>,
    ) -> Value {
        self.violation("actor spawn is not allowed in a pure staged turn");
        Value::nil()
    }

    fn send_message(&mut self, _target: Value, _behavior_id: u16, _args: &[Value]) {
        self.violation("actor send is not allowed in a pure staged turn");
    }

    fn get_state_field(&self, field: &str) -> Value {
        let result = {
            let staged = self.state.borrow();
            staged.fields.get(field).and_then(pure_scalar_value)
        };
        match result {
            Some(value) => value,
            None => {
                self.violation("staged state read is not a declared scalar field");
                Value::nil()
            }
        }
    }

    fn set_state_field(&mut self, field: &str, value: Value) {
        let mut shared = self.state.borrow_mut();
        let updated = shared
            .fields
            .get(field)
            .and_then(|expected| preserve_scalar_type(expected, value));
        match updated {
            Some(updated) => {
                shared.fields.insert(field.to_string(), updated);
            }
            None => {
                shared.error = Some(format!(
                    "staged state write changes the type of field '{field}'"
                ));
            }
        }
    }
}

fn execute_isolated_scalar_handler(
    module: CodeModule,
    behavior_idx: usize,
    args: &[PersistedValue],
    state: HashMap<String, PersistedValue>,
) -> io::Result<HashMap<String, PersistedValue>> {
    let offset = validate_pure_body(&module, behavior_idx, &state)?;
    let shared = Rc::new(RefCell::new(StageState {
        fields: state,
        error: None,
    }));
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    vm.set_actor_callbacks(Box::new(PureVmCallbacks {
        state: Rc::clone(&shared),
    }));
    let mut frame = Frame::new(None, 0);
    frame.pc = offset;
    for (index, arg) in args.iter().enumerate() {
        frame.regs[index] = pure_scalar_value(arg).ok_or_else(|| {
            Error::new(ErrorKind::Unsupported, "pure actor arguments must be scalar")
        })?;
    }
    vm.set_current_frame(frame);
    vm.run_from(0, offset).map_err(|error| {
        Error::new(
            ErrorKind::InvalidData,
            format!("isolated pure actor execution failed: {error}"),
        )
    })?;
    let mut staged = shared.borrow_mut();
    if let Some(error) = staged.error.take() {
        return Err(Error::new(ErrorKind::InvalidData, error));
    }
    Ok(std::mem::take(&mut staged.fields))
}

impl Runtime {
    /// Execute a strictly bounded, straight-line scalar actor handler in a
    /// detached VM, then atomically commit its state before publishing it.
    ///
    /// Deliberately excludes calls, branches, allocations, IO, sends,
    /// algebraic effects, suspension, and any non-durable field access.
    /// Caller must have previously admitted this exact message through the
    /// epoch-fenced command admission API. The existing completion adapter
    /// persists first and only then releases the mailbox fence.
    pub fn execute_and_commit_admitted_pure_typed_actor_turn(
        &mut self,
        actor_id: u64,
        admitted_sequence: u64,
    ) -> io::Result<super::DurableCommit> {
        let actor = self
            .actors
            .get(&actor_id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "actor does not exist"))?;
        if actor.mailbox.admitted_command_sequence() != Some(admitted_sequence)
            || !actor.persistent
            || actor.is_workflow
            || actor.is_agent
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "pure execution requires an exact admitted persistent ordinary actor command",
            ));
        }
        let module = actor
            .bytecode_module
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "compiled actor module is missing"))?;
        let meta = behavior_ownership::actor_meta_for_runtime_name(module, &actor.name)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "compiled actor schema is missing"))?;
        if !meta.persistent {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "compiled actor is not declared persistent",
            ));
        }

        let journal = self.persistence.read_journal(actor_id);
        let mut matching = journal
            .iter()
            .filter(|entry| entry.sequence == admitted_sequence);
        let command = matching.next().ok_or_else(|| {
            Error::new(ErrorKind::InvalidData, "admitted command has no persisted journal entry")
        })?;
        if matching.next().is_some() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "admitted command has ambiguous persisted journal history",
            ));
        }
        let behavior_idx = behavior_ownership::module_behavior_index_for_actor(
            module,
            &actor.name,
            command.behavior_id as usize,
        )
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "journal behavior is not actor-owned"))?;
        let signature = module.behaviors.get(behavior_idx).ok_or_else(|| {
            Error::new(ErrorKind::InvalidInput, "compiled behavior signature is missing")
        })?;
        if signature.param_count != command.payload.len() || command.payload.len() > 256 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "persisted arguments do not match the compiled behavior signature",
            ));
        }
        let mut state = HashMap::new();
        for (field, model) in &actor.state_models {
            if !matches!(model, StateModel::Durable | StateModel::Local) {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "pure staged execution does not support CRDT or event-sourced state",
                ));
            }
            if *model != StateModel::Durable {
                continue;
            }
            let live = actor.state_data.get(field).ok_or_else(|| {
                Error::new(ErrorKind::InvalidData, format!("durable field '{field}' is missing"))
            })?;
            let persisted = actor.persist_value(live);
            let expected = meta
                .state_defaults
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, literal)| literal);
            let compatible = matches!(
                (expected, &persisted),
                (Some(Constant::Int(_)), PersistedValue::Int(_))
                    | (Some(Constant::Float(_)), PersistedValue::Float(_))
                    | (Some(Constant::Bool(_)), PersistedValue::Bool(_))
                    | (Some(Constant::Nil), PersistedValue::Nil)
                    | (Some(Constant::Unit), PersistedValue::Unit)
            );
            // Legacy persistence conversion normalizes unsupported heap
            // pointers to Nil. A staged handler must never treat that lossy
            // conversion as a real primitive snapshot.
            let lossless = preserve_scalar_type(&persisted, *live).as_ref() == Some(&persisted);
            if !compatible || pure_scalar_value(&persisted).is_none() || !lossless {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    format!("durable field '{field}' cannot be staged as a scalar"),
                ));
            }
            state.insert(field.clone(), persisted);
        }
        let staged = execute_isolated_scalar_handler(
            (**module).clone(),
            behavior_idx,
            &command.payload,
            state,
        )?;
        self.commit_admitted_typed_actor_result(
            actor_id,
            admitted_sequence,
            staged,
            vec![],
            vec![],
        )
    }
}
