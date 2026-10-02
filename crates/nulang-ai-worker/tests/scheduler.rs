use nulang_ai_core::{ManagerKind, Task, TaskStatus};
use nulang_ai_worker::{
    AdmissionError, AdmissionPool, ResourceVector, SwarmScheduler, WorkItem, Worker,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

fn task(goal_id: Uuid, description: &str) -> Task {
    Task::new(goal_id, description, ManagerKind::Engineering)
}

struct ProbeWorker {
    current: AtomicUsize,
    peak: AtomicUsize,
    sleep_ms: u64,
}

impl ProbeWorker {
    fn new(sleep_ms: u64) -> Self {
        Self {
            current: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            sleep_ms,
        }
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl Worker for ProbeWorker {
    fn agent_id(&self) -> &str {
        "probe"
    }

    fn execute(&self, task: &Task) -> Task {
        let active = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(self.sleep_ms));
        self.current.fetch_sub(1, Ordering::SeqCst);

        let mut completed = task.clone();
        completed.status = TaskStatus::Completed;
        completed
    }
}

#[test]
fn admission_pool_enforces_named_resources_and_releases() {
    let capacity = ResourceVector::new().with("worker", 2).with("browser", 1);
    let mut pool = AdmissionPool::new(capacity);
    let browser = ResourceVector::new().with("worker", 1).with("browser", 1);

    let first = pool.try_acquire(Uuid::new_v4(), browser.clone()).unwrap();
    assert_eq!(pool.in_use("worker"), 1);
    assert_eq!(pool.in_use("browser"), 1);

    let err = pool
        .try_acquire(Uuid::new_v4(), browser.clone())
        .unwrap_err();
    assert!(matches!(
        err,
        AdmissionError::TemporarilyUnavailable { ref resource, .. } if resource == "browser"
    ));

    pool.release(first).unwrap();
    let second = pool.try_acquire(Uuid::new_v4(), browser).unwrap();
    pool.release(second).unwrap();
    assert_eq!(pool.in_use("worker"), 0);
    assert_eq!(pool.in_use("browser"), 0);
}

#[test]
fn scatter_runs_real_work_concurrently_and_preserves_input_order() {
    let goal_id = Uuid::new_v4();
    let worker = ProbeWorker::new(25);
    let scheduler = SwarmScheduler::new(ResourceVector::new().with("worker", 2)).unwrap();
    let items = vec![
        WorkItem::new(task(goal_id, "one")),
        WorkItem::new(task(goal_id, "two")),
        WorkItem::new(task(goal_id, "three")),
        WorkItem::new(task(goal_id, "four")),
    ];

    let report = scheduler.scatter(&worker, items).unwrap();

    assert_eq!(worker.peak(), 2);
    assert_eq!(report.metrics.peak_parallelism, 2);
    assert_eq!(report.reports.len(), 4);
    assert_eq!(report.reports[0].task.description, "one");
    assert_eq!(report.reports[1].task.description, "two");
    assert_eq!(report.reports[2].task.description, "three");
    assert_eq!(report.reports[3].task.description, "four");
    assert!(report
        .reports
        .iter()
        .all(|item| item.task.status == TaskStatus::Completed));
    assert!(report.metrics.critical_path_micros > 0);
    assert!(report.metrics.total_worker_micros >= report.metrics.critical_path_micros);
}

#[test]
fn scarce_named_resource_limits_parallelism_even_with_free_workers() {
    let goal_id = Uuid::new_v4();
    let worker = ProbeWorker::new(20);
    let scheduler = SwarmScheduler::new(
        ResourceVector::new().with("worker", 4).with("browser", 1),
    )
    .unwrap();
    let items = vec![
        WorkItem::new(task(goal_id, "browser-a")).require("browser", 1),
        WorkItem::new(task(goal_id, "browser-b")).require("browser", 1),
        WorkItem::new(task(goal_id, "browser-c")).require("browser", 1),
    ];

    let report = scheduler.scatter(&worker, items).unwrap();

    assert_eq!(worker.peak(), 1);
    assert_eq!(report.metrics.peak_parallelism, 1);
    assert_eq!(report.reports.len(), 3);
}

#[test]
fn scheduler_fails_closed_when_a_request_can_never_fit() {
    let goal_id = Uuid::new_v4();
    let worker = ProbeWorker::new(1);
    let scheduler = SwarmScheduler::new(ResourceVector::new().with("worker", 2)).unwrap();
    let item = WorkItem::new(task(goal_id, "too-large")).require("worker", 3);

    let err = scheduler.scatter(&worker, vec![item]).unwrap_err();
    assert!(matches!(
        err,
        AdmissionError::ExceedsCapacity {
            ref resource,
            requested: 3,
            capacity: 2,
        } if resource == "worker"
    ));
}
