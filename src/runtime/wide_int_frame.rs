//! Safe interchange between real VM Frame and scoped ORCA-rooted Int64 values.
//!
//! Production Frame contains a publicly cloneable [Value; 256]. A pointer to
//! a WideInt ORCA allocation cannot safely enter that type until every clone,
//! drop, continuation, container, and JIT boundary installs GC barriers.
//! This numeric-only adapter enforces that invariant at export. It also
//! allows a semantic (pointer-free) scoped snapshot of a wide Int64 frame.
//!
//! No production opcode, v1 bytecode or persistent format changes here.

use super::actor::Actor;
use super::wide_int_registers::{IntRegisterError, RootedIntRegisters};
use crate::value_layout::int48_in_range;
use crate::vm::{Frame, Value};

const VM_REGISTERS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameInt64BoundaryError {
    UnsupportedSpillSlots,
    UnsupportedClosureEnvironment,
    UnsupportedValue { register: usize },
    WideIntegerRequiresNewAbi { register: usize },
    InvalidSnapshotLength,
    Register(IntRegisterError),
}

impl From<IntRegisterError> for FrameInt64BoundaryError {
    fn from(error: IntRegisterError) -> Self {
        Self::Register(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameControl {
    module_idx: usize,
    pc: usize,
    return_dst: u8,
    caller_idx: Option<usize>,
}

impl FrameControl {
    fn new(module_idx: usize) -> Self {
        Self {
            module_idx,
            pc: 0,
            return_dst: 0,
            caller_idx: None,
        }
    }

    fn from_frame(frame: &Frame) -> Self {
        Self {
            module_idx: frame.module_idx,
            pc: frame.pc,
            return_dst: frame.return_dst,
            caller_idx: frame.caller_idx,
        }
    }
}

/// Pointer-free semantic register contents. This is **not** a wire format:
/// it has no format version and must not be persisted by durable runtimes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameInt64Snapshot {
    control: FrameControl,
    values: Vec<Option<i64>>,
}

/// A single VM-shaped, numeric-only frame rooted in one actor's ORCA heap.
/// The borrowed heap is exclusive and each wide register owns one local ref.
pub struct RootedIntFrame<'a> {
    control: FrameControl,
    regs: RootedIntRegisters<'a>,
}

impl<'a> RootedIntFrame<'a> {
    pub fn new(actor: &'a mut Actor, module_idx: usize) -> Self {
        Self {
            control: FrameControl::new(module_idx),
            regs: RootedIntRegisters::new(actor, VM_REGISTERS),
        }
    }

    /// Import a real frame only if all its values have unambiguous numeric
    /// semantics and there are no hidden roots in spills or closure captures.
    /// Validate *before* borrowing the actor's heap to avoid partial effects.
    pub fn try_import(
        actor: &'a mut Actor,
        frame: &Frame,
    ) -> Result<Self, FrameInt64BoundaryError> {
        if !frame.spilled.is_empty() {
            return Err(FrameInt64BoundaryError::UnsupportedSpillSlots);
        }
        if frame.closure_env.is_some() {
            return Err(FrameInt64BoundaryError::UnsupportedClosureEnvironment);
        }
        for (index, value) in frame.regs.iter().enumerate() {
            if !value.is_nil() && value.as_int().is_none() {
                return Err(FrameInt64BoundaryError::UnsupportedValue { register: index });
            }
        }
        let mut owned = Self {
            control: FrameControl::from_frame(frame),
            regs: RootedIntRegisters::new(actor, VM_REGISTERS),
        };
        for (index, value) in frame.regs.iter().enumerate() {
            if let Some(number) = value.as_int() {
                owned.regs.store(index, number)?;
            }
        }
        Ok(owned)
    }

    /// Export to the existing, freely cloneable production Frame only if
    /// every integer fits its original 48-bit inline payload.
    /// In particular, never manufacture a wide-Int pointer in Frame.regs.
    pub fn try_export(&self) -> Result<Frame, FrameInt64BoundaryError> {
        let mut frame = Frame::new(self.control.caller_idx, self.control.module_idx);
        frame.pc = self.control.pc;
        frame.return_dst = self.control.return_dst;
        for index in 0..VM_REGISTERS {
            if let Some(number) = self.regs.read(index)? {
                if !int48_in_range(number) {
                    return Err(FrameInt64BoundaryError::WideIntegerRequiresNewAbi {
                        register: index,
                    });
                }
                frame.regs[index] = Value::try_int(number)
                    .expect("an in-range signed integer has an inline encoding");
            }
        }
        Ok(frame)
    }

    pub fn read(&self, register: usize) -> Result<Option<i64>, IntRegisterError> {
        self.regs.read(register)
    }

    pub fn store(&mut self, register: usize, number: i64) -> Result<(), IntRegisterError> {
        self.regs.store(register, number)
    }

    pub fn checked_add_into(
        &mut self,
        dst: usize,
        left: usize,
        right: usize,
    ) -> Result<(), IntRegisterError> {
        self.regs.checked_add_into(dst, left, right)
    }

    /// Capture portable numeric *values*, not raw tagged pointer bits.
    /// No VM frame clone, ORCA pointer or non-versioned durable artifact
    /// escapes from this helper.
    pub fn snapshot(&self) -> Result<FrameInt64Snapshot, IntRegisterError> {
        let mut values = Vec::with_capacity(VM_REGISTERS);
        for index in 0..VM_REGISTERS {
            values.push(self.regs.read(index)?);
        }
        Ok(FrameInt64Snapshot {
            control: self.control,
            values,
        })
    }

    /// Reconstitute fresh ORCA roots from semantic integer values. Any
    /// allocation failure drops the partially constructed bank, releasing
    /// roots installed before the error.
    pub fn try_restore(
        actor: &'a mut Actor,
        snapshot: &FrameInt64Snapshot,
    ) -> Result<Self, FrameInt64BoundaryError> {
        if snapshot.values.len() != VM_REGISTERS {
            return Err(FrameInt64BoundaryError::InvalidSnapshotLength);
        }
        let mut frame = Self {
            control: snapshot.control,
            regs: RootedIntRegisters::new(actor, VM_REGISTERS),
        };
        for (index, value) in snapshot.values.iter().enumerate() {
            if let Some(number) = value {
                frame.regs.store(index, *number)?;
            }
        }
        Ok(frame)
    }
}
