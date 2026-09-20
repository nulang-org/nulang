//! Durable source-controller journal for cross-node cache slot migration.
//!
//! The journal is intentionally separate from CacheStore durability. It records
//! the proof needed to resume/refuse a migration after controller failure:
//! intent, exact transfer envelopes, exact application ACKs, source-drain
//! observations, convergence probes, and final ownership publication.
//!
//! Records are append-only, length bounded, BLAKE3 checksummed, and fsynced
//! before append returns. A crash-truncated tail is discarded on reopen;
//! checksum failure in a complete record fails closed as corruption.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::cache::CacheTransferImport;
use super::cache_routing::CacheShardOwner;
use super::cache_transport::{CacheTransportMessage, MAX_CACHE_TRANSPORT_BYTES};

const JOURNAL_MAGIC: &[u8; 4] = b"NCMJ";
const JOURNAL_VERSION: u8 = 1;
const HEADER_LEN: u64 = 5;
const CHECKSUM_LEN: usize = 32;
const RECORD_HEADER_LEN: usize = 5;
const MAX_JOURNAL_RECORD_BYTES: usize = MAX_CACHE_TRANSPORT_BYTES + 256;

const KIND_INTENT: u8 = 1;
const KIND_TRANSFER_SENT: u8 = 2;
const KIND_TRANSFER_ACK: u8 = 3;
const KIND_SOURCE_REMAINING: u8 = 4;
const KIND_CONVERGENCE: u8 = 5;
const KIND_COMPLETED: u8 = 6;
const KIND_COMMIT_INTENT: u8 = 7;
const KIND_COMMIT_ABORTED: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheMigrationKey {
    pub started_epoch: u64,
    pub slot: u16,
    pub source: CacheShardOwner,
    pub target: CacheShardOwner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMigrationConvergenceEvidence {
    pub probe_id: u64,
    pub target_accepted: bool,
    pub source_remaining: usize,
    pub target_live_entries: u64,
    pub target_import_fences: u64,
    pub target_conflicts: u64,
    pub target_wrong_slot: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMigrationRecoveredTransfer {
    pub transfer_id: u64,
    pub request: CacheTransportMessage,
    pub ack: Option<CacheTransportMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMigrationRecoveryState {
    pub key: CacheMigrationKey,
    pub source_incarnation: [u8; 16],
    pub transfers: HashMap<u64, CacheMigrationRecoveredTransfer>,
    pub source_remaining: Option<usize>,
    pub convergence: Option<CacheMigrationConvergenceEvidence>,
    pub pending_commit_epoch: Option<u64>,
    pub completed_commit_epoch: Option<u64>,
}

impl CacheMigrationRecoveryState {
    fn new(key: CacheMigrationKey, source_incarnation: [u8; 16]) -> Self {
        Self {
            key,
            source_incarnation,
            transfers: HashMap::new(),
            source_remaining: None,
            convergence: None,
            pending_commit_epoch: None,
            completed_commit_epoch: None,
        }
    }

    pub fn all_sent_transfers_acked(&self) -> bool {
        self.transfers
            .values()
            .all(|transfer| transfer.ack.is_some())
    }

    pub fn unfinished_transfer_ids(&self) -> Vec<u64> {
        let mut ids: Vec<_> = self
            .transfers
            .values()
            .filter(|transfer| transfer.ack.is_none())
            .map(|transfer| transfer.transfer_id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Number of unique migration keys for which the target must still retain
    /// import-fence history. ExpiredInTransit is included because the target
    /// records a migration tombstone fence for accepted expiry.
    pub fn expected_import_fences(&self) -> usize {
        let mut keys = HashSet::new();
        for transfer in self.transfers.values() {
            let CacheTransportMessage::TransferBatch { batch, .. } = &transfer.request else {
                continue;
            };
            let Some(CacheTransportMessage::TransferAck { results, .. }) = &transfer.ack else {
                continue;
            };
            for (entry, result) in batch.entries.iter().zip(results.iter()) {
                if matches!(
                    result,
                    CacheTransferImport::Imported
                        | CacheTransferImport::AlreadyImported
                        | CacheTransferImport::ExpiredInTransit
                ) {
                    keys.insert(*blake3::hash(&entry.key).as_bytes());
                }
            }
        }
        keys.len()
    }

    /// A restarted controller may issue a fresh target probe only after every
    /// sent batch has a durable application ACK and the journal records a
    /// drained source.
    pub fn restart_reprobe_candidate(&self) -> bool {
        self.completed_commit_epoch.is_none()
            && self.pending_commit_epoch.is_none()
            && self.source_remaining == Some(0)
            && self.all_sent_transfers_acked()
    }

    /// Evaluate a *fresh* convergence observation against durable history.
    ///
    /// Old journaled convergence is informational only; callers must obtain a
    /// new exact-epoch target probe after restart and pass it here.
    pub fn accepts_fresh_convergence(&self, evidence: &CacheMigrationConvergenceEvidence) -> bool {
        self.restart_reprobe_candidate()
            && self
                .convergence
                .as_ref()
                .is_none_or(|previous| previous.probe_id != evidence.probe_id)
            && evidence.target_accepted
            && evidence.source_remaining == 0
            && evidence.target_conflicts == 0
            && evidence.target_wrong_slot == 0
            && evidence.target_import_fences >= self.expected_import_fences() as u64
    }
}

#[derive(Debug)]
pub struct CacheMigrationJournal {
    path: PathBuf,
    file: File,
    states: HashMap<CacheMigrationKey, CacheMigrationRecoveryState>,
}

impl CacheMigrationJournal {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;

        if file.metadata()?.len() == 0 {
            file.write_all(JOURNAL_MAGIC)?;
            file.write_all(&[JOURNAL_VERSION])?;
            file.sync_all()?;
        }

        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if bytes.len() < HEADER_LEN as usize
            || &bytes[..4] != JOURNAL_MAGIC
            || bytes[4] != JOURNAL_VERSION
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid cache migration journal header",
            ));
        }

        let mut states = HashMap::new();
        let mut cursor = HEADER_LEN as usize;
        let mut valid_end = cursor;
        while cursor < bytes.len() {
            if bytes.len() - cursor < RECORD_HEADER_LEN {
                break;
            }
            let kind = bytes[cursor];
            let len = u32::from_be_bytes(
                bytes[cursor + 1..cursor + 5]
                    .try_into()
                    .expect("record length slice"),
            ) as usize;
            if len > MAX_JOURNAL_RECORD_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cache migration journal record too large",
                ));
            }
            let record_end = cursor
                .checked_add(RECORD_HEADER_LEN)
                .and_then(|v| v.checked_add(len))
                .and_then(|v| v.checked_add(CHECKSUM_LEN))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "cache migration journal length overflow",
                    )
                })?;
            if record_end > bytes.len() {
                break;
            }

            let payload_start = cursor + RECORD_HEADER_LEN;
            let payload_end = payload_start + len;
            let checksum = &bytes[payload_end..record_end];
            let expected = record_checksum(kind, len as u32, &bytes[payload_start..payload_end]);
            if checksum != expected.as_slice() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cache migration journal checksum mismatch",
                ));
            }

            apply_record(&mut states, kind, &bytes[payload_start..payload_end])?;
            cursor = record_end;
            valid_end = cursor;
        }

        if valid_end < bytes.len() {
            file.set_len(valid_end as u64)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;

        Ok(Self { path, file, states })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn recovery_states(&self) -> impl Iterator<Item = &CacheMigrationRecoveryState> {
        self.states.values()
    }

    pub fn recovery_state(&self, key: CacheMigrationKey) -> Option<&CacheMigrationRecoveryState> {
        self.states.get(&key)
    }

    pub fn record_intent(
        &mut self,
        key: CacheMigrationKey,
        source_incarnation: [u8; 16],
    ) -> io::Result<()> {
        if let Some(existing) = self.states.get(&key) {
            if existing.source_incarnation == source_incarnation {
                return Ok(());
            }
            return Err(invalid_data(
                "cache migration source incarnation changed; ephemeral source continuity is unproven",
            ));
        }
        let mut payload = Vec::with_capacity(46);
        write_key(&mut payload, key);
        payload.extend_from_slice(&source_incarnation);
        self.append_record(KIND_INTENT, &payload)?;
        self.states.insert(
            key,
            CacheMigrationRecoveryState::new(key, source_incarnation),
        );
        Ok(())
    }

    pub fn record_transfer_sent(
        &mut self,
        key: CacheMigrationKey,
        message: &CacheTransportMessage,
    ) -> io::Result<()> {
        let (transfer_id, placement_epoch, slot, source, target) =
            transfer_request_identity(message)?;
        require_message_matches_migration(key, placement_epoch, slot, source, target)?;
        let state = self.require_state(key)?;
        if let Some(existing) = state.transfers.get(&transfer_id) {
            if &existing.request == message {
                return Ok(());
            }
            return Err(invalid_data("transfer id reused with different request"));
        }

        let wire = message
            .to_wire_bytes()
            .map_err(|_| invalid_data("unable to encode transfer request"))?;
        let mut payload = Vec::with_capacity(42 + wire.len());
        write_key(&mut payload, key);
        write_u64(&mut payload, transfer_id);
        write_blob(&mut payload, &wire)?;
        self.append_record(KIND_TRANSFER_SENT, &payload)?;
        self.states
            .get_mut(&key)
            .expect("migration state disappeared")
            .transfers
            .insert(
                transfer_id,
                CacheMigrationRecoveredTransfer {
                    transfer_id,
                    request: message.clone(),
                    ack: None,
                },
            );
        Ok(())
    }

    pub fn record_transfer_ack(
        &mut self,
        key: CacheMigrationKey,
        message: &CacheTransportMessage,
    ) -> io::Result<()> {
        let (transfer_id, placement_epoch, slot, source, target) = transfer_ack_identity(message)?;
        require_message_matches_migration(key, placement_epoch, slot, source, target)?;
        let state = self.require_state(key)?;
        let transfer = state
            .transfers
            .get(&transfer_id)
            .ok_or_else(|| invalid_data("transfer ACK has no durable sent record"))?;
        validate_ack_matches_request(&transfer.request, message)?;
        if let Some(existing) = &transfer.ack {
            if existing == message {
                return Ok(());
            }
            return Err(invalid_data("transfer id reused with different ACK"));
        }

        let wire = message
            .to_wire_bytes()
            .map_err(|_| invalid_data("unable to encode transfer ACK"))?;
        let mut payload = Vec::with_capacity(42 + wire.len());
        write_key(&mut payload, key);
        write_u64(&mut payload, transfer_id);
        write_blob(&mut payload, &wire)?;
        self.append_record(KIND_TRANSFER_ACK, &payload)?;
        self.states
            .get_mut(&key)
            .expect("migration state disappeared")
            .transfers
            .get_mut(&transfer_id)
            .expect("transfer disappeared")
            .ack = Some(message.clone());
        Ok(())
    }

    pub fn record_source_remaining(
        &mut self,
        key: CacheMigrationKey,
        remaining: usize,
    ) -> io::Result<()> {
        self.require_state(key)?;
        let mut payload = Vec::with_capacity(38);
        write_key(&mut payload, key);
        write_u64(
            &mut payload,
            u64::try_from(remaining).map_err(|_| invalid_data("source count overflow"))?,
        );
        self.append_record(KIND_SOURCE_REMAINING, &payload)?;
        self.states
            .get_mut(&key)
            .expect("migration state disappeared")
            .source_remaining = Some(remaining);
        Ok(())
    }

    pub fn record_convergence(
        &mut self,
        key: CacheMigrationKey,
        evidence: CacheMigrationConvergenceEvidence,
    ) -> io::Result<()> {
        self.require_state(key)?;
        let mut payload = Vec::with_capacity(87);
        write_key(&mut payload, key);
        write_u64(&mut payload, evidence.probe_id);
        payload.push(u8::from(evidence.target_accepted));
        write_u64(
            &mut payload,
            u64::try_from(evidence.source_remaining)
                .map_err(|_| invalid_data("source count overflow"))?,
        );
        write_u64(&mut payload, evidence.target_live_entries);
        write_u64(&mut payload, evidence.target_import_fences);
        write_u64(&mut payload, evidence.target_conflicts);
        write_u64(&mut payload, evidence.target_wrong_slot);
        self.append_record(KIND_CONVERGENCE, &payload)?;
        self.states
            .get_mut(&key)
            .expect("migration state disappeared")
            .convergence = Some(evidence);
        Ok(())
    }

    pub fn record_commit_intent(
        &mut self,
        key: CacheMigrationKey,
        commit_epoch: u64,
    ) -> io::Result<()> {
        let state = self.require_state(key)?;
        if state.completed_commit_epoch == Some(commit_epoch)
            || state.pending_commit_epoch == Some(commit_epoch)
        {
            return Ok(());
        }
        if state.pending_commit_epoch.is_some() {
            return Err(invalid_data(
                "different cache migration commit is already pending",
            ));
        }
        let mut payload = Vec::with_capacity(38);
        write_key(&mut payload, key);
        write_u64(&mut payload, commit_epoch);
        self.append_record(KIND_COMMIT_INTENT, &payload)?;
        self.states
            .get_mut(&key)
            .expect("migration state disappeared")
            .pending_commit_epoch = Some(commit_epoch);
        Ok(())
    }

    pub fn record_commit_aborted(
        &mut self,
        key: CacheMigrationKey,
        commit_epoch: u64,
    ) -> io::Result<()> {
        let state = self.require_state(key)?;
        if state.pending_commit_epoch != Some(commit_epoch) {
            return Err(invalid_data(
                "cache migration commit abort does not match pending intent",
            ));
        }
        let mut payload = Vec::with_capacity(38);
        write_key(&mut payload, key);
        write_u64(&mut payload, commit_epoch);
        self.append_record(KIND_COMMIT_ABORTED, &payload)?;
        self.states
            .get_mut(&key)
            .expect("migration state disappeared")
            .pending_commit_epoch = None;
        Ok(())
    }

    pub fn record_completed(
        &mut self,
        key: CacheMigrationKey,
        commit_epoch: u64,
    ) -> io::Result<()> {
        self.require_state(key)?;
        let mut payload = Vec::with_capacity(38);
        write_key(&mut payload, key);
        write_u64(&mut payload, commit_epoch);
        self.append_record(KIND_COMPLETED, &payload)?;
        let state = self
            .states
            .get_mut(&key)
            .expect("migration state disappeared");
        state.pending_commit_epoch = None;
        state.completed_commit_epoch = Some(commit_epoch);
        Ok(())
    }

    fn require_state(&self, key: CacheMigrationKey) -> io::Result<&CacheMigrationRecoveryState> {
        self.states
            .get(&key)
            .ok_or_else(|| invalid_data("migration intent is not durable"))
    }

    fn append_record(&mut self, kind: u8, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_JOURNAL_RECORD_BYTES {
            return Err(invalid_data("cache migration journal record too large"));
        }
        let len = u32::try_from(payload.len())
            .map_err(|_| invalid_data("cache migration journal record length overflow"))?;
        let checksum = record_checksum(kind, len, payload);
        self.file.write_all(&[kind])?;
        self.file.write_all(&len.to_be_bytes())?;
        self.file.write_all(payload)?;
        self.file.write_all(&checksum)?;
        self.file.sync_all()?;
        Ok(())
    }
}

