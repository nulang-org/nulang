//! NuDB WAL gate latency benchmark. Run manually, never as a CI timing gate.

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn percentile_summary_uses_nearest_rank_without_discarding_slow_samples() {
        let durations = [1, 3, 5, 7, 9]
            .into_iter()
            .map(Duration::from_micros)
            .collect::<Vec<_>>();
        let stats = summarize(&durations);
        assert_eq!(stats.count, 5);
        assert_eq!(stats.p50_us, 5.0);
        assert_eq!(stats.p95_us, 9.0);
        assert_eq!(stats.p99_us, 9.0);
        assert_eq!(stats.max_us, 9.0);
    }

    #[test]
    fn benchmark_cli_rejects_zero_iterations_and_excessive_worker_count() {
        assert!(Settings::parse(["--iterations", "0"].map(str::to_string)).is_err());
        assert!(Settings::parse(["--threads", "100"].map(str::to_string)).is_err());
        assert_eq!(
            Settings::parse(["--iterations", "25", "--threads", "2"].map(str::to_string))
                .unwrap()
                .iterations,
            25
        );
    }
}
