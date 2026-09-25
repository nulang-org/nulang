//! Diagnostic multicore shard-scaling benchmark.
//!
//! This harness measures Nulang's best-case same-shard parallel scaling. It
//! creates one actor per shard, preloads a fixed total number of local messages
//! outside the timed region, then runs one scheduler thread per shard.
//!
//! Cross-shard transport, actor construction, message enqueue, and thread setup
//! before `Instant::now()` are intentionally excluded. Use the dedicated
//! same-vs-cross-shard transport benchmarks for communication overhead.
//!
//! Example:
//! cargo run --locked --release --no-default-features --features savina-bench \
//!   --bin nulang-shard-bench -- --shards 1,2,4,8 --messages 200000 --repeat 5 --format jsonl

use std::env;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nulang::runtime::{Actor, Runtime};
use nulang::vm::Value;
use serde_json::json;

#[derive(Clone, Copy)]
enum OutputFormat {
    Human,
    Jsonl,
}

struct Config {
    format: OutputFormat,
    shard_counts: Vec<usize>,
    messages: usize,
    repeat: u32,
}

#[derive(Debug, Clone)]
struct Measurement {
    shards: usize,
    messages: usize,
    elapsed: Duration,
}

impl Measurement {
    fn messages_per_second(&self) -> f64 {
        self.messages as f64 / self.elapsed.as_secs_f64()
    }

    fn ns_per_message(&self) -> f64 {
        self.elapsed.as_nanos() as f64 / self.messages as f64
    }
}

fn increment(actor: &mut Actor, _args: &[Value]) {
    let count = actor
        .get_state_field("count")
        .and_then(|value| value.as_int())
        .unwrap_or(0);
    actor.set_state_field("count", Value::int(count + 1));
}

fn parse_shards(raw: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            return Err("shard counts must not contain empty entries".to_string());
        }
        let count = token
            .parse::<usize>()
            .map_err(|_| format!("invalid shard count: {token}"))?;
        if count == 0 {
            return Err("shard counts must be >= 1".to_string());
        }
        if out.contains(&count) {
            return Err(format!("duplicate shard count: {count}"));
        }
        out.push(count);
    }
    if out.is_empty() {
        return Err("at least one shard count is required".to_string());
    }
    Ok(out)
}

fn spawn_counter_for_shard(runtime: &mut Runtime, shard_index: usize, shard_count: usize) -> u64 {
    loop {
        let actor_id = runtime.spawn_actor(Box::new(|| {
            vec![("count".to_string(), Value::int(0))]
        }));
        if actor_id % shard_count as u64 == shard_index as u64 {
            runtime
                .actors
                .get_mut(&actor_id)
                .expect("new benchmark actor must exist")
                .register_behavior("inc", increment);
            return actor_id;
        }
    }
}

