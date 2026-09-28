//! Tablet ownership and bounded admission for NulangDB.
//!
//! The dispatcher keeps database hot paths out of the generic actor mailbox:
//!
//! - work already on the owning shard executes directly;
//! - another local shard receives one typed request through a bounded queue;
//! - another node becomes an explicit remote handoff for a future transport;
//! - ownership epochs are fenced before local queue admission or remote routing.
//!
//! A runtime actor can own and wake a TabletShardInbox later without
//! changing this storage-facing contract.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};

use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{TabletId, TabletWrite};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabletDispatchConfigError {
    InvalidShardCount,
    InvalidQueueCapacity,
    InvalidLocalShard { shard: u16, shard_count: u16 },
    InvalidOwnershipEpoch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabletOwner {
    node_id: u64,
    shard: u16,
    ownership_epoch: u64,
}

impl TabletOwner {
    pub fn new(
        node_id: u64,
        shard: u16,
        ownership_epoch: u64,
    ) -> Result<Self, TabletDispatchConfigError> {
        if ownership_epoch == 0 {
            return Err(TabletDispatchConfigError::InvalidOwnershipEpoch);
        }
        Ok(Self {
            node_id,
            shard,
            ownership_epoch,
        })
    }

    pub fn node_id(self) -> u64 {
        self.node_id
    }

    pub fn shard(self) -> u16 {
        self.shard
    }

    pub fn ownership_epoch(self) -> u64 {
        self.ownership_epoch
    }
}

#[derive(Debug, Clone, Default)]
pub struct TabletPlacementMap {
    owners: BTreeMap<TabletId, TabletOwner>,
}

impl TabletPlacementMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, tablet_id: TabletId, owner: TabletOwner) -> Option<TabletOwner> {
        self.owners.insert(tablet_id, owner)
    }

    pub fn owner_for(&self, tablet_id: TabletId) -> Option<TabletOwner> {
        self.owners.get(&tablet_id).copied()
    }

    pub fn remove(&mut self, tablet_id: TabletId) -> Option<TabletOwner> {
        self.owners.remove(&tablet_id)
    }

    pub fn len(&self) -> usize {
        self.owners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabletDispatchError {
    UnknownTablet(TabletId),
    OwnershipEpochMismatch { placement: u64, presented: u64 },
    MissingLocalTablet(TabletId),
    UnknownLocalShard(u16),
    QueueFull(u16),
    QueueDisconnected(u16),
    Commit(WalBackedError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabletExecutionError {
    MissingTablet(TabletId),
    Commit(WalBackedError),
    ReplyDisconnected,
}

#[derive(Debug, Clone)]
pub struct TabletRemoteRequest {
    owner: TabletOwner,
    write: TabletWrite,
}

impl TabletRemoteRequest {
    pub fn owner(&self) -> TabletOwner {
        self.owner
    }

    pub fn write(&self) -> &TabletWrite {
        &self.write
    }

    pub fn into_write(self) -> TabletWrite {
        self.write
    }
}

pub struct TabletLocalReply {
    receiver: Receiver<Result<u64, TabletExecutionError>>,
}

impl fmt::Debug for TabletLocalReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TabletLocalReply").finish_non_exhaustive()
    }
}

impl TabletLocalReply {
    pub fn try_recv(&self) -> Result<Option<u64>, TabletExecutionError> {
        match self.receiver.try_recv() {
            Ok(result) => result.map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(TabletExecutionError::ReplyDisconnected),
        }
    }

    pub fn recv(self) -> Result<u64, TabletExecutionError> {
        self.receiver
            .recv()
            .unwrap_or(Err(TabletExecutionError::ReplyDisconnected))
    }
}

#[derive(Debug)]
pub enum TabletDispatchOutcome {
    Executed { sequence: u64 },
    LocalQueued { shard: u16, reply: TabletLocalReply },
    Remote { request: TabletRemoteRequest },
}

struct TabletShardRequest {
    write: TabletWrite,
    reply: SyncSender<Result<u64, TabletExecutionError>>,
}

#[derive(Clone)]
pub struct TabletDispatchChannels {
    senders: Vec<SyncSender<TabletShardRequest>>,
}

impl TabletDispatchChannels {
    pub fn new(
        shard_count: u16,
        queue_capacity: usize,
    ) -> Result<(Self, Vec<TabletShardInbox>), TabletDispatchConfigError> {
        if shard_count == 0 {
            return Err(TabletDispatchConfigError::InvalidShardCount);
        }
        if queue_capacity == 0 {
            return Err(TabletDispatchConfigError::InvalidQueueCapacity);
        }

        let mut senders = Vec::with_capacity(shard_count as usize);
        let mut inboxes = Vec::with_capacity(shard_count as usize);
        for shard in 0..shard_count {
            let (sender, receiver) = mpsc::sync_channel(queue_capacity);
            senders.push(sender);
            inboxes.push(TabletShardInbox { shard, receiver });
        }

        Ok((Self { senders }, inboxes))
    }

    pub fn shard_count(&self) -> u16 {
        self.senders.len() as u16
    }

    fn try_send(&self, shard: u16, request: TabletShardRequest) -> Result<(), TabletDispatchError> {
        let Some(sender) = self.senders.get(shard as usize) else {
            return Err(TabletDispatchError::UnknownLocalShard(shard));
        };

        match sender.try_send(request) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(TabletDispatchError::QueueFull(shard)),
            Err(TrySendError::Disconnected(_)) => {
                Err(TabletDispatchError::QueueDisconnected(shard))
            }
        }
    }
}