fn record_checksum(kind: u8, len: u32, payload: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[kind]);
    hasher.update(&len.to_be_bytes());
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

fn apply_record(
    states: &mut HashMap<CacheMigrationKey, CacheMigrationRecoveryState>,
    kind: u8,
    payload: &[u8],
) -> io::Result<()> {
    let mut reader = JournalReader::new(payload);
    let key = reader.key()?;

    match kind {
        KIND_INTENT => {
            let source_incarnation: [u8; 16] = reader
                .take(16)?
                .try_into()
                .expect("source incarnation slice");
            reader.finish()?;
            match states.get(&key) {
                Some(existing) if existing.source_incarnation == source_incarnation => {}
                Some(_) => {
                    return Err(invalid_data(
                        "conflicting source incarnation in migration journal",
                    ));
                }
                None => {
                    states.insert(
                        key,
                        CacheMigrationRecoveryState::new(key, source_incarnation),
                    );
                }
            }
        }
        KIND_TRANSFER_SENT => {
            let transfer_id = reader.u64()?;
            let wire = reader.blob()?;
            reader.finish()?;
            let message = CacheTransportMessage::from_wire_bytes(&wire)
                .map_err(|_| invalid_data("invalid transfer request in migration journal"))?;
            let (decoded_id, placement_epoch, slot, source, target) =
                transfer_request_identity(&message)?;
            if decoded_id != transfer_id {
                return Err(invalid_data("transfer request id mismatch in journal"));
            }
            require_message_matches_migration(key, placement_epoch, slot, source, target)?;
            let state = states
                .get_mut(&key)
                .ok_or_else(|| invalid_data("transfer request precedes migration intent"))?;
            match state.transfers.get(&transfer_id) {
                Some(existing) if existing.request == message => {}
                Some(_) => return Err(invalid_data("conflicting transfer request in journal")),
                None => {
                    state.transfers.insert(
                        transfer_id,
                        CacheMigrationRecoveredTransfer {
                            transfer_id,
                            request: message,
                            ack: None,
                        },
                    );
                }
            }
        }
        KIND_TRANSFER_ACK => {
            let transfer_id = reader.u64()?;
            let wire = reader.blob()?;
            reader.finish()?;
            let message = CacheTransportMessage::from_wire_bytes(&wire)
                .map_err(|_| invalid_data("invalid transfer ACK in migration journal"))?;
            let (decoded_id, placement_epoch, slot, source, target) =
                transfer_ack_identity(&message)?;
            if decoded_id != transfer_id {
                return Err(invalid_data("transfer ACK id mismatch in journal"));
            }
            require_message_matches_migration(key, placement_epoch, slot, source, target)?;
            let state = states
                .get_mut(&key)
                .ok_or_else(|| invalid_data("transfer ACK precedes migration intent"))?;
            let transfer = state
                .transfers
                .get_mut(&transfer_id)
                .ok_or_else(|| invalid_data("transfer ACK precedes sent record"))?;
            validate_ack_matches_request(&transfer.request, &message)?;
            match &transfer.ack {
                Some(existing) if existing == &message => {}
                Some(_) => return Err(invalid_data("conflicting transfer ACK in journal")),
                None => transfer.ack = Some(message),
            }
        }
        KIND_SOURCE_REMAINING => {
            let remaining = usize::try_from(reader.u64()?)
                .map_err(|_| invalid_data("source count overflow in journal"))?;
            reader.finish()?;
            states
                .get_mut(&key)
                .ok_or_else(|| invalid_data("source progress precedes migration intent"))?
                .source_remaining = Some(remaining);
        }
        KIND_CONVERGENCE => {
            let evidence = CacheMigrationConvergenceEvidence {
                probe_id: reader.u64()?,
                target_accepted: reader.boolean()?,
                source_remaining: usize::try_from(reader.u64()?)
                    .map_err(|_| invalid_data("source count overflow in journal"))?,
                target_live_entries: reader.u64()?,
                target_import_fences: reader.u64()?,
                target_conflicts: reader.u64()?,
                target_wrong_slot: reader.u64()?,
            };
            reader.finish()?;
            states
                .get_mut(&key)
                .ok_or_else(|| invalid_data("convergence precedes migration intent"))?
                .convergence = Some(evidence);
        }
        KIND_COMPLETED => {
            let commit_epoch = reader.u64()?;
            reader.finish()?;
            let state = states
                .get_mut(&key)
                .ok_or_else(|| invalid_data("completion precedes migration intent"))?;
            if state
                .pending_commit_epoch
                .is_some_and(|pending| pending != commit_epoch)
            {
                return Err(invalid_data(
                    "completion does not match pending commit intent",
                ));
            }
            state.pending_commit_epoch = None;
            state.completed_commit_epoch = Some(commit_epoch);
        }
        KIND_COMMIT_INTENT => {
            let commit_epoch = reader.u64()?;
            reader.finish()?;
            let state = states
                .get_mut(&key)
                .ok_or_else(|| invalid_data("commit intent precedes migration intent"))?;
            if state.completed_commit_epoch == Some(commit_epoch) {
                // Idempotent replay after a compact/copy sequence.
            } else if state.pending_commit_epoch.is_none()
                || state.pending_commit_epoch == Some(commit_epoch)
            {
                state.pending_commit_epoch = Some(commit_epoch);
            } else {
                return Err(invalid_data(
                    "conflicting commit intents in migration journal",
                ));
            }
        }
        KIND_COMMIT_ABORTED => {
            let commit_epoch = reader.u64()?;
            reader.finish()?;
            let state = states
                .get_mut(&key)
                .ok_or_else(|| invalid_data("commit abort precedes migration intent"))?;
            if state.pending_commit_epoch != Some(commit_epoch) {
                return Err(invalid_data("commit abort does not match pending intent"));
            }
            state.pending_commit_epoch = None;
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown cache migration journal record kind {other}"),
            ));
        }
    }

    Ok(())
}

