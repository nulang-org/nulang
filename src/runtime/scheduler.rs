//! Priority-aware work-stealing scheduler built on Chase-Lev deques.
//!
//! Each worker owns three local queues (High, Normal, Low). Keeping local
//! queues split by priority lets the scheduler batch-transfer work out of the
//! shared injectors without weakening the runtime's strict High > Normal > Low
//! scheduling semantics. Within a priority, workers prefer local work, then
//! batch from the global injector, then steal from peers.
//!
//! The live runtime currently has one owning scheduler thread per shard. Its
//! [`Scheduler::dequeue`] path therefore uses worker slot 0 as a locality
//! anchor and deliberately skips peer-steal scans. The generic
//! [`Scheduler::next_task`] path retains peer stealing for callers that run
//! actual multi-worker scheduling.
//!
//! This design provides:
//! - lock-free local push/pop on the owning worker
//! - batched global-to-local transfers to reduce shared-injector contention
//! - lock-free peer stealing for true multi-worker callers
//! - strict priority across both global and worker-local queues
//! - no useless peer scans on the current single-owner runtime path
//! - backoff and sleep for idle workers
//!
//! Based on the Chase-Lev algorithm (PPoPP 2005) as implemented by
//! crossbeam::deque.

use super::actor::ActorPriority;
use crossbeam::deque::{Injector, Steal, Stealer, Worker};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;

/// Best-effort pin of the calling thread to a specific logical CPU.
///
/// Realizes Nulang's thread-per-core model: each shard's scheduler thread is
/// bound to its own core so it never migrates, keeping its Chase-Lev deque
/// and ORCA state cache-hot. Returns `false` (and does nothing) when the
/// platform lacks `sched_setaffinity` or the call fails — pinning is always
/// an optional optimization, never a correctness requirement.
///
/// Enabled only when the `NULANG_PIN_CORES` env var is non-empty (opt-in).
#[cfg(target_os = "linux")]
pub fn pin_current_thread_to_cpu(cpu: usize) -> bool {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        let tid = libc::syscall(libc::SYS_gettid) as libc::c_int;
        libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

/// Non-Linux fallback: core pinning is unavailable, so report failure.
#[cfg(not(target_os = "linux"))]
pub fn pin_current_thread_to_cpu(_cpu: usize) -> bool {
    false
}

/// Whether core pinning is enabled. Reads `NULANG_PIN_CORES` (non-empty =
/// on) and caches the result after the first read so the env-mutex + String
/// alloc isn't paid on every shard start.
pub fn core_pinning_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("NULANG_PIN_CORES")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    })
}

/// Lightweight, atomics-based profiling metrics for the scheduler.
///
/// All counters are monotonically increasing unless reset via
/// [`Scheduler::reset_stats`]. They are snapshots of the underlying atomic
/// counters and are therefore not guaranteed to be mutually consistent in a
/// concurrent execution.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SchedulerStats {
    /// Total tasks successfully retrieved by any worker (local, global, or stolen).
    pub total_tasks_processed: u64,
    /// Tasks retrieved from the calling worker's own local deque.
    pub tasks_from_local_queue: u64,
    /// Tasks returned directly from a global injector queue.
    ///
    /// A batched transfer counts only its immediately returned task here;
    /// transferred remainder items count as local tasks when later consumed.
    pub tasks_from_global_queue: u64,
    /// Tasks stolen from another worker's local deque.
    pub tasks_from_steal: u64,
    /// Individual `steal()` calls against another worker's deque.
    pub steal_attempts: u64,
    /// `steal()` calls that returned a task.
    pub steal_successes: u64,
    /// Times a scheduler lookup found no work anywhere it was allowed to look.
    pub empty_polls: u64,
}

/// Profiling counters touched by worker threads. Padded to a cache line so
/// counter traffic does not false-share with scheduler queue metadata.
#[repr(align(64))]
struct SchedulerStatsInternal {
    total_tasks_processed: AtomicU64,
    tasks_from_local_queue: AtomicU64,
    tasks_from_global_queue: AtomicU64,
    tasks_from_steal: AtomicU64,
    steal_attempts: AtomicU64,
    steal_successes: AtomicU64,
    empty_polls: AtomicU64,
}

