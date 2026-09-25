//! Diagnostic multicore shard-scaling benchmark.
//!
//! Implementation follows the tests in this file. The harness deliberately
//! keeps cross-shard transport out of the timed workload: work is preloaded
//! onto actors owned by each shard, then one scheduler thread per shard drains
//! the same fixed total message count.

#[cfg(test)]
mod tests {
    #[test]
    fn parses_ordered_unique_positive_shard_counts() {
        assert_eq!(super::parse_shards("1,2,4,8").unwrap(), vec![1, 2, 4, 8]);
        assert!(super::parse_shards("1,0,2").is_err());
        assert!(super::parse_shards("1,2,2").is_err());
    }

    #[test]
    fn two_shards_process_the_exact_fixed_workload() {
        let measurement = super::run_independent(2, 2_048);
        assert_eq!(measurement.shards, 2);
        assert_eq!(measurement.messages, 2_048);
        assert!(measurement.elapsed.as_nanos() > 0);
    }
}
