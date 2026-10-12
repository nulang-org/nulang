//! Ractor actor-density companion to Nulang's examples/actor_density.rs.
//!
//! Measures fast unsupervised actor publication, runtime startup settlement,
//! and resident-memory delta on Linux. Separate process per count is required.
//! This is diagnostic: Ractor spawn_instant and Nulang Runtime::spawn_actor
//! expose different actor lifecycle semantics.
use ractor::actor::ActorRuntime;
use ractor::{Actor, ActorProcessingErr, ActorRef};
use std::hint::black_box;
use std::time::Instant;

struct Idle;
enum IdleMsg { Noop }

impl Actor for Idle {
    type Msg = IdleMsg;
    type State = ();
    type Arguments = ();

    async fn pre_start(
        &self,
        _: ActorRef<Self::Msg>,
        _: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(())
    }

    async fn handle(
        &self,
        _: ActorRef<Self::Msg>,
        message: Self::Msg,
        _: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message { IdleMsg::Noop => {} }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse::<u64>().ok()?.checked_mul(1024)
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes() -> Option<u64> { None }

fn print_rss(name: &str, bytes: Option<u64>) {
    if let Some(bytes) = bytes {
        println!("{name}={bytes}");
    } else {
        println!("{name}=unavailable");
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 || args[1] != "idle" {
        eprintln!("usage: density idle <positive-actor-count>");
        std::process::exit(2);
    }
    let actor_count: usize = args[2].parse().expect("invalid actor count");
    assert!((1..=1_000_000).contains(&actor_count), "actor count outside 1..=1000000");

    let rss_before = resident_bytes();
    let mut refs = Vec::with_capacity(actor_count);
    let mut start_tasks = Vec::with_capacity(actor_count);
    let start = Instant::now();
    for _ in 0..actor_count {
        // spawn_instant does NOT await pre_start: equivalent in intent to
        // Nulang's synchronous spawn_actor publication operation.
        let (actor, started) = ActorRuntime::<Idle>::spawn_instant(None, Idle, ())
            .expect("idle actor admission failed");
        refs.push(actor);
        start_tasks.push(started);
    }
    let elapsed = start.elapsed();
    let rss_after_spawn = resident_bytes();

    let settle = Instant::now();
    let mut actor_tasks = Vec::with_capacity(actor_count);
    for started in start_tasks {
        actor_tasks.push(started.await.expect("spawn join failed")
            .expect("actor pre_start failed"));
    }
    let settle_elapsed = settle.elapsed();
    let rss_after_settle = resident_bytes();

    black_box(&refs);
    black_box(&actor_tasks);
    println!("mode=idle");
    println!("runtime=ractor");
    println!("actor_count={actor_count}");
    println!("spawn_ns_per_item={:.1}", elapsed.as_nanos() as f64 / actor_count as f64);
    println!("spawn_seconds={:.6}", elapsed.as_secs_f64());
    println!("scheduler_settle_seconds={:.6}", settle_elapsed.as_secs_f64());
    print_rss("rss_before_bytes", rss_before);
    print_rss("rss_after_spawn_bytes", rss_after_spawn);
    print_rss("rss_after_settle_bytes", rss_after_settle);
    // RSS before initialization and after full actor startup measure different
    // lifecycle boundaries. Record both, but compare settled RSS by default.
    match (rss_before, rss_after_spawn) {
        (Some(before), Some(after)) => println!(
            "rss_spawn_delta_bytes_per_actor={:.1}",
            after.saturating_sub(before) as f64 / actor_count as f64
        ),
        _ => println!("rss_spawn_delta_bytes_per_actor=unavailable"),
    }
    match (rss_before, rss_after_settle) {
        (Some(before), Some(after)) => println!(
            "rss_settled_delta_bytes_per_actor={:.1}",
            after.saturating_sub(before) as f64 / actor_count as f64
        ),
        _ => println!("rss_settled_delta_bytes_per_actor=unavailable"),
    }

    for actor in refs {
        actor.stop(None);
    }
    for task in actor_tasks {
        task.await.expect("idle actor terminated unexpectedly");
    }
}
