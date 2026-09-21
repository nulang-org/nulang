use crate::{AcceleratorError, DType, TensorSpec};

/// Deterministic CPU reference tensor used as the semantic oracle for
/// accelerator backends.
///
/// Values are stored as f64 so the reference executor favors clarity and
/// determinism over matching device storage exactly. The logical dtype is
/// still preserved in TensorSpec; lowering backends may use narrower storage.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuTensor {
    pub spec: TensorSpec,
    pub data: Vec<f64>,
}

impl CpuTensor {
    pub fn new(spec: TensorSpec, data: Vec<f64>) -> Result<Self, AcceleratorError> {
        ensure_reference_dtype(spec.dtype)?;
        let expected = spec.element_count()?;
        if expected != data.len() as u64 {
            return Err(AcceleratorError::TensorDataLengthMismatch {
                expected,
                found: data.len(),
            });
        }
        Ok(Self { spec, data })
    }

    pub fn matrix(
        dtype: DType,
        rows: u64,
        cols: u64,
        data: Vec<f64>,
    ) -> Result<Self, AcceleratorError> {
        Self::new(TensorSpec::contiguous(dtype, vec![rows, cols]), data)
    }

    pub fn zeros(dtype: DType, shape: impl Into<Vec<u64>>) -> Result<Self, AcceleratorError> {
        let spec = TensorSpec::contiguous(dtype, shape);
        ensure_reference_dtype(dtype)?;
        let len = usize::try_from(spec.element_count()?)
            .map_err(|_| AcceleratorError::TensorSizeOverflow)?;
        Self::new(spec, vec![0.0; len])
    }

    pub fn matrix_shape(&self) -> Result<(usize, usize), AcceleratorError> {
        if self.spec.shape.len() != 2 {
            return Err(AcceleratorError::ExpectedMatrix {
                shape: self.spec.shape.clone(),
            });
        }
        let rows =
            usize::try_from(self.spec.shape[0]).map_err(|_| AcceleratorError::TensorSizeOverflow)?;
        let cols =
            usize::try_from(self.spec.shape[1]).map_err(|_| AcceleratorError::TensorSizeOverflow)?;
        Ok((rows, cols))
    }

    pub fn add(&self, rhs: &Self) -> Result<Self, AcceleratorError> {
        if self.spec.shape != rhs.spec.shape || self.spec.dtype != rhs.spec.dtype {
            return Err(AcceleratorError::ShapeMismatch {
                left: self.spec.shape.clone(),
                right: rhs.spec.shape.clone(),
            });
        }
        let data = self
            .data
            .iter()
            .zip(&rhs.data)
            .map(|(a, b)| a + b)
            .collect();
        Self::new(self.spec.clone(), data)
    }

    pub fn relu(&self) -> Result<Self, AcceleratorError> {
        let data = self.data.iter().map(|v| v.max(0.0)).collect();
        Self::new(self.spec.clone(), data)
    }

    pub fn matmul(&self, rhs: &Self) -> Result<Self, AcceleratorError> {
        let (m, k_left) = self.matrix_shape()?;
        let (k_right, n) = rhs.matrix_shape()?;
        if k_left != k_right || self.spec.dtype != rhs.spec.dtype {
            return Err(AcceleratorError::MatMulShapeMismatch {
                left: self.spec.shape.clone(),
                right: rhs.spec.shape.clone(),
            });
        }

        let mut out = vec![0.0; m.checked_mul(n).ok_or(AcceleratorError::TensorSizeOverflow)?];
        for row in 0..m {
            for col in 0..n {
                let mut acc = 0.0;
                for k in 0..k_left {
                    acc += self.data[row * k_left + k] * rhs.data[k * n + col];
                }
                out[row * n + col] = acc;
            }
        }

        Self::matrix(self.spec.dtype, m as u64, n as u64, out)
    }
}

fn ensure_reference_dtype(dtype: DType) -> Result<(), AcceleratorError> {
    match dtype {
        DType::Fp8E4M3Fn
        | DType::Fp8E5M2
        | DType::F16
        | DType::Bf16
        | DType::F32
        | DType::F64 => Ok(()),
        other => Err(AcceleratorError::UnsupportedReferenceDType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_matmul_matches_hand_calculation() {
        let a = CpuTensor::matrix(DType::F64, 2, 3, vec![1., 2., 3., 4., 5., 6.]).unwrap();
        let b = CpuTensor::matrix(DType::F64, 3, 2, vec![7., 8., 9., 10., 11., 12.]).unwrap();

        let out = a.matmul(&b).unwrap();
        assert_eq!(out.spec.shape, vec![2, 2]);
        assert_eq!(out.data, vec![58., 64., 139., 154.]);
    }

    #[test]
    fn reference_add_and_relu_preserve_shape() {
        let a = CpuTensor::matrix(DType::F32, 1, 3, vec![-3., 1., 4.]).unwrap();
        let b = CpuTensor::matrix(DType::F32, 1, 3, vec![1., 2., -10.]).unwrap();
        let out = a.add(&b).unwrap().relu().unwrap();

        assert_eq!(out.spec.shape, vec![1, 3]);
        assert_eq!(out.data, vec![0., 3., 0.]);
    }

    #[test]
    fn reference_rejects_shape_mismatch() {
        let a = CpuTensor::zeros(DType::F64, vec![2, 2]).unwrap();
        let b = CpuTensor::zeros(DType::F64, vec![4]).unwrap();
        assert!(matches!(
            a.add(&b),
            Err(AcceleratorError::ShapeMismatch { .. })
        ));
    }
}
