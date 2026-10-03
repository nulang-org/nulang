//! Backend-neutral compute IR for machine-local and accelerator-oriented work.
//!
//! This module intentionally contains no actor, durability, or vendor-specific
//! concepts. It models the execution properties that CPU SIMD, GPU, and future
//! accelerator backends need to agree on: vector shape, memory locality,
//! layouts, structured parallel loops, tiling, asynchronous copies, barriers,
//! reductions, and broadcasts.
//!
//! The initial IR is deliberately small. Frontends and analyses can target this
//! vocabulary without committing Nulang source syntax to CUDA, PTX, a fixed
//! warp size, or any particular GPU vendor.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::num::NonZeroU16;

/// Scalar element types that may participate in compute kernels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarType {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F16,
    Bf16,
    F32,
    F64,
}

impl ScalarType {
    pub const fn bit_width(self) -> u16 {
        match self {
            Self::Bool | Self::I8 | Self::U8 => 8,
            Self::I16 | Self::U16 | Self::F16 | Self::Bf16 => 16,
            Self::I32 | Self::U32 | Self::F32 => 32,
            Self::I64 | Self::U64 | Self::F64 => 64,
        }
    }

    pub const fn byte_width(self) -> u16 {
        self.bit_width() / 8
    }
}

/// Whether a vector has a fixed hardware-independent lane count or a scalable
/// minimum width (for targets such as SVE/RVV).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VectorWidthKind {
    Fixed,
    Scalable,
}

/// A valid SIMD/vector width. Zero lanes are unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VectorWidth {
    kind: VectorWidthKind,
    min_lanes: NonZeroU16,
}

impl VectorWidth {
    pub fn fixed(lanes: u16) -> Result<Self, ComputeIrError> {
        Self::new(VectorWidthKind::Fixed, lanes)
    }

    pub fn scalable(min_lanes: u16) -> Result<Self, ComputeIrError> {
        Self::new(VectorWidthKind::Scalable, min_lanes)
    }

    fn new(kind: VectorWidthKind, lanes: u16) -> Result<Self, ComputeIrError> {
        let min_lanes = NonZeroU16::new(lanes).ok_or(ComputeIrError::ZeroVectorWidth)?;
        Ok(Self { kind, min_lanes })
    }

    pub const fn kind(self) -> VectorWidthKind {
        self.kind
    }

    pub const fn min_lanes(self) -> u16 {
        self.min_lanes.get()
    }

    pub const fn is_scalable(self) -> bool {
        matches!(self.kind, VectorWidthKind::Scalable)
    }
}

/// A first-class SIMD value type independent of a concrete backend register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VectorType {
    pub element: ScalarType,
    pub width: VectorWidth,
}

impl VectorType {
    pub const fn new(element: ScalarType, width: VectorWidth) -> Self {
        Self { element, width }
    }
}

/// Compute values are either scalar or vector values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueType {
    Scalar(ScalarType),
    Vector(VectorType),
}

/// Locality hierarchy used for synchronization and memory visibility.
///
/// The hierarchy is intentionally broader than a GPU hierarchy so the same IR
/// can eventually express work from a SIMD lane through a distributed system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LocalityScope {
    Lane,
    Group,
    Device,
    Node,
    Cluster,
    System,
}

impl LocalityScope {
    const fn rank(self) -> u8 {
        match self {
            Self::Lane => 0,
            Self::Group => 1,
            Self::Device => 2,
            Self::Node => 3,
            Self::Cluster => 4,
            Self::System => 5,
        }
    }

    /// Returns true when `self` contains `inner` in the locality hierarchy.
    pub const fn encloses(self, inner: Self) -> bool {
        self.rank() >= inner.rank()
    }
}

/// Abstract storage classes. Backends map these to registers, stack/local
/// memory, GPU shared/global memory, host memory, or distributed storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemorySpace {
    LaneLocal,
    GroupShared,
    DeviceGlobal,
    NodeHost,
    ClusterShared,
    System,
}

impl MemorySpace {
    pub const fn visibility_scope(self) -> LocalityScope {
        match self {
            Self::LaneLocal => LocalityScope::Lane,
            Self::GroupShared => LocalityScope::Group,
            Self::DeviceGlobal => LocalityScope::Device,
            Self::NodeHost => LocalityScope::Node,
            Self::ClusterShared => LocalityScope::Cluster,
            Self::System => LocalityScope::System,
        }
    }

    pub const fn is_visible_to(self, scope: LocalityScope) -> bool {
        self.visibility_scope().encloses(scope)
    }
}

