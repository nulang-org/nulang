//! Experimental, Rust-hosted, finite dataflow reference model.
//!
//! This is not a new Nulang source-language construct or a distributed stream
//! processor. Operators are homogeneous in T and must be pure by caller
//! contract. The Nulang compiler does not yet establish that purity.
//!
//! Nodes are appended in dependency order, so a graph cannot contain a cycle.
//! Evaluating a target materializes only its transitive dependency frontier;
//! execution is sequential and deterministic for deterministic callbacks.
//! Logical item limits prevent uncontrolled queue growth and oversized
//! intermediate results; they are not a byte-level allocator budget.

use std::collections::VecDeque;

/// Opaque index into a graph. Unknown or forward references are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

/// Conservative limits for the experimental finite graph executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    max_nodes: usize,
    max_items_per_node: usize,
    max_materialized_items: usize,
}

impl Limits {
    pub fn new(
        max_nodes: usize,
        max_items_per_node: usize,
        max_materialized_items: usize,
    ) -> Result<Self, GraphError> {
        for (name, value) in [
            ("max_nodes", max_nodes),
            ("max_items_per_node", max_items_per_node),
            ("max_materialized_items", max_materialized_items),
        ] {
            if value == 0 {
                return Err(GraphError::ZeroLimit(name));
            }
        }
        Ok(Self {
            max_nodes,
            max_items_per_node,
            max_materialized_items,
        })
    }
}

/// Admission and evaluation failures are explicit, never silently truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphError {
    ZeroLimit(&'static str),
    InvalidNode(NodeId),
    TooManyNodes { limit: usize },
    TooManyItems { limit: usize },
    MaterializationBudgetExceeded { limit: usize },
}

type Mapper<T> = Box<dyn Fn(&T) -> T + Send + Sync>;
type Predicate<T> = Box<dyn Fn(&T) -> bool + Send + Sync>;
type Folder<T> = Box<dyn Fn(T, &T) -> T + Send + Sync>;

enum Node<T> {
    Source(Vec<T>),
    Map {
        input: NodeId,
        apply: Mapper<T>,
    },
    Filter {
        input: NodeId,
        retain: Predicate<T>,
    },
    Fold {
        input: NodeId,
        initial: T,
        fold: Folder<T>,
    },
    Concat {
        left: NodeId,
        right: NodeId,
    },
}

impl<T> Node<T> {
    fn dependencies(&self) -> [Option<NodeId>; 2] {
        match self {
            Self::Source(_) => [None, None],
            Self::Map { input, .. } | Self::Filter { input, .. } | Self::Fold { input, .. } => {
                [Some(*input), None]
            }
            Self::Concat { left, right } => [Some(*left), Some(*right)],
        }
    }
}

/// A finite, homogeneous, typed DAG with a sequential reference executor.
///
/// `fold` emits a one-element collection containing the accumulator, even
/// for empty input. `concat` preserves left-before-right order. No operator
/// may be used to infer effect or ownership safety of Nulang source code.
pub struct DataflowGraph<T> {
    limits: Limits,
    nodes: Vec<Node<T>>,
}

impl<T> DataflowGraph<T> {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            nodes: Vec::new(),
        }
    }

    fn add(&mut self, node: Node<T>) -> Result<NodeId, GraphError> {
        if self.nodes.len() >= self.limits.max_nodes {
            return Err(GraphError::TooManyNodes {
                limit: self.limits.max_nodes,
            });
        }
        let id = NodeId(self.nodes.len());
        self.nodes.push(node);
        Ok(id)
    }

    fn validate(&self, node: NodeId) -> Result<(), GraphError> {
        if node.0 >= self.nodes.len() {
            return Err(GraphError::InvalidNode(node));
        }
        Ok(())
    }

    pub fn source(&mut self, values: Vec<T>) -> Result<NodeId, GraphError> {
        if values.len() > self.limits.max_items_per_node {
            return Err(GraphError::TooManyItems {
                limit: self.limits.max_items_per_node,
            });
        }
        self.add(Node::Source(values))
    }

    pub fn map(
        &mut self,
        input: NodeId,
        apply: impl Fn(&T) -> T + Send + Sync + 'static,
    ) -> Result<NodeId, GraphError> {
        self.validate(input)?;
        self.add(Node::Map {
            input,
            apply: Box::new(apply),
        })
    }

    pub fn filter(
        &mut self,
        input: NodeId,
        retain: impl Fn(&T) -> bool + Send + Sync + 'static,
    ) -> Result<NodeId, GraphError> {
        self.validate(input)?;
        self.add(Node::Filter {
            input,
            retain: Box::new(retain),
        })
    }

    pub fn fold(
        &mut self,
        input: NodeId,
        initial: T,
        fold: impl Fn(T, &T) -> T + Send + Sync + 'static,
    ) -> Result<NodeId, GraphError> {
        self.validate(input)?;
        self.add(Node::Fold {
            input,
            initial,
            fold: Box::new(fold),
        })
    }

    pub fn concat(&mut self, left: NodeId, right: NodeId) -> Result<NodeId, GraphError> {
        self.validate(left)?;
        self.validate(right)?;
        self.add(Node::Concat { left, right })
    }

    fn check_output_budget(
        &self,
        length: usize,
        already_materialized: usize,
    ) -> Result<(), GraphError> {
        if length > self.limits.max_items_per_node {
            return Err(GraphError::TooManyItems {
                limit: self.limits.max_items_per_node,
            });
        }
        if length > self.limits.max_materialized_items.saturating_sub(already_materialized) {
            return Err(GraphError::MaterializationBudgetExceeded {
                limit: self.limits.max_materialized_items,
            });
        }
        Ok(())
    }
}

