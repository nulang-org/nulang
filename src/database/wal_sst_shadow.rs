//! Non-authoritative WAL-to-SST materialization. The full WAL remains the
//! source of truth, including transaction IDs and receipts. No WAL deletion,
//! read routing, retention advancement, or checkpoint replacement.
use std::collections::BTreeMap;
use std::io;

use super::indexed_sst::{write_immutable_table, DiskLookup, IndexedSst, SstHeader, SstVersion};
use super::sst_manifest::{ManifestStore, TableEntry};
use super::tablet::{TabletDescriptor, TabletMutation};
use super::wal::{group_input_bytes, FileWal, WalRecord};

const MAX_RECORDS: usize = 64;
const MAX_INPUT: usize = 8 * 1024 * 1024;
const MAX_MUTATIONS: usize = 262_144;

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

type Rows<'a> = BTreeMap<&'a [u8], Vec<SstVersion<'a>>>;

fn admitted(records: &[WalRecord]) -> bool {
    records.len() <= MAX_RECORDS
        && records
            .iter()
            .map(|r| r.mutations().len())
            .fold(0usize, usize::saturating_add)
            <= MAX_MUTATIONS
        && group_input_bytes(records.iter().map(WalRecord::mutations)) <= MAX_INPUT
}

fn rows_for(records: &[WalRecord]) -> io::Result<Rows<'_>> {
    if records.is_empty() || !admitted(records) {
        return Err(invalid("shadow interval exceeds bounded admission"));
    }
    let mut rows = Rows::new();
    for record in records {
        for mutation in record.mutations() {
            let value = match mutation {
                TabletMutation::Put { value, .. } => Some(value.as_slice()),
                TabletMutation::Delete { .. } => None,
            };
            let history = rows.entry(mutation.key()).or_default();
            if let Some(last) = history
                .last_mut()
                .filter(|last| last.0 == record.sequence())
            {
                last.1 = value;
            } else {
                history.push((record.sequence(), value));
            }
        }
    }
    Ok(rows)
}

fn verify(
    table: &mut IndexedSst,
    generation: u64,
    records: &[WalRecord],
    rows: &Rows<'_>,
) -> io::Result<()> {
    let min = records
        .first()
        .ok_or_else(|| invalid("empty shadow interval"))?
        .sequence();
    let max = records.last().unwrap().sequence();
    let count: usize = rows.values().map(Vec::len).sum();
    if table.generation() != generation
        || table.sequence_interval() != (min, max)
        || table.indexed_versions() != count
    {
        return Err(invalid("shadow SST interval or count disagrees with WAL"));
    }
    for (&key, history) in rows {
        for &(sequence, expected) in history {
            let actual = table.read_at(key, sequence)?;
            let matches = match (actual, expected) {
                (DiskLookup::Deleted { sequence: found }, None) => found == sequence,
                (
                    DiskLookup::Value {
                        sequence: found,
                        value,
                    },
                    Some(expected),
                ) => found == sequence && value == expected,
                _ => false,
            };
            if !matches {
                return Err(invalid("shadow SST contents disagree with WAL"));
            }
        }
    }
    Ok(())
}

fn interval(records: &[WalRecord], min: u64, max: u64) -> io::Result<&[WalRecord]> {
    let start = usize::try_from(
        min.checked_sub(1)
            .ok_or_else(|| invalid("invalid interval start"))?,
    )
    .map_err(|_| invalid("interval start overflow"))?;
    let end = usize::try_from(max).map_err(|_| invalid("interval end overflow"))?;
    records
        .get(start..end)
        .filter(|r| !r.is_empty())
        .ok_or_else(|| invalid("manifest interval outside complete WAL"))
}

