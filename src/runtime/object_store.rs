//! Immutable shared object store for large `val` buffers.
//!
//! This module provides a per-shard (per-Runtime) store for immutable byte
//! buffers.  A buffer is inserted once and referenced by a lightweight
//! `ObjectId`.  The runtime can pass these identifiers between actors in place
//! of copying the buffer through mailboxes.
//!
//! # Scope
//!
//! The MVP store is **per-shard**: each `Runtime` owns its own `ObjectStore`,
//! and a cross-shard message that carries an object reference serializes the
//! bytes into the cross-shard channel so the target shard can insert a local
//! copy.  This keeps the implementation simple and avoids cross-thread sharing
//! and locking.  A future node-wide shared-memory pool can replace this without
//! changing the `Value::object` representation or the public API.
//!
//! # Lifecycle
//!
//! - `put` inserts a buffer with refcount `1`.
//! - `clone_ref` increments the refcount when an actor receives a message that
//!   holds the object id.
//! - `drop_ref` decrements the refcount when an actor exits or overwrites a
//!   register holding the object id.
//! - When the refcount reaches zero the entry is removed and the bytes freed.
//!
//! All operations run on the owning shard's scheduler thread; no interior
//! mutability is required.

use std::collections::HashSet;

pub type ObjectId = u64;

/// An immutable buffer stored in the object store.
#[derive(Debug)]
pub struct ObjectEntry {
    pub id: ObjectId,
    bytes: Box<[u8]>,
    ref_count: usize,
}

impl ObjectEntry {
    /// Return a slice to the immutable bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Return the byte length.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Return the current reference count.
    pub fn ref_count(&self) -> usize {
        self.ref_count
    }
}

/// Per-shard object store.
#[derive(Debug, Default)]
pub struct ObjectStore {
    next_id: ObjectId,
    entries: std::collections::HashMap<ObjectId, ObjectEntry>,
}

impl ObjectStore {
    /// Create an empty object store.
    pub fn new() -> Self {
        ObjectStore {
            next_id: 1,
            entries: std::collections::HashMap::new(),
        }
    }

    /// Store an immutable buffer and return its object id.  Refcount starts at 1.
    pub fn put(&mut self, bytes: Box<[u8]>) -> ObjectId {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.insert(
            id,
            ObjectEntry {
                id,
                bytes,
                ref_count: 1,
            },
        );
        id
    }

    /// Borrow an entry by id.
    pub fn get(&self, id: ObjectId) -> Option<&ObjectEntry> {
        self.entries.get(&id)
    }

    /// Increment the refcount for `id`.  Returns `true` if the id existed.
    pub fn clone_ref(&mut self, id: ObjectId) -> bool {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.ref_count = entry.ref_count.saturating_add(1);
            true
        } else {
            false
        }
    }

    /// Decrement the refcount for `id`, removing the entry if it reaches zero.
    /// Returns `true` if the id existed.
    pub fn drop_ref(&mut self, id: ObjectId) -> bool {
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.ref_count = entry.ref_count.saturating_sub(1);
            if entry.ref_count == 0 {
                self.entries.remove(&id);
            }
            true
        } else {
            false
        }
    }

    /// Number of stored objects.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop every refcount held by `ids`.  Convenience for actor exit cleanup.
    pub fn drop_refs(&mut self, ids: &HashSet<ObjectId>) {
        for &id in ids {
            self.drop_ref(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_put_get() {
        let mut store = ObjectStore::new();
        let bytes: Box<[u8]> = vec![1, 2, 3, 4].into_boxed_slice();
        let id = store.put(bytes);
        let entry = store.get(id).unwrap();
        assert_eq!(entry.as_bytes(), &[1, 2, 3, 4]);
        assert_eq!(entry.len(), 4);
    }

    #[test]
    fn test_ref_count_lifecycle() {
        let mut store = ObjectStore::new();
        let id = store.put(Box::new([10, 20, 30]));
        assert!(store.clone_ref(id));
        assert!(store.clone_ref(id));
        store.drop_ref(id);
        store.drop_ref(id);
        assert!(store.get(id).is_some());
        store.drop_ref(id);
        assert!(store.get(id).is_none());
    }

    #[test]
    fn test_drop_unknown_is_noop() {
        let mut store = ObjectStore::new();
        assert!(!store.drop_ref(123));
    }

    #[test]
    fn test_drop_refs_bulk() {
        let mut store = ObjectStore::new();
        let id1 = store.put(Box::new([1]));
        let id2 = store.put(Box::new([2]));
        store.clone_ref(id1);
        store.clone_ref(id2);

        let mut held = HashSet::new();
        held.insert(id1);
        held.insert(id2);
        store.drop_refs(&held);

        // original refs still alive
        assert!(store.get(id1).is_some());
        assert!(store.get(id2).is_some());

        store.drop_refs(&held);
        assert!(store.get(id1).is_none());
        assert!(store.get(id2).is_none());
    }
}
