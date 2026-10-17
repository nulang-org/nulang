//! Actor-scoped Int64 frame stack with explicit ORCA root lifetimes.
//!
//! This module is a staging model of production frame push/clone/return.
//! It never exposes tagged pointers, is not the VM's live `Vec<Frame>`,
//! and must not be serialized as an NLCS v1 continuation.

use super::actor::Actor;
use super::wide_int_registers::{IntRegisterError, RootedIntRegisters};
use crate::int64_semantics;

const FRAME_REGISTERS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntFrameStackError {
    EmptyStack,
    InvalidFrame,
    InvalidCheckpoint,
    Register(IntRegisterError),
}

impl From<IntRegisterError> for IntFrameStackError {
    fn from(value: IntRegisterError) -> Self {
        Self::Register(value)
    }
}

#[derive(Debug, Clone, Copy)]
struct FrameMetadata {
    #[allow(dead_code)]
    module_idx: usize,
}

/// Detached semantic representation of a rooted numeric frame stack.
///
/// Contains signed numbers and module indices, never NaN-boxed pointer bits.
/// This is an **in-process checkpoint**, not a versioned durability ABI.
/// The snapshot can outlive its source actor safely because it holds no
/// references to that actor's heap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootedIntStackCheckpoint {
    frames: Vec<(usize, Vec<Option<i64>>)>,
}

/// A single actor-owned contiguous register bank partitioned into stack
/// frames. Each occupied wide register owns exactly one ORCA local ref.
///
/// Unlike the legacy `Frame` (which is freely cloneable), this type clones
/// one frame only by retaining all live wide-object references. Returning
/// or unwinding a frame releases all references in that frame.
pub struct RootedIntFrameStack<'a> {
    registers: RootedIntRegisters<'a>,
    frames: Vec<FrameMetadata>,
}