impl SchedulerStatsInternal {
    fn snapshot(&self) -> SchedulerStats {
        SchedulerStats {
            total_tasks_processed: self.total_tasks_processed.load(Ordering::Relaxed),
            tasks_from_local_queue: self.tasks_from_local_queue.load(Ordering::Relaxed),
            tasks_from_global_queue: self.tasks_from_global_queue.load(Ordering::Relaxed),
            tasks_from_steal: self.tasks_from_steal.load(Ordering::Relaxed),
            steal_attempts: self.steal_attempts.load(Ordering::Relaxed),
            steal_successes: self.steal_successes.load(Ordering::Relaxed),
            empty_polls: self.empty_polls.load(Ordering::Relaxed),
        }
    }

    fn reset(&self) {
        self.total_tasks_processed.store(0, Ordering::Relaxed);
        self.tasks_from_local_queue.store(0, Ordering::Relaxed);
        self.tasks_from_global_queue.store(0, Ordering::Relaxed);
        self.tasks_from_steal.store(0, Ordering::Relaxed);
        self.steal_attempts.store(0, Ordering::Relaxed);
        self.steal_successes.store(0, Ordering::Relaxed);
        self.empty_polls.store(0, Ordering::Relaxed);
    }
}

/// A strict-priority work-stealing scheduler with Chase-Lev deques.
///
/// Each worker has one local deque per actor priority. A single local queue
/// would allow previously batched Low/Normal work to run in front of newly
/// arrived High work. Splitting local queues preserves the same priority
/// contract as the global injectors while still allowing batch transfer and
/// cache-local dispatch.
#[repr(align(64))]
pub struct Scheduler {
    global_high: Injector<u64>,
    global: Injector<u64>,
    global_low: Injector<u64>,

    workers_high: Vec<Worker<u64>>,
    workers: Vec<Worker<u64>>,
    workers_low: Vec<Worker<u64>>,

    stealers_high: Vec<Stealer<u64>>,
    stealers: Vec<Stealer<u64>>,
    stealers_low: Vec<Stealer<u64>>,

    worker_count: usize,
    processed_count: AtomicUsize,
    stats: SchedulerStatsInternal,
}

impl Scheduler {
    /// Create a scheduler with `worker_count` worker slots.
    pub fn new(worker_count: usize) -> Self {
        fn make_band(count: usize) -> (Vec<Worker<u64>>, Vec<Stealer<u64>>) {
            let mut workers = Vec::with_capacity(count);
            let mut stealers = Vec::with_capacity(count);
            for _ in 0..count {
                let worker = Worker::new_fifo();
                stealers.push(worker.stealer());
                workers.push(worker);
            }
            (workers, stealers)
        }

        let (workers_high, stealers_high) = make_band(worker_count);
        let (workers, stealers) = make_band(worker_count);
        let (workers_low, stealers_low) = make_band(worker_count);

        Self {
            global_high: Injector::new(),
            global: Injector::new(),
            global_low: Injector::new(),
            workers_high,
            workers,
            workers_low,
            stealers_high,
            stealers,
            stealers_low,
            worker_count,
            processed_count: AtomicUsize::new(0),
            stats: SchedulerStatsInternal {
                total_tasks_processed: AtomicU64::new(0),
                tasks_from_local_queue: AtomicU64::new(0),
                tasks_from_global_queue: AtomicU64::new(0),
                tasks_from_steal: AtomicU64::new(0),
                steal_attempts: AtomicU64::new(0),
                steal_successes: AtomicU64::new(0),
                empty_polls: AtomicU64::new(0),
            },
        }
    }

    #[inline]
    fn global_for(&self, priority: ActorPriority) -> &Injector<u64> {
        match priority {
            ActorPriority::High => &self.global_high,
            ActorPriority::Normal => &self.global,
            ActorPriority::Low => &self.global_low,
        }
    }

