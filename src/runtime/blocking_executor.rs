//! Bounded worker pool for host operations that must not run on actor scheduler threads.
//!
//! The executor is deliberately generic and knows nothing about VM values,
//! actors, Python, or effect names. Jobs must own Send + 'static data so raw
//! actor-heap pointers cannot accidentally cross worker threads.

use crossbeam::channel::{self, Receiver, Sender, TryRecvError, TrySendError};
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread::{self, JoinHandle};

pub type BlockingJobId = u64;

struct BlockingJob<R: Send + 'static> {
    id: BlockingJobId,
    task: Box<dyn FnOnce() -> R + Send + 'static>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BlockingCompletion<R> {
    Finished { id: BlockingJobId, value: R },
    Panicked { id: BlockingJobId },
}

impl<R> BlockingCompletion<R> {
    pub fn id(&self) -> BlockingJobId {
        match self {
            Self::Finished { id, .. } | Self::Panicked { id } => *id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockingSubmitError {
    QueueFull,
    Closed,
    IdExhausted,
}

impl fmt::Display for BlockingSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull => write!(f, "blocking executor queue is full"),
            Self::Closed => write!(f, "blocking executor is closed"),
            Self::IdExhausted => write!(f, "blocking executor job id space exhausted"),
        }
    }
}

impl std::error::Error for BlockingSubmitError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockingExecutorConfigError {
    ZeroWorkers,
    ZeroQueueCapacity,
    WorkerSpawn(String),
}

impl fmt::Display for BlockingExecutorConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWorkers => write!(f, "blocking executor requires at least one worker"),
            Self::ZeroQueueCapacity => {
                write!(f, "blocking executor queue capacity must be non-zero")
            }
            Self::WorkerSpawn(message) => write!(f, "failed to spawn blocking worker: {message}"),
        }
    }
}

impl std::error::Error for BlockingExecutorConfigError {}

/// Fixed-size worker pool with bounded, non-blocking admission.
pub struct BlockingExecutor<R: Send + 'static> {
    request_tx: Option<Sender<BlockingJob<R>>>,
    completion_rx: Option<Receiver<BlockingCompletion<R>>>,
    workers: Vec<JoinHandle<()>>,
    next_id: BlockingJobId,
}

