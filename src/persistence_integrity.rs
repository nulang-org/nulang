//! Integrity primitives for durable persistence recovery.
//!
//! Persistence backends should use these helpers at recovery boundaries instead
//! of opportunistically skipping malformed records. A durable replay may only
//! consume a valid prefix: once an interior record is corrupt or sequence order
//! becomes invalid, later records must not be applied.

use serde::de::DeserializeOwned;
use std::fmt;

/// Sequence-order contract for one persisted stream.
///
/// Not every Nulang persistence stream is contiguous by itself: message,
/// workflow, and event records may draw from an actor-level sequence space and
/// therefore legitimately contain gaps when viewed independently. Callers must
/// opt into contiguity only when the stream owns every sequence value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequencePolicy {
    /// Every decoded sequence must be greater than the previous sequence.
    /// Gaps are permitted.
    StrictlyIncreasing,
    /// Every decoded sequence after the first must equal `previous + 1`.
    Contiguous,
}

/// Policy for a malformed final JSONL record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TornTailPolicy {
    /// Any malformed record is corruption, including the final record.
    Reject,
    /// A malformed *unterminated* final line may be discarded as a torn append.
    /// A malformed newline-terminated final record is still corruption.
    AllowUnterminatedFinalRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryIntegrityError {
    MalformedRecord {
        line: usize,
        message: String,
    },
    DuplicateOrRegressingSequence {
        line: usize,
        previous: u64,
        found: u64,
    },
    SequenceGap {
        line: usize,
        expected: u64,
        found: u64,
    },
    SequenceOverflow {
        line: usize,
        previous: u64,
    },
}

impl fmt::Display for RecoveryIntegrityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedRecord { line, message } => {
                write!(f, "malformed persistence record at line {line}: {message}")
            }
            Self::DuplicateOrRegressingSequence {
                line,
                previous,
                found,
            } => write!(
                f,
                "non-monotonic persistence sequence at line {line}: previous {previous}, found {found}"
            ),
            Self::SequenceGap {
                line,
                expected,
                found,
            } => write!(
                f,
                "persistence sequence gap at line {line}: expected {expected}, found {found}"
            ),
            Self::SequenceOverflow { line, previous } => write!(
                f,
                "persistence sequence overflow before line {line}: previous {previous}"
            ),
        }
    }
}

impl std::error::Error for RecoveryIntegrityError {}

