//! Experimental fixed-term quorum-durable NuDB ownership journal.
//!
//! This replicates committed ownership records into independently fsynced
//! *local files* and replays only the majority-identical prefix. Each file is
//! a stand-in for a durable voting replica. It is NOT a distributed Raft
//! service: it has no elections, network transport, membership changes,
//! authenticated proposals, or remote failure domains. The term is immutable.
//! It MUST NOT be used to establish multi-node leadership in production.
//!
//! A real Raft adapter may reuse the ownership state machine and these
//! deterministic durability tests, but must implement election, AppendEntries,
//! term persistence, log conflict repair, network authentication and snapshots.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::ownership::{
    CommittedOwnershipEntry, OwnershipCommand, OwnershipError, OwnershipGrant, OwnershipStateMachine,
};

const JOURNAL_MAGIC: &[u8; 8] = b"NUOWLOG1";
const MAX_ENTRY_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct FileReplica {
    file: File,
    // Sidecar lock must not be unlinked: ownership survives journal reopen.
    _writer_lock: File,
    entries: Vec<CommittedOwnershipEntry>,
}

impl FileReplica {
    fn open(path: &Path, create: bool) -> Result<Self, QuorumLogError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if create {
                fs::create_dir_all(parent)?;
            }
        }
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(PathBuf::from(lock_path))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(QuorumLogError::ReplicaBusy),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)?;
        if file.metadata()?.len() == 0 {
            if !create {
                return Err(QuorumLogError::InvalidJournal);
            }
            file.write_all(JOURNAL_MAGIC)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0_u8; 8];
        file.read_exact(&mut magic)?;
        if &magic != JOURNAL_MAGIC {
            return Err(QuorumLogError::InvalidJournal);
        }

        let mut entries = Vec::new();
        loop {
            let mut first = [0_u8; 1];
            if file.read(&mut first)? == 0 {
                break;
            }
            let mut rest = [0_u8; 3];
            file.read_exact(&mut rest).map_err(|_| QuorumLogError::InvalidJournal)?;
            let length = u32::from_le_bytes([first[0], rest[0], rest[1], rest[2]]) as usize;
            if length == 0 || length > MAX_ENTRY_BYTES {
                return Err(QuorumLogError::InvalidJournal);
            }
            let mut payload = vec![0_u8; length];
            file.read_exact(&mut payload)
                .map_err(|_| QuorumLogError::InvalidJournal)?;
            let mut digest = [0_u8; 32];
            file.read_exact(&mut digest)
                .map_err(|_| QuorumLogError::InvalidJournal)?;
            if digest != *blake3::hash(&payload).as_bytes() {
                return Err(QuorumLogError::InvalidJournal);
            }
            let entry: CommittedOwnershipEntry =
                serde_json::from_slice(&payload).map_err(|_| QuorumLogError::InvalidJournal)?;
            if entry.index != entries.len() as u64 + 1
                || entry.term == 0
                || entries.last().is_some_and(|last: &CommittedOwnershipEntry| entry.term < last.term)
            {
                return Err(QuorumLogError::InvalidJournal);
            }
            entries.push(entry);
        }
        file.seek(SeekFrom::End(0))?;
        Ok(Self { file, _writer_lock: lock, entries })
    }

    fn append_durable(&mut self, entry: CommittedOwnershipEntry) -> Result<(), QuorumLogError> {
        let offset = (entry.index as usize)
            .checked_sub(1)
            .ok_or(QuorumLogError::InvalidJournal)?;
        if let Some(existing) = self.entries.get(offset) {
            return if *existing == entry {
                Ok(())
            } else {
                Err(QuorumLogError::ConflictingReplicaEntry { index: entry.index })
            };
        }
        if offset != self.entries.len() {
            return Err(QuorumLogError::ReplicaGap {
                expected: self.entries.len() as u64 + 1,
                presented: entry.index,
            });
        }
        let payload = serde_json::to_vec(&entry).map_err(|e| QuorumLogError::Storage(e.to_string()))?;
        if payload.is_empty() || payload.len() > MAX_ENTRY_BYTES {
            return Err(QuorumLogError::InvalidJournal);
        }
        let length = u32::try_from(payload.len()).map_err(|_| QuorumLogError::InvalidJournal)?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&length.to_le_bytes())?;
        self.file.write_all(&payload)?;
        self.file.write_all(blake3::hash(&payload).as_bytes())?;
        self.file.sync_data()?;
        self.entries.push(entry);
        Ok(())
    }
}

/// Fixed-membership, one-term quorum journal for offline / single-host tests.
///
/// A majority of separate fsynced files is necessary for commitment. It is
/// **not** a proof of consensus across physical machines. The client must
/// never treat these file votes as an authenticated distributed leader grant.
#[derive(Debug)]
pub struct FixedTermQuorumLog {
    term: u64,
    quorum: usize,
    replicas: Vec<Option<FileReplica>>,
    authority: OwnershipStateMachine,
    needs_reopen: bool,
}

