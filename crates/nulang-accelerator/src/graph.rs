use crate::{AcceleratorError, CpuTensor, TensorSpec};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TensorId(pub u32);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum GraphOp {
    Input { spec: TensorSpec },
    Add { lhs: TensorId, rhs: TensorId },
    MatMul { lhs: TensorId, rhs: TensorId },
    Relu { input: TensorId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct GraphNode {
    id: TensorId,
    spec: TensorSpec,
    op: GraphOp,
}

/// Small accelerator-neutral graph IR.
///
/// The graph stores already-inferred output specs, making validation explicit
/// before a backend sees the graph. Future IREE/MLIR lowering can consume the
/// same graph that the CPU reference executor uses for differential tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TensorGraph {
    nodes: Vec<GraphNode>,
    outputs: Vec<TensorId>,
    next_id: u32,
}

impl TensorGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn input(&mut self, spec: TensorSpec) -> TensorId {
        let id = self.alloc_id();
        self.nodes.push(GraphNode {
            id,
            spec: spec.clone(),
            op: GraphOp::Input { spec },
        });
        id
    }

    pub fn add(&mut self, lhs: TensorId, rhs: TensorId) -> Result<TensorId, AcceleratorError> {
        let left = self.spec(lhs)?.clone();
        let right = self.spec(rhs)?.clone();
        if left != right {
            return Err(AcceleratorError::ShapeMismatch {
                left: left.shape,
                right: right.shape,
            });
        }
        Ok(self.push_node(left, GraphOp::Add { lhs, rhs }))
    }

    pub fn matmul(
        &mut self,
        lhs: TensorId,
        rhs: TensorId,
    ) -> Result<TensorId, AcceleratorError> {
        let left = self.spec(lhs)?.clone();
        let right = self.spec(rhs)?.clone();
        if left.shape.len() != 2 || right.shape.len() != 2 {
            return Err(AcceleratorError::ExpectedMatrix {
                shape: if left.shape.len() != 2 {
                    left.shape
                } else {
                    right.shape
                },
            });
        }
        if left.shape[1] != right.shape[0] || left.dtype != right.dtype {
            return Err(AcceleratorError::MatMulShapeMismatch {
                left: left.shape,
                right: right.shape,
            });
        }
        let spec = TensorSpec {
            dtype: left.dtype,
            shape: vec![left.shape[0], right.shape[1]],
            layout: left.layout,
        };
        Ok(self.push_node(spec, GraphOp::MatMul { lhs, rhs }))
    }

    pub fn relu(&mut self, input: TensorId) -> Result<TensorId, AcceleratorError> {
        let spec = self.spec(input)?.clone();
        Ok(self.push_node(spec, GraphOp::Relu { input }))
    }

    pub fn set_outputs(&mut self, outputs: impl Into<Vec<TensorId>>) -> Result<(), AcceleratorError> {
        let outputs = outputs.into();
        for id in &outputs {
            self.spec(*id)?;
        }
        self.outputs = outputs;
        Ok(())
    }

    pub fn outputs(&self) -> &[TensorId] {
        &self.outputs
    }

    pub fn spec(&self, id: TensorId) -> Result<&TensorSpec, AcceleratorError> {
        self.nodes
            .iter()
            .find(|node| node.id == id)
            .map(|node| &node.spec)
            .ok_or(AcceleratorError::UnknownTensor(id.0))
    }

    pub fn execute_cpu(
        &self,
        inputs: &BTreeMap<TensorId, CpuTensor>,
    ) -> Result<BTreeMap<TensorId, CpuTensor>, AcceleratorError> {
        let mut values = BTreeMap::new();

        for node in &self.nodes {
            let value = match &node.op {
                GraphOp::Input { spec } => {
                    let value = inputs
                        .get(&node.id)
                        .ok_or(AcceleratorError::UnknownTensor(node.id.0))?;
                    if &value.spec != spec {
                        return Err(AcceleratorError::GraphInputMismatch { id: node.id.0 });
                    }
                    value.clone()
                }
                GraphOp::Add { lhs, rhs } => {
                    let left = values
                        .get(lhs)
                        .ok_or(AcceleratorError::UnknownTensor(lhs.0))?;
                    let right = values
                        .get(rhs)
                        .ok_or(AcceleratorError::UnknownTensor(rhs.0))?;
                    left.add(right)?
                }
                GraphOp::MatMul { lhs, rhs } => {
                    let left = values
                        .get(lhs)
                        .ok_or(AcceleratorError::UnknownTensor(lhs.0))?;
                    let right = values
                        .get(rhs)
                        .ok_or(AcceleratorError::UnknownTensor(rhs.0))?;
                    left.matmul(right)?
                }
                GraphOp::Relu { input } => values
                    .get(input)
                    .ok_or(AcceleratorError::UnknownTensor(input.0))?
                    .relu()?,
            };
            debug_assert_eq!(value.spec, node.spec);
            values.insert(node.id, value);
        }

        if self.outputs.is_empty() {
            return Ok(values);
        }

        let mut selected = BTreeMap::new();
        for id in &self.outputs {
            selected.insert(
                *id,
                values
                    .get(id)
                    .ok_or(AcceleratorError::UnknownTensor(id.0))?
                    .clone(),
            );
        }
        Ok(selected)
    }

    fn alloc_id(&mut self) -> TensorId {
        let id = TensorId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn push_node(&mut self, spec: TensorSpec, op: GraphOp) -> TensorId {
        let id = self.alloc_id();
        self.nodes.push(GraphNode { id, spec, op });
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DType;

    #[test]
    fn graph_executes_matmul_relu_with_reference_backend() {
        let mut graph = TensorGraph::new();
        let a = graph.input(TensorSpec::contiguous(DType::F64, vec![2, 2]));
        let b = graph.input(TensorSpec::contiguous(DType::F64, vec![2, 2]));
        let product = graph.matmul(a, b).unwrap();
        let output = graph.relu(product).unwrap();
        graph.set_outputs(vec![output]).unwrap();

        let inputs = BTreeMap::from([
            (
                a,
                CpuTensor::matrix(DType::F64, 2, 2, vec![1., -2., 3., 4.]).unwrap(),
            ),
            (
                b,
                CpuTensor::matrix(DType::F64, 2, 2, vec![5., 6., 7., 8.]).unwrap(),
            ),
        ]);

        let result = graph.execute_cpu(&inputs).unwrap();
        assert_eq!(result[&output].data, vec![0., 0., 43., 50.]);
    }

    #[test]
    fn graph_rejects_invalid_matmul_during_construction() {
        let mut graph = TensorGraph::new();
        let a = graph.input(TensorSpec::contiguous(DType::F64, vec![2, 3]));
        let b = graph.input(TensorSpec::contiguous(DType::F64, vec![4, 2]));
        assert!(matches!(
            graph.matmul(a, b),
            Err(AcceleratorError::MatMulShapeMismatch { .. })
        ));
    }
}
