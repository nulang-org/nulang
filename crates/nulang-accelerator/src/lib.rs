//! Provider-neutral accelerator execution primitives for Nulang.
//!
//! This crate is intentionally below the Nulang language surface. It models
//! tensors, accelerator capabilities, device requirements, deterministic
//! placement, and backend discovery without committing the frozen language or
//! artifact formats to CUDA, ROCm, Metal, IREE, or any other implementation.
//!
//! Accelerator work remains an effect executed by an actor. Device choice is
//! placement policy, not a new runtime primitive.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod graph;
pub mod reference;
pub mod session;

pub use graph::{GraphOp, TensorGraph, TensorId};
pub use reference::CpuTensor;
pub use session::{AcceleratorSession, BufferId, BufferSpec, MemoryClass};
use std::fmt;
use thiserror::Error;

/// Extensible identifier for an accelerator backend.
///
/// Known helper constructors cover the common backends, while new runtimes can
/// introduce identifiers without requiring an enum/version bump.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BackendId(String);

impl BackendId {
    pub fn new(value: impl Into<String>) -> Result<Self, AcceleratorError> {
        let value = value.into();
        let normalized = value.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err(AcceleratorError::InvalidIdentifier {
                kind: "backend",
                value,
            });
        }
        Ok(Self(normalized))
    }

    pub fn cpu() -> Self {
        Self("cpu".into())
    }

    pub fn cuda() -> Self {
        Self("cuda".into())
    }

    pub fn rocm() -> Self {
        Self("rocm".into())
    }

    pub fn metal() -> Self {
        Self("metal".into())
    }

    pub fn vulkan() -> Self {
        Self("vulkan".into())
    }

    pub fn webgpu() -> Self {
        Self("webgpu".into())
    }

    pub fn iree() -> Self {
        Self("iree".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BackendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for BackendId {
    type Error = AcceleratorError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<BackendId> for String {
    fn from(value: BackendId) -> Self {
        value.0
    }
}

/// Stable identifier for one locally visible device.
///
/// Backends should namespace identifiers when necessary (for example
/// "cuda:0" or "metal:registry-id") so identifiers stay unique inside one
/// runtime registry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(value: impl Into<String>) -> Result<Self, AcceleratorError> {
        let value = value.into();
        let normalized = value.trim().to_string();
        if normalized.is_empty() {
            return Err(AcceleratorError::InvalidIdentifier {
                kind: "device",
                value,
            });
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for DeviceId {
    type Error = AcceleratorError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<DeviceId> for String {
    fn from(value: DeviceId) -> Self {
        value.0
    }
}

/// Open-ended accelerator feature identifier.
///
/// Features are strings rather than a closed enum because hardware features
/// evolve faster than Nulang's compatibility surface.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FeatureId(String);

impl FeatureId {
    pub fn new(value: impl Into<String>) -> Result<Self, AcceleratorError> {
        let value = value.into();
        let normalized = value.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err(AcceleratorError::InvalidIdentifier {
                kind: "feature",
                value,
            });
        }
        Ok(Self(normalized))
    }

    pub fn tensor_cores() -> Self {
        Self("tensor_cores".into())
    }

    pub fn async_copy() -> Self {
        Self("async_copy".into())
    }

    pub fn peer_to_peer() -> Self {
        Self("peer_to_peer".into())
    }

    pub fn unified_memory() -> Self {
        Self("unified_memory".into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FeatureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for FeatureId {
    type Error = AcceleratorError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<FeatureId> for String {
    fn from(value: FeatureId) -> Self {
        value.0
    }
}

/// Broad device class used for hard placement constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    Cpu,
    Gpu,
    Npu,
    Other,
}

/// Scalar element type understood by the accelerator-neutral tensor layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DType {
    Bool,
    U8,
    I8,
    I32,
    I64,
    Fp8E4M3Fn,
    Fp8E5M2,
    F16,
    Bf16,
    F32,
    F64,
}

impl DType {
    pub const fn size_bytes(self) -> u64 {
        match self {
            Self::Bool | Self::U8 | Self::I8 | Self::Fp8E4M3Fn | Self::Fp8E5M2 => 1,
            Self::F16 | Self::Bf16 => 2,
            Self::I32 | Self::F32 => 4,
            Self::I64 | Self::F64 => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TensorLayout {
    RowMajor,
    ColumnMajor,
}

/// Logical tensor metadata independent of storage allocation or backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TensorSpec {
    pub dtype: DType,
    pub shape: Vec<u64>,
    pub layout: TensorLayout,
}

impl TensorSpec {
    pub fn contiguous(dtype: DType, shape: impl Into<Vec<u64>>) -> Self {
        Self {
            dtype,
            shape: shape.into(),
            layout: TensorLayout::RowMajor,
        }
    }

    /// Number of logical elements. An empty shape is a scalar and therefore
    /// contains one element. Zero-sized dimensions are valid and yield zero.
    pub fn element_count(&self) -> Result<u64, AcceleratorError> {
        self.shape.iter().try_fold(1u64, |count, dimension| {
            count
                .checked_mul(*dimension)
                .ok_or(AcceleratorError::TensorSizeOverflow)
        })
    }

    /// Minimum bytes required by a contiguous representation.
    pub fn byte_len(&self) -> Result<u64, AcceleratorError> {
        self.element_count()?
            .checked_mul(self.dtype.size_bytes())
            .ok_or(AcceleratorError::TensorSizeOverflow)
    }
}

/// Normalized capabilities for one device.
///
/// Backend adapters are responsible for translating native APIs into this
/// structure. Unknown capabilities should be omitted rather than guessed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    pub id: DeviceId,
    pub backend: BackendId,
    pub class: DeviceClass,
    pub name: String,
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
    pub max_allocation_bytes: u64,
    pub supported_dtypes: BTreeSet<DType>,
    pub features: BTreeSet<FeatureId>,
}

impl DeviceCapabilities {
    pub fn validate(&self) -> Result<(), AcceleratorError> {
        if self.available_memory_bytes > self.total_memory_bytes {
            return Err(AcceleratorError::InvalidMemory {
                device: self.id.clone(),
                field: "available_memory_bytes",
                value: self.available_memory_bytes,
                total: self.total_memory_bytes,
            });
        }
        if self.max_allocation_bytes > self.total_memory_bytes {
            return Err(AcceleratorError::InvalidMemory {
                device: self.id.clone(),
                field: "max_allocation_bytes",
                value: self.max_allocation_bytes,
                total: self.total_memory_bytes,
            });
        }
        Ok(())
    }
}

/// Hard constraints for selecting a device.
///
/// Empty backend/class sets mean "any". There is no implicit CPU fallback:
/// callers decide which execution classes are semantically acceptable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceRequirements {
    pub allowed_backends: BTreeSet<BackendId>,
    pub allowed_classes: BTreeSet<DeviceClass>,
    pub min_total_memory_bytes: u64,
    pub min_available_memory_bytes: u64,
    pub min_max_allocation_bytes: u64,
    pub required_dtypes: BTreeSet<DType>,
    pub required_features: BTreeSet<FeatureId>,
}

impl DeviceRequirements {
    pub fn matches(&self, device: &DeviceCapabilities) -> bool {
        (self.allowed_backends.is_empty() || self.allowed_backends.contains(&device.backend))
            && (self.allowed_classes.is_empty() || self.allowed_classes.contains(&device.class))
            && device.total_memory_bytes >= self.min_total_memory_bytes
            && device.available_memory_bytes >= self.min_available_memory_bytes
            && device.max_allocation_bytes >= self.min_max_allocation_bytes
            && self.required_dtypes.is_subset(&device.supported_dtypes)
            && self.required_features.is_subset(&device.features)
    }
}

/// Full placement request: hard requirements plus ordered backend preference.
///
/// Backends not present in preferred_backends remain eligible; they simply
/// rank after explicitly preferred backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceRequest {
    pub requirements: DeviceRequirements,
    pub preferred_backends: Vec<BackendId>,
}

/// Provider-neutral discovery boundary.
///
/// CUDA, ROCm, Metal, IREE, Vulkan, CPU SIMD, and future NPU adapters should
/// implement this trait instead of leaking vendor handles into Nulang core.
pub trait AcceleratorBackend: Send + Sync {
    fn backend_id(&self) -> &BackendId;
    fn discover(&self) -> Result<Vec<DeviceCapabilities>, AcceleratorError>;
}

/// Registry of currently discoverable accelerator devices.
///
/// Refreshing one backend is atomic: invalid or inconsistent discovery data
/// leaves the previously known registry untouched.
#[derive(Debug, Clone, Default)]
pub struct DeviceRegistry {
    devices: BTreeMap<DeviceId, DeviceCapabilities>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub fn get(&self, id: &DeviceId) -> Option<&DeviceCapabilities> {
        self.devices.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &DeviceCapabilities> {
        self.devices.values()
    }

    pub fn upsert(
        &mut self,
        device: DeviceCapabilities,
    ) -> Result<Option<DeviceCapabilities>, AcceleratorError> {
        device.validate()?;
        if let Some(existing) = self.devices.get(&device.id) {
            if existing.backend != device.backend {
                return Err(AcceleratorError::DuplicateDevice(device.id));
            }
        }
        Ok(self.devices.insert(device.id.clone(), device))
    }

    /// Replace the snapshot for one backend after validating the complete new
    /// snapshot. Other backends are not touched.
    pub fn refresh_backend(
        &mut self,
        backend: &dyn AcceleratorBackend,
    ) -> Result<usize, AcceleratorError> {
        let backend_id = backend.backend_id().clone();
        let discovered = backend.discover()?;
        let mut ids = BTreeSet::new();

        for device in &discovered {
            device.validate()?;
            if device.backend != backend_id {
                return Err(AcceleratorError::BackendMismatch {
                    expected: backend_id.clone(),
                    actual: device.backend.clone(),
                    device: device.id.clone(),
                });
            }
            if !ids.insert(device.id.clone()) {
                return Err(AcceleratorError::DuplicateDevice(device.id.clone()));
            }
            if let Some(existing) = self.devices.get(&device.id) {
                if existing.backend != backend_id {
                    return Err(AcceleratorError::DuplicateDevice(device.id.clone()));
                }
            }
        }

        self.devices
            .retain(|_, device| device.backend != backend_id);
        let count = discovered.len();
        for device in discovered {
            self.devices.insert(device.id.clone(), device);
        }
        Ok(count)
    }

    /// Deterministically select the best currently eligible device.
    ///
    /// Ordering is: preferred backend rank, available memory descending,
    /// total memory descending, then stable device id ascending.
    pub fn select(&self, request: &DeviceRequest) -> Option<&DeviceCapabilities> {
        let mut candidates = self
            .devices
            .values()
            .filter(|device| request.requirements.matches(device))
            .collect::<Vec<_>>();

        candidates.sort_by(|left, right| {
            backend_rank(request, &left.backend)
                .cmp(&backend_rank(request, &right.backend))
                .then_with(|| {
                    right
                        .available_memory_bytes
                        .cmp(&left.available_memory_bytes)
                })
                .then_with(|| right.total_memory_bytes.cmp(&left.total_memory_bytes))
                .then_with(|| left.id.cmp(&right.id))
        });
        candidates.into_iter().next()
    }
}

fn backend_rank(request: &DeviceRequest, backend: &BackendId) -> usize {
    if request.preferred_backends.is_empty() {
        return 0;
    }
    request
        .preferred_backends
        .iter()
        .position(|candidate| candidate == backend)
        .unwrap_or(request.preferred_backends.len())
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AcceleratorError {
    #[error("invalid {kind} identifier: {value:?}")]
    InvalidIdentifier {
        kind: &'static str,
        value: String,
    },
    #[error(
        "device {device} reports invalid {field}={value}; total_memory_bytes={total}"
    )]
    InvalidMemory {
        device: DeviceId,
        field: &'static str,
        value: u64,
        total: u64,
    },
    #[error("tensor byte size overflow")]
    TensorSizeOverflow,
    #[error("tensor data length mismatch: expected {expected} elements, found {found}")]
    TensorDataLengthMismatch { expected: u64, found: usize },
    #[error("CPU reference executor does not support dtype {0:?}")]
    UnsupportedReferenceDType(DType),
    #[error("expected a rank-2 matrix, found shape {shape:?}")]
    ExpectedMatrix { shape: Vec<u64> },
    #[error("tensor shape mismatch: left={left:?}, right={right:?}")]
    ShapeMismatch { left: Vec<u64>, right: Vec<u64> },
    #[error("matrix multiplication shape mismatch: left={left:?}, right={right:?}")]
    MatMulShapeMismatch { left: Vec<u64>, right: Vec<u64> },
    #[error("unknown tensor id {0}")]
    UnknownTensor(u32),
    #[error("graph input {id} does not match declared tensor spec")]
    GraphInputMismatch { id: u32 },
    #[error("buffer allocation of {requested} bytes exceeds device/session capacity")]
    BufferCapacityExceeded { requested: u64 },
    #[error("duplicate device id {0}")]
    DuplicateDevice(DeviceId),
    #[error(
        "backend {expected} discovered device {device} belonging to backend {actual}"
    )]
    BackendMismatch {
        expected: BackendId,
        actual: BackendId,
        device: DeviceId,
    },
    #[error("accelerator backend {backend} failed: {message}")]
    BackendFailure {
        backend: BackendId,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn device(
        id: &str,
        backend: BackendId,
        class: DeviceClass,
        total_gib: u64,
        available_gib: u64,
    ) -> DeviceCapabilities {
        DeviceCapabilities {
            id: DeviceId::new(id).unwrap(),
            backend,
            class,
            name: id.into(),
            total_memory_bytes: total_gib * GIB,
            available_memory_bytes: available_gib * GIB,
            max_allocation_bytes: total_gib * GIB,
            supported_dtypes: BTreeSet::from([DType::F16, DType::Bf16, DType::F32]),
            features: BTreeSet::new(),
        }
    }

    #[test]
    fn tensor_size_is_overflow_safe() {
        let tensor = TensorSpec::contiguous(DType::F16, vec![2, 4, 8]);
        assert_eq!(tensor.element_count().unwrap(), 64);
        assert_eq!(tensor.byte_len().unwrap(), 128);

        let scalar = TensorSpec::contiguous(DType::F32, vec![]);
        assert_eq!(scalar.element_count().unwrap(), 1);
        assert_eq!(scalar.byte_len().unwrap(), 4);

        let overflow = TensorSpec::contiguous(DType::F64, vec![u64::MAX, 2]);
        assert_eq!(
            overflow.byte_len(),
            Err(AcceleratorError::TensorSizeOverflow)
        );
    }

    #[test]
    fn requirements_are_hard_constraints() {
        let mut gpu = device("cuda:0", BackendId::cuda(), DeviceClass::Gpu, 80, 72);
        gpu.features.insert(FeatureId::tensor_cores());

        let requirements = DeviceRequirements {
            allowed_classes: BTreeSet::from([DeviceClass::Gpu]),
            min_available_memory_bytes: 60 * GIB,
            required_dtypes: BTreeSet::from([DType::Bf16]),
            required_features: BTreeSet::from([FeatureId::tensor_cores()]),
            ..DeviceRequirements::default()
        };
        assert!(requirements.matches(&gpu));

        let too_large = DeviceRequirements {
            min_available_memory_bytes: 79 * GIB,
            ..requirements
        };
        assert!(!too_large.matches(&gpu));
    }

    #[test]
    fn selection_honors_backend_preference_before_free_memory() {
        let mut registry = DeviceRegistry::new();
        registry
            .upsert(device(
                "cuda:0",
                BackendId::cuda(),
                DeviceClass::Gpu,
                80,
                40,
            ))
            .unwrap();
        registry
            .upsert(device(
                "rocm:0",
                BackendId::rocm(),
                DeviceClass::Gpu,
                192,
                160,
            ))
            .unwrap();

        let request = DeviceRequest {
            requirements: DeviceRequirements {
                allowed_classes: BTreeSet::from([DeviceClass::Gpu]),
                ..DeviceRequirements::default()
            },
            preferred_backends: vec![BackendId::cuda(), BackendId::rocm()],
        };

        assert_eq!(registry.select(&request).unwrap().id.as_str(), "cuda:0");

        let memory_first = DeviceRequest {
            preferred_backends: vec![],
            ..request
        };
        assert_eq!(
            registry.select(&memory_first).unwrap().id.as_str(),
            "rocm:0"
        );
    }

    struct MockBackend {
        id: BackendId,
        devices: Vec<DeviceCapabilities>,
    }

    impl AcceleratorBackend for MockBackend {
        fn backend_id(&self) -> &BackendId {
            &self.id
        }

        fn discover(&self) -> Result<Vec<DeviceCapabilities>, AcceleratorError> {
            Ok(self.devices.clone())
        }
    }

    #[test]
    fn backend_refresh_replaces_only_its_own_snapshot() {
        let mut registry = DeviceRegistry::new();
        registry
            .upsert(device(
                "cuda:old",
                BackendId::cuda(),
                DeviceClass::Gpu,
                24,
                20,
            ))
            .unwrap();
        registry
            .upsert(device(
                "cpu:0",
                BackendId::cpu(),
                DeviceClass::Cpu,
                64,
                32,
            ))
            .unwrap();

        let backend = MockBackend {
            id: BackendId::cuda(),
            devices: vec![device(
                "cuda:new",
                BackendId::cuda(),
                DeviceClass::Gpu,
                80,
                70,
            )],
        };

        assert_eq!(registry.refresh_backend(&backend).unwrap(), 1);
        assert!(registry
            .get(&DeviceId::new("cuda:old").unwrap())
            .is_none());
        assert!(registry
            .get(&DeviceId::new("cuda:new").unwrap())
            .is_some());
        assert!(registry
            .get(&DeviceId::new("cpu:0").unwrap())
            .is_some());
    }

    #[test]
    fn invalid_refresh_is_atomic() {
        let mut registry = DeviceRegistry::new();
        registry
            .upsert(device(
                "cuda:old",
                BackendId::cuda(),
                DeviceClass::Gpu,
                24,
                20,
            ))
            .unwrap();

        let backend = MockBackend {
            id: BackendId::cuda(),
            devices: vec![device(
                "rocm:wrong",
                BackendId::rocm(),
                DeviceClass::Gpu,
                64,
                60,
            )],
        };

        assert!(matches!(
            registry.refresh_backend(&backend),
            Err(AcceleratorError::BackendMismatch { .. })
        ));
        assert!(registry
            .get(&DeviceId::new("cuda:old").unwrap())
            .is_some());
    }

    #[test]
    fn identifiers_and_requests_round_trip_through_serde() {
        assert!(serde_json::from_str::<BackendId>("\"\"").is_err());
        assert_eq!(
            serde_json::from_str::<BackendId>("\" CUDA \"").unwrap(),
            BackendId::cuda()
        );

        let request = DeviceRequest {
            requirements: DeviceRequirements {
                allowed_backends: BTreeSet::from([BackendId::cuda()]),
                required_dtypes: BTreeSet::from([DType::Bf16]),
                ..DeviceRequirements::default()
            },
            preferred_backends: vec![BackendId::cuda()],
        };
        let json = serde_json::to_string(&request).unwrap();
        let decoded: DeviceRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, request);
    }
}
