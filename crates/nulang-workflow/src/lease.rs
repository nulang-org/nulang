use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerLease {
    pub resource_id: String,
    pub owner_id: String,
    pub fencing_token: u64,
    pub acquired_at_millis: u64,
    pub heartbeat_at_millis: u64,
    pub expires_at_millis: u64,
}

impl WorkerLease {
    pub fn is_expired_at(&self, now_millis: u64) -> bool {
        self.expires_at_millis <= now_millis
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseAcquireOutcome {
    Acquired(WorkerLease),
    AlreadyHeldByCaller(WorkerLease),
    HeldByOther {
        owner_id: String,
        fencing_token: u64,
        expires_at_millis: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseRenewOutcome {
    Renewed(WorkerLease),
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseReleaseOutcome {
    Released,
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceCheck {
    Current,
    Lost,
}

/// Durable worker-ownership boundary.
///
/// Implementations must issue a strictly larger fencing token whenever an
/// expired/released resource is acquired again, including by the same owner.
/// A stale worker may continue running after lease loss, so every durable state
/// commit produced by that worker must be guarded by the fencing token.
pub trait WorkerLeaseStore {
    type Error;

    fn try_acquire(
        &mut self,
        resource_id: &str,
        owner_id: &str,
        now_millis: u64,
        lease_duration_millis: u64,
    ) -> Result<LeaseAcquireOutcome, Self::Error>;

    fn heartbeat(
        &mut self,
        lease: &WorkerLease,
        now_millis: u64,
        lease_duration_millis: u64,
    ) -> Result<LeaseRenewOutcome, Self::Error>;

    fn release(
        &mut self,
        lease: &WorkerLease,
        now_millis: u64,
    ) -> Result<LeaseReleaseOutcome, Self::Error>;

    fn check_fence(
        &mut self,
        lease: &WorkerLease,
        now_millis: u64,
    ) -> Result<FenceCheck, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_expiration_is_inclusive() {
        let lease = WorkerLease {
            resource_id: "task:1".into(),
            owner_id: "worker:a".into(),
            fencing_token: 7,
            acquired_at_millis: 100,
            heartbeat_at_millis: 100,
            expires_at_millis: 200,
        };

        assert!(!lease.is_expired_at(199));
        assert!(lease.is_expired_at(200));
    }
}