    #[inline]
    fn workers_for(&self, priority: ActorPriority) -> &[Worker<u64>] {
        match priority {
            ActorPriority::High => &self.workers_high,
            ActorPriority::Normal => &self.workers,
            ActorPriority::Low => &self.workers_low,
        }
    }

    #[inline]
    fn stealers_for(&self, priority: ActorPriority) -> &[Stealer<u64>] {
        match priority {
            ActorPriority::High => &self.stealers_high,
            ActorPriority::Normal => &self.stealers,
            ActorPriority::Low => &self.stealers_low,
        }
    }

    #[inline]
    fn record_local_task(&self) {
        self.stats
            .total_tasks_processed
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .tasks_from_local_queue
            .fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn record_global_task(&self) {
        self.stats
            .total_tasks_processed
            .fetch_add(1, Ordering::Relaxed);
        self.stats
            .tasks_from_global_queue
            .fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    fn record_steal_attempts(&self, attempts: u64) {
        if attempts != 0 {
            self.stats
                .steal_attempts
                .fetch_add(attempts, Ordering::Relaxed);
        }
    }

    #[inline]
    fn record_stolen_task(&self, attempts: u64) {
        self.stats
            .total_tasks_processed
            .fetch_add(1, Ordering::Relaxed);
        self.stats.tasks_from_steal.fetch_add(1, Ordering::Relaxed);
        self.stats.steal_successes.fetch_add(1, Ordering::Relaxed);
        self.record_steal_attempts(attempts);
    }

    /// Push an actor at Normal priority.
    pub fn enqueue(&self, actor_id: u64) {
        self.enqueue_with_priority(actor_id, ActorPriority::Normal);
    }

    /// Push an actor onto the global injector for its priority.
    pub fn enqueue_with_priority(&self, actor_id: u64, priority: ActorPriority) {
        self.global_for(priority).push(actor_id);
    }

    /// Push onto a worker's Normal-priority local queue.
    ///
    /// Retained for compatibility with existing callers. Priority-aware code
    /// should use [`Scheduler::enqueue_local_with_priority`].
    pub fn enqueue_local(&self, worker_idx: usize, actor_id: u64) {
        self.enqueue_local_with_priority(worker_idx, actor_id, ActorPriority::Normal);
    }

    /// Push an actor onto a specific worker's local queue for its priority.
    pub fn enqueue_local_with_priority(
        &self,
        worker_idx: usize,
        actor_id: u64,
        priority: ActorPriority,
    ) {
        if let Some(worker) = self.workers_for(priority).get(worker_idx) {
            worker.push(actor_id);
        } else {
            self.global_for(priority).push(actor_id);
        }
    }

    /// Try the worker-local queue for one priority.
    fn pop_local(&self, worker_idx: usize, priority: ActorPriority) -> Option<u64> {
        let task = self.workers_for(priority).get(worker_idx)?.pop()?;
        self.record_local_task();
        Some(task)
    }

    /// Steal one item from a global priority queue without a worker-local
    /// destination. Used by external event loops and [`Scheduler::steal_one`].
    fn steal_global_for(&self, priority: ActorPriority) -> Option<u64> {
        loop {
            match self.global_for(priority).steal() {
                Steal::Success(task) => {
                    self.record_global_task();
                    return Some(task);
                }
                Steal::Retry => std::hint::spin_loop(),
                Steal::Empty => return None,
            }
        }
    }

    /// Batch work from one global priority queue into the matching worker-local
    /// queue and return one task immediately.
    fn steal_global_batch_for(&self, worker_idx: usize, priority: ActorPriority) -> Option<u64> {
        let Some(worker) = self.workers_for(priority).get(worker_idx) else {
            return self.steal_global_for(priority);
        };

        loop {
            match self.global_for(priority).steal_batch_and_pop(worker) {
                Steal::Success(task) => {
                    self.record_global_task();
                    return Some(task);
                }
                Steal::Retry => std::hint::spin_loop(),
                Steal::Empty => return None,
            }
        }
    }

    /// Steal from another worker's local queue within one priority band.
    fn steal_peer_for(&self, worker_idx: usize, priority: ActorPriority) -> (Option<u64>, u64) {
        let stealers = self.stealers_for(priority);
        let mut attempts = 0u64;

        for i in 0..stealers.len() {
            let steal_idx = (worker_idx + i + 1) % stealers.len();
            if steal_idx == worker_idx {
                continue;
            }
            attempts += 1;
            loop {
                match stealers[steal_idx].steal() {
                    Steal::Success(task) => return (Some(task), attempts),
                    Steal::Retry => std::hint::spin_loop(),
                    Steal::Empty => break,
                }
            }
        }

        (None, attempts)
    }

    /// Pop the next actor for a worker while preserving strict priority.
    ///
    /// For each priority band in High → Normal → Low order:
    /// 1. consume worker-local work,
    /// 2. batch from that priority's global injector,
    /// 3. steal from peers in that priority.
    ///
    /// This ordering is the key invariant that makes batch transfer safe: a
    /// previously batched Low task can never run before currently available
    /// High or Normal work.
    pub fn next_task(&self, worker_idx: usize) -> Option<u64> {
        let mut steal_attempts = 0u64;

        for priority in [
            ActorPriority::High,
            ActorPriority::Normal,
            ActorPriority::Low,
        ] {
            if let Some(task) = self.pop_local(worker_idx, priority) {
                self.record_steal_attempts(steal_attempts);
                return Some(task);
            }

            if let Some(task) = self.steal_global_batch_for(worker_idx, priority) {
                self.record_steal_attempts(steal_attempts);
                return Some(task);
            }

            let (task, attempts) = self.steal_peer_for(worker_idx, priority);
            steal_attempts += attempts;
            if let Some(task) = task {
                self.record_stolen_task(steal_attempts);
                return Some(task);
            }
        }

        self.stats.empty_polls.fetch_add(1, Ordering::Relaxed);
        self.record_steal_attempts(steal_attempts);
        None
    }

    /// Pop work for a scheduler thread that owns one worker-local slot.
    ///
    /// The current runtime has exactly one owning scheduler thread per shard.
    /// It therefore has no useful peers to steal from even though the
    /// scheduler is provisioned with multiple generic worker slots. Skipping
    /// peer scans here removes three empty High-band probes from the common
    /// Normal-priority path while retaining batch transfer and strict priority.
    fn next_owner_task(&self, worker_idx: usize) -> Option<u64> {
        for priority in [
            ActorPriority::High,
            ActorPriority::Normal,
            ActorPriority::Low,
        ] {
            if let Some(task) = self.pop_local(worker_idx, priority) {
                return Some(task);
            }
            if let Some(task) = self.steal_global_batch_for(worker_idx, priority) {
                return Some(task);
            }
        }

        self.stats.empty_polls.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Pop the next task for the live runtime scheduler thread.
    ///
    /// Worker slot 0 is the runtime's locality anchor. This path intentionally
    /// avoids peer-steal probes because no other scheduler worker owns the
    /// remaining slots today. Generic multi-worker callers should use
    /// [`Scheduler::next_task`].
    pub fn dequeue(&self) -> Option<u64> {
        self.next_owner_task(0)
    }

    /// Steal one task from any source without owning a local worker slot.
    ///
    /// Priority is strict: all High global/local sources are inspected before
    /// Normal, and all Normal sources before Low.
    pub fn steal_one(&self) -> Option<u64> {
        let mut steal_attempts = 0u64;

        for priority in [
            ActorPriority::High,
            ActorPriority::Normal,
            ActorPriority::Low,
        ] {
            if let Some(task) = self.steal_global_for(priority) {
                self.record_steal_attempts(steal_attempts);
                return Some(task);
            }

            for stealer in self.stealers_for(priority) {
                steal_attempts += 1;
                loop {
                    match stealer.steal() {
                        Steal::Success(task) => {
                            self.record_stolen_task(steal_attempts);
                            return Some(task);
                        }
                        Steal::Retry => std::hint::spin_loop(),
                        Steal::Empty => break,
                    }
                }
            }
        }

        self.stats.empty_polls.fetch_add(1, Ordering::Relaxed);
        self.record_steal_attempts(steal_attempts);
        None
    }

    /// Run a worker until the scheduler stays empty through its backoff.
    pub fn run_worker<F>(&self, worker_idx: usize, mut process_fn: F)
    where
        F: FnMut(u64),
    {
        const MAX_STEAL_ATTEMPTS: usize = 3;
        const EMPTY_SLEEP_US: u64 = 100;

        let mut empty_count = 0;

        loop {
            if let Some(actor_id) = self.next_task(worker_idx) {
                empty_count = 0;
                process_fn(actor_id);
                self.processed_count.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            empty_count += 1;
            if empty_count < MAX_STEAL_ATTEMPTS {
                continue;
            }

            thread::sleep(std::time::Duration::from_micros(EMPTY_SLEEP_US));

            // Process work discovered by the post-backoff probe. Calling only
            // `.is_none()` here would dequeue and silently drop a task.
            match self.next_task(worker_idx) {
                Some(actor_id) => {
                    empty_count = 0;
                    process_fn(actor_id);
                    self.processed_count.fetch_add(1, Ordering::Relaxed);
                }
                None => return,
            }
        }
    }

    /// Process one task for the given worker.
    pub fn run_one<F>(&self, worker_idx: usize, mut process_fn: F) -> bool
    where
        F: FnMut(u64),
    {
        if let Some(actor_id) = self.next_task(worker_idx) {
            process_fn(actor_id);
            self.processed_count.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Number of configured worker slots.
    pub fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// Total number of tasks processed through `run_one` / `run_worker`.
    pub fn processed_count(&self) -> usize {
        self.processed_count.load(Ordering::Relaxed)
    }

    /// Reset the processed count.
    pub fn reset_processed_count(&self) {
        self.processed_count.store(0, Ordering::Relaxed);
    }

    /// Snapshot scheduler profiling counters.
    pub fn stats(&self) -> SchedulerStats {
        self.stats.snapshot()
    }

    /// Reset scheduler profiling counters.
    pub fn reset_stats(&self) {
        self.stats.reset();
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_enqueue_dequeue() {
        let s = Scheduler::new(2);
        s.enqueue(42);
        s.enqueue(43);
        let v1 = s.steal_one().unwrap();
        let v2 = s.steal_one().unwrap();
        assert!((v1 == 42 && v2 == 43) || (v1 == 43 && v2 == 42));
        assert!(s.steal_one().is_none());
    }

    #[test]
    fn test_local_enqueue() {
        let s = Scheduler::new(2);
        s.enqueue_local(0, 100);
        s.enqueue_local(1, 200);
        assert_eq!(s.next_task(0).unwrap(), 100);
        assert_eq!(s.next_task(1).unwrap(), 200);
    }

    #[test]
    fn test_local_enqueue_preserves_priority() {
        let s = Scheduler::new(1);
        s.enqueue_local_with_priority(0, 1, ActorPriority::Low);
        s.enqueue_local_with_priority(0, 2, ActorPriority::Normal);
        s.enqueue_local_with_priority(0, 3, ActorPriority::High);

        assert_eq!(s.next_task(0), Some(3));
        assert_eq!(s.next_task(0), Some(2));
        assert_eq!(s.next_task(0), Some(1));
    }

    #[test]
    fn test_global_steal_batches_into_matching_local_queue() {
        let s = Scheduler::new(1);
        for id in 0..32 {
            s.enqueue(id);
        }

        let first = s.next_task(0).expect("global queue should have work");
        let first_stats = s.stats();
        assert_eq!(first_stats.tasks_from_global_queue, 1);
        assert_eq!(first_stats.tasks_from_local_queue, 0);

        let second = s
            .next_task(0)
            .expect("batched local queue should have work");
        let second_stats = s.stats();
        assert_eq!(second_stats.tasks_from_global_queue, 1);
        assert_eq!(second_stats.tasks_from_local_queue, 1);
        assert_ne!(first, second);

        let mut seen = HashSet::from([first, second]);
        while let Some(id) = s.next_task(0) {
            assert!(seen.insert(id), "actor id {id} was scheduled twice");
        }
        assert_eq!(seen.len(), 32);
    }

    #[test]
    fn test_high_preempts_batched_low_work() {
        let s = Scheduler::new(1);
        for id in 1..=32 {
            s.enqueue_with_priority(id, ActorPriority::Low);
        }
        assert!(s.next_task(0).is_some());

        s.enqueue_with_priority(999, ActorPriority::High);
        assert_eq!(s.next_task(0), Some(999));
    }

    #[test]
    fn test_normal_preempts_batched_low_work() {
        let s = Scheduler::new(1);
        for id in 1..=32 {
            s.enqueue_with_priority(id, ActorPriority::Low);
        }
        assert!(s.next_task(0).is_some());

        s.enqueue_with_priority(777, ActorPriority::Normal);
        assert_eq!(s.next_task(0), Some(777));
    }

    #[test]
    fn test_dequeue_uses_priority_safe_local_batching() {
        let s = Scheduler::new(4);
        for id in 0..16 {
            s.enqueue(id);
        }
        assert!(s.dequeue().is_some());
        assert!(s.dequeue().is_some());
        let stats = s.stats();
        assert_eq!(stats.tasks_from_global_queue, 1);
        assert_eq!(stats.tasks_from_local_queue, 1);
    }

    #[test]
    fn test_dequeue_owner_path_skips_unused_peer_scans() {
        let s = Scheduler::new(4);
        s.enqueue(42);
        assert_eq!(s.dequeue(), Some(42));
        let stats = s.stats();
        assert_eq!(stats.tasks_from_global_queue, 1);
        assert_eq!(stats.steal_attempts, 0);
        assert_eq!(stats.steal_successes, 0);
    }

    #[test]
    fn test_dequeue_preserves_priority_with_batched_local_work() {
        let s = Scheduler::new(4);
        for id in 1..=32 {
            s.enqueue_with_priority(id, ActorPriority::Low);
        }
        assert!(s.dequeue().is_some());
        s.enqueue_with_priority(1000, ActorPriority::High);
        assert_eq!(s.dequeue(), Some(1000));
    }

    #[test]
    fn test_run_one() {
        let s = Scheduler::new(2);
        let processed = Arc::new(AtomicU64::new(0));
        s.enqueue(1);
        s.enqueue(2);
        s.enqueue(3);
        for _ in 0..3 {
            let p = Arc::clone(&processed);
            s.run_one(0, |_id| {
                p.fetch_add(1, Ordering::Relaxed);
            });
        }
        assert_eq!(processed.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn test_processed_count() {
        let s = Scheduler::new(1);
        assert_eq!(s.processed_count(), 0);
        s.enqueue(7);
        s.run_one(0, |_id| {});
        assert_eq!(s.processed_count(), 1);
    }

    #[test]
    fn test_empty_scheduler() {
        let s = Scheduler::new(1);
        assert!(s.next_task(0).is_none());
        assert!(s.steal_one().is_none());
    }

    #[test]
    fn test_concurrent_enqueue() {
        // Scheduler contains owning Worker handles and is intentionally !Sync.
        // Feed it from worker threads through a channel, mirroring the runtime's
        // cross-shard channel ownership model.
        use std::thread;
        let s = Scheduler::new(4);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut handles = Vec::new();
        for t in 0..4 {
            let tx_clone = tx.clone();
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    tx_clone.send((t * 100 + i) as u64).unwrap();
                }
            }));
        }
        drop(tx);
        for val in rx {
            s.enqueue(val);
        }
        for h in handles {
            h.join().unwrap();
        }
        let count = Arc::new(AtomicU64::new(0));
        for _ in 0..400 {
            let c = Arc::clone(&count);
            s.run_one(0, move |_id| {
                c.fetch_add(1, Ordering::Relaxed);
            });
        }
        assert_eq!(count.load(Ordering::Relaxed), 400);
    }

    #[test]
    fn test_stats_local_queue() {
        let s = Scheduler::new(2);
        s.enqueue_local(0, 42);
        s.run_one(0, |_id| {});
        let stats = s.stats();
        assert_eq!(stats.total_tasks_processed, 1);
        assert_eq!(stats.tasks_from_local_queue, 1);
        assert_eq!(stats.tasks_from_global_queue, 0);
        assert_eq!(stats.tasks_from_steal, 0);
        assert_eq!(stats.steal_attempts, 0);
        assert_eq!(stats.steal_successes, 0);
        assert_eq!(stats.empty_polls, 0);
    }

    #[test]
    fn test_stats_global_queue() {
        let s = Scheduler::new(2);
        s.enqueue(42);
        s.run_one(0, |_id| {});
        let stats = s.stats();
        assert_eq!(stats.total_tasks_processed, 1);
        assert_eq!(stats.tasks_from_local_queue, 0);
        assert_eq!(stats.tasks_from_global_queue, 1);
        assert_eq!(stats.tasks_from_steal, 0);
        // next_task checks High peers before reaching Normal global work.
        assert!(stats.steal_attempts >= 1);
        assert_eq!(stats.steal_successes, 0);
        assert_eq!(stats.empty_polls, 0);
    }

    #[test]
    fn test_stats_count_failed_higher_priority_steals_before_local_work() {
        let s = Scheduler::new(2);
        s.enqueue_local_with_priority(0, 7, ActorPriority::Normal);
        assert_eq!(s.next_task(0), Some(7));
        let stats = s.stats();
        // Worker 0 checks the empty High band (including peer worker 1)
        // before consuming Normal-local work; that failed probe must be counted.
        assert_eq!(stats.tasks_from_local_queue, 1);
        assert!(stats.steal_attempts >= 1);
    }

    #[test]
    fn test_stats_steal() {
        let s = Scheduler::new(2);
        s.enqueue_local(0, 42);
        assert_eq!(s.next_task(1).unwrap(), 42);
        let stats = s.stats();
        assert_eq!(stats.total_tasks_processed, 1);
        assert_eq!(stats.tasks_from_local_queue, 0);
        assert_eq!(stats.tasks_from_global_queue, 0);
        assert_eq!(stats.tasks_from_steal, 1);
        assert_eq!(stats.steal_successes, 1);
        assert!(stats.steal_attempts >= 1);
        assert_eq!(stats.empty_polls, 0);
    }

    #[test]
    fn test_stats_empty_poll() {
        let s = Scheduler::new(1);
        assert!(s.next_task(0).is_none());
        let stats = s.stats();
        assert_eq!(stats.empty_polls, 1);
        assert_eq!(stats.total_tasks_processed, 0);
        assert_eq!(stats.steal_attempts, 0);
    }

    #[test]
    fn test_stats_steal_one_empty() {
        let s = Scheduler::new(1);
        assert!(s.steal_one().is_none());
        let stats = s.stats();
        assert_eq!(stats.empty_polls, 1);
        assert_eq!(stats.total_tasks_processed, 0);
        // steal_one has no owner slot, so it probes the one worker in each of
        // the three priority bands.
        assert_eq!(stats.steal_attempts, 3);
    }

    #[test]
    fn test_stats_reset() {
        let s = Scheduler::new(1);
        s.enqueue(1);
        s.run_one(0, |_id| {});
        assert_eq!(s.stats().total_tasks_processed, 1);
        s.reset_stats();
        let stats = s.stats();
        assert_eq!(stats.total_tasks_processed, 0);
        assert_eq!(stats.tasks_from_local_queue, 0);
        assert_eq!(stats.tasks_from_global_queue, 0);
        assert_eq!(stats.tasks_from_steal, 0);
        assert_eq!(stats.steal_attempts, 0);
        assert_eq!(stats.steal_successes, 0);
        assert_eq!(stats.empty_polls, 0);
        assert_eq!(s.processed_count(), 1);
    }
}
