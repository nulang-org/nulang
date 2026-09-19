//! Per-connection RESP response sequencing.
//!
//! Cross-shard and remote cache requests may complete out of order, but RESP
//! pipelining requires replies to be emitted in request order. This module
//! keeps only the minimum per-connection state needed to preserve that order.

use std::collections::{HashMap, VecDeque};

use super::cache::CacheStore;
use super::cache_dispatch::{
    CacheDispatchError, CacheDispatchOutcome, CacheDispatcher, CacheLocalReply, CacheRemoteRequest,
    CacheReplyError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePipelineError {
    Dispatch(CacheDispatchError),
    Reply(CacheReplyError),
    PipelineFull,
    UnknownRemoteRequest(u64),
}

impl From<CacheDispatchError> for CachePipelineError {
    fn from(value: CacheDispatchError) -> Self {
        Self::Dispatch(value)
    }
}

impl From<CacheReplyError> for CachePipelineError {
    fn from(value: CacheReplyError) -> Self {
        Self::Reply(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheSequencedRemoteRequest {
    pub request_id: u64,
    pub request: CacheRemoteRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePipelineSubmit {
    pub consumed: usize,
    pub remote: Option<CacheSequencedRemoteRequest>,
}

enum PendingResponse {
    Ready(Vec<u8>),
    Local(CacheLocalReply),
    Remote(u64),
}

/// Connection-local response orderer.
///
/// Direct commands use one reusable scratch buffer and emit immediately when
/// no earlier asynchronous response is pending. A direct response is copied
/// into owned storage only when it must wait behind an earlier local or remote
/// request.
pub struct CacheResponsePipeline {
    max_pending: usize,
    next_request_id: u64,
    pending: VecDeque<PendingResponse>,
    remote_ready: HashMap<u64, Vec<u8>>,
    direct_scratch: Vec<u8>,
}

impl CacheResponsePipeline {
    pub fn new(max_pending: usize) -> Self {
        assert!(max_pending > 0, "cache pipeline capacity must be non-zero");
        Self {
            max_pending,
            next_request_id: 1,
            pending: VecDeque::new(),
            remote_ready: HashMap::new(),
            direct_scratch: Vec::with_capacity(128),
        }
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Submit exactly one complete-or-incomplete RESP frame prefix.
    ///
    /// socket_out receives only responses that are safe to send in-order.
    /// Ok(None) means more input bytes are required.
    pub fn submit_frame(
        &mut self,
        dispatcher: &CacheDispatcher,
        store: &mut CacheStore,
        input: &[u8],
        now_ms: u64,
        socket_out: &mut Vec<u8>,
    ) -> Result<Option<CachePipelineSubmit>, CachePipelineError> {
        if self.pending.len() >= self.max_pending {
            return Err(CachePipelineError::PipelineFull);
        }

        self.direct_scratch.clear();
        let Some(outcome) =
            dispatcher.dispatch_frame(store, input, now_ms, &mut self.direct_scratch)?
        else {
            return Ok(None);
        };

        let submit = match outcome {
            CacheDispatchOutcome::Executed { consumed } => {
                if self.pending.is_empty() {
                    socket_out.extend_from_slice(&self.direct_scratch);
                } else {
                    self.pending
                        .push_back(PendingResponse::Ready(self.direct_scratch.clone()));
                }
                CachePipelineSubmit {
                    consumed,
                    remote: None,
                }
            }
            CacheDispatchOutcome::LocalQueued {
                consumed, reply, ..
            } => {
                debug_assert!(self.direct_scratch.is_empty());
                self.pending.push_back(PendingResponse::Local(reply));
                CachePipelineSubmit {
                    consumed,
                    remote: None,
                }
            }
            CacheDispatchOutcome::Remote { consumed, request } => {
                debug_assert!(self.direct_scratch.is_empty());
                let request_id = self.next_request_id;
                self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
                self.pending.push_back(PendingResponse::Remote(request_id));
                CachePipelineSubmit {
                    consumed,
                    remote: Some(CacheSequencedRemoteRequest {
                        request_id,
                        request,
                    }),
                }
            }
        };

        self.drain_ready(socket_out)?;
        Ok(Some(submit))
    }

    /// Record a remote response. It is held until every earlier request has
    /// completed, preserving RESP pipeline order.
    pub fn complete_remote(
        &mut self,
        request_id: u64,
        response: Vec<u8>,
    ) -> Result<(), CachePipelineError> {
        let known = self
            .pending
            .iter()
            .any(|pending| matches!(pending, PendingResponse::Remote(id) if *id == request_id));
        if !known {
            return Err(CachePipelineError::UnknownRemoteRequest(request_id));
        }
        self.remote_ready.insert(request_id, response);
        Ok(())
    }

    /// Flush the longest contiguous prefix of completed replies.
    pub fn drain_ready(&mut self, socket_out: &mut Vec<u8>) -> Result<usize, CachePipelineError> {
        let mut flushed = 0;

        loop {
            let action = match self.pending.front() {
                Some(PendingResponse::Ready(_)) => FrontAction::MoveReady,
                Some(PendingResponse::Local(reply)) => match reply.try_recv()? {
                    Some(bytes) => FrontAction::Completed(bytes),
                    None => FrontAction::Pending,
                },
                Some(PendingResponse::Remote(request_id)) => {
                    match self.remote_ready.remove(request_id) {
                        Some(bytes) => FrontAction::Completed(bytes),
                        None => FrontAction::Pending,
                    }
                }
                None => FrontAction::Pending,
            };

            let bytes = match action {
                FrontAction::MoveReady => {
                    let Some(PendingResponse::Ready(bytes)) = self.pending.pop_front() else {
                        unreachable!("front response changed while draining")
                    };
                    bytes
                }
                FrontAction::Completed(bytes) => {
                    self.pending.pop_front();
                    bytes
                }
                FrontAction::Pending => break,
            };

            socket_out.extend_from_slice(&bytes);
            flushed += 1;
        }

        Ok(flushed)
    }
}

enum FrontAction {
    MoveReady,
    Completed(Vec<u8>),
    Pending,
}

#[cfg(test)]
mod tests {
    use super::super::cache::redis_slot;
    use super::super::cache_dispatch::CacheDispatchChannels;
    use super::super::cache_routing::{CacheShardOwner, CacheSlotMap, CacheSlotRange};
    use super::*;

    fn key_for_shard(map: &CacheSlotMap, shard: u16) -> Vec<u8> {
        for i in 0..10_000 {
            let key = format!("pipeline-key-{i}").into_bytes();
            if map.owner_for_key(&key).shard == shard {
                return key;
            }
        }
        panic!("could not find key for shard");
    }

    fn frame(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = format!("*{}\r\n", parts.len()).into_bytes();
        for part in parts {
            out.extend_from_slice(format!("${}\r\n", part.len()).as_bytes());
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    #[test]
    fn direct_responses_flush_immediately() {
        let placement = CacheSlotMap::new_local(1, 1).unwrap();
        let (channels, _inboxes) = CacheDispatchChannels::new(1, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, placement, channels).unwrap();
        let mut pipeline = CacheResponsePipeline::new(16);
        let mut store = CacheStore::new();
        let mut socket_out = Vec::new();

        let ping = frame(&[b"PING"]);
        let submit = pipeline
            .submit_frame(&dispatcher, &mut store, &ping, 0, &mut socket_out)
            .unwrap()
            .unwrap();

        assert_eq!(submit.consumed, ping.len());
        assert!(submit.remote.is_none());
        assert_eq!(socket_out, b"+PONG\r\n");
        assert!(pipeline.is_empty());
    }

    #[test]
    fn later_direct_response_waits_for_earlier_cross_shard_reply() {
        let placement = CacheSlotMap::new_local(1, 2).unwrap();
        let remote_local_key = key_for_shard(&placement, 1);
        let (channels, mut inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, placement, channels).unwrap();
        let mut pipeline = CacheResponsePipeline::new(16);
        let mut ingress_store = CacheStore::new();
        let mut owner_store = CacheStore::new();
        let mut socket_out = Vec::new();

        let set = frame(&[b"SET", &remote_local_key, b"value"]);
        pipeline
            .submit_frame(&dispatcher, &mut ingress_store, &set, 0, &mut socket_out)
            .unwrap()
            .unwrap();

        let ping = frame(&[b"PING"]);
        pipeline
            .submit_frame(&dispatcher, &mut ingress_store, &ping, 0, &mut socket_out)
            .unwrap()
            .unwrap();

        assert!(socket_out.is_empty());
        assert_eq!(pipeline.pending_len(), 2);

        assert!(inboxes[1].try_process_one(&mut owner_store));
        assert_eq!(pipeline.drain_ready(&mut socket_out).unwrap(), 2);
        assert_eq!(socket_out, b"+OK\r\n+PONG\r\n");
        assert!(pipeline.is_empty());
    }

    #[test]
    fn remote_response_orders_before_later_direct_response() {
        let mut placement = CacheSlotMap::new_local(1, 1).unwrap();
        let key = b"remote-pipeline-key";
        let slot = redis_slot(key);
        placement
            .apply_epoch(
                1,
                &[CacheSlotRange {
                    start: slot,
                    end: slot,
                    owner: CacheShardOwner {
                        node_id: 9,
                        shard: 0,
                    },
                }],
            )
            .unwrap();

        let (channels, _inboxes) = CacheDispatchChannels::new(1, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, placement, channels).unwrap();
        let mut pipeline = CacheResponsePipeline::new(16);
        let mut store = CacheStore::new();
        let mut socket_out = Vec::new();

        let get = frame(&[b"GET", key]);
        let remote = pipeline
            .submit_frame(&dispatcher, &mut store, &get, 0, &mut socket_out)
            .unwrap()
            .unwrap()
            .remote
            .expect("remote handoff");

        let ping = frame(&[b"PING"]);
        pipeline
            .submit_frame(&dispatcher, &mut store, &ping, 0, &mut socket_out)
            .unwrap()
            .unwrap();

        assert!(socket_out.is_empty());
        pipeline
            .complete_remote(remote.request_id, b"$5\r\nvalue\r\n".to_vec())
            .unwrap();
        assert_eq!(pipeline.drain_ready(&mut socket_out).unwrap(), 2);
        assert_eq!(socket_out, b"$5\r\nvalue\r\n+PONG\r\n");
    }

    #[test]
    fn pending_limit_applies_backpressure_before_consuming_next_request() {
        let placement = CacheSlotMap::new_local(1, 2).unwrap();
        let key = key_for_shard(&placement, 1);
        let (channels, _inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, placement, channels).unwrap();
        let mut pipeline = CacheResponsePipeline::new(1);
        let mut store = CacheStore::new();
        let mut socket_out = Vec::new();

        let get = frame(&[b"GET", &key]);
        pipeline
            .submit_frame(&dispatcher, &mut store, &get, 0, &mut socket_out)
            .unwrap()
            .unwrap();

        let ping = frame(&[b"PING"]);
        assert_eq!(
            pipeline.submit_frame(&dispatcher, &mut store, &ping, 0, &mut socket_out),
            Err(CachePipelineError::PipelineFull)
        );
        assert!(socket_out.is_empty());
    }

    #[test]
    fn unknown_remote_completion_is_rejected() {
        let mut pipeline = CacheResponsePipeline::new(4);
        assert_eq!(
            pipeline.complete_remote(99, b"+OK\r\n".to_vec()),
            Err(CachePipelineError::UnknownRemoteRequest(99))
        );
    }
}