/// Physical buffer layout. Strides are expressed in elements, not bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    element: ScalarType,
    shape: Vec<u64>,
    strides: Vec<u64>,
    alignment: u32,
}

impl Layout {
    /// Construct a contiguous row-major layout.
    pub fn row_major(
        element: ScalarType,
        shape: impl Into<Vec<u64>>,
    ) -> Result<Self, ComputeIrError> {
        let shape = shape.into();
        let mut strides = vec![0; shape.len()];
        let mut stride = 1_u64;

        for axis in (0..shape.len()).rev() {
            strides[axis] = stride;
            stride = stride
                .checked_mul(shape[axis])
                .ok_or(ComputeIrError::LayoutSizeOverflow)?;
        }

        Self::from_parts(element, shape, strides, element.byte_width() as u32)
    }

    /// Construct a layout with explicit element strides and byte alignment.
    pub fn from_parts(
        element: ScalarType,
        shape: impl Into<Vec<u64>>,
        strides: impl Into<Vec<u64>>,
        alignment: u32,
    ) -> Result<Self, ComputeIrError> {
        let shape = shape.into();
        let strides = strides.into();

        if shape.len() != strides.len() {
            return Err(ComputeIrError::LayoutRankMismatch {
                shape_rank: shape.len(),
                stride_rank: strides.len(),
            });
        }
        if alignment == 0 || !alignment.is_power_of_two() {
            return Err(ComputeIrError::InvalidAlignment(alignment));
        }

        let layout = Self {
            element,
            shape,
            strides,
            alignment,
        };
        layout.byte_len()?;
        Ok(layout)
    }

    pub const fn element(&self) -> ScalarType {
        self.element
    }

    pub fn shape(&self) -> &[u64] {
        &self.shape
    }

    pub fn strides(&self) -> &[u64] {
        &self.strides
    }

    pub const fn alignment(&self) -> u32 {
        self.alignment
    }

    pub fn rank(&self) -> usize {
        self.shape.len()
    }

    pub fn element_count(&self) -> Result<u64, ComputeIrError> {
        self.shape.iter().try_fold(1_u64, |count, extent| {
            count
                .checked_mul(*extent)
                .ok_or(ComputeIrError::LayoutSizeOverflow)
        })
    }

    pub fn byte_len(&self) -> Result<u64, ComputeIrError> {
        self.element_count()?
            .checked_mul(self.element.byte_width() as u64)
            .ok_or(ComputeIrError::LayoutSizeOverflow)
    }

    pub fn is_contiguous_row_major(&self) -> bool {
        let mut expected = 1_u64;
        for axis in (0..self.shape.len()).rev() {
            if self.strides[axis] != expected {
                return false;
            }
            let Some(next) = expected.checked_mul(self.shape[axis]) else {
                return false;
            };
            expected = next;
        }
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LoopId(pub u32);

impl fmt::Display for BufferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "buffer#{}", self.0)
    }
}

impl fmt::Display for LoopId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "loop#{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferDecl {
    pub id: BufferId,
    pub layout: Layout,
    pub memory: MemorySpace,
}

impl BufferDecl {
    pub const fn new(id: BufferId, layout: Layout, memory: MemorySpace) -> Self {
        Self { id, layout, memory }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReductionOp {
    Add,
    Mul,
    Min,
    Max,
    And,
    Or,
    Xor,
}

/// Backend-independent compute and scheduling operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeOp {
    ParallelFor {
        loop_id: LoopId,
        start: i64,
        end: i64,
        step: i64,
        scope: LocalityScope,
    },
    Tile {
        loop_id: LoopId,
        tile_size: u64,
    },
    Vectorize {
        loop_id: LoopId,
        width: VectorWidth,
    },
    AsyncCopy {
        src: BufferId,
        dst: BufferId,
        bytes: u64,
        scope: LocalityScope,
    },
    Barrier {
        scope: LocalityScope,
    },
    Reduce {
        input: BufferId,
        output: BufferId,
        op: ReductionOp,
        scope: LocalityScope,
    },
    Broadcast {
        input: BufferId,
        output: BufferId,
        scope: LocalityScope,
    },
}

/// A validated unit of portable compute work.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComputeRegion {
    pub buffers: Vec<BufferDecl>,
    pub ops: Vec<ComputeOp>,
}

impl ComputeRegion {
    pub const fn new() -> Self {
        Self {
            buffers: Vec::new(),
            ops: Vec::new(),
        }
    }

    pub fn add_buffer(&mut self, buffer: BufferDecl) {
        self.buffers.push(buffer);
    }

