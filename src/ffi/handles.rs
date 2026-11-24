//! Explicitly owned, generation-checked host buffers for future FFI bindings.
//!
//! These handles are intentionally **not** raw pointer addresses and are not
//! serialized into the Nulang Value ABI. All safe accesses use bounds-checked
//! byte slices. A native pointer may be leased only in an explicit unsafe
//! scope, where the caller must guarantee that native code does not retain the
//! pointer and respects its checked length and permissions.
//!
//! This is an opt-in Rust-side foundation, not a claim that arbitrary native
//! libraries can be made memory-safe in process. Native code may retain raw
//! pointers or corrupt memory outside the provided buffer; isolate untrusted
//! native code in a separate process or sandbox.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REGISTRY_ID: AtomicU64 = AtomicU64::new(1);

/// Capabilities for a registry-owned, fixed-size allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignAccess {
    ReadOnly,
    ReadWrite,
}

/// A registry-scoped handle. It cannot be constructed by safe external code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForeignBufferHandle {
    registry: u64,
    slot: usize,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignMemoryError {
    InvalidHandle,
    InvalidLength,
    PermissionDenied,
    OutOfBounds,
    AllocationFailed,
}

struct Buffer {
    bytes: Box<[u8]>,
    access: ForeignAccess,
}

struct Slot {
    generation: u64,
    buffer: Option<Buffer>,
}

/// Owns fixed-size allocations behind generation-checked, process-local handles.
///
/// Dropping the registry releases its allocations. The registry itself is not
/// Clone, and each instance has a different ownership domain.
pub struct ForeignBufferRegistry {
    registry: u64,
    slots: Vec<Slot>,
}

impl Default for ForeignBufferRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ForeignBufferRegistry {
    #[must_use]
    pub fn new() -> Self {
        // Never reuse a registry identifier after overflow; that would allow a
        // stale handle from an earlier domain to gain accidental authority.
        let registry = NEXT_REGISTRY_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .expect("exhausted foreign memory registry identifiers");
        Self {
            registry,
            slots: Vec::new(),
        }
    }

    /// Copy bytes into a new fixed-size allocation.
    pub fn register(
        &mut self,
        bytes: &[u8],
        access: ForeignAccess,
    ) -> Result<ForeignBufferHandle, ForeignMemoryError> {
        if bytes.is_empty() {
            return Err(ForeignMemoryError::InvalidLength);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| ForeignMemoryError::AllocationFailed)?;
        owned.extend_from_slice(bytes);
        self.insert(Buffer {
            bytes: owned.into_boxed_slice(),
            access,
        })
    }

    /// Create an initialized, fixed-size allocation (never uninitialized C memory).
    pub fn allocate_zeroed(
        &mut self,
        len: usize,
        access: ForeignAccess,
    ) -> Result<ForeignBufferHandle, ForeignMemoryError> {
        if len == 0 {
            return Err(ForeignMemoryError::InvalidLength);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(len)
            .map_err(|_| ForeignMemoryError::AllocationFailed)?;
        owned.resize(len, 0);
        self.insert(Buffer {
            bytes: owned.into_boxed_slice(),
            access,
        })
    }

    fn insert(&mut self, buffer: Buffer) -> Result<ForeignBufferHandle, ForeignMemoryError> {
        // Reuse only vacant, non-retired slots. Exhausted generations are never
        // recycled, preventing a wrapped counter from reviving a stale handle.
        if let Some((slot, vacant)) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| entry.buffer.is_none() && entry.generation != 0)
        {
            vacant.buffer = Some(buffer);
            return Ok(ForeignBufferHandle {
                registry: self.registry,
                slot,
                generation: vacant.generation,
            });
        }
        self.slots
            .try_reserve(1)
            .map_err(|_| ForeignMemoryError::AllocationFailed)?;
        let slot = self.slots.len();
        self.slots.push(Slot {
            generation: 1,
            buffer: Some(buffer),
        });
        Ok(ForeignBufferHandle {
            registry: self.registry,
            slot,
            generation: 1,
        })
    }

