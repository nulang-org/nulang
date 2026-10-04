//! Hybrid Logical Clock primitives for distributed runtime ordering.
//!
//! The clock deliberately does not read `SystemTime` itself. Callers supply
//! physical time in microseconds, which keeps the ordering algorithm usable
//! under Nulang's deterministic simulator as well as production clocks.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A Hybrid Logical Clock timestamp.
///
/// Ordering is lexicographic by `(physical_micros, logical)`. HLC timestamps
/// capture causal ordering when every receive path calls [`HybridLogicalClock::observe`].
/// They are not globally unique by themselves; protocols that require a total
/// tie-break across nodes should pair the timestamp with a stable node/actor id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HlcTimestamp {
    physical_micros: u64,
    logical: u32,
}

impl HlcTimestamp {
    pub const fn new(physical_micros: u64, logical: u32) -> Self {
        Self {
            physical_micros,
            logical,
        }
    }

    pub const fn physical_micros(self) -> u64 {
        self.physical_micros
    }

    pub const fn logical(self) -> u32 {
        self.logical
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HlcError {
    /// Advancing at the same physical component would wrap the logical counter.
    LogicalOverflow { physical_micros: u64 },
    /// A received timestamp is farther in the future than this clock permits.
    RemoteClockTooFarAhead {
        remote_physical_micros: u64,
        local_physical_micros: u64,
        max_future_drift_micros: u64,
    },
}

impl fmt::Display for HlcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LogicalOverflow { physical_micros } => write!(
                f,
                "hybrid logical clock counter overflow at physical time {physical_micros}us"
            ),
            Self::RemoteClockTooFarAhead {
                remote_physical_micros,
                local_physical_micros,
                max_future_drift_micros,
            } => write!(
                f,
                "remote physical clock {remote_physical_micros}us exceeds local physical clock \
                 {local_physical_micros}us by more than the configured {max_future_drift_micros}us drift"
            ),
        }
    }
}

impl std::error::Error for HlcError {}

/// Stateful Hybrid Logical Clock.
///
/// `max_future_drift_micros` is a fail-closed admission bound for received
/// timestamps. It prevents a peer with a badly skewed wall clock from forcing
/// this process arbitrarily far into the future. Set it to `u64::MAX` when the
/// surrounding protocol intentionally performs no skew validation.
#[derive(Clone, Debug)]
pub struct HybridLogicalClock {
    last: HlcTimestamp,
    max_future_drift_micros: u64,
}

impl HybridLogicalClock {
    pub const fn new(max_future_drift_micros: u64) -> Self {
        Self {
            last: HlcTimestamp::new(0, 0),
            max_future_drift_micros,
        }
    }

    pub const fn last(&self) -> HlcTimestamp {
        self.last
    }

    pub const fn max_future_drift_micros(&self) -> u64 {
        self.max_future_drift_micros
    }

    /// Produce the next timestamp for a local event.
    ///
    /// Moving physical time resets the logical component. Equal or backwards
    /// physical time increments the logical component, preserving monotonicity
    /// across clock rollback and repeated reads of the same wall-clock tick.
    pub fn tick(&mut self, physical_micros: u64) -> Result<HlcTimestamp, HlcError> {
        let next = if physical_micros > self.last.physical_micros {
            HlcTimestamp::new(physical_micros, 0)
        } else {
            HlcTimestamp::new(
                self.last.physical_micros,
                increment_logical(self.last.logical, self.last.physical_micros)?,
            )
        };

        self.last = next;
        Ok(next)
    }

    /// Merge a timestamp received from another process and produce the next
    /// local timestamp causally after both the previous local state and the
    /// remote event.
    ///
    /// The state is left unchanged when skew validation or logical overflow
    /// fails, which lets callers reject the message without partially advancing
    /// their local clock.
    pub fn observe(
        &mut self,
        remote: HlcTimestamp,
        local_physical_micros: u64,
    ) -> Result<HlcTimestamp, HlcError> {
        let remote_ahead_by = remote
            .physical_micros
            .saturating_sub(local_physical_micros);
        if remote_ahead_by > self.max_future_drift_micros {
            return Err(HlcError::RemoteClockTooFarAhead {
                remote_physical_micros: remote.physical_micros,
                local_physical_micros,
                max_future_drift_micros: self.max_future_drift_micros,
            });
        }

        let physical = self
            .last
            .physical_micros
            .max(remote.physical_micros)
            .max(local_physical_micros);

        let logical = match (
            physical == self.last.physical_micros,
            physical == remote.physical_micros,
        ) {
            (true, true) => increment_logical(
                self.last.logical.max(remote.logical),
                physical,
            )?,
            (true, false) => increment_logical(self.last.logical, physical)?,
            (false, true) => increment_logical(remote.logical, physical)?,
            (false, false) => 0,
        };

        let next = HlcTimestamp::new(physical, logical);
        self.last = next;
        Ok(next)
    }
}

fn increment_logical(logical: u32, physical_micros: u64) -> Result<u32, HlcError> {
    logical
        .checked_add(1)
        .ok_or(HlcError::LogicalOverflow { physical_micros })
}