impl<'a> RootedIntFrameStack<'a> {
    pub fn new(actor: &'a mut Actor) -> Self {
        Self {
            registers: RootedIntRegisters::new(actor, 0),
            frames: Vec::new(),
        }
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Enter a new nil-initialized frame. Existing roots are unaffected.
    pub fn push_empty(&mut self, module_idx: usize) -> usize {
        let index = self.frames.len();
        self.registers.append_nil_slots(FRAME_REGISTERS);
        self.frames.push(FrameMetadata { module_idx });
        index
    }

    /// Model capturing/copying a VM frame by creating independent local
    /// roots for every wide integer. No raw Value pointer is duplicated
    /// without a preceding ORCA local_ref barrier.
    pub fn clone_top(&mut self) -> Result<usize, IntFrameStackError> {
        let original = self
            .frames
            .last()
            .copied()
            .ok_or(IntFrameStackError::EmptyStack)?;
        let previous_base = (self.frames.len() - 1) * FRAME_REGISTERS;
        let index = self.push_empty(original.module_idx);
        let new_base = index * FRAME_REGISTERS;
        for reg in 0..FRAME_REGISTERS {
            // These indices are in bounds; copying nil/inline values cannot
            // fail, and copying a wide object retains before overwriting.
            self.registers
                .copy_into(new_base + reg, previous_base + reg)?;
        }
        Ok(index)
    }

    /// Suspend this staging stack to detached numeric values. Ownership of
    /// the frame stack is consumed; its ORCA roots are all dropped as the
    /// method returns. No raw pointer or legacy VM Frame may escape.
    pub fn suspend(self) -> Result<RootedIntStackCheckpoint, IntFrameStackError> {
        let mut frames = Vec::with_capacity(self.frames.len());
        for (index, meta) in self.frames.iter().enumerate() {
            let base = index * FRAME_REGISTERS;
            let mut values = Vec::with_capacity(FRAME_REGISTERS);
            for slot in 0..FRAME_REGISTERS {
                values.push(self.registers.read(base + slot)?);
            }
            frames.push((meta.module_idx, values));
        }
        Ok(RootedIntStackCheckpoint { frames })
    }

    /// Rebuild independent ORCA roots on the *destination* actor. A failed
    /// allocation unwinds the partially constructed stack automatically.
    /// The checkpoint can be reused without sharing heap pointers.
    pub fn resume(
        actor: &'a mut Actor,
        checkpoint: &RootedIntStackCheckpoint,
    ) -> Result<Self, IntFrameStackError> {
        // Validate the complete checkpoint before allocating any roots.
        if checkpoint
            .frames
            .iter()
            .any(|(_, values)| values.len() != FRAME_REGISTERS)
        {
            return Err(IntFrameStackError::InvalidCheckpoint);
        }
        let mut stack = Self::new(actor);
        for (module_idx, values) in &checkpoint.frames {
            let frame = stack.push_empty(*module_idx);
            for (reg, value) in values.iter().enumerate() {
                if let Some(number) = value {
                    stack.store(frame, reg, *number)?;
                }
            }
        }
        Ok(stack)
    }

    /// Return/unwind the top frame, releasing every root it owns.
    pub fn pop(&mut self) -> Result<(), IntFrameStackError> {
        if self.frames.pop().is_none() {
            return Err(IntFrameStackError::EmptyStack);
        }
        self.registers
            .unwind_to(self.frames.len() * FRAME_REGISTERS);
        Ok(())
    }

    /// Resolve a register to one of the contiguous root slots. Bounds
    /// checks precede all mutation to keep invalid operations atomic.
    fn slot(&self, frame: usize, register: usize) -> Result<usize, IntFrameStackError> {
        if frame >= self.frames.len() {
            return Err(IntFrameStackError::InvalidFrame);
        }
        if register >= FRAME_REGISTERS {
            return Err(IntFrameStackError::Register(
                IntRegisterError::InvalidRegister,
            ));
        }
        Ok(frame * FRAME_REGISTERS + register)
    }

    pub fn read(&self, frame: usize, register: usize) -> Result<Option<i64>, IntFrameStackError> {
        Ok(self.registers.read(self.slot(frame, register)?)?)
    }

    pub fn store(
        &mut self,
        frame: usize,
        register: usize,
        number: i64,
    ) -> Result<(), IntFrameStackError> {
        let slot = self.slot(frame, register)?;
        Ok(self.registers.store(slot, number)?)
    }

    pub fn clear(&mut self, frame: usize, register: usize) -> Result<(), IntFrameStackError> {
        let slot = self.slot(frame, register)?;
        Ok(self.registers.clear(slot)?)
    }

    /// Cross-frame root copy, retaining the source before the destination
    /// reference is released. Self-copy cannot invalidate a root.
    pub fn copy_into(
        &mut self,
        dst_frame: usize,
        dst_reg: usize,
        src_frame: usize,
        src_reg: usize,
    ) -> Result<(), IntFrameStackError> {
        let dst = self.slot(dst_frame, dst_reg)?;
        let src = self.slot(src_frame, src_reg)?;
        Ok(self.registers.copy_into(dst, src)?)
    }

    /// Cross-frame move, transferring one root with no refcount increase.
    pub fn move_into(
        &mut self,
        dst_frame: usize,
        dst_reg: usize,
        src_frame: usize,
        src_reg: usize,
    ) -> Result<(), IntFrameStackError> {
        let dst = self.slot(dst_frame, dst_reg)?;
        let src = self.slot(src_frame, src_reg)?;
        Ok(self.registers.move_into(dst, src)?)
    }

    /// Compute a checked signed-64-bit result across active frames.
    /// Mathematical failure or invalid register leaves the destination
    /// unchanged. A result outside the inline range allocates a fresh ORCA
    /// root before replacing the destination.
    pub fn checked_add_into(
        &mut self,
        dst_frame: usize,
        dst_reg: usize,
        lhs_frame: usize,
        lhs_reg: usize,
        rhs_frame: usize,
        rhs_reg: usize,
    ) -> Result<(), IntFrameStackError> {
        let dst = self.slot(dst_frame, dst_reg)?;
        let lhs = self.slot(lhs_frame, lhs_reg)?;
        let rhs = self.slot(rhs_frame, rhs_reg)?;
        let left = self
            .registers
            .read(lhs)?
            .ok_or(IntFrameStackError::Register(
                IntRegisterError::EmptyRegister,
            ))?;
        let right = self
            .registers
            .read(rhs)?
            .ok_or(IntFrameStackError::Register(
                IntRegisterError::EmptyRegister,
            ))?;
        let value = int64_semantics::add(left, right)
            .map_err(|e| IntFrameStackError::Register(IntRegisterError::Arithmetic(e)))?;
        Ok(self.registers.store(dst, value)?)
    }
}
