//! File-backed recovery adapter for strict JSONL persistence streams.
//!
//! The parser in `persistence_integrity` identifies a valid recovery prefix and
//! the exact byte boundary of a crash-torn terminal record. This module applies
//! that decision to the physical file: tolerated torn tails are truncated and
//! synced before the caller may append again; interior corruption is returned as
//! an error without modifying the file.

use crate::persistence_integrity::{
    decode_jsonl_recovery_prefix, RecoveryIntegrityError, SequencePolicy, TornTailPolicy,
};
use serde::de::DeserializeOwned;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JsonlFileRecovery<T> {
    pub records: Vec<T>,
    pub valid_bytes: usize,
    pub repaired_torn_tail: bool,
}

fn integrity_io_error(error: RecoveryIntegrityError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// Recover one JSONL file as a valid ordered prefix.
///
/// If the strict parser accepts an unterminated malformed final record as a
/// crash-torn suffix, this function truncates the file to the parser's exact
/// `valid_bytes` boundary and calls `sync_all()` before returning. The caller may
/// append only after this function succeeds.
///
/// This helper assumes the persistence backend already serializes writes for the
/// actor/stream. Cross-node stale-writer prevention is a separate activation
/// fencing invariant (#296).
pub(crate) fn recover_jsonl_file_in_place<T, F>(
    path: &Path,
    sequence_policy: SequencePolicy,
    torn_tail_policy: TornTailPolicy,
    sequence_of: F,
) -> io::Result<JsonlFileRecovery<T>>
where
    T: DeserializeOwned,
    F: FnMut(&T) -> u64,
{
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(JsonlFileRecovery {
                records: Vec::new(),
                valid_bytes: 0,
                repaired_torn_tail: false,
            });
        }
        Err(error) => return Err(error),
    };

    let original_len = data.len();
    let recovery = decode_jsonl_recovery_prefix(
        &data,
        sequence_policy,
        torn_tail_policy,
        sequence_of,
    )
    .map_err(integrity_io_error)?;

    let requires_truncation = recovery.requires_truncation(original_len);
    if requires_truncation {
        // Do not use append mode here: the whole point is to remove the partial
        // terminal record before the stream can be extended again.
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(recovery.valid_bytes as u64)?;
        file.sync_all()?;
    }

    Ok(JsonlFileRecovery {
        records: recovery.records,
        valid_bytes: recovery.valid_bytes,
        repaired_torn_tail: requires_truncation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Record {
        sequence: u64,
        value: String,
    }

    fn temp_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "nulang-jsonl-recovery-{label}-{}-{nonce}.jsonl",
            std::process::id()
        ))
    }

    fn recover(path: &Path) -> io::Result<JsonlFileRecovery<Record>> {
        recover_jsonl_file_in_place(
            path,
            SequencePolicy::Contiguous,
            TornTailPolicy::AllowUnterminatedFinalRecord,
            |record: &Record| record.sequence,
        )
    }

    #[test]
    fn missing_file_is_empty_not_corrupt() {
        let path = temp_path("missing");
        let recovery = recover(&path).unwrap();
        assert!(recovery.records.is_empty());
        assert_eq!(recovery.valid_bytes, 0);
        assert!(!recovery.repaired_torn_tail);
    }

    #[test]
    fn torn_terminal_record_is_physically_truncated() {
        let path = temp_path("torn");
        let good = "{\"sequence\":1,\"value\":\"a\"}\n{\"sequence\":2,\"value\":\"b\"}\n";
        let damaged = format!("{good}{{\"sequence\":3");
        fs::write(&path, damaged.as_bytes()).unwrap();

        let recovery = recover(&path).unwrap();
        assert_eq!(recovery.records.len(), 2);
        assert!(recovery.repaired_torn_tail);
        assert_eq!(recovery.valid_bytes, good.len());
        assert_eq!(fs::read_to_string(&path).unwrap(), good);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn repaired_stream_accepts_future_append_and_restarts_cleanly() {
        let path = temp_path("append-after-repair");
        let good = "{\"sequence\":1,\"value\":\"a\"}\n";
        fs::write(&path, format!("{good}{{\"sequence\":2")).unwrap();

        let first = recover(&path).unwrap();
        assert!(first.repaired_torn_tail);

        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(file, "{{\"sequence\":2,\"value\":\"b\"}}").unwrap();
            file.sync_all().unwrap();
        }

        let second = recover(&path).unwrap();
        assert_eq!(second.records.len(), 2);
        assert_eq!(second.records[0].sequence, 1);
        assert_eq!(second.records[1].sequence, 2);
        assert!(!second.repaired_torn_tail);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn interior_corruption_fails_without_modifying_file() {
        let path = temp_path("interior");
        let damaged = "{\"sequence\":1,\"value\":\"a\"}\nnot-json\n{\"sequence\":3,\"value\":\"c\"}\n";
        fs::write(&path, damaged).unwrap();
        let before = fs::read(&path).unwrap();

        let error = recover(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), before);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn newline_terminated_bad_tail_is_corruption_not_repairable_tear() {
        let path = temp_path("terminated-corrupt-tail");
        let damaged = "{\"sequence\":1,\"value\":\"a\"}\nnot-json\n";
        fs::write(&path, damaged).unwrap();
        let before = fs::read(&path).unwrap();

        let error = recover(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), before);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn non_monotonic_sequence_fails_without_truncation() {
        let path = temp_path("non-monotonic");
        let damaged = "{\"sequence\":2,\"value\":\"a\"}\n{\"sequence\":1,\"value\":\"b\"}\n";
        fs::write(&path, damaged).unwrap();
        let before = fs::read(&path).unwrap();

        let error = recover(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), before);

        let _ = fs::remove_file(path);
    }
}
