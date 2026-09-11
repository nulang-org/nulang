//! Work-stealing scheduler: Chase-Lev deque per worker thread.
//!
//! Each worker thread maintains a local Chase-Lev deque for LIFO
//! push/pop of actor IDs. When a worker's local deque is empty, it
//! attempts to pull a batch from the priority-ordered global injector,
//! then steals from other workers' deques (FIFO steal for load balancing).
//!
//! This design provides:
//! - Lock-free local operations (push/pop on own deque)
//! - Batched global-to-local transfers to reduce injector contention
//! - Lock-free work stealing from other workers
//! - Global overflow queues for newly spawned / requeued actors, split
//!   by actor priority (High drains before Normal before Low)
//! - Backoff and sleep for idle workers (avoids busy-waiting)
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
/// [`Scheduler::reset_stats`]. They are snapshots of the underlying
/// atomic counters and are therefore not guaranteed to be mutually
/// consistent in a concurrent execution.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SchedulerStats {
    /// Total tasks successfully retrieved by any worker (local, global, or stolen).
    pub total_tasks_processed: u64,
    /// Tasks retrieved from the calling worker's own local deque.
    pub tasks_from_local_queue: u64,
    /// Tasks retrieved directly from a global injector queue.
    ///
    /// Tasks transferred from a global injector into a worker deque by a
    /// batched steal are counted here only for the item returned immediately;
    /// the remaining items are counted as local-queue tasks when consumed.
    pub tasks_from_global_queue: u64,
    /// Tasks stolen from another worker's deque.
    pub tasks_from_steal: u64,
    /// Individual `steal()` calls against another worker's deque.
    pub steal_attempts: u64,
    /// `steal()` calls that returned a task.
    pub steal_successes: u64,
    /// Times `next_task` or `steal_one` found no work anywhere.
    pub empty_polls: u64,
}

/// Profiling counters touched by every worker thread. Padded to a cache
/// line so a worker updating one counter doesn't invalidate the line holding
/// another counter being read concurrently on a different core (false
/// sharing). Monotonically increasing unless reset via
/// [`Scheduler::reset_stats`].
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

/// A work-stealing scheduler with Chase-Lev deques.
///
/// Created with a fixed number of worker slots. Each worker thread
/// claims one slot and uses its local deque for LIFO operations.
/// Stealing uses FIFO order to promote breadth-first execution.
///
/// Actor priority: the global injector is split into three priority
/// queues (High/Normal/Low). Dequeue drains every High entry before any
/// Normal, and every Normal before any Low — strict per-level preference
/// (Erlang-like), FIFO within a level. A sustained stream of High work
/// can therefore starve lower levels; priority is a scheduling hint for
/// latency-sensitive actors, not a fairness mechanism (fairness comes
/// from the per-turn reduction budget, which is unchanged).
///
/// Padded to a cache line so the lock-free `Injector`/`Worker` deques and
/// the counters in `SchedulerStatsInternal` don't share cache lines across
/// the worker threads that access them concurrently.
#[repr(align(64))]
pub struct Scheduler {
    /// Global overflow queue for High-priority actors.
    global_high: Injector<u64>,

    /// Global overflow queue for Normal-priority actors (the default).
    global: Injector<u64>,

    /// Global overflow queue for Low-priority actors.
    global_low: Injector<u64>,

    /// Per-worker deques. Each worker has one Worker handle;
    /// all other workers hold Stealer handles to it.
    workers: Vec<Worker<u64>>,
    stealers: Vec<Stealer<u64>>,

    /// Number of worker threads this scheduler was configured for.
    worker_count: usize,

    /// Total number of actors processed (statistics).
    processed_count: AtomicUsize,

    /// Lightweight profiling counters.
    stats: SchedulerStatsInternal,
}

impl Scheduler {
    /// Create a new work-stealing scheduler for `worker_count` threads.
    ///
    /// Each worker gets its own Chase-Lev deque. The global injector
    /// handles overflow.
    pub fn new(worker_count: usize) -> Self {
        let mut workers = Vec::with_capacity(worker_count);
        let mut stealers = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let w = Worker::new_fifo();
            stealers.push(w.stealer());
            workers.push(w);
        }