    fn buffer(&self, handle: ForeignBufferHandle) -> Result<&Buffer, ForeignMemoryError> {
        if handle.registry != self.registry {
            return Err(ForeignMemoryError::InvalidHandle);
        }
        let entry = self
            .slots
            .get(handle.slot)
            .ok_or(ForeignMemoryError::InvalidHandle)?;
        if handle.generation == 0 || entry.generation != handle.generation {
            return Err(ForeignMemoryError::InvalidHandle);
        }
        entry
            .buffer
            .as_ref()
            .ok_or(ForeignMemoryError::InvalidHandle)
    }

    fn buffer_mut(
        &mut self,
        handle: ForeignBufferHandle,
    ) -> Result<&mut Buffer, ForeignMemoryError> {
        if handle.registry != self.registry {
            return Err(ForeignMemoryError::InvalidHandle);
        }
        let entry = self
            .slots
            .get_mut(handle.slot)
            .ok_or(ForeignMemoryError::InvalidHandle)?;
        if handle.generation == 0 || entry.generation != handle.generation {
            return Err(ForeignMemoryError::InvalidHandle);
        }
        entry
            .buffer
            .as_mut()
            .ok_or(ForeignMemoryError::InvalidHandle)
    }

    fn checked_range(
        offset: usize,
        len: usize,
        total: usize,
    ) -> Result<std::ops::Range<usize>, ForeignMemoryError> {
        let end = offset
            .checked_add(len)
            .ok_or(ForeignMemoryError::OutOfBounds)?;
        if offset > total || end > total {
            return Err(ForeignMemoryError::OutOfBounds);
        }
        Ok(offset..end)
    }

    /// Borrow a range only while the allocation remains live in this registry.
    pub fn read(
        &self,
        handle: ForeignBufferHandle,
        offset: usize,
        len: usize,
    ) -> Result<&[u8], ForeignMemoryError> {
        let buffer = self.buffer(handle)?;
        let range = Self::checked_range(offset, len, buffer.bytes.len())?;
        Ok(&buffer.bytes[range])
    }

    /// Write within a checked range only when the handle grants mutation.
    pub fn write(
        &mut self,
        handle: ForeignBufferHandle,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), ForeignMemoryError> {
        let buffer = self.buffer_mut(handle)?;
        if buffer.access != ForeignAccess::ReadWrite {
            return Err(ForeignMemoryError::PermissionDenied);
        }
        let range = Self::checked_range(offset, bytes.len(), buffer.bytes.len())?;
        buffer.bytes[range].copy_from_slice(bytes);
        Ok(())
    }

    /// Invalidate a handle and release its owned allocation.
    ///
    /// This checks both registry identity and generation, preventing stale
    /// handles from freeing a newer occupant of the same slot.
    pub fn release(&mut self, handle: ForeignBufferHandle) -> Result<(), ForeignMemoryError> {
        if handle.registry != self.registry {
            return Err(ForeignMemoryError::InvalidHandle);
        }
        let entry = self
            .slots
            .get_mut(handle.slot)
            .ok_or(ForeignMemoryError::InvalidHandle)?;
        if handle.generation == 0 || entry.generation != handle.generation || entry.buffer.is_none()
        {
            return Err(ForeignMemoryError::InvalidHandle);
        }
        entry.buffer = None;
        // Generation zero retires an exhausted slot forever.
        entry.generation = entry.generation.checked_add(1).unwrap_or(0);
        Ok(())
    }