impl<T: Clone> DataflowGraph<T> {
    /// Execute a target and only the transitive nodes required by that target.
    ///
    /// Each call recomputes its target; no memoization across calls or effect
    /// replay is implied. For `filter`, item budgets are checked before each
    /// retained element is cloned. A callback panic propagates as a panic.
    pub fn execute(&self, target: NodeId) -> Result<Vec<T>, GraphError> {
        self.validate(target)?;

        // Discover reachable predecessors iteratively to avoid stack overflows.
        let mut needed = vec![false; self.nodes.len()];
        let mut stack = vec![target];
        while let Some(id) = stack.pop() {
            if needed[id.0] {
                continue;
            }
            needed[id.0] = true;
            for predecessor in self.nodes[id.0].dependencies().into_iter().flatten() {
                stack.push(predecessor);
            }
        }

        // All dependencies have lower IDs; insertion order is a valid topo sort.
        let mut results: Vec<Option<Vec<T>>> = (0..self.nodes.len()).map(|_| None).collect();
        let mut materialized = 0usize;

        for (index, node) in self.nodes.iter().enumerate() {
            if !needed[index] {
                continue;
            }

            let output = match node {
                Node::Source(values) => {
                    self.check_output_budget(values.len(), materialized)?;
                    values.clone()
                }
                Node::Map { input, apply } => {
                    let values = results[input.0].as_ref().expect("validated predecessor");
                    self.check_output_budget(values.len(), materialized)?;
                    values.iter().map(apply.as_ref()).collect()
                }
                Node::Filter { input, retain } => {
                    let values = results[input.0].as_ref().expect("validated predecessor");
                    let mut filtered = Vec::new();
                    for value in values {
                        if retain(value) {
                            self.check_output_budget(filtered.len() + 1, materialized)?;
                            filtered.push(value.clone());
                        }
                    }
                    filtered
                }
                Node::Fold {
                    input,
                    initial,
                    fold,
                } => {
                    let values = results[input.0].as_ref().expect("validated predecessor");
                    self.check_output_budget(1, materialized)?;
                    let value = values.iter().fold(initial.clone(), fold.as_ref());
                    vec![value]
                }
                Node::Concat { left, right } => {
                    let a = results[left.0].as_ref().expect("validated left predecessor");
                    let b = results[right.0].as_ref().expect("validated right predecessor");
                    let length = a
                        .len()
                        .checked_add(b.len())
                        .ok_or(GraphError::TooManyItems {
                            limit: self.limits.max_items_per_node,
                        })?;
                    self.check_output_budget(length, materialized)?;
                    a.iter().chain(b.iter()).cloned().collect()
                }
            };
            materialized += output.len();
            results[index] = Some(output);
        }

        Ok(results[target.0].take().expect("target is reachable"))
    }
}

/// Bounded FIFO admission primitive for future dataflow edges.
///
/// Rejected values are returned to the producer for retry or cancellation.
/// This queue is local and synchronous; it does not acknowledge processing,
/// deliver messages between actors, or provide durable/exactly-once semantics.
pub struct BoundedQueue<T> {
    capacity: usize,
    pending: VecDeque<T>,
}

impl<T> BoundedQueue<T> {
    pub fn new(capacity: usize) -> Result<Self, GraphError> {
        if capacity == 0 {
            return Err(GraphError::ZeroLimit("queue_capacity"));
        }
        Ok(Self {
            capacity,
            pending: VecDeque::new(),
        })
    }

