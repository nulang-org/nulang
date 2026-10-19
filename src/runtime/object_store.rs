//! Immutable node-shared object store for large `val` buffers.
//!
//! A buffer is inserted once and referenced by a lightweight `ObjectId`.
//! `ObjectStore` is cheap to clone: clones share one node-local backing map,
//! allowing runtime shards in the same process to exchange `TAG_OBJECT`
//! handles without copying the underlying bytes.
//!
//! Cross-node transport remains explicit: wire encoding resolves an object to
//! bytes, and the receiving node interns a new local object.
//!
//! # Lifecycle
//!
//! - `put` inserts a buffer with refcount 1.
//! - A receiving actor acquires at most one actor-lifetime hold per ObjectId.
//! - `clone_ref` increments that hold count.
//! - `drop_ref` decrements it and removes the object at zero.
//!
//! Actor heaps and ORCA remain shard-confined. Shared objects are immutable and
//! never contain actor-heap pointers.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

pub type ObjectId = u64;

/// An immutable buffer stored outside actor heaps.
#[derive(Debug)]
pub struct ObjectEntry {
    pub id: ObjectId,
    bytes: Arc<[u8]>,
    ref_count: AtomicUsize,
}

impl ObjectEntry {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn ref_count(&self) -> usize {
        self.ref_count.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct ObjectStoreInner {
    next_id: AtomicU64,
    entries: RwLock<HashMap<ObjectId, Arc<ObjectEntry>>>,
}

/// Cloneable node-local store. Every clone addresses the same immutable
/// objects and refcounts.
#[derive(Debug, Clone)]
pub struct ObjectStore {
    inner: Arc<ObjectStoreInner>,
}

impl Default for ObjectStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ObjectStoreInner {
                next_id: AtomicU64::new(1),
                entries: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// True when two handles share the same node-local store.
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Store an immutable buffer and return its object id. Refcount starts at 1.
    pub fn put(&self, bytes: Box<[u8]>) -> ObjectId {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(ObjectEntry {
            id,
            bytes: Arc::from(bytes),
            ref_count: AtomicUsize::new(1),
        });
        self.inner
            .entries
            .write()
            .expect("object-store write lock poisoned")
            .insert(id, entry);
        id
    }

    /// Return a shared immutable entry by id.
    pub fn get(&self, id: ObjectId) -> Option<Arc<ObjectEntry>> {
        self.inner
            .entries
            .read()
            .expect("object-store read lock poisoned")
            .get(&id)
            .cloned()
    }

    /// Increment the refcount for id. Returns true when the id exists.
    pub fn clone_ref(&self, id: ObjectId) -> bool {
        let entries = self
            .inner
            .entries
            .read()
            .expect("object-store read lock poisoned");
        let Some(entry) = entries.get(&id) else {
            return false;
        };
        entry.ref_count.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Decrement the refcount for id, removing the entry at zero.
    pub fn drop_ref(&self, id: ObjectId) -> bool {
        let mut entries = self
            .inner
            .entries
            .write()
            .expect("object-store write lock poisoned");
        let Some(entry) = entries.get(&id) else {
            return false;
        };

        let previous = entry.ref_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "object-store refcount underflow");
        if previous == 1 {
            entries.remove(&id);
        }
        true
    }

    pub fn len(&self) -> usize {
        self.inner
            .entries
            .read()
            .expect("object-store read lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn drop_refs(&self, ids: &HashSet<ObjectId>) {
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
        let store = ObjectStore::new();
        let id = store.put(vec![1, 2, 3, 4].into_boxed_slice());
        let entry = store.get(id).unwrap();
        assert_eq!(entry.as_bytes(), &[1, 2, 3, 4]);
        assert_eq!(entry.len(), 4);
    }

    #[test]
    fn test_ref_count_lifecycle() {
        let store = ObjectStore::new();
        let id = store.put(vec![10, 20, 30].into_boxed_slice());
        assert!(store.clone_ref(id));
        assert!(store.clone_ref(id));
        store.drop_ref(id);
        store.drop_ref(id);
        assert!(store.get(id).is_some());
        store.drop_ref(id);
        assert!(store.get(id).is_none());
    }

    #[test]
    fn test_clones_share_entries() {
        let store = ObjectStore::new();
        let clone = store.clone();
        assert!(store.shares_storage_with(&clone));

        let id = store.put(vec![7, 8, 9].into_boxed_slice());
        let from_store = store.get(id).unwrap();
        let from_clone = clone.get(id).unwrap();
        assert!(Arc::ptr_eq(&from_store, &from_clone));
    }

    #[test]
    fn test_drop_unknown_is_noop() {
        let store = ObjectStore::new();
        assert!(!store.drop_ref(123));
    }

    #[test]
    fn test_drop_refs_bulk() {
        let store = ObjectStore::new();
        let id1 = store.put(vec![1].into_boxed_slice());
        let id2 = store.put(vec![2].into_boxed_slice());
        store.clone_ref(id1);
        store.clone_ref(id2);

        let held = HashSet::from([id1, id2]);
        store.drop_refs(&held);

        assert!(store.get(id1).is_some());
        assert!(store.get(id2).is_some());

        store.drop_refs(&held);
        assert!(store.get(id1).is_none());
        assert!(store.get(id2).is_none());
    }
}