    /// Call native code with a stable, scoped, checked const pointer.
    ///
    /// # Safety
    /// The native callee must not retain the pointer after the callback returns,
    /// access beyond its length, deallocate it, mutate it through another alias,
    /// or otherwise violate Rust aliasing and thread-safety rules.
    /// The callback must not re-enter this registry unsafely.
    pub unsafe fn with_raw_read<R>(
        &self,
        handle: ForeignBufferHandle,
        offset: usize,
        len: usize,
        f: impl FnOnce(*const u8, usize) -> R,
    ) -> Result<R, ForeignMemoryError> {
        let bytes = self.read(handle, offset, len)?;
        Ok(f(bytes.as_ptr(), bytes.len()))
    }

    /// Call native code with a stable, scoped, checked mutable pointer.
    ///
    /// # Safety
    /// The native callee must not retain the pointer, access outside its
    /// checked range, deallocate it, or alias it with conflicting accesses.
    /// All mutable access must finish before the callback returns.
    pub unsafe fn with_raw_write<R>(
        &mut self,
        handle: ForeignBufferHandle,
        offset: usize,
        len: usize,
        f: impl FnOnce(*mut u8, usize) -> R,
    ) -> Result<R, ForeignMemoryError> {
        let buffer = self.buffer_mut(handle)?;
        if buffer.access != ForeignAccess::ReadWrite {
            return Err(ForeignMemoryError::PermissionDenied);
        }
        let range = Self::checked_range(offset, len, buffer.bytes.len())?;
        let bytes = &mut buffer.bytes[range];
        Ok(f(bytes.as_mut_ptr(), bytes.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_buffer_allows_bounded_reads_and_writes() {
        let mut memory = ForeignBufferRegistry::new();
        let handle = memory
            .register(&[10, 20, 30], ForeignAccess::ReadWrite)
            .unwrap();
        assert_eq!(memory.read(handle, 1, 2).unwrap(), &[20, 30]);
        memory.write(handle, 1, &[40, 50]).unwrap();
        assert_eq!(memory.read(handle, 0, 3).unwrap(), &[10, 40, 50]);
    }

    #[test]
    fn released_handle_is_rejected_after_slot_reuse() {
        let mut memory = ForeignBufferRegistry::new();
        let old = memory.register(&[1], ForeignAccess::ReadWrite).unwrap();
        memory.release(old).unwrap();
        let next = memory.register(&[2], ForeignAccess::ReadWrite).unwrap();
        assert_ne!(old, next);
        assert_eq!(
            memory.read(old, 0, 1),
            Err(ForeignMemoryError::InvalidHandle)
        );
        assert_eq!(memory.release(old), Err(ForeignMemoryError::InvalidHandle));
        assert_eq!(memory.read(next, 0, 1).unwrap(), &[2]);
    }

    #[test]
    fn exhausted_generation_retires_slot_without_wraparound() {
        let mut memory = ForeignBufferRegistry::new();
        let original = memory.register(&[1], ForeignAccess::ReadWrite).unwrap();
        // Simulate a slot at the very end of its generation space.
        memory.slots[original.slot].generation = u64::MAX;
        let last = ForeignBufferHandle {
            generation: u64::MAX,
            ..original
        };
        memory.release(last).unwrap();
        assert_eq!(memory.slots[original.slot].generation, 0);
        let next = memory.register(&[2], ForeignAccess::ReadWrite).unwrap();
        assert_ne!(
            original.slot, next.slot,
            "retired slots must never be reused"
        );
        assert_eq!(
            memory.read(last, 0, 1),
            Err(ForeignMemoryError::InvalidHandle)
        );
        assert_eq!(memory.read(next, 0, 1).unwrap(), &[2]);
    }

    #[test]
    fn handles_cannot_cross_registry_ownership_domains() {
        let mut a = ForeignBufferRegistry::new();
        let mut b = ForeignBufferRegistry::new();
        let from_a = a.register(&[3], ForeignAccess::ReadWrite).unwrap();
        let from_b = b.register(&[4], ForeignAccess::ReadWrite).unwrap();
        assert_eq!(b.read(from_a, 0, 1), Err(ForeignMemoryError::InvalidHandle));
        assert_eq!(
            a.write(from_b, 0, &[8]),
            Err(ForeignMemoryError::InvalidHandle)
        );
        assert_eq!(b.release(from_a), Err(ForeignMemoryError::InvalidHandle));
    }

    #[test]
    fn read_only_allocation_cannot_be_modified() {
        let mut memory = ForeignBufferRegistry::new();
        let handle = memory.register(&[1, 2], ForeignAccess::ReadOnly).unwrap();
        assert_eq!(
            memory.write(handle, 0, &[9]),
            Err(ForeignMemoryError::PermissionDenied)
        );
        assert_eq!(memory.read(handle, 0, 2).unwrap(), &[1, 2]);
    }

    #[test]
    fn rejects_oob_arithmetic_overflow_and_empty_allocations() {
        let mut memory = ForeignBufferRegistry::new();
        assert_eq!(
            memory.register(&[], ForeignAccess::ReadWrite),
            Err(ForeignMemoryError::InvalidLength)
        );
        assert_eq!(
            memory.allocate_zeroed(0, ForeignAccess::ReadWrite),
            Err(ForeignMemoryError::InvalidLength)
        );
        let handle = memory
            .register(&[1, 2, 3], ForeignAccess::ReadWrite)
            .unwrap();
        assert_eq!(
            memory.read(handle, 2, 2),
            Err(ForeignMemoryError::OutOfBounds)
        );
        assert_eq!(
            memory.read(handle, usize::MAX, 2),
            Err(ForeignMemoryError::OutOfBounds)
        );
        assert_eq!(
            memory.write(handle, usize::MAX, &[1, 2]),
            Err(ForeignMemoryError::OutOfBounds)
        );
        assert!(memory.read(handle, 3, 0).unwrap().is_empty());
    }

    #[test]
    fn zeroed_buffer_is_initialised_before_it_can_be_read() {
        let mut memory = ForeignBufferRegistry::new();
        let handle = memory.allocate_zeroed(4, ForeignAccess::ReadWrite).unwrap();
        assert_eq!(memory.read(handle, 0, 4).unwrap(), &[0, 0, 0, 0]);
    }

    #[test]
    fn foreign_call_leases_are_scoped_and_validated() {
        extern "C" fn sum_bytes(p: *const u8, len: usize) -> u64 {
            // SAFETY: the test passes a checked, live, in-bounds allocation.
            unsafe {
                std::slice::from_raw_parts(p, len)
                    .iter()
                    .map(|b| u64::from(*b))
                    .sum()
            }
        }
        extern "C" fn fill_bytes(p: *mut u8, len: usize, value: u8) {
            // SAFETY: the test passes a checked, exclusive mutable allocation.
            unsafe { std::slice::from_raw_parts_mut(p, len).fill(value) }
        }
        let mut memory = ForeignBufferRegistry::new();
        let handle = memory
            .register(&[1, 2, 3], ForeignAccess::ReadWrite)
            .unwrap();
        let sum = unsafe { memory.with_raw_read(handle, 0, 3, |p, n| sum_bytes(p, n)) }.unwrap();
        assert_eq!(sum, 6);
        unsafe { memory.with_raw_write(handle, 1, 2, |p, n| fill_bytes(p, n, 9)) }.unwrap();
        assert_eq!(memory.read(handle, 0, 3).unwrap(), &[1, 9, 9]);
        memory.release(handle).unwrap();
        assert_eq!(
            unsafe { memory.with_raw_read(handle, 0, 1, |_, _| 0) },
            Err(ForeignMemoryError::InvalidHandle)
        );
    }

    #[test]
    fn raw_mutable_lease_respects_access_permissions() {
        let mut memory = ForeignBufferRegistry::new();
        let handle = memory.register(&[8], ForeignAccess::ReadOnly).unwrap();
        assert_eq!(
            unsafe { memory.with_raw_write(handle, 0, 1, |_, _| ()) },
            Err(ForeignMemoryError::PermissionDenied)
        );
    }
}
