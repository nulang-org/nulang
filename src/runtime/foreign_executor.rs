//! Blocking foreign-runtime executor built on the bounded host worker pool.
//!
//! The adapter intentionally uses one worker per backend instance. Foreign
//! runtimes such as CPython carry mutable interpreter/module state; serializing
//! access here preserves that state while still removing blocking work from
//! actor scheduler threads. A future backend may use one backend instance per
//! worker when it can prove safe parallelism.

use super::{
    BlockingCompletion, BlockingExecutor, BlockingExecutorConfigError, BlockingJobId,
    BlockingSubmitError, ForeignCallRequest, ForeignCallResult,
};
use crate::backends::ForeignInterop;
use crossbeam::channel::TryRecvError;
use std::sync::{Arc, Mutex};

pub struct BlockingForeignExecutor {
    backend: Arc<Mutex<Box<dyn ForeignInterop>>>,
    executor: BlockingExecutor<ForeignCallResult>,
}

impl BlockingForeignExecutor {
    pub fn new(
        backend: Box<dyn ForeignInterop>,
        queue_capacity: usize,
    ) -> Result<Self, BlockingExecutorConfigError> {
        Ok(Self {
            backend: Arc::new(Mutex::new(backend)),
            executor: BlockingExecutor::new(1, queue_capacity)?,
        })
    }

    /// Submit one fully-owned foreign call without blocking the caller.
    ///
    /// The worker owns no VM or actor references. The backend mutex is acquired
    /// only on the dedicated worker thread, never on an actor scheduler thread.
    pub fn try_submit(
        &mut self,
        request: ForeignCallRequest,
    ) -> Result<BlockingJobId, BlockingSubmitError> {
        let backend = Arc::clone(&self.backend);
        self.executor.try_submit(move || {
            let mut backend = backend
                .lock()
                .map_err(|_| "foreign backend mutex poisoned".to_string())?;
            backend.call_owned(&request)
        })
    }

    pub fn try_recv(
        &self,
    ) -> Result<BlockingCompletion<ForeignCallResult>, TryRecvError> {
        self.executor.try_recv()
    }

    pub fn drain_ready(&self) -> Vec<BlockingCompletion<ForeignCallResult>> {
        self.executor.drain_ready()
    }

    pub fn close(&mut self) {
        self.executor.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::OwnedForeignValue;
    use crate::vm::Value;
    use std::time::{Duration, Instant};

    struct StatefulFakeForeign {
        calls: i64,
    }

    impl ForeignInterop for StatefulFakeForeign {
        fn call(
            &mut self,
            _module: &str,
            _function: &str,
            _args: &[Value],
        ) -> Result<Value, String> {
            Err("legacy call path unused in test".to_string())
        }

        fn call_owned(&mut self, _request: &ForeignCallRequest) -> ForeignCallResult {
            self.calls += 1;
            Ok(OwnedForeignValue::Int(self.calls))
        }

        fn import(&mut self, _name: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn recv_until(
        executor: &BlockingForeignExecutor,
        deadline: Instant,
    ) -> BlockingCompletion<ForeignCallResult> {
        loop {
            match executor.try_recv() {
                Ok(completion) => return completion,
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(error) => panic!("foreign completion did not arrive: {error:?}"),
            }
        }
    }

    #[test]
    fn executes_owned_call_off_thread() {
        let backend = Box::new(StatefulFakeForeign { calls: 0 });
        let mut executor = BlockingForeignExecutor::new(backend, 4).unwrap();
        let request = ForeignCallRequest::new("fake", "call", vec![]);
        let id = executor.try_submit(request).unwrap();

        let completion = recv_until(&executor, Instant::now() + Duration::from_secs(1));
        assert_eq!(
            completion,
            BlockingCompletion::Finished {
                id,
                value: Ok(OwnedForeignValue::Int(1)),
            }
        );
    }

    #[test]
    fn backend_state_persists_between_jobs() {
        let backend = Box::new(StatefulFakeForeign { calls: 0 });
        let mut executor = BlockingForeignExecutor::new(backend, 4).unwrap();

        let first = executor
            .try_submit(ForeignCallRequest::new("fake", "one", vec![]))
            .unwrap();
        let first_result =
            recv_until(&executor, Instant::now() + Duration::from_secs(1));
        assert_eq!(
            first_result,
            BlockingCompletion::Finished {
                id: first,
                value: Ok(OwnedForeignValue::Int(1)),
            }
        );

        let second = executor
            .try_submit(ForeignCallRequest::new("fake", "two", vec![]))
            .unwrap();
        let second_result =
            recv_until(&executor, Instant::now() + Duration::from_secs(1));
        assert_eq!(
            second_result,
            BlockingCompletion::Finished {
                id: second,
                value: Ok(OwnedForeignValue::Int(2)),
            }
        );
    }
}