impl FixedTermQuorumLog {
    /// Open three or more odd-numbered, distinct replica paths.
    ///
    /// New journal paths are created only for an entirely fresh membership.
    /// An unavailable member of an established set is never silently
    /// replaced with an empty journal (that needs authenticated resync).
    pub fn open(paths: &[PathBuf], term: u64) -> Result<Self, QuorumLogError> {
        if paths.len() < 3 || paths.len() % 2 == 0 {
            return Err(QuorumLogError::InvalidMembership);
        }
        if term == 0 {
            return Err(QuorumLogError::InvalidTerm);
        }
        // A literal duplicate path must never count as two votes. This does
        // not establish independence of filesystems, disks, or machines.
        let unique: BTreeSet<_> = paths.iter().collect();
        if unique.len() != paths.len() {
            return Err(QuorumLogError::DuplicateReplicaPath);
        }

        let fresh = paths.iter().all(|path| !path.exists());
        let quorum = paths.len() / 2 + 1;
        let mut replicas = Vec::with_capacity(paths.len());
        for path in paths {
            // Once initialized, a missing file represents an offline voter.
            if !fresh && !path.exists() {
                replicas.push(None);
                continue;
            }
            match FileReplica::open(path, fresh) {
                Ok(replica) => replicas.push(Some(replica)),
                Err(_) => replicas.push(None),
            }
        }
        let available = replicas.iter().filter(|replica| replica.is_some()).count();
        if available < quorum {
            return Err(QuorumLogError::NotEnoughReplicas {
                available,
                required: quorum,
            });
        }

        // Recover only the majority-identical prefix. A minority suffix is
        // NOT a committed decision, even if it was successfully fsynced.
        let mut authority = OwnershipStateMachine::new();
        loop {
            let index = authority.applied_index() as usize;
            let mut majority = None;
            for candidate in replicas
                .iter()
                .flatten()
                .filter_map(|replica| replica.entries.get(index))
            {
                let count = replicas.iter().flatten().filter(|replica| {
                    replica.entries.get(index) == Some(candidate)
                }).count();
                if count >= quorum {
                    majority = Some(*candidate);
                    break;
                }
            }
            let Some(committed) = majority else { break };
            // Applying a semantically rejected committed command still
            // advances the cursor. Only log/term corruption is fatal here.
            let _ = authority.apply_committed(committed);
            if authority.applied_index() != committed.index {
                return Err(QuorumLogError::InvalidJournal);
            }
        }
        if authority.applied_index() != 0 && authority.applied_term() != term {
            return Err(QuorumLogError::UnsupportedTermChange {
                durable: authority.applied_term(),
                requested: term,
            });
        }

        Ok(Self { term, quorum, replicas, authority, needs_reopen: false })
    }

    pub fn authority(&self) -> &OwnershipStateMachine {
        &self.authority
    }

    /// Append and fsync on a strict majority BEFORE applying ownership.
    ///
    /// If a quorum was not acknowledged, the journal refuses further
    /// proposals until reopen has reconciled possibly ambiguous writes.
    pub fn propose(&mut self, command: OwnershipCommand) -> Result<OwnershipGrant, QuorumLogError> {
        if self.needs_reopen {
            return Err(QuorumLogError::RequiresReopen);
        }
        let next = self.authority.applied_index()
            .checked_add(1)
            .ok_or(QuorumLogError::InvalidJournal)?;
        let entry = CommittedOwnershipEntry {
            index: next,
            term: self.term,
            command,
        };
        let mut acknowledged = 0;
        for replica in self.replicas.iter_mut().flatten() {
            if replica.append_durable(entry).is_ok() {
                acknowledged += 1;
            }
        }
        if acknowledged < self.quorum {
            self.needs_reopen = true;
            return Err(QuorumLogError::QuorumNotReached {
                acknowledged,
                required: self.quorum,
            });
        }
        self.authority.apply_committed(entry).map_err(QuorumLogError::CommandRejected)
    }

    /// Fault-injection hook for tests; not for building a real cluster.
    #[doc(hidden)]
    pub fn disable_replica_for_test(&mut self, index: usize) {
        if let Some(slot) = self.replicas.get_mut(index) {
            *slot = None;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuorumLogError {
    InvalidMembership,
    DuplicateReplicaPath,
    InvalidTerm,
    InvalidJournal,
    ReplicaBusy,
    NotEnoughReplicas { available: usize, required: usize },
    QuorumNotReached { acknowledged: usize, required: usize },
    RequiresReopen,
    UnsupportedTermChange { durable: u64, requested: u64 },
    ConflictingReplicaEntry { index: u64 },
    ReplicaGap { expected: u64, presented: u64 },
    CommandRejected(OwnershipError),
    Storage(String),
}

impl From<std::io::Error> for QuorumLogError {
    fn from(value: std::io::Error) -> Self {
        Self::Storage(value.to_string())
    }
}

impl fmt::Display for QuorumLogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for QuorumLogError {}