    pub fn push(&mut self, op: ComputeOp) {
        self.ops.push(op);
    }

    /// Validate invariants that every backend may rely on.
    pub fn validate(&self) -> Result<(), ComputeIrError> {
        let mut buffers = HashMap::with_capacity(self.buffers.len());
        for buffer in &self.buffers {
            if buffers.insert(buffer.id, buffer).is_some() {
                return Err(ComputeIrError::DuplicateBuffer(buffer.id));
            }
            buffer.layout.byte_len()?;
        }

        let mut loops = HashMap::new();
        for op in &self.ops {
            if let ComputeOp::ParallelFor {
                loop_id,
                start,
                end,
                step,
                scope,
            } = *op
            {
                if loops.insert(loop_id, scope).is_some() {
                    return Err(ComputeIrError::DuplicateLoop(loop_id));
                }
                if step == 0 {
                    return Err(ComputeIrError::ZeroLoopStep(loop_id));
                }
                if (step > 0 && end < start) || (step < 0 && end > start) {
                    return Err(ComputeIrError::InvalidLoopDirection {
                        loop_id,
                        start,
                        end,
                        step,
                    });
                }
            }
        }

        let mut vectorized_loops = HashSet::new();
        for op in &self.ops {
            match *op {
                ComputeOp::ParallelFor { .. } => {}
                ComputeOp::Tile {
                    loop_id,
                    tile_size,
                } => {
                    require_loop(&loops, loop_id)?;
                    if tile_size == 0 {
                        return Err(ComputeIrError::ZeroTileSize(loop_id));
                    }
                }
                ComputeOp::Vectorize { loop_id, .. } => {
                    let scope = require_loop(&loops, loop_id)?;
                    if scope > LocalityScope::Device {
                        return Err(ComputeIrError::NonLocalVectorization { loop_id, scope });
                    }
                    if !vectorized_loops.insert(loop_id) {
                        return Err(ComputeIrError::DuplicateVectorization(loop_id));
                    }
                }
                ComputeOp::AsyncCopy {
                    src,
                    dst,
                    bytes,
                    scope,
                } => {
                    if src == dst {
                        return Err(ComputeIrError::SelfCopy(src));
                    }
                    if bytes == 0 {
                        return Err(ComputeIrError::ZeroCopySize);
                    }
                    let src_decl = require_buffer(&buffers, src, scope)?;
                    let dst_decl = require_buffer(&buffers, dst, scope)?;
                    validate_copy_bound(src_decl, bytes)?;
                    validate_copy_bound(dst_decl, bytes)?;
                }
                ComputeOp::Barrier { scope } => {
                    if scope == LocalityScope::Lane {
                        return Err(ComputeIrError::LaneBarrier);
                    }
                }
                ComputeOp::Reduce {
                    input,
                    output,
                    scope,
                    ..
                }
                | ComputeOp::Broadcast {
                    input,
                    output,
                    scope,
                } => {
                    require_buffer(&buffers, input, scope)?;
                    require_buffer(&buffers, output, scope)?;
                }
            }
        }

        Ok(())
    }
}

fn require_loop(
    loops: &HashMap<LoopId, LocalityScope>,
    loop_id: LoopId,
) -> Result<LocalityScope, ComputeIrError> {
    loops
        .get(&loop_id)
        .copied()
        .ok_or(ComputeIrError::UnknownLoop(loop_id))
}

fn require_buffer<'a>(
    buffers: &'a HashMap<BufferId, &'a BufferDecl>,
    buffer_id: BufferId,
    scope: LocalityScope,
) -> Result<&'a BufferDecl, ComputeIrError> {
    let buffer = buffers
        .get(&buffer_id)
        .copied()
        .ok_or(ComputeIrError::UnknownBuffer(buffer_id))?;

    if !buffer.memory.is_visible_to(scope) {
        return Err(ComputeIrError::BufferNotVisible {
            buffer: buffer_id,
            memory: buffer.memory,
            scope,
        });
    }
    Ok(buffer)
}