pub struct TabletShardInbox {
    shard: u16,
    receiver: Receiver<TabletShardRequest>,
}

impl TabletShardInbox {
    pub fn shard(&self) -> u16 {
        self.shard
    }

    pub fn try_process_one(&mut self, tablets: &mut BTreeMap<TabletId, WalBackedTablet>) -> bool {
        let request = match self.receiver.try_recv() {
            Ok(request) => request,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return false,
        };

        let tablet_id = request.write.tablet_id();
        let result = match tablets.get_mut(&tablet_id) {
            Some(tablet) => tablet
                .commit(request.write)
                .map_err(TabletExecutionError::Commit),
            None => Err(TabletExecutionError::MissingTablet(tablet_id)),
        };
        let _ = request.reply.send(result);
        true
    }

    pub fn drain(
        &mut self,
        tablets: &mut BTreeMap<TabletId, WalBackedTablet>,
        max_writes: usize,
    ) -> usize {
        let mut processed = 0;
        while processed < max_writes && self.try_process_one(tablets) {
            processed += 1;
        }
        processed
    }
}

pub struct TabletDispatcher {
    local_node_id: u64,
    local_shard: u16,
    placement: TabletPlacementMap,
    channels: TabletDispatchChannels,
}

impl TabletDispatcher {
    pub fn new(
        local_node_id: u64,
        local_shard: u16,
        placement: TabletPlacementMap,
        channels: TabletDispatchChannels,
    ) -> Result<Self, TabletDispatchConfigError> {
        if local_shard >= channels.shard_count() {
            return Err(TabletDispatchConfigError::InvalidLocalShard {
                shard: local_shard,
                shard_count: channels.shard_count(),
            });
        }

        Ok(Self {
            local_node_id,
            local_shard,
            placement,
            channels,
        })
    }

    pub fn local_node_id(&self) -> u64 {
        self.local_node_id
    }

    pub fn local_shard(&self) -> u16 {
        self.local_shard
    }

    pub fn placement(&self) -> &TabletPlacementMap {
        &self.placement
    }

    pub fn install_placement(&mut self, placement: TabletPlacementMap) {
        self.placement = placement;
    }

    pub fn dispatch_write(
        &self,
        tablets: &mut BTreeMap<TabletId, WalBackedTablet>,
        write: TabletWrite,
    ) -> Result<TabletDispatchOutcome, TabletDispatchError> {
        let tablet_id = write.tablet_id();
        let owner = self
            .placement
            .owner_for(tablet_id)
            .ok_or(TabletDispatchError::UnknownTablet(tablet_id))?;

        if write.ownership_epoch() != owner.ownership_epoch {
            return Err(TabletDispatchError::OwnershipEpochMismatch {
                placement: owner.ownership_epoch,
                presented: write.ownership_epoch(),
            });
        }

        if owner.node_id != self.local_node_id {
            return Ok(TabletDispatchOutcome::Remote {
                request: TabletRemoteRequest { owner, write },
            });
        }

        if owner.shard == self.local_shard {
            let tablet = tablets
                .get_mut(&tablet_id)
                .ok_or(TabletDispatchError::MissingLocalTablet(tablet_id))?;
            let sequence = tablet.commit(write).map_err(TabletDispatchError::Commit)?;
            return Ok(TabletDispatchOutcome::Executed { sequence });
        }

        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.channels.try_send(
            owner.shard,
            TabletShardRequest {
                write,
                reply: reply_tx,
            },
        )?;
        Ok(TabletDispatchOutcome::LocalQueued {
            shard: owner.shard,
            reply: TabletLocalReply { receiver: reply_rx },
        })
    }
}