fn transfer_request_identity(
    message: &CacheTransportMessage,
) -> io::Result<(u64, u64, u16, CacheShardOwner, CacheShardOwner)> {
    match message {
        CacheTransportMessage::TransferBatch {
            transfer_id,
            placement_epoch,
            source,
            target,
            batch,
        } => Ok((*transfer_id, *placement_epoch, batch.slot, *source, *target)),
        _ => Err(invalid_data(
            "journal transfer request is not TransferBatch",
        )),
    }
}

fn transfer_ack_identity(
    message: &CacheTransportMessage,
) -> io::Result<(u64, u64, u16, CacheShardOwner, CacheShardOwner)> {
    match message {
        CacheTransportMessage::TransferAck {
            transfer_id,
            placement_epoch,
            source,
            target,
            slot,
            ..
        } => Ok((*transfer_id, *placement_epoch, *slot, *source, *target)),
        _ => Err(invalid_data("journal transfer ACK is not TransferAck")),
    }
}

fn require_message_matches_migration(
    key: CacheMigrationKey,
    placement_epoch: u64,
    slot: u16,
    source: CacheShardOwner,
    target: CacheShardOwner,
) -> io::Result<()> {
    if placement_epoch < key.started_epoch {
        return Err(invalid_data("cache transfer predates migration intent"));
    }
    if slot == key.slot && source == key.source && target == key.target {
        Ok(())
    } else {
        Err(invalid_data("cache migration journal key mismatch"))
    }
}