    pub fn try_push(&mut self, item: T) -> Result<(), T> {
        if self.pending.len() >= self.capacity {
            return Err(item);
        }
        self.pending.push_back(item);
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        self.pending.pop_front()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundedQueue, DataflowGraph, GraphError, Limits, NodeId};

    #[test]
    fn branching_graph_evaluates_in_dependency_order_and_is_repeatable() {
        let mut graph = DataflowGraph::new(Limits::new(8, 12, 40).unwrap());
        let source = graph.source(vec![1, 2, 3, 4]).unwrap();
        let even = graph.filter(source, |value| value % 2 == 0).unwrap();
        let doubled = graph.map(even, |value| value * 2).unwrap();
        let combined = graph.concat(source, doubled).unwrap();

        assert_eq!(graph.execute(combined).unwrap(), vec![1, 2, 3, 4, 4, 8]);
        assert_eq!(graph.execute(combined).unwrap(), vec![1, 2, 3, 4, 4, 8]);
    }

    #[test]
    fn fold_emits_one_typed_value_even_for_empty_input() {
        let mut graph = DataflowGraph::new(Limits::new(4, 4, 8).unwrap());
        let source = graph.source(Vec::<i32>::new()).unwrap();
        let sum = graph.fold(source, 7, |acc, value| acc + value).unwrap();
        assert_eq!(graph.execute(sum).unwrap(), vec![7]);
    }

    #[test]
    fn rejects_forward_references_and_unknown_targets() {
        let mut graph = DataflowGraph::<i32>::new(Limits::new(4, 8, 20).unwrap());
        assert_eq!(
            graph.map(NodeId(0), |value| *value),
            Err(GraphError::InvalidNode(NodeId(0)))
        );
        let first = graph.source(vec![1]).unwrap();
        assert_eq!(first, NodeId(0));
        assert_eq!(
            graph.execute(NodeId(42)),
            Err(GraphError::InvalidNode(NodeId(42)))
        );
    }

    #[test]
    fn rejects_unbounded_configuration_and_full_graph() {
        assert_eq!(Limits::new(0, 4, 4), Err(GraphError::ZeroLimit("max_nodes")));
        assert_eq!(
            Limits::new(1, 0, 4),
            Err(GraphError::ZeroLimit("max_items_per_node"))
        );
        assert_eq!(
            Limits::new(1, 4, 0),
            Err(GraphError::ZeroLimit("max_materialized_items"))
        );
        let mut graph = DataflowGraph::new(Limits::new(1, 2, 3).unwrap());
        assert_eq!(
            graph.source(vec![1, 2, 3]),
            Err(GraphError::TooManyItems { limit: 2 })
        );
        graph.source(vec![1]).unwrap();
        assert_eq!(
            graph.source(vec![2]),
            Err(GraphError::TooManyNodes { limit: 1 })
        );
    }

    #[test]
    fn concat_fails_closed_when_output_exceeds_item_limit() {
        let mut graph = DataflowGraph::new(Limits::new(4, 3, 8).unwrap());
        let left = graph.source(vec![1, 2]).unwrap();
        let right = graph.source(vec![3, 4]).unwrap();
        let both = graph.concat(left, right).unwrap();
        assert_eq!(
            graph.execute(both),
            Err(GraphError::TooManyItems { limit: 3 })
        );
    }

    #[test]
    fn materialization_budget_rejects_overcommit() {
        let mut graph = DataflowGraph::new(Limits::new(4, 4, 5).unwrap());
        let source = graph.source(vec![1, 2, 3]).unwrap();
        let mapped = graph.map(source, |value| value + 1).unwrap();
        assert_eq!(
            graph.execute(mapped),
            Err(GraphError::MaterializationBudgetExceeded { limit: 5 })
        );
    }

    #[test]
    fn only_target_dependencies_are_executed() {
        let mut graph = DataflowGraph::new(Limits::new(4, 4, 4).unwrap());
        graph.source(vec![9, 9, 9, 9]).unwrap();
        let wanted = graph.source(vec![1, 2]).unwrap();
        assert_eq!(graph.execute(wanted).unwrap(), vec![1, 2]);
    }

    #[test]
    fn bounded_queue_reports_backpressure_without_losing_input() {
        let mut queue = BoundedQueue::new(1).unwrap();
        assert_eq!(queue.try_push(10), Ok(()));
        assert_eq!(queue.try_push(20), Err(20));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop(), Some(10));
        assert_eq!(queue.try_push(20), Ok(()));
        assert_eq!(queue.pop(), Some(20));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn zero_queue_capacity_is_rejected() {
        assert!(matches!(
            BoundedQueue::<u32>::new(0),
            Err(GraphError::ZeroLimit("queue_capacity"))
        ));
    }
}
