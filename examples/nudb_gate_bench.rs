//! Reproducible, opt-in NuDB WAL write-gate latency benchmark.
//!
//! cargo run --release --no-default-features --example nudb_gate_bench -- \
//!   --iterations 500 --threads 4 --warmup 20
//!
//! Both modes execute identical FileWal append+fsync operations and payloads:
//! one WAL per worker, either in a shared directory (one contended gate) or
//! separate directories (independent gates). All paths are on the same local
//! filesystem. This measures end-to-end WAL append latency under directory
//! contention, not the isolated nanosecond cost of File::lock or distributed
//! database throughput. Run multiple times; fsync noise is significant.

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletWrite,
};
use nulang::database::wal::FileWal;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    iterations: usize,
    threads: usize,
    warmup: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            iterations: 500,
            threads: 4,
            warmup: 20,
        }
    }
}

impl Settings {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut settings = Self::default();
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or_else(|| format!("missing numeric value for {flag}"))?;
            let parsed: usize = value
                .parse()
                .map_err(|_| format!("{flag} requires a positive integer"))?;
            match flag.as_str() {
                "--iterations" => settings.iterations = parsed,
                "--threads" => settings.threads = parsed,
                "--warmup" => settings.warmup = parsed,
                _ => return Err(format!("unknown option {flag}")),
            }
        }
        if !(1..=100_000).contains(&settings.iterations) {
            return Err("iterations must be between 1 and 100000".into());
        }
        if !(1..=32).contains(&settings.threads) {
            return Err("threads must be between 1 and 32".into());
        }
        if settings.warmup > 10_000 {
            return Err("warmup must be at most 10000".into());
        }
        Ok(settings)
    }
}

#[derive(Debug, serde::Serialize)]
struct LatencySummary {
    count: usize,
    mean_us: f64,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    max_us: f64,
}

fn summarize(samples: &[Duration]) -> LatencySummary {
    assert!(!samples.is_empty(), "benchmark must record at least one write");
    let mut us: Vec<f64> = samples
        .iter()
        .map(|sample| sample.as_secs_f64() * 1_000_000.0)
        .collect();
    us.sort_by(f64::total_cmp);
    let percentile = |p: f64| -> f64 {
        let rank = ((us.len() as f64 * p).ceil() as usize).saturating_sub(1);
        us[rank.min(us.len() - 1)]
    };
    LatencySummary {
        count: us.len(),
        mean_us: us.iter().sum::<f64>() / us.len() as f64,
        p50_us: percentile(0.50),
        p95_us: percentile(0.95),
        p99_us: percentile(0.99),
        max_us: *us.last().unwrap(),
    }
}

#[derive(Debug)]
struct Worker {
    first_write: Instant,
    last_write: Instant,
    latencies: Vec<Duration>,
}

#[derive(Debug, serde::Serialize)]
struct ModeResult {
    mode: &'static str,
    threads: usize,
    iterations_per_thread: usize,
    wall_seconds: f64,
    throughput_writes_per_second: f64,
    append_latency: LatencySummary,
}

fn as_io_error(error: impl Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(error)
}

fn write(
    descriptor: &TabletDescriptor,
    previous_sequence: u64,
    worker: usize,
) -> io::Result<TabletWrite> {
    TabletWrite::prepare(
        descriptor,
        descriptor.ownership_epoch(),
        previous_sequence,
        previous_sequence,
        vec![TabletMutation::Put {
            key: b"k".to_vec(),
            value: vec![worker as u8; 64],
        }],
    )
    .map_err(as_io_error)
}

fn measure(root: &Path, shared: bool, settings: &Settings) -> io::Result<ModeResult> {
    let gate = Arc::new(Barrier::new(settings.threads));
    let mut handles = Vec::with_capacity(settings.threads);
    for worker in 0..settings.threads {
        let gate = Arc::clone(&gate);
        let iterations = settings.iterations;
        let warmup = settings.warmup;
        let dir = if shared {
            root.join("shared")
        } else {
            root.join("isolated").join(format!("worker-{worker}"))
        };
        let path = dir.join(format!("tablet-{worker}.wal"));
        handles.push(thread::spawn(move || -> io::Result<Worker> {
            fs::create_dir_all(&dir)?;
            let id = TabletId::new(100 + worker as u64).map_err(as_io_error)?;
            let range = KeyRange::new(b"a".to_vec(), Some(b"z".to_vec()))
                .map_err(as_io_error)?;
            let descriptor = TabletDescriptor::new(id, range, 3).map_err(as_io_error)?;
            let mut wal = FileWal::open(path).map_err(as_io_error)?;
            let mut sequence = 0_u64;

            for _ in 0..warmup {
                wal.append_write(&write(&descriptor, sequence, worker)?)
                    .map_err(as_io_error)?;
                sequence += 1;
            }

            // All writers start measuring in the same phase, without counting
            // WAL setup/creation or uneven compiler/runtime warmup.
            gate.wait();
            let first_write = Instant::now();
            let mut latencies = Vec::with_capacity(iterations);
            for _ in 0..iterations {
                let prepared = write(&descriptor, sequence, worker)?;
                let started = Instant::now();
                wal.append_write(&prepared).map_err(as_io_error)?;
                latencies.push(started.elapsed());
                sequence += 1;
            }
            let last_write = Instant::now();
            Ok(Worker {
                first_write,
                last_write,
                latencies,
            })
        }));
    }

    let mut workers = Vec::with_capacity(settings.threads);
    for handle in handles {
        workers.push(
            handle
                .join()
                .map_err(|_| io::Error::other("NuDB WAL benchmark worker panicked"))??,
        );
    }
    let first = workers.iter().map(|worker| worker.first_write).min().unwrap();
    let last = workers.iter().map(|worker| worker.last_write).max().unwrap();
    let elapsed = last.duration_since(first);
    let samples: Vec<Duration> = workers
        .into_iter()
        .flat_map(|worker| worker.latencies)
        .collect();
    let count = samples.len();
    Ok(ModeResult {
        mode: if shared {
            "shared_directory_gate"
        } else {
            "isolated_directory_gates"
        },
        threads: settings.threads,
        iterations_per_thread: settings.iterations,
        wall_seconds: elapsed.as_secs_f64(),
        throughput_writes_per_second: count as f64 / elapsed.as_secs_f64(),
        append_latency: summarize(&samples),
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let settings = Settings::parse(std::env::args().skip(1))
        .map_err(io::Error::other)?;
    let seed = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root: PathBuf = std::env::temp_dir().join(format!(
        "nudb_gate_bench_{}_{}",
        std::process::id(),
        seed
    ));
    fs::create_dir_all(&root)?;

    // Running isolated first consistently is easy to reproduce; repeat runs
    // and alternate externally if the benchmark is used in performance gates.
    let outcome = (|| -> io::Result<Vec<ModeResult>> {
        let isolated = measure(&root, false, &settings)?;
        let shared = measure(&root, true, &settings)?;
        Ok(vec![isolated, shared])
    })();
    let _ = fs::remove_dir_all(&root);
    println!("{}", serde_json::to_string_pretty(&outcome?)?);
    Ok(())
}

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