pub(super) fn flush(
    wal: &FileWal,
    descriptor: &TabletDescriptor,
    manifest: &mut ManifestStore,
    max_records: usize,
) -> io::Result<Option<TableEntry>> {
    if !(1..=MAX_RECORDS).contains(&max_records) {
        return Err(invalid("shadow record limit must be between 1 and 64"));
    }
    if manifest.requires_reopen()
        || manifest.current().tablet_id() != descriptor.id().get()
        || manifest.current().epoch() != descriptor.ownership_epoch()
    {
        return Err(invalid("manifest recovery or identity mismatch"));
    }
    let records = wal.records();
    // Verify every referenced interval before extending or reporting caught up.
    // This deliberately trades throughput for correctness during qualification.
    for entry in manifest.current().entries() {
        let (min, max) = entry.sequence_interval();
        let batch = interval(records, min, max)?;
        let rows = rows_for(batch)?;
        let mut table = manifest.open_verified_table(entry)?;
        verify(&mut table, entry.generation(), batch, &rows)?;
    }
    let start = usize::try_from(manifest.current().durable_sequence())
        .map_err(|_| invalid("manifest sequence overflow"))?;
    let pending = records
        .get(start..)
        .ok_or_else(|| invalid("manifest ahead of WAL"))?;
    if pending.is_empty() {
        return Ok(None);
    }
    let generation = manifest.current().entries().len() as u64 + 1;
    let dir = manifest.directory();
    let path = dir.join(format!("nudb-sst-{generation:020}.sst"));
    // An interrupted publication can leave a complete orphan. Never replace
    // it; require exact logical correspondence to its WAL interval.
    let entry = match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let entry = TableEntry::inspect(dir, generation)?;
            let (min, max) = entry.sequence_interval();
            if min != pending[0].sequence() {
                return Err(invalid("orphan SST starts at wrong sequence"));
            }
            let batch = interval(records, min, max)?;
            let rows = rows_for(batch)?;
            let mut table = manifest.open_verified_table(&entry)?;
            verify(&mut table, generation, batch, &rows)?;
            entry
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut count = pending.len().min(max_records);
            while count > 0 && !admitted(&pending[..count]) {
                count -= 1;
            }
            if count == 0 {
                return Err(invalid("single WAL record exceeds shadow admission"));
            }
            let batch = &pending[..count];
            let rows = rows_for(batch)?;
            let header = SstHeader {
                generation,
                min_sequence: batch[0].sequence(),
                max_sequence: batch[count - 1].sequence(),
                key_count: rows.len() as u64,
                version_count: rows.values().map(|v| v.len() as u64).sum(),
            };
            write_immutable_table(
                &path,
                header,
                rows.iter().map(|(&key, history)| (key, history.as_slice())),
            )?;
            let entry = TableEntry::inspect(dir, generation)?;
            let mut table = manifest.open_verified_table(&entry)?;
            verify(&mut table, generation, batch, &rows)?;
            entry
        }
        Err(error) => return Err(error),
    };
    manifest.append(entry.clone())?;
    Ok(Some(entry))
}

/// Qualification path: only one SST index at a time. Read newest eligible
/// interval first; a tombstone terminates the search just like a value.
/// Independently derive the exact visible mutation from the full WAL.
pub(super) fn read_verified(
    wal: &FileWal,
    descriptor: &TabletDescriptor,
    manifest: &ManifestStore,
    key: &[u8],
    snapshot: u64,
    live: Option<&[u8]>,
) -> io::Result<DiskLookup> {
    if manifest.requires_reopen()
        || manifest.current().tablet_id() != descriptor.id().get()
        || manifest.current().epoch() != descriptor.ownership_epoch()
        || manifest.current().durable_sequence() > wal.last_sequence()
        || snapshot > manifest.current().durable_sequence()
    {
        return Err(invalid(
            "shadow manifest identity, recovery, or snapshot mismatch",
        ));
    }
    let end = usize::try_from(snapshot).map_err(|_| invalid("shadow snapshot overflow"))?;
    let prefix = wal
        .records()
        .get(..end)
        .ok_or_else(|| invalid("shadow snapshot outside complete WAL"))?;
    let expected = prefix.iter().rev().find_map(|record| {
        record
            .mutations()
            .iter()
            .rev()
            .find(|m| m.key() == key)
            .map(|m| (record.sequence(), m))
    });
    let mut actual = DiskLookup::Missing;
    for entry in manifest.current().entries().iter().rev() {
        if entry.sequence_interval().0 > snapshot {
            continue;
        }
        let mut table = manifest.open_verified_table(entry)?;
        let candidate = table.read_at(key, snapshot)?;
        if candidate != DiskLookup::Missing {
            actual = candidate;
            break;
        }
    }
    let matches = match (&actual, expected) {
        (DiskLookup::Missing, None) => live.is_none(),
        (DiskLookup::Deleted { sequence }, Some((expected_seq, TabletMutation::Delete { .. }))) => {
            *sequence == expected_seq && live.is_none()
        }
        (
            DiskLookup::Value { sequence, value },
            Some((
                expected_seq,
                TabletMutation::Put {
                    value: expected_value,
                    ..
                },
            )),
        ) => *sequence == expected_seq && value == expected_value && live == Some(value.as_slice()),
        _ => false,
    };
    if !matches {
        return Err(invalid(
            "shadow point read disagrees with WAL or live tablet",
        ));
    }
    Ok(actual)
}
