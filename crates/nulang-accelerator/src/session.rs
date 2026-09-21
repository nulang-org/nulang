use crate::{AcceleratorError, DeviceCapabilities};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BufferId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryClass {
    DeviceLocal,
    HostVisible,
    Unified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BufferSpec {
    pub size_bytes: u64,
    pub alignment_bytes: u64,
    pub memory_class: MemoryClass,
}

/// Backend-neutral ownership ledger for accelerator allocations.
///
/// Concrete backends attach native buffers/queues to BufferId in their own
/// adapter state. This core ledger deliberately contains no CUDA/ROCm/Metal
/// handles, so it is safe to discard and reconstruct across actor restart.
#[derive(Debug, Clone)]
pub struct AcceleratorSession {
    device: DeviceCapabilities,
    next_id: u64,
    allocated_bytes: u64,
    buffers: BTreeMap<BufferId, BufferSpec>,
}

impl AcceleratorSession {
    pub fn new(device: DeviceCapabilities) -> Result<Self, AcceleratorError> {
        device.validate()?;
        Ok(Self {
            device,
            next_id: 1,
            allocated_bytes: 0,
            buffers: BTreeMap::new(),
        })
    }

    pub fn device(&self) -> &DeviceCapabilities {
        &self.device
    }

    pub fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes
    }

    pub fn allocate(&mut self, spec: BufferSpec) -> Result<BufferId, AcceleratorError> {
        if spec.size_bytes > self.device.max_allocation_bytes {
            return Err(AcceleratorError::BufferCapacityExceeded {
                requested: spec.size_bytes,
            });
        }
        let next_total = self
            .allocated_bytes
            .checked_add(spec.size_bytes)
            .ok_or(AcceleratorError::BufferCapacityExceeded {
                requested: spec.size_bytes,
            })?;
        if next_total > self.device.available_memory_bytes {
            return Err(AcceleratorError::BufferCapacityExceeded {
                requested: spec.size_bytes,
            });
        }

        let id = BufferId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.allocated_bytes = next_total;
        self.buffers.insert(id, spec);
        Ok(id)
    }

    pub fn get(&self, id: BufferId) -> Option<&BufferSpec> {
        self.buffers.get(&id)
    }

    pub fn release(&mut self, id: BufferId) -> Option<BufferSpec> {
        let spec = self.buffers.remove(&id)?;
        self.allocated_bytes = self.allocated_bytes.saturating_sub(spec.size_bytes);
        Some(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendId, DType, DeviceClass, DeviceId};
    use std::collections::BTreeSet;

    #[test]
    fn session_enforces_allocation_and_total_memory_limits() {
        let device = DeviceCapabilities {
            id: DeviceId::new("cuda:0").unwrap(),
            backend: BackendId::cuda(),
            class: DeviceClass::Gpu,
            name: "test".into(),
            total_memory_bytes: 1024,
            available_memory_bytes: 800,
            max_allocation_bytes: 600,
            supported_dtypes: BTreeSet::from([DType::F16]),
            features: BTreeSet::new(),
        };
        let mut session = AcceleratorSession::new(device).unwrap();

        let first = session
            .allocate(BufferSpec {
                size_bytes: 500,
                alignment_bytes: 256,
                memory_class: MemoryClass::DeviceLocal,
            })
            .unwrap();
        assert_eq!(session.allocated_bytes(), 500);

        assert!(session
            .allocate(BufferSpec {
                size_bytes: 400,
                alignment_bytes: 256,
                memory_class: MemoryClass::DeviceLocal,
            })
            .is_err());

        session.release(first).unwrap();
        assert_eq!(session.allocated_bytes(), 0);
    }
}