fn validate_ack_matches_request(
    request: &CacheTransportMessage,
    ack: &CacheTransportMessage,
) -> io::Result<()> {
    let CacheTransportMessage::TransferBatch {
        transfer_id: request_id,
        placement_epoch: request_epoch,
        source: request_source,
        target: request_target,
        batch,
    } = request
    else {
        return Err(invalid_data("durable request is not TransferBatch"));
    };
    let CacheTransportMessage::TransferAck {
        transfer_id: ack_id,
        placement_epoch: ack_epoch,
        source: ack_source,
        target: ack_target,
        slot,
        results,
    } = ack
    else {
        return Err(invalid_data("durable ACK is not TransferAck"));
    };

    if request_id != ack_id
        || request_epoch != ack_epoch
        || request_source != ack_source
        || request_target != ack_target
        || batch.slot != *slot
        || batch.entries.len() != results.len()
    {
        return Err(invalid_data("transfer ACK does not match durable request"));
    }
    Ok(())
}

fn write_key(out: &mut Vec<u8>, key: CacheMigrationKey) {
    write_u64(out, key.started_epoch);
    write_u16(out, key.slot);
    write_owner(out, key.source);
    write_owner(out, key.target);
}

fn write_owner(out: &mut Vec<u8>, owner: CacheShardOwner) {
    write_u64(out, owner.node_id);
    write_u16(out, owner.shard);
}

fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn write_blob(out: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| invalid_data("journal blob too large"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

struct JournalReader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> JournalReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let end = self
            .cursor
            .checked_add(len)
            .ok_or_else(|| invalid_data("journal record length overflow"))?;
        if end > self.bytes.len() {
            return Err(invalid_data("truncated cache migration journal record"));
        }
        let slice = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(slice)
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("u16 slice"),
        ))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("u32 slice"),
        ))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("u64 slice"),
        ))
    }

    fn boolean(&mut self) -> io::Result<bool> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid_data("invalid journal boolean")),
        }
    }

    fn owner(&mut self) -> io::Result<CacheShardOwner> {
        Ok(CacheShardOwner {
            node_id: self.u64()?,
            shard: self.u16()?,
        })
    }

    fn key(&mut self) -> io::Result<CacheMigrationKey> {
        Ok(CacheMigrationKey {
            started_epoch: self.u64()?,
            slot: self.u16()?,
            source: self.owner()?,
            target: self.owner()?,
        })
    }

    fn blob(&mut self) -> io::Result<Vec<u8>> {
        let len = self.u32()? as usize;
        if len > MAX_JOURNAL_RECORD_BYTES {
            return Err(invalid_data("journal blob too large"));
        }
        Ok(self.take(len)?.to_vec())
    }

    fn finish(self) -> io::Result<()> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid_data(
                "trailing bytes in cache migration journal record",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::{
        redis_slot, CacheTransferBatch, CacheTransferEntry, CacheTransferToken, CacheTransferValue,
    };
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "nulang-{name}-{}-{nonce}.journal",
            std::process::id()
        ))
    }

    fn incarnation() -> [u8; 16] {
        [0x5a; 16]
    }

    fn key() -> CacheMigrationKey {
        CacheMigrationKey {
            started_epoch: 7,
            slot: redis_slot(b"k{journal}"),
            source: CacheShardOwner {
                node_id: 1,
                shard: 0,
            },
            target: CacheShardOwner {
                node_id: 2,
                shard: 0,
            },
        }
    }

    fn request(key: CacheMigrationKey, transfer_id: u64) -> CacheTransportMessage {
        CacheTransportMessage::TransferBatch {
            transfer_id,
            placement_epoch: key.started_epoch,
            source: key.source,
            target: key.target,
            batch: CacheTransferBatch {
                slot: key.slot,
                entries: vec![CacheTransferEntry {
                    key: b"k{journal}".to_vec(),
                    value: CacheTransferValue::Bytes(b"value".to_vec()),
                    ttl_ms: None,
                    token: CacheTransferToken {
                        source_slot: 3,
                        source_generation: 9,
                    },
                }],
                next_cursor: None,
                scanned_slots: 1,
                payload_bytes: 15,
                exported_at_ms: 0,
            },
        }
    }

    fn ack(key: CacheMigrationKey, transfer_id: u64) -> CacheTransportMessage {
        CacheTransportMessage::TransferAck {
            transfer_id,
            placement_epoch: key.started_epoch,
            source: key.source,
            target: key.target,
            slot: key.slot,
            results: vec![CacheTransferImport::Imported],
        }
    }

    #[test]
    fn journal_round_trips_restart_proof_and_requires_fresh_probe() {
        let path = temp_path("migration-roundtrip");
        let migration = key();
        {
            let mut journal = CacheMigrationJournal::open(&path).unwrap();
            journal.record_intent(migration, incarnation()).unwrap();
            journal
                .record_transfer_sent(migration, &request(migration, 41))
                .unwrap();
            journal
                .record_transfer_ack(migration, &ack(migration, 41))
                .unwrap();
            journal.record_source_remaining(migration, 0).unwrap();
            journal
                .record_convergence(
                    migration,
                    CacheMigrationConvergenceEvidence {
                        probe_id: 50,
                        target_accepted: true,
                        source_remaining: 0,
                        target_live_entries: 1,
                        target_import_fences: 1,
                        target_conflicts: 0,
                        target_wrong_slot: 0,
                    },
                )
                .unwrap();
        }

        let journal = CacheMigrationJournal::open(&path).unwrap();
        let state = journal.recovery_state(migration).unwrap();
        assert!(state.restart_reprobe_candidate());
        assert_eq!(state.expected_import_fences(), 1);
        assert!(
            state.accepts_fresh_convergence(&CacheMigrationConvergenceEvidence {
                probe_id: 51,
                target_accepted: true,
                source_remaining: 0,
                target_live_entries: 1,
                target_import_fences: 1,
                target_conflicts: 0,
                target_wrong_slot: 0,
            })
        );
        assert!(!state.accepts_fresh_convergence(state.convergence.as_ref().unwrap()));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn crash_truncated_tail_is_discarded_before_new_appends() {
        let path = temp_path("migration-truncate");
        let migration = key();
        {
            let mut journal = CacheMigrationJournal::open(&path).unwrap();
            journal.record_intent(migration, incarnation()).unwrap();
        }
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&[KIND_SOURCE_REMAINING, 0, 0]).unwrap();
            file.sync_all().unwrap();
        }

        let mut journal = CacheMigrationJournal::open(&path).unwrap();
        assert!(journal.recovery_state(migration).is_some());
        journal.record_source_remaining(migration, 0).unwrap();
        drop(journal);

        let journal = CacheMigrationJournal::open(&path).unwrap();
        assert_eq!(
            journal.recovery_state(migration).unwrap().source_remaining,
            Some(0)
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn checksum_corruption_fails_closed() {
        let path = temp_path("migration-corrupt");
        let migration = key();
        {
            let mut journal = CacheMigrationJournal::open(&path).unwrap();
            journal.record_intent(migration, incarnation()).unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        fs::write(&path, bytes).unwrap();

        let error = CacheMigrationJournal::open(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reopened_journal_rejects_different_source_incarnation() {
        let path = temp_path("migration-incarnation");
        let migration = key();
        {
            let mut journal = CacheMigrationJournal::open(&path).unwrap();
            journal.record_intent(migration, incarnation()).unwrap();
        }

        let mut journal = CacheMigrationJournal::open(&path).unwrap();
        let error = journal.record_intent(migration, [0xa5; 16]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn pending_commit_intent_is_restart_ambiguous_until_completed_or_aborted() {
        let path = temp_path("migration-commit-intent");
        let migration = key();
        {
            let mut journal = CacheMigrationJournal::open(&path).unwrap();
            journal.record_intent(migration, incarnation()).unwrap();
            journal.record_source_remaining(migration, 0).unwrap();
            journal.record_commit_intent(migration, 8).unwrap();
        }

        let mut journal = CacheMigrationJournal::open(&path).unwrap();
        let state = journal.recovery_state(migration).unwrap();
        assert_eq!(state.pending_commit_epoch, Some(8));
        assert!(!state.restart_reprobe_candidate());

        journal.record_commit_aborted(migration, 8).unwrap();
        assert!(journal
            .recovery_state(migration)
            .unwrap()
            .pending_commit_epoch
            .is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn unacked_transfer_blocks_restart_reprobe() {
        let path = temp_path("migration-unacked");
        let migration = key();
        let mut journal = CacheMigrationJournal::open(&path).unwrap();
        journal.record_intent(migration, incarnation()).unwrap();
        journal
            .record_transfer_sent(migration, &request(migration, 77))
            .unwrap();
        journal.record_source_remaining(migration, 0).unwrap();

        let state = journal.recovery_state(migration).unwrap();
        assert!(!state.restart_reprobe_candidate());
        assert_eq!(state.unfinished_transfer_ids(), vec![77]);
        drop(journal);
        fs::remove_file(path).unwrap();
    }
}