fn run_independent(shard_count: usize, total_messages: usize) -> Measurement {
    assert!(shard_count > 0, "shard_count must be positive");
    assert!(total_messages > 0, "total_messages must be positive");

    let mut runtimes = Runtime::new_sharded(shard_count);
    let mut actor_ids = Vec::with_capacity(shard_count);
    let mut expected_per_shard = Vec::with_capacity(shard_count);

    for (index, runtime) in runtimes.iter_mut().enumerate() {
        let actor_id = spawn_counter_for_shard(runtime, index, shard_count);
        actor_ids.push(actor_id);

        // Clear any spawn-time ready state before preloading benchmark work.
        runtime.run_scheduler();

        let base = total_messages / shard_count;
        let remainder = total_messages % shard_count;
        let messages = base + usize::from(index < remainder);
        expected_per_shard.push(messages);

        for _ in 0..messages {
            runtime.send_message_by_id(actor_id, 0, &[]);
        }

        assert_eq!(
            runtime
                .actors
                .get(&actor_id)
                .expect("benchmark actor must remain live")
                .mailbox
                .len(),
            messages,
            "all timed work must be preloaded before scheduler timing"
        );
    }

    let started = Instant::now();
    std::thread::scope(|scope| {
        for runtime in &mut runtimes {
            scope.spawn(move || runtime.run_scheduler());
        }
    });
    let elapsed = started.elapsed();

    let mut processed = 0usize;
    for ((runtime, actor_id), expected) in runtimes
        .iter()
        .zip(actor_ids.iter())
        .zip(expected_per_shard.iter())
    {
        let count = runtime
            .actors
            .get(actor_id)
            .and_then(|actor| actor.get_state_field("count"))
            .and_then(|value| value.as_int())
            .expect("benchmark counter state must remain an Int");
        assert_eq!(
            count, *expected as i64,
            "each shard must drain exactly its assigned local workload"
        );
        processed += count as usize;
    }
    assert_eq!(
        processed, total_messages,
        "fixed total work must remain identical across shard counts"
    );

    Measurement {
        shards: shard_count,
        messages: total_messages,
        elapsed,
    }
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut format = OutputFormat::Human;
    let mut shard_counts = vec![1, 2, 4, 8];
    let mut messages = 200_000usize;
    let mut repeat = 5u32;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--format" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--format requires human or jsonl".to_string())?;
                format = match value.as_str() {
                    "human" => OutputFormat::Human,
                    "jsonl" => OutputFormat::Jsonl,
                    _ => return Err(format!("unsupported format: {value}")),
                };
            }
            "--shards" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--shards requires a comma-separated list".to_string())?;
                shard_counts = parse_shards(&value)?;
            }
            "--messages" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--messages requires a positive integer".to_string())?;
                messages = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid message count: {value}"))?;
                if messages == 0 {
                    return Err("--messages must be at least 1".to_string());
                }
            }
            "--repeat" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--repeat requires a positive integer".to_string())?;
                repeat = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid repeat count: {value}"))?;
                if repeat == 0 {
                    return Err("--repeat must be at least 1".to_string());
                }
            }
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    Ok(Some(Config {
        format,
        shard_counts,
        messages,
        repeat,
    }))
}

fn print_usage() {
    eprintln!(
        "Usage: nulang-shard-bench [--format human|jsonl] [--shards 1,2,4,8] [--messages N] [--repeat N]"
    );
}

fn emit(
    measurement: &Measurement,
    iteration: u32,
    baseline: Option<&Measurement>,
    format: OutputFormat,
) {
    let speedup = baseline.map(|base| {
        base.elapsed.as_secs_f64() / measurement.elapsed.as_secs_f64()
    });
    let efficiency = speedup.map(|value| value / measurement.shards as f64);
    let available_parallelism = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);

    match format {
        OutputFormat::Human => {
            println!(
                "[shard-bench] shards={} iteration={} messages={} elapsed={:.3}ms msg/s={:.0} ns/msg={:.1} speedup={} efficiency={} host_parallelism={}",
                measurement.shards,
                iteration,
                measurement.messages,
                measurement.elapsed.as_secs_f64() * 1_000.0,
                measurement.messages_per_second(),
                measurement.ns_per_message(),
                speedup
                    .map(|value| format!("{value:.3}x"))
                    .unwrap_or_else(|| "n/a".to_string()),
                efficiency
                    .map(|value| format!("{:.1}%", value * 100.0))
                    .unwrap_or_else(|| "n/a".to_string()),
                available_parallelism,
            );
        }
        OutputFormat::Jsonl => {
            println!(
                "{}",
                json!({
                    "schema": 1,
                    "runtime": "nulang",
                    "suite": "shard-scaling",
                    "workload": "independent_local_actor_drain",
                    "iteration": iteration,
                    "shards": measurement.shards,
                    "messages": measurement.messages,
                    "elapsed_ns": u64::try_from(measurement.elapsed.as_nanos())
                        .expect("benchmark duration must fit in u64"),
                    "messages_per_second": measurement.messages_per_second(),
                    "ns_per_message": measurement.ns_per_message(),
                    "speedup_vs_1_shard": speedup,
                    "parallel_efficiency": efficiency,
                    "host_parallelism": available_parallelism,
                })
            );
        }
    }
}

fn main() -> ExitCode {
    let config = match parse_args() {
        Ok(Some(config)) => config,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    for iteration in 1..=config.repeat {
        let measurements: Vec<Measurement> = config
            .shard_counts
            .iter()
            .map(|&shards| run_independent(shards, config.messages))
            .collect();
        let baseline = measurements.iter().find(|measurement| measurement.shards == 1);

        for measurement in &measurements {
            emit(measurement, iteration, baseline, config.format);
        }
    }

    ExitCode::SUCCESS
}

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
