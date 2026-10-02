//! Resource-aware concurrent scheduling for NLAP workers.
//!
//! The scheduler deliberately keeps resource policy out of the NLAP `Task`
//! wire model. Hosts attach a [`ResourceVector`] to each [`WorkItem`] at
//! execution time, which lets local and hosted runtimes express resources such
//! as worker slots, browsers, GPUs, or provider-specific concurrency without
//! changing durable task identity.

use crate::Worker;
use nulang_ai_core::Task;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

/// Deterministic named resource quantities.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceVector {
    units: BTreeMap<String, u32>,
}

impl ResourceVector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the quantity for `resource`. A zero quantity removes the entry.
    pub fn with(mut self, resource: impl Into<String>, units: u32) -> Self {
        let resource = resource.into();
        if units == 0 {
            self.units.remove(&resource);
        } else {
            self.units.insert(resource, units);
        }
        self
    }

    pub fn get(&self, resource: &str) -> u32 {
        self.units.get(resource).copied().unwrap_or(0)
    }

    fn iter(&self) -> impl Iterator<Item = (&String, &u32)> {
        self.units.iter()
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    #[error("scheduler capacity for {resource} must be greater than zero")]
    InvalidCapacity { resource: String },
    #[error("task {task_id} is already admitted")]
    DuplicateLease { task_id: Uuid },
    #[error("task {task_id} appears more than once in the scatter set")]
    DuplicateTask { task_id: Uuid },
    #[error(
        "resource {resource} request {requested} exceeds total scheduler capacity {capacity}"
    )]
    ExceedsCapacity {
        resource: String,
        requested: u32,
        capacity: u32,
    },
    #[error(
        "resource {resource} request {requested} exceeds currently available capacity {available}"
    )]
    TemporarilyUnavailable {
        resource: String,
        requested: u32,
        available: u32,
    },
    #[error("no active admission lease exists for task {task_id}")]
    UnknownLease { task_id: Uuid },
    #[error("worker panicked while executing task {task_id}")]
    WorkerPanicked { task_id: Uuid },
    #[error("scheduler completion channel closed while {running} tasks were active")]
    CompletionChannelClosed { running: usize },
    #[error("scheduler made no progress with {pending} tasks pending")]
    SchedulerStalled { pending: usize },
    #[error("scheduler completed {completed} of {expected} submitted tasks")]
    IncompleteScatter { expected: usize, completed: usize },
}

/// An owned admission token. Fields are private so callers cannot forge a
/// different resource release than the one recorded by the pool.
#[derive(Debug)]
pub struct AdmissionLease {
    task_id: Uuid,
    resources: ResourceVector,
}

/// In-memory admission state for recyclable concurrent resources.
#[derive(Debug)]
pub struct AdmissionPool {
    capacity: ResourceVector,
    used: ResourceVector,
    active: BTreeMap<Uuid, ResourceVector>,
}

impl AdmissionPool {
    pub fn new(capacity: ResourceVector) -> Self {
        Self {
            capacity,
            used: ResourceVector::new(),
            active: BTreeMap::new(),
        }
    }

    pub fn in_use(&self, resource: &str) -> u32 {
        self.used.get(resource)
    }

    fn validate_request(&self, request: &ResourceVector) -> Result<(), AdmissionError> {
        for (resource, requested) in request.iter() {
            let capacity = self.capacity.get(resource);
            if *requested > capacity {
                return Err(AdmissionError::ExceedsCapacity {
                    resource: resource.clone(),
                    requested: *requested,
                    capacity,
                });
            }
        }
        Ok(())
    }

    pub fn try_acquire(
        &mut self,
        task_id: Uuid,
        resources: ResourceVector,
    ) -> Result<AdmissionLease, AdmissionError> {
        if self.active.contains_key(&task_id) {
            return Err(AdmissionError::DuplicateLease { task_id });
        }
        self.validate_request(&resources)?;

        for (resource, requested) in resources.iter() {
            let available = self
                .capacity
                .get(resource)
                .saturating_sub(self.used.get(resource));
            if *requested > available {
                return Err(AdmissionError::TemporarilyUnavailable {
                    resource: resource.clone(),
                    requested: *requested,
                    available,
                });
            }
        }

        for (resource, requested) in resources.iter() {
            let next = self.used.get(resource).saturating_add(*requested);
            self.used.units.insert(resource.clone(), next);
        }
        self.active.insert(task_id, resources.clone());

        Ok(AdmissionLease { task_id, resources })
    }

    pub fn release(&mut self, lease: AdmissionLease) -> Result<(), AdmissionError> {
        let recorded = self
            .active
            .remove(&lease.task_id)
            .ok_or(AdmissionError::UnknownLease {
                task_id: lease.task_id,
            })?;

        // The lease is not forgeable outside this module. Keep the equality
        // assertion in debug builds as an internal scheduler invariant.
        debug_assert_eq!(recorded, lease.resources);
        for (resource, released) in recorded.iter() {
            let remaining = self.used.get(resource).saturating_sub(*released);
            if remaining == 0 {
                self.used.units.remove(resource);
            } else {
                self.used.units.insert(resource.clone(), remaining);
            }
        }
        Ok(())
    }
}

/// One task plus host-side concurrent resource requirements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub task: Task,
    pub resources: ResourceVector,
}

impl WorkItem {
    /// Every task consumes one logical worker slot unless explicitly overridden.
    pub fn new(task: Task) -> Self {
        Self {
            task,
            resources: ResourceVector::new().with("worker", 1),
        }
    }

    pub fn require(mut self, resource: impl Into<String>, units: u32) -> Self {
        self.resources = self.resources.with(resource, units);
        self
    }
}

