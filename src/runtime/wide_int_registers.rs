//! Scoped, ORCA-rooted Int64 value registers.
//!
//! This is a VM-shaped **pre-adoption staging layer**, not yet the public
//! bytecode VM register file. It uses the existing 8-byte `Value` word for
//! inline Ints and a tagged ORCA pointer to a `TypeTag::WideInt` payload for
//! full-width Int64 values. Every pointer is rooted in a slot; copy, move,
//! overwrite and drop perform their corresponding GC reference barriers.
//!
//! The bank exclusively borrows its actor's heap and GC. Raw tagged words are
//! never exposed to callers, copied across actors, or persisted as NLCS v1.
//! The future backend migration must preserve these ownership invariants.

use super::actor::Actor;
use super::gc::OrcaGc;
use super::heap::{ActorHeap, TypeTag};
use crate::int64_semantics::{self, Int64Error};
use crate::vm::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntRegisterError {
    InvalidRegister,
    EmptyRegister,
    AllocationFailed,
    PointerUnrepresentable,
    Arithmetic(Int64Error),
}

/// Generic 8-byte-value register bank for the eventual checked Int64 ABI.
///
/// This is a scoped proof of root/retain/release semantics rather than a
/// published VM/wire ABI. It cannot outlive the actor it borrows.
pub struct RootedIntRegisters<'a> {
    heap: &'a mut ActorHeap,
    gc: &'a mut OrcaGc,
    slots: Vec<Value>,
}

impl<'a> RootedIntRegisters<'a> {
    pub fn new(actor: &'a mut Actor, count: usize) -> Self {
        Self {
            heap: &mut actor.heap,
            gc: &mut actor.orca_gc,
            slots: vec![Value::nil(); count],
        }
    }

    fn slot(&self, index: usize) -> Result<Value, IntRegisterError> {
        self.slots
            .get(index)
            .copied()
            .ok_or(IntRegisterError::InvalidRegister)
    }

    /// Read the owned scalar; empty registers return `None`.
    pub fn read(&self, index: usize) -> Result<Option<i64>, IntRegisterError> {
        let value = self.slot(index)?;
        if value.is_nil() {
            return Ok(None);
        }
        if let Some(n) = value.as_int() {
            return Ok(Some(n));
        }
        if let Some(ptr) = value.as_ptr() {
            // SAFETY: the only way for an ORCA pointer to enter a slot is
            // `encode`, which creates a live WideInt allocation on this heap.
            // Copy barriers retain and overwrite/drop barriers release it.
            // The exclusive heap borrow prevents outside mutation while
            // the bank lives.
            let header = unsafe { &*ActorHeap::header_of(ptr) };
            debug_assert_eq!(header.type_tag, TypeTag::WideInt);
            debug_assert_eq!(header.actor_id, self.gc.actor_id());
            debug_assert_eq!(header.payload_size, std::mem::size_of::<i64>());
            // SAFETY: same live and initialized eight-byte payload proof.
            return Ok(Some(unsafe { ptr.cast::<i64>().read() }));
        }
        unreachable!("rooted integer slots contain only nil, Int, or WideInt")
    }

    /// Materialize an owned root. Errors leave existing register contents
    /// unchanged; a failed pointer-width check releases the new allocation.
    fn encode(&mut self, number: i64) -> Result<Value, IntRegisterError> {
        if let Some(inline) = Value::try_int(number) {
            return Ok(inline);
        }
        let ptr = self
            .gc
            .alloc_object(self.heap, std::mem::size_of::<i64>(), TypeTag::WideInt)
            .ok_or(IntRegisterError::AllocationFailed)?;
        if !crate::value_layout::ptr_fits_payload(ptr as u64) {
            // SAFETY: ptr was freshly allocated on precisely this heap/GC.
            unsafe {
                self.gc.drop_local_ref(self.heap, ptr);
            }
            return Err(IntRegisterError::PointerUnrepresentable);
        }
        // SAFETY: freshly allocated, aligned, actor-owned eight-byte payload.
        // The pointer cannot escape this exclusive register-bank borrow.
        unsafe {
            ptr.cast::<i64>().write(number);
            Ok(Value::ptr(ptr))
        }
    }

    fn release(&mut self, value: Value) {
        if let Some(ptr) = value.as_ptr() {
            // SAFETY: each pointer in a slot owns exactly one counted local
            // reference acquired by encode or by the copy_into barrier.
            unsafe {
                self.gc.drop_local_ref(self.heap, ptr);
            }
        }
    }

    pub fn store(&mut self, index: usize, number: i64) -> Result<(), IntRegisterError> {
        if index >= self.slots.len() {
            return Err(IntRegisterError::InvalidRegister);
        }
        let new_value = self.encode(number)?;
        let old = std::mem::replace(&mut self.slots[index], new_value);
        self.release(old);
        Ok(())
    }

    pub fn clear(&mut self, index: usize) -> Result<(), IntRegisterError> {
        if index >= self.slots.len() {
            return Err(IntRegisterError::InvalidRegister);
        }
        let old = std::mem::replace(&mut self.slots[index], Value::nil());
        self.release(old);
        Ok(())
    }

    /// Copy one root, retaining the heap value before releasing the old
    /// destination. Self-copy is a no-op to prevent use-after-free.
    pub fn copy_into(&mut self, dst: usize, src: usize) -> Result<(), IntRegisterError> {
        let source = self.slot(src)?;
        if dst >= self.slots.len() {
            return Err(IntRegisterError::InvalidRegister);
        }
        if dst == src {
            return Ok(());
        }
        if let Some(ptr) = source.as_ptr() {
            // SAFETY: the source slot guarantees a live, actor-owned ORCA root.
            unsafe {
                self.gc.local_ref(self.heap, ptr);
            }
        }
        let old = std::mem::replace(&mut self.slots[dst], source);
        self.release(old);
        Ok(())
    }

    /// Move a root rather than copying it. No retain on the moved source.
    pub fn move_into(&mut self, dst: usize, src: usize) -> Result<(), IntRegisterError> {
        if dst >= self.slots.len() || src >= self.slots.len() {
            return Err(IntRegisterError::InvalidRegister);
        }
        if dst == src {
            return Ok(());
        }
        let moved = std::mem::replace(&mut self.slots[src], Value::nil());
        let old = std::mem::replace(&mut self.slots[dst], moved);
        self.release(old);
        Ok(())
    }

    /// Checked Int64 arithmetic over generic VM-shaped slots. Compute first,
    /// then store; overflow leaves the old destination/root unchanged.
    pub fn checked_add_into(
        &mut self,
        dst: usize,
        left: usize,
        right: usize,
    ) -> Result<(), IntRegisterError> {
        if dst >= self.slots.len() {
            return Err(IntRegisterError::InvalidRegister);
        }
        let a = self.read(left)?.ok_or(IntRegisterError::EmptyRegister)?;
        let b = self.read(right)?.ok_or(IntRegisterError::EmptyRegister)?;
        let result = int64_semantics::add(a, b).map_err(IntRegisterError::Arithmetic)?;
        self.store(dst, result)
    }
}

impl Drop for RootedIntRegisters<'_> {
    fn drop(&mut self) {
        for index in 0..self.slots.len() {
            let value = std::mem::replace(&mut self.slots[index], Value::nil());
            self.release(value);
        }
    }
}