/// Decode a JSONL stream without ever skipping an invalid interior record.
///
/// `sequence_of` extracts the ordering sequence from the decoded record. The
/// first record may start at any sequence; ordering rules apply between records.
/// Empty lines are records too and therefore fail JSON decoding rather than
/// being silently ignored.
///
/// With [`TornTailPolicy::AllowUnterminatedFinalRecord`], only a malformed last
/// line in a file that does *not* end in `\n` is treated as a torn append and
/// discarded. This distinguishes a plausible crash during append from a fully
/// written but corrupt record.
pub fn decode_jsonl_recovery_prefix<T, F>(
    data: &str,
    sequence_policy: SequencePolicy,
    torn_tail_policy: TornTailPolicy,
    mut sequence_of: F,
) -> Result<Vec<T>, RecoveryIntegrityError>
where
    T: DeserializeOwned,
    F: FnMut(&T) -> u64,
{
    let final_line_unterminated = !data.is_empty() && !data.ends_with('\n');
    let line_count = data.lines().count();
    let mut records = Vec::new();
    let mut previous_sequence = None;

    for (index, line) in data.lines().enumerate() {
        let line_number = index + 1;
        let record: T = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(error)
                if torn_tail_policy == TornTailPolicy::AllowUnterminatedFinalRecord
                    && final_line_unterminated
                    && line_number == line_count =>
            {
                break;
            }
            Err(error) => {
                return Err(RecoveryIntegrityError::MalformedRecord {
                    line: line_number,
                    message: error.to_string(),
                });
            }
        };

        let sequence = sequence_of(&record);
        if let Some(previous) = previous_sequence {
            if sequence <= previous {
                return Err(RecoveryIntegrityError::DuplicateOrRegressingSequence {
                    line: line_number,
                    previous,
                    found: sequence,
                });
            }

            if sequence_policy == SequencePolicy::Contiguous {
                let expected = previous.checked_add(1).ok_or(
                    RecoveryIntegrityError::SequenceOverflow {
                        line: line_number,
                        previous,
                    },
                )?;
                if sequence != expected {
                    return Err(RecoveryIntegrityError::SequenceGap {
                        line: line_number,
                        expected,
                        found: sequence,
                    });
                }
            }
        }

        previous_sequence = Some(sequence);
        records.push(record);
    }

    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct Record {
        sequence: u64,
        value: String,
    }

    fn decode(
        data: &str,
        sequence_policy: SequencePolicy,
        tail_policy: TornTailPolicy,
    ) -> Result<Vec<Record>, RecoveryIntegrityError> {
        decode_jsonl_recovery_prefix(data, sequence_policy, tail_policy, |record: &Record| {
            record.sequence
        })
    }

    #[test]
    fn valid_stream_replays_every_record() {
        let records = decode(
            "{\"sequence\":1,\"value\":\"a\"}\n{\"sequence\":2,\"value\":\"b\"}\n{\"sequence\":3,\"value\":\"c\"}\n",
            SequencePolicy::Contiguous,
            TornTailPolicy::Reject,
        )
        .unwrap();

        assert_eq!(records.len(), 3);
        assert_eq!(records[2].sequence, 3);
    }

    #[test]
    fn malformed_interior_record_fails_closed_and_never_returns_later_record() {
        let error = decode(
            "{\"sequence\":1,\"value\":\"a\"}\nnot-json\n{\"sequence\":3,\"value\":\"c\"}\n",
            SequencePolicy::StrictlyIncreasing,
            TornTailPolicy::AllowUnterminatedFinalRecord,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RecoveryIntegrityError::MalformedRecord { line: 2, .. }
        ));
    }

    #[test]
    fn unterminated_malformed_final_record_can_be_discarded_as_torn_suffix() {
        let records = decode(
            "{\"sequence\":1,\"value\":\"a\"}\n{\"sequence\":2,\"value\":\"b\"}\n{\"sequence\":3",
            SequencePolicy::Contiguous,
            TornTailPolicy::AllowUnterminatedFinalRecord,
        )
        .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[1].sequence, 2);
    }

    #[test]
    fn newline_terminated_malformed_final_record_is_corruption() {
        let error = decode(
            "{\"sequence\":1,\"value\":\"a\"}\nnot-json\n",
            SequencePolicy::Contiguous,
            TornTailPolicy::AllowUnterminatedFinalRecord,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RecoveryIntegrityError::MalformedRecord { line: 2, .. }
        ));
    }

    #[test]
    fn duplicate_sequence_is_rejected() {
        let error = decode(
            "{\"sequence\":1,\"value\":\"a\"}\n{\"sequence\":1,\"value\":\"b\"}\n",
            SequencePolicy::StrictlyIncreasing,
            TornTailPolicy::Reject,
        )
        .unwrap_err();

        assert_eq!(
            error,
            RecoveryIntegrityError::DuplicateOrRegressingSequence {
                line: 2,
                previous: 1,
                found: 1,
            }
        );
    }

    #[test]
    fn regressing_sequence_is_rejected() {
        let error = decode(
            "{\"sequence\":2,\"value\":\"a\"}\n{\"sequence\":1,\"value\":\"b\"}\n",
            SequencePolicy::StrictlyIncreasing,
            TornTailPolicy::Reject,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RecoveryIntegrityError::DuplicateOrRegressingSequence {
                previous: 2,
                found: 1,
                ..
            }
        ));
    }

    #[test]
    fn gaps_are_allowed_for_interleaved_sequence_spaces() {
        let records = decode(
            "{\"sequence\":1,\"value\":\"a\"}\n{\"sequence\":3,\"value\":\"c\"}\n",
            SequencePolicy::StrictlyIncreasing,
            TornTailPolicy::Reject,
        )
        .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[1].sequence, 3);
    }

    #[test]
    fn contiguous_policy_rejects_gaps() {
        let error = decode(
            "{\"sequence\":1,\"value\":\"a\"}\n{\"sequence\":3,\"value\":\"c\"}\n",
            SequencePolicy::Contiguous,
            TornTailPolicy::Reject,
        )
        .unwrap_err();

        assert_eq!(
            error,
            RecoveryIntegrityError::SequenceGap {
                line: 2,
                expected: 2,
                found: 3,
            }
        );
    }
}
