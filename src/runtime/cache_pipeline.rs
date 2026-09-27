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

const DEFAULT_MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

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
    max_pending_bytes: usize,
    pending_bytes: usize,
    next_request_id: u64,
    pending: VecDeque<PendingResponse>,
    remote_ready: HashMap<u64, Vec<u8>>,
    direct_scratch: Vec<u8>,
}

impl CacheResponsePipeline {
    pub fn new(max_pending: usize) -> Self {
        Self::with_limits(max_pending, DEFAULT_MAX_PENDING_BYTES)
    }

    pub fn with_limits(max_pending: usize, max_pending_bytes: usize) -> Self {
        assert!(max_pending > 0, "cache pipeline capacity must be non-zero");
        assert!(
            max_pending_bytes > 0,
            "cache pipeline byte capacity must be non-zero"
        );
        Self {
            max_pending,
            max_pending_bytes,
            pending_bytes: 0,
            next_request_id: 1,
            pending: VecDeque::new(),
            remote_ready: HashMap::new(),
            direct_scratch: Vec::with_capacity(128),
        }
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn pending_bytes(&self) -> usize {
        self.pending_bytes
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
        self.collect_ready_local()?;
        self.drain_ready(socket_out)?;

        if self.pending.len() >= self.max_pending || self.pending_bytes >= self.max_pending_bytes {
            return Err(CachePipelineError::PipelineFull);
        }

        self.direct_scratch.clear();
        let Some(outcome) =
            dispatcher.dispatch_frame(store, input, now_ms, &mut self.direct_scratch)?
        else {
            return Ok(None);
        };

        let submit = match outcome {
            CacheDispatchOutcome::Executed { consumed }
            | CacheDispatchOutcome::Redirected { consumed, .. } => {
                if self.pending.is_empty() {
                    socket_out.extend_from_slice(&self.direct_scratch);
                } else {
                    self.pending_bytes =
                        self.pending_bytes.saturating_add(self.direct_scratch.len());
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

    /// Collect completed cross-shard replies even when they are not yet at
    /// the front of the ordered pipeline. Their owned response bytes must
    /// participate in the same per-connection high-water accounting as direct
    /// and remote responses.
    fn collect_ready_local(&mut self) -> Result<(), CachePipelineError> {
        for pending in &mut self.pending {
            let ready = match pending {
                PendingResponse::Local(reply) => reply.try_recv()?,
                _ => None,
            };

            if let Some(bytes) = ready {
                self.pending_bytes = self.pending_bytes.saturating_add(bytes.len());
                *pending = PendingResponse::Ready(bytes);
            }
        }
        Ok(())
    }

    /// Record a remote response. It is held until every earlier request has
    /// completed, preserving RESP pipeline order.
    pub fn complete_remote(
        &mut self,
        request_id: u64,
        response: Vec<u8>,
    ) -> Result<(), CachePipelineError> {
        self.collect_ready_local()?;

        let known = self
            .pending
            .iter()
            .any(|pending| matches!(pending, PendingResponse::Remote(id) if *id == request_id));
        if !known {
            return Err(CachePipelineError::UnknownRemoteRequest(request_id));
        }
        let is_front = matches!(
            self.pending.front(),
            Some(PendingResponse::Remote(id)) if *id == request_id
        );

        let previous_len = self
            .remote_ready
            .get(&request_id)
            .map(|previous| previous.len())
            .unwrap_or(0);
        let retained_without_previous = self.pending_bytes.saturating_sub(previous_len);

        // Treat the byte limit as a high-water mark. A single response that
        // crosses the mark is retained so an already-executed request does not
        // lose its reply, but no additional response is accepted until draining
        // brings retained bytes back below the limit.
        if previous_len == 0 && retained_without_previous >= self.max_pending_bytes && !is_front {
            return Err(CachePipelineError::PipelineFull);
        }

        self.pending_bytes = retained_without_previous.saturating_add(response.len());
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
                        Some(bytes) => {
                            self.pending_bytes = self.pending_bytes.saturating_sub(bytes.len());
                            FrontAction::Completed(bytes)
                        }
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
                    self.pending_bytes = self.pending_bytes.saturating_sub(bytes.len());
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
    fn test_direct_responses_flush_immediately() {
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
    fn test_later_direct_response_waits_for_earlier_cross_shard_reply() {
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
    fn test_remote_response_orders_before_later_direct_response() {
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
    fn test_pending_limit_applies_backpressure_before_consuming_next_request() {
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
    fn test_pending_byte_high_water_applies_backpressure() {
        let mut placement = CacheSlotMap::new_local(1, 1).unwrap();
        let key = b"remote-byte-budget";
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
        let mut pipeline = CacheResponsePipeline::with_limits(16, 8);
        let mut store = CacheStore::new();
        let mut socket_out = Vec::new();

        let get = frame(&[b"GET", key]);
        pipeline
            .submit_frame(&dispatcher, &mut store, &get, 0, &mut socket_out)
            .unwrap()
            .unwrap();

        let ping = frame(&[b"PING", b"0123456789abcdef"]);
        pipeline
            .submit_frame(&dispatcher, &mut store, &ping, 0, &mut socket_out)
            .unwrap()
            .unwrap();

        assert!(pipeline.pending_bytes() >= 8);

        let next = frame(&[b"PING"]);
        assert_eq!(
            pipeline.submit_frame(&dispatcher, &mut store, &next, 0, &mut socket_out),
            Err(CachePipelineError::PipelineFull)
        );
    }

    #[test]
    fn test_completed_local_reply_counts_toward_byte_limit_and_front_remote_unblocks() {
        let mut placement = CacheSlotMap::new_local(1, 2).unwrap();
        let local_key = key_for_shard(&placement, 1);
        let remote_key = b"remote-front-key";
        let remote_slot = redis_slot(remote_key);
        placement
            .apply_epoch(
                1,
                &[CacheSlotRange {
                    start: remote_slot,
                    end: remote_slot,
                    owner: CacheShardOwner {
                        node_id: 9,
                        shard: 0,
                    },
                }],
            )
            .unwrap();

        let (channels, mut inboxes) = CacheDispatchChannels::new(2, 8).unwrap();
        let dispatcher = CacheDispatcher::new(1, 0, placement, channels).unwrap();
        let mut pipeline = CacheResponsePipeline::with_limits(16, 8);
        let mut ingress_store = CacheStore::new();
        let mut owner_store = CacheStore::new();
        owner_store.set_bytes(&local_key, b"0123456789abcdef", None, 0);
        let mut socket_out = Vec::new();

        let remote_get = frame(&[b"GET", remote_key]);
        let remote = pipeline
            .submit_frame(
                &dispatcher,
                &mut ingress_store,
                &remote_get,
                0,
                &mut socket_out,
            )
            .unwrap()
            .unwrap()
            .remote
            .expect("remote handoff");

        let local_get = frame(&[b"GET", &local_key]);
        pipeline
            .submit_frame(
                &dispatcher,
                &mut ingress_store,
                &local_get,
                0,
                &mut socket_out,
            )
            .unwrap()
            .unwrap();
        assert!(inboxes[1].try_process_one(&mut owner_store));

        // The completed local reply sits behind the unresolved remote front.
        // A new submission first collects/account its bytes, then applies
        // backpressure without consuming the new request.
        let ping = frame(&[b"PING"]);
        assert_eq!(
            pipeline.submit_frame(&dispatcher, &mut ingress_store, &ping, 0, &mut socket_out,),
            Err(CachePipelineError::PipelineFull)
        );
        assert!(pipeline.pending_bytes() >= 8);

        // Even above the high-water mark, the front response must be accepted:
        // rejecting it would deadlock the ordered pipeline because the later
        // local response cannot drain first.
        pipeline
            .complete_remote(remote.request_id, b"$-1\r\n".to_vec())
            .unwrap();
        assert_eq!(pipeline.drain_ready(&mut socket_out).unwrap(), 2);
        assert_eq!(pipeline.pending_bytes(), 0);
        assert!(pipeline.is_empty());
    }

    #[test]
    fn test_remote_ready_bytes_are_released_when_drained() {
        let mut placement = CacheSlotMap::new_local(1, 1).unwrap();
        let key = b"remote-drain-key";
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
        let mut pipeline = CacheResponsePipeline::with_limits(16, 64);
        let mut store = CacheStore::new();
        let mut socket_out = Vec::new();

        let get = frame(&[b"GET", key]);
        let remote = pipeline
            .submit_frame(&dispatcher, &mut store, &get, 0, &mut socket_out)
            .unwrap()
            .unwrap()
            .remote
            .expect("remote handoff");

        pipeline
            .complete_remote(remote.request_id, b"$5\r\nvalue\r\n".to_vec())
            .unwrap();
        assert!(pipeline.pending_bytes() > 0);

        assert_eq!(pipeline.drain_ready(&mut socket_out).unwrap(), 1);
        assert_eq!(pipeline.pending_bytes(), 0);
        assert!(pipeline.is_empty());
    }

    #[test]
    fn test_unknown_remote_completion_is_rejected() {
        let mut pipeline = CacheResponsePipeline::new(4);
        assert_eq!(
            pipeline.complete_remote(99, b"+OK\r\n".to_vec()),
            Err(CachePipelineError::UnknownRemoteRequest(99))
        );
    }
}
