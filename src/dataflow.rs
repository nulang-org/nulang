//! Experimental in-memory dataflow planning and reference execution.
//!
//! This module intentionally has no effect, actor, network, or persistent
//! execution semantics. Rust-hosted operators must be pure by contract;
//! source-language effect/capability checking is a future integration step.

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