impl<R: Send + 'static> BlockingExecutor<R> {
    pub fn new(
        worker_count: usize,
        queue_capacity: usize,
    ) -> Result<Self, BlockingExecutorConfigError> {
        if worker_count == 0 {
            return Err(BlockingExecutorConfigError::ZeroWorkers);
        }
        if queue_capacity == 0 {
            return Err(BlockingExecutorConfigError::ZeroQueueCapacity);
        }

        let (request_tx, request_rx) = channel::bounded::<BlockingJob<R>>(queue_capacity);
        let completion_capacity = queue_capacity.saturating_add(worker_count);
        let (completion_tx, completion_rx) =
            channel::bounded::<BlockingCompletion<R>>(completion_capacity);
        let mut workers = Vec::with_capacity(worker_count);

        for index in 0..worker_count {
            let rx = request_rx.clone();
            let tx = completion_tx.clone();
            let handle = thread::Builder::new()
                .name(format!("nulang-blocking-{index}"))
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        let id = job.id;
                        let task = job.task;
                        let completion = match catch_unwind(AssertUnwindSafe(move || task())) {
                            Ok(value) => BlockingCompletion::Finished { id, value },
                            Err(_) => BlockingCompletion::Panicked { id },
                        };
                        if tx.send(completion).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|error| BlockingExecutorConfigError::WorkerSpawn(error.to_string()))?;
            workers.push(handle);
        }
        drop(completion_tx);

        Ok(Self {
            request_tx: Some(request_tx),
            completion_rx: Some(completion_rx),
            workers,
            next_id: 1,
        })
    }

    /// Try to enqueue work without parking the caller.
    pub fn try_submit<F>(&mut self, task: F) -> Result<BlockingJobId, BlockingSubmitError>
    where
        F: FnOnce() -> R + Send + 'static,
    {
        let tx = self.request_tx.as_ref().ok_or(BlockingSubmitError::Closed)?;
        let id = self.next_id;
        if id == 0 {
            return Err(BlockingSubmitError::IdExhausted);
        }
        let next = id.wrapping_add(1);
        let job = BlockingJob {
            id,
            task: Box::new(task),
        };

        match tx.try_send(job) {
            Ok(()) => {
                self.next_id = next;
                Ok(id)
            }
            Err(TrySendError::Full(_)) => Err(BlockingSubmitError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(BlockingSubmitError::Closed),
        }
    }

    pub fn try_recv(&self) -> Result<BlockingCompletion<R>, TryRecvError> {
        match &self.completion_rx {
            Some(rx) => rx.try_recv(),
            None => Err(TryRecvError::Disconnected),
        }
    }

    pub fn drain_ready(&self) -> Vec<BlockingCompletion<R>> {
        self.completion_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default()
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Stop accepting new jobs. Workers finish already-admitted work.
    pub fn close(&mut self) {
        self.request_tx.take();
    }
}

impl<R: Send + 'static> Drop for BlockingExecutor<R> {
    fn drop(&mut self) {
        self.request_tx.take();
        // Drop the receiver before joining so workers can never deadlock while
        // trying to report a completion during shutdown.
        self.completion_rx.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    fn recv_until<R: Send + 'static>(
        executor: &BlockingExecutor<R>,
        deadline: Instant,
    ) -> BlockingCompletion<R> {
        loop {
            match executor.try_recv() {
                Ok(completion) => return completion,
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::yield_now()
                }
                Err(error) => panic!("completion did not arrive: {error:?}"),
            }
        }
    }

    #[test]
    fn executes_owned_work_and_returns_job_identity() {
        let mut executor = BlockingExecutor::new(1, 4).unwrap();
        let id = executor.try_submit(|| 21 * 2).unwrap();
        let completion = recv_until(&executor, Instant::now() + Duration::from_secs(1));
        assert_eq!(completion, BlockingCompletion::Finished { id, value: 42 });
    }

    #[test]
    fn bounded_admission_reports_backpressure() {
        let mut executor = BlockingExecutor::new(1, 1).unwrap();
        let gate = Arc::new(Barrier::new(2));
        let worker_gate = gate.clone();

        executor
            .try_submit(move || {
                worker_gate.wait();
                1u8
            })
            .unwrap();

        loop {
            match executor.try_submit(|| 2u8) {
                Ok(_) => break,
                Err(BlockingSubmitError::QueueFull) => std::thread::yield_now(),
                Err(error) => panic!("unexpected submit error: {error}"),
            }
        }

        assert_eq!(
            executor.try_submit(|| 3u8),
            Err(BlockingSubmitError::QueueFull)
        );
        gate.wait();
    }

    #[test]
    fn panic_is_isolated_and_worker_continues() {
        let mut executor = BlockingExecutor::new(1, 4).unwrap();
        let panicked = executor
            .try_submit(|| -> u32 { panic!("host operation panic") })
            .unwrap();
        let finished = executor.try_submit(|| 7u32).unwrap();

        assert_eq!(
            recv_until(&executor, Instant::now() + Duration::from_secs(1)),
            BlockingCompletion::Panicked { id: panicked }
        );
        assert_eq!(
            recv_until(&executor, Instant::now() + Duration::from_secs(1)),
            BlockingCompletion::Finished {
                id: finished,
                value: 7
            }
        );
    }

    #[test]
    fn invalid_configuration_fails_closed() {
        assert!(matches!(
            BlockingExecutor::<u8>::new(0, 1),
            Err(BlockingExecutorConfigError::ZeroWorkers)
        ));
        assert!(matches!(
            BlockingExecutor::<u8>::new(1, 0),
            Err(BlockingExecutorConfigError::ZeroQueueCapacity)
        ));
    }
}