/// Structured result for one isolated worker execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerReport {
    pub task: Task,
    pub agent_id: String,
    pub resources: ResourceVector,
    pub elapsed_micros: u64,
    pub started_offset_micros: u64,
    pub completed_offset_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScatterMetrics {
    pub submitted: usize,
    pub completed: usize,
    pub peak_parallelism: usize,
    /// Longest individual branch in this scatter stage.
    pub critical_path_micros: u64,
    pub total_worker_micros: u64,
    /// End-to-end scheduler time including resource queueing and scheduler cost.
    pub wall_time_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScatterReport {
    /// Reports are returned in input order, independent of completion order.
    pub reports: Vec<WorkerReport>,
    pub metrics: ScatterMetrics,
}

#[derive(Debug, Clone)]
pub struct SwarmScheduler {
    capacity: ResourceVector,
}

impl SwarmScheduler {
    pub fn new(capacity: ResourceVector) -> Result<Self, AdmissionError> {
        if capacity.get("worker") == 0 {
            return Err(AdmissionError::InvalidCapacity {
                resource: "worker".to_string(),
            });
        }
        Ok(Self { capacity })
    }

    /// Execute all work with deterministic first-fit admission and true
    /// concurrent worker calls. Impossible requests are validated before any
    /// task starts, so configuration errors fail closed without partial work.
    pub fn scatter<W: Worker>(
        &self,
        worker: &W,
        items: Vec<WorkItem>,
    ) -> Result<ScatterReport, AdmissionError> {
        let submitted = items.len();
        let mut pool = AdmissionPool::new(self.capacity.clone());
        let mut seen = BTreeSet::new();

        for item in &items {
            if !seen.insert(item.task.id) {
                return Err(AdmissionError::DuplicateTask {
                    task_id: item.task.id,
                });
            }
            pool.validate_request(&item.resources)?;
        }

        if items.is_empty() {
            return Ok(ScatterReport {
                reports: Vec::new(),
                metrics: ScatterMetrics {
                    submitted: 0,
                    completed: 0,
                    peak_parallelism: 0,
                    critical_path_micros: 0,
                    total_worker_micros: 0,
                    wall_time_micros: 0,
                },
            });
        }

        let run_started = Instant::now();
        let (tx, rx) = mpsc::channel();
        let mut pending: VecDeque<_> = items.into_iter().enumerate().collect();
        let mut reports: Vec<Option<WorkerReport>> = vec![None; submitted];
        let mut running = 0usize;
        let mut peak_parallelism = 0usize;

        std::thread::scope(|scope| -> Result<(), AdmissionError> {
            while !pending.is_empty() || running > 0 {
                let mut deferred = VecDeque::new();

                while let Some((index, item)) = pending.pop_front() {
                    match pool.try_acquire(item.task.id, item.resources.clone()) {
                        Ok(lease) => {
                            running += 1;
                            peak_parallelism = peak_parallelism.max(running);

                            let tx = tx.clone();
                            let agent_id = worker.agent_id().to_string();
                            let task = item.task;
                            let resources = item.resources;
                            scope.spawn(move || {
                                let task_id = task.id;
                                let started_offset_micros = micros(run_started.elapsed());
                                let execution_started = Instant::now();
                                let execution = catch_unwind(AssertUnwindSafe(|| worker.execute(&task)));
                                let elapsed_micros = micros(execution_started.elapsed());
                                let completed_offset_micros = micros(run_started.elapsed());

                                let message = match execution {
                                    Ok(task) => Completion::Finished {
                                        index,
                                        lease,
                                        report: WorkerReport {
                                            task,
                                            agent_id,
                                            resources,
                                            elapsed_micros,
                                            started_offset_micros,
                                            completed_offset_micros,
                                        },
                                    },
                                    Err(_) => Completion::Panicked {
                                        task_id,
                                        lease,
                                    },
                                };
                                let _ = tx.send(message);
                            });
                        }
                        Err(AdmissionError::TemporarilyUnavailable { .. }) => {
                            deferred.push_back((index, item));
                        }
                        Err(error) => return Err(error),
                    }
                }

                pending = deferred;
                if running == 0 {
                    if pending.is_empty() {
                        break;
                    }
                    return Err(AdmissionError::SchedulerStalled {
                        pending: pending.len(),
                    });
                }

                match rx.recv() {
                    Ok(Completion::Finished {
                        index,
                        lease,
                        report,
                    }) => {
                        pool.release(lease)?;
                        reports[index] = Some(report);
                        running -= 1;
                    }
                    Ok(Completion::Panicked { task_id, lease }) => {
                        pool.release(lease)?;
                        return Err(AdmissionError::WorkerPanicked { task_id });
                    }
                    Err(_) => {
                        return Err(AdmissionError::CompletionChannelClosed { running });
                    }
                }
            }
            Ok(())
        })?;

        let completed = reports.iter().filter(|report| report.is_some()).count();
        if completed != submitted {
            return Err(AdmissionError::IncompleteScatter {
                expected: submitted,
                completed,
            });
        }

        let reports: Vec<WorkerReport> = reports.into_iter().flatten().collect();
        let critical_path_micros = reports
            .iter()
            .map(|report| report.elapsed_micros)
            .max()
            .unwrap_or(0);
        let total_worker_micros = reports.iter().fold(0u64, |total, report| {
            total.saturating_add(report.elapsed_micros)
        });

        Ok(ScatterReport {
            reports,
            metrics: ScatterMetrics {
                submitted,
                completed,
                peak_parallelism,
                critical_path_micros,
                total_worker_micros,
                wall_time_micros: micros(run_started.elapsed()),
            },
        })
    }
}

enum Completion {
    Finished {
        index: usize,
        lease: AdmissionLease,
        report: WorkerReport,
    },
    Panicked {
        task_id: Uuid,
        lease: AdmissionLease,
    },
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
