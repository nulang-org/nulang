//! Backend-neutral SIMD semantic types.
//!
//! These types describe vector lane semantics shared by native JIT analysis
//! and portable WASM lowering. They intentionally contain no Cranelift or
//! Wasmtime dependencies so backend feature profiles do not depend on each
//! other's implementation modules.

/// The scalar element type packed into SIMD vectors.
///
/// This determines both the lane width and the backend vector type:
/// - `Int64` -> 2 lanes on a 128-bit vector
/// - `Float64` -> 2 lanes on a 128-bit vector
/// - `Int32` -> 4 lanes on a 128-bit vector
/// - `Float32` -> 4 lanes on a 128-bit vector
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SimdElemType {
    Int64,
    Float64,
    Int32,
    Float32,
}

impl SimdElemType {
    /// Return true if this is a floating-point type.
    pub fn is_float(&self) -> bool {
        matches!(self, SimdElemType::Float64 | SimdElemType::Float32)
    }

    /// Return true if this is an integer type.
    pub fn is_int(&self) -> bool {
        !self.is_float()
    }

    /// Return the SIMD lane width for this element type on a 128-bit vector.
    pub fn lane_count(&self) -> usize {
        match self {
            SimdElemType::Int64 | SimdElemType::Float64 => 2,
            SimdElemType::Int32 | SimdElemType::Float32 => 4,
        }
    }

    /// Return the element size in bytes.
    pub fn elem_size(&self) -> usize {
        match self {
            SimdElemType::Int64 | SimdElemType::Float64 => 8,
            SimdElemType::Int32 | SimdElemType::Float32 => 4,
        }
    }
}