        Scheduler {
            global_high: Injector::new(),
            global: Injector::new(),
            global_low: Injector::new(),
            workers,
            stealers,
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

    /// Push an actor ID onto the global injector queue at Normal priority.
    ///
    /// Used when:
    /// - A new actor is spawned (no affinity yet)
    /// - An actor is requeued after yielding / completing a message
    /// - An actor is woken from a timer or I/O event
    ///
    /// The next worker to need work will pick this actor up from the
    /// global queue or steal it via FIFO from another worker.
    pub fn enqueue(&self, actor_id: u64) {
        self.enqueue_with_priority(actor_id, ActorPriority::Normal);
    }

    /// Push an actor ID onto the global queue for its priority level.
    ///
    /// Dequeue preference is strict per level — all High entries drain
    /// before any Normal, all Normal before any Low — and FIFO within a
    /// level. The runtime reads the priority off the actor at enqueue
    /// time, so a priority change takes effect on the actor's next
    /// (re)queue.
    pub fn enqueue_with_priority(&self, actor_id: u64, priority: ActorPriority) {
        match priority {
            ActorPriority::High => self.global_high.push(actor_id),
            ActorPriority::Normal => self.global.push(actor_id),
            ActorPriority::Low => self.global_low.push(actor_id),
        }
    }

    /// Push an actor ID onto a specific worker's local deque.
    ///
    /// Used for actor affinity — if an actor was just processed by
    /// worker N, requeue it to worker N's local deque for cache
    /// locality (LIFO = hot actor stays hot).
    pub fn enqueue_local(&self, worker_idx: usize, actor_id: u64) {
        if worker_idx < self.workers.len() {
            self.workers[worker_idx].push(actor_id);
        } else {
            self.global.push(actor_id);
        }
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

    /// Steal one task from the priority-ordered global queues: every High
    /// entry drains before any Normal, every Normal before any Low (FIFO
    /// within a level). All three count toward `tasks_from_global_queue`.
    fn steal_global(&self) -> Option<u64> {
        for queue in [&self.global_high, &self.global, &self.global_low] {
            loop {
                match queue.steal() {
                    Steal::Success(task) => {
                        self.record_global_task();
                        return Some(task);
                    }
                    Steal::Retry => std::hint::spin_loop(),
                    Steal::Empty => break,
                }
            }
        }
        None
    }

    /// Pull work from the global injector into a worker-local deque and
    /// return one task immediately.
    ///
    /// `Injector::steal_batch_and_pop` moves roughly half of the selected
    /// injector queue into the worker deque in one synchronization operation.
    /// Subsequent scheduler iterations therefore hit the cheap local deque
    /// instead of contending on the shared injector for every actor. Priority
    /// ordering is preserved when choosing which global queue to batch from.
    fn steal_global_batch(&self, worker_idx: usize) -> Option<u64> {
        let Some(worker) = self.workers.get(worker_idx) else {
            return self.steal_global();
        };

        for queue in [&self.global_high, &self.global, &self.global_low] {
            loop {
                match queue.steal_batch_and_pop(worker) {
                    Steal::Success(task) => {
                        self.record_global_task();
                        return Some(task);
                    }
                    Steal::Retry => std::hint::spin_loop(),
                    Steal::Empty => break,
                }
            }
        }
        None
    }

    /// Pop the next actor ID for the given worker.
    ///
    /// Tries in order:
    /// 1. Worker's own local deque (LIFO — hot cache; not priority-aware)
    /// 2. Global injector queues (High, then Normal, then Low), batching
    ///    additional work into the local deque
    /// 3. Steal from other workers' deques (FIFO — load balancing)
    ///
    /// Returns `None` if no work is available across all sources.
    pub fn next_task(&self, worker_idx: usize) -> Option<u64> {
        // 1. Try local deque first (LIFO — cache hot)
        if worker_idx < self.workers.len() {
            if let Some(task) = self.workers[worker_idx].pop() {
                self.stats
                    .total_tasks_processed
                    .fetch_add(1, Ordering::Relaxed);
                self.stats
                    .tasks_from_local_queue
                    .fetch_add(1, Ordering::Relaxed);
                return Some(task);
            }
        }

        // 2. Pull a batch from the highest-priority non-empty global injector.
        //    The returned item counts as global; the remainder are consumed
        //    from the worker-local deque on subsequent iterations.
        if let Some(task) = self.steal_global_batch(worker_idx) {
            return Some(task);
        }

        // 3. Steal from other workers (FIFO — promotes breadth-first)
        //    We iterate in a different order per worker to reduce
        //    contention (each worker starts stealing from a different
        //    neighbor).
        let mut steal_attempts: u64 = 0;
        for i in 0..self.stealers.len() {
            let steal_idx = (worker_idx + i + 1) % self.stealers.len();
            if steal_idx == worker_idx {
                continue; // Don't steal from self
            }
            steal_attempts += 1;
            if let Steal::Success(task) = self.stealers[steal_idx].steal() {
                self.stats
                    .total_tasks_processed
                    .fetch_add(1, Ordering::Relaxed);
                self.stats.tasks_from_steal.fetch_add(1, Ordering::Relaxed);
                self.stats.steal_successes.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .steal_attempts
                    .fetch_add(steal_attempts, Ordering::Relaxed);
                return Some(task);
            }
        }

        self.stats.empty_polls.fetch_add(1, Ordering::Relaxed);
        self.stats
            .steal_attempts
            .fetch_add(steal_attempts, Ordering::Relaxed);
        None
    }

    /// Pop the next task from the scheduler.
    ///
    /// Alias for `steal_one` — used by the runtime's scheduler loop.
    pub fn dequeue(&self) -> Option<u64> {
        self.steal_one()
    }

    /// Steal one task from any source, without a local deque.
    ///
    /// Used by external event loops (I/O, timers) that need to
    /// grab work but don't have a dedicated worker thread.
    pub fn steal_one(&self) -> Option<u64> {
        // Try the global injectors first, in priority order
        if let Some(task) = self.steal_global() {
            return Some(task);
        }
        // Try any worker
        let mut steal_attempts: u64 = 0;
        for stealer in &self.stealers {
            steal_attempts += 1;
            if let Steal::Success(task) = stealer.steal() {
                self.stats
                    .total_tasks_processed
                    .fetch_add(1, Ordering::Relaxed);
                self.stats.tasks_from_steal.fetch_add(1, Ordering::Relaxed);
                self.stats.steal_successes.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .steal_attempts
                    .fetch_add(steal_attempts, Ordering::Relaxed);
                return Some(task);
            }
        }
        self.stats.empty_polls.fetch_add(1, Ordering::Relaxed);
        self.stats
            .steal_attempts
            .fetch_add(steal_attempts, Ordering::Relaxed);
        None
    }

    /// Run the scheduler loop for the given worker.
    ///
    /// Repeatedly calls `process_fn` with dequeued actor IDs until
    /// no work is available and all steal attempts fail. Then
    /// returns, allowing the caller to park the thread or check
    /// for external events.
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
            } else {
                empty_count += 1;

                if empty_count >= MAX_STEAL_ATTEMPTS {
                    // No work after multiple attempts — sleep briefly
                    // to avoid busy-waiting, then check again.
                    thread::sleep(std::time::Duration::from_micros(EMPTY_SLEEP_US));

                    // If still no work, let the caller decide
                    // whether to park or continue.
                    if self.next_task(worker_idx).is_none() {
                        return;
                    }
                }
            }
        }
    }

    /// Process one task for the given worker.
    ///
    /// Returns `true` if a task was processed, `false` if no work
    /// was available.
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

    /// Number of configured worker threads.
    pub fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// Total number of actors processed since creation.
    pub fn processed_count(&self) -> usize {
        self.processed_count.load(Ordering::Relaxed)
    }

    /// Reset the processed count to zero.
    pub fn reset_processed_count(&self) {
        self.processed_count.store(0, Ordering::Relaxed);
    }

    /// Snapshot the current scheduler profiling metrics.
    pub fn stats(&self) -> SchedulerStats {
        self.stats.snapshot()
    }

    /// Reset all scheduler profiling metrics to zero.
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
    fn test_global_steal_batches_into_local_queue() {
        let s = Scheduler::new(1);
        for id in 0..32 {
            s.enqueue(id);
        }

        let first = s.next_task(0).expect("global queue should have work");
        let first_stats = s.stats();
        assert_eq!(first_stats.tasks_from_global_queue, 1);
        assert_eq!(first_stats.tasks_from_local_queue, 0);

        // steal_batch_and_pop transfers additional global work to worker 0,
        // so the next dispatch should avoid the shared injector.
        let second = s.next_task(0).expect("batched local queue should have work");
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
        // Scheduler is !Sync (Worker contains Cell), so we can't share
        // Arc<Scheduler> across threads. Instead, use a channel to collect
        // values from worker threads and enqueue from the main thread.
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
        drop(tx); // Close original sender; rx ends when all clones dropped.
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
        assert_eq!(stats.steal_attempts, 0);
        assert_eq!(stats.steal_successes, 0);
        assert_eq!(stats.empty_polls, 0);
    }

    #[test]
    fn test_stats_steal() {
        let s = Scheduler::new(2);
        s.enqueue_local(0, 42);
        // Worker 1 has no local work and no global work, so it steals from worker 0.
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
        assert_eq!(stats.steal_attempts, 0); // no other workers to attempt stealing from
    }

    #[test]
    fn test_stats_steal_one_empty() {
        let s = Scheduler::new(1);
        assert!(s.steal_one().is_none());
        let stats = s.stats();
        assert_eq!(stats.empty_polls, 1);
        assert_eq!(stats.total_tasks_processed, 0);
        // steal_one probes every stealer, including the single worker's own deque.
        assert_eq!(stats.steal_attempts, 1);
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
        assert_eq!(s.processed_count(), 1); // existing API unaffected
    }
}