fn validate_copy_bound(buffer: &BufferDecl, bytes: u64) -> Result<(), ComputeIrError> {
    let available = buffer.layout.byte_len()?;
    if bytes > available {
        return Err(ComputeIrError::CopyOutOfBounds {
            buffer: buffer.id,
            requested: bytes,
            available,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputeIrError {
    ZeroVectorWidth,
    InvalidAlignment(u32),
    LayoutRankMismatch {
        shape_rank: usize,
        stride_rank: usize,
    },
    LayoutSizeOverflow,
    DuplicateBuffer(BufferId),
    DuplicateLoop(LoopId),
    UnknownBuffer(BufferId),
    UnknownLoop(LoopId),
    ZeroLoopStep(LoopId),
    InvalidLoopDirection {
        loop_id: LoopId,
        start: i64,
        end: i64,
        step: i64,
    },
    ZeroTileSize(LoopId),
    DuplicateVectorization(LoopId),
    NonLocalVectorization {
        loop_id: LoopId,
        scope: LocalityScope,
    },
    BufferNotVisible {
        buffer: BufferId,
        memory: MemorySpace,
        scope: LocalityScope,
    },
    SelfCopy(BufferId),
    ZeroCopySize,
    CopyOutOfBounds {
        buffer: BufferId,
        requested: u64,
        available: u64,
    },
    LaneBarrier,
}

impl fmt::Display for ComputeIrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroVectorWidth => write!(f, "vector width must contain at least one lane"),
            Self::InvalidAlignment(alignment) => {
                write!(f, "alignment must be a non-zero power of two, got {alignment}")
            }
            Self::LayoutRankMismatch {
                shape_rank,
                stride_rank,
            } => write!(
                f,
                "layout shape rank {shape_rank} does not match stride rank {stride_rank}"
            ),
            Self::LayoutSizeOverflow => write!(f, "layout size overflows u64"),
            Self::DuplicateBuffer(buffer) => write!(f, "duplicate {buffer}"),
            Self::DuplicateLoop(loop_id) => write!(f, "duplicate {loop_id}"),
            Self::UnknownBuffer(buffer) => write!(f, "unknown {buffer}"),
            Self::UnknownLoop(loop_id) => write!(f, "unknown {loop_id}"),
            Self::ZeroLoopStep(loop_id) => write!(f, "{loop_id} has a zero step"),
            Self::InvalidLoopDirection {
                loop_id,
                start,
                end,
                step,
            } => write!(
                f,
                "{loop_id} has inconsistent bounds start={start}, end={end}, step={step}"
            ),
            Self::ZeroTileSize(loop_id) => write!(f, "{loop_id} has a zero tile size"),
            Self::DuplicateVectorization(loop_id) => {
                write!(f, "{loop_id} is vectorized more than once")
            }
            Self::NonLocalVectorization { loop_id, scope } => write!(
                f,
                "{loop_id} cannot be vectorized at non-local scope {scope:?}"
            ),
            Self::BufferNotVisible {
                buffer,
                memory,
                scope,
            } => write!(
                f,
                "{buffer} in {memory:?} memory is not visible to {scope:?} scope"
            ),
            Self::SelfCopy(buffer) => write!(f, "async copy source and destination are both {buffer}"),
            Self::ZeroCopySize => write!(f, "async copy must move at least one byte"),
            Self::CopyOutOfBounds {
                buffer,
                requested,
                available,
            } => write!(
                f,
                "async copy requests {requested} bytes from {buffer}, which has {available} bytes"
            ),
            Self::LaneBarrier => write!(f, "a lane-scoped barrier cannot synchronize another worker"),
        }
    }
}

impl std::error::Error for ComputeIrError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_major_layout_computes_element_strides() {
        let layout = Layout::row_major(ScalarType::F32, vec![2, 3, 4]).unwrap();

        assert_eq!(layout.strides(), &[12, 4, 1]);
        assert_eq!(layout.element_count().unwrap(), 24);
        assert_eq!(layout.byte_len().unwrap(), 96);
        assert!(layout.is_contiguous_row_major());
    }

    #[test]
    fn explicit_layout_rejects_rank_mismatch() {
        let err = Layout::from_parts(ScalarType::I64, vec![4, 4], vec![4], 8).unwrap_err();

        assert_eq!(
            err,
            ComputeIrError::LayoutRankMismatch {
                shape_rank: 2,
                stride_rank: 1,
            }
        );
    }

    #[test]
    fn vector_widths_are_nonzero_and_support_scalable_targets() {
        assert_eq!(VectorWidth::fixed(0), Err(ComputeIrError::ZeroVectorWidth));

        let fixed = VectorWidth::fixed(8).unwrap();
        assert_eq!(fixed.kind(), VectorWidthKind::Fixed);
        assert_eq!(fixed.min_lanes(), 8);
        assert!(!fixed.is_scalable());

        let scalable = VectorWidth::scalable(4).unwrap();
        assert_eq!(scalable.kind(), VectorWidthKind::Scalable);
        assert_eq!(scalable.min_lanes(), 4);
        assert!(scalable.is_scalable());
    }

    #[test]
    fn locality_scopes_form_a_visibility_hierarchy() {
        assert!(LocalityScope::Device.encloses(LocalityScope::Group));
        assert!(MemorySpace::DeviceGlobal.is_visible_to(LocalityScope::Group));
        assert!(!MemorySpace::GroupShared.is_visible_to(LocalityScope::Device));
        assert!(!MemorySpace::LaneLocal.is_visible_to(LocalityScope::Group));
    }

    #[test]
    fn validates_portable_tiled_vectorized_copy_pipeline() {
        let mut region = ComputeRegion::new();
        region.add_buffer(BufferDecl::new(
            BufferId(0),
            Layout::row_major(ScalarType::F32, vec![64]).unwrap(),
            MemorySpace::DeviceGlobal,
        ));
        region.add_buffer(BufferDecl::new(
            BufferId(1),
            Layout::row_major(ScalarType::F32, vec![32]).unwrap(),
            MemorySpace::GroupShared,
        ));
        region.push(ComputeOp::ParallelFor {
            loop_id: LoopId(0),
            start: 0,
            end: 64,
            step: 1,
            scope: LocalityScope::Group,
        });
        region.push(ComputeOp::Tile {
            loop_id: LoopId(0),
            tile_size: 32,
        });
        region.push(ComputeOp::Vectorize {
            loop_id: LoopId(0),
            width: VectorWidth::fixed(4).unwrap(),
        });
        region.push(ComputeOp::AsyncCopy {
            src: BufferId(0),
            dst: BufferId(1),
            bytes: 128,
            scope: LocalityScope::Group,
        });
        region.push(ComputeOp::Barrier {
            scope: LocalityScope::Group,
        });

        assert_eq!(region.validate(), Ok(()));
    }

    #[test]
    fn rejects_group_access_to_lane_local_memory() {
        let mut region = ComputeRegion::new();
        region.add_buffer(BufferDecl::new(
            BufferId(0),
            Layout::row_major(ScalarType::I32, vec![4]).unwrap(),
            MemorySpace::LaneLocal,
        ));
        region.add_buffer(BufferDecl::new(
            BufferId(1),
            Layout::row_major(ScalarType::I32, vec![4]).unwrap(),
            MemorySpace::GroupShared,
        ));
        region.push(ComputeOp::Reduce {
            input: BufferId(0),
            output: BufferId(1),
            op: ReductionOp::Add,
            scope: LocalityScope::Group,
        });

        assert_eq!(
            region.validate(),
            Err(ComputeIrError::BufferNotVisible {
                buffer: BufferId(0),
                memory: MemorySpace::LaneLocal,
                scope: LocalityScope::Group,
            })
        );
    }

    #[test]
    fn rejects_vectorization_of_distributed_loop_scope() {
        let mut region = ComputeRegion::new();
        region.push(ComputeOp::ParallelFor {
            loop_id: LoopId(7),
            start: 0,
            end: 10,
            step: 1,
            scope: LocalityScope::Node,
        });
        region.push(ComputeOp::Vectorize {
            loop_id: LoopId(7),
            width: VectorWidth::fixed(4).unwrap(),
        });

        assert_eq!(
            region.validate(),
            Err(ComputeIrError::NonLocalVectorization {
                loop_id: LoopId(7),
                scope: LocalityScope::Node,
            })
        );
    }

    #[test]
    fn rejects_async_copy_past_buffer_bounds() {
        let mut region = ComputeRegion::new();
        region.add_buffer(BufferDecl::new(
            BufferId(0),
            Layout::row_major(ScalarType::U8, vec![16]).unwrap(),
            MemorySpace::DeviceGlobal,
        ));
        region.add_buffer(BufferDecl::new(
            BufferId(1),
            Layout::row_major(ScalarType::U8, vec![8]).unwrap(),
            MemorySpace::GroupShared,
        ));
        region.push(ComputeOp::AsyncCopy {
            src: BufferId(0),
            dst: BufferId(1),
            bytes: 16,
            scope: LocalityScope::Group,
        });

        assert_eq!(
            region.validate(),
            Err(ComputeIrError::CopyOutOfBounds {
                buffer: BufferId(1),
                requested: 16,
                available: 8,
            })
        );
    }

    #[test]
    fn descending_parallel_loop_is_valid_when_step_is_negative() {
        let mut region = ComputeRegion::new();
        region.push(ComputeOp::ParallelFor {
            loop_id: LoopId(0),
            start: 10,
            end: 0,
            step: -1,
            scope: LocalityScope::Device,
        });

        assert_eq!(region.validate(), Ok(()));
    }
}
