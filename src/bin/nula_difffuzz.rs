//! nula_difffuzz — grammar-based differential fuzz driver.
//!
//! Generates well-formed Nulang programs from a seeded RNG and runs each
//! through the bytecode VM (cold), the VM with forced JIT tier-up (warm),
//! and the AOT native backend (when the program compiles under it). Any
//! disagreement is a bug; crashers are persisted under
//! `fuzz/differential/crashers/`.
//!
//! Usage:
//!   nula_difffuzz [--seeds N] [--time SECS] [--seed-base HEX_OR_DEC]
//!                 [--crashers DIR] [--quiet]
//!
//! Defaults: --seeds 10000, no time limit, --seed-base 0,
//! --crashers fuzz/differential/crashers. Stops at whichever of --seeds or
//! --time is exhausted first. Exit code 0 = no divergences, 1 = at least
//! one divergence, 2 = usage error.

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn parse_u64(s: &str) -> Result<u64, String> {
    if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).map_err(|e| format!("bad hex value {:?}: {}", s, e))
    } else {
        s.parse::<u64>()
            .map_err(|e| format!("bad value {:?}: {}", s, e))
    }
}

fn main() {
    let mut seeds: u64 = 10_000;
    let mut time_secs: Option<u64> = None;
    let mut seed_base: u64 = 0;
    let mut crashers = PathBuf::from("fuzz/differential/crashers");
    let mut verbose = true;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut take = |name: &str| -> Result<String, String> {
            args.next()
                .ok_or_else(|| format!("{} requires a value", name))
        };
        match arg.as_str() {
            "--seeds" => match take("--seeds").and_then(|v| parse_u64(&v)) {
                Ok(v) => seeds = v,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            },
            "--time" => match take("--time").and_then(|v| parse_u64(&v)) {
                Ok(v) => time_secs = Some(v),
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            },
            "--seed-base" => match take("--seed-base").and_then(|v| parse_u64(&v)) {
                Ok(v) => seed_base = v,
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            },
            "--crashers" => match take("--crashers") {
                Ok(v) => crashers = PathBuf::from(v),
                Err(e) => {
                    eprintln!("error: {}", e);
                    std::process::exit(2);
                }
            },
            "--quiet" => verbose = false,
            "--help" | "-h" => {
                println!(
                    "nula_difffuzz [--seeds N] [--time SECS] [--seed-base N|0xN] [--crashers DIR] [--quiet]"
                );
                return;
            }
            other => {
                eprintln!("error: unknown argument {:?}", other);
                std::process::exit(2);
            }
        }
    }

    let deadline = time_secs.map(|s| Instant::now() + Duration::from_secs(s));
    let started = Instant::now();
    eprintln!(
        "nula_difffuzz: seeds {}..{} ({}), time limit {:?}",
        seed_base,
        seed_base.wrapping_add(seeds),
        seeds,
        time_secs
    );

    let stats =
        nulang::difffuzz::run_campaign(seed_base, seeds, deadline, Some(&crashers), verbose);

    println!(
        "campaign: {} programs generated in {:.1}s\n  agreed: {} ({} also agreed under AOT)\n  uncomparable: {}\n  compile failures: {}\n  known-overflow divergences: {}\n  divergences: {}",
        stats.generated,
        started.elapsed().as_secs_f64(),
        stats.agreed,
        stats.aot_agreed,
        stats.uncomparable,
        stats.compile_failures.len(),
        stats.known_overflow.len(),
        stats.divergences.len(),
    );
    for (seed, _) in stats.compile_failures.iter().take(10) {
        println!("  compile-failure seed: {0} (0x{0:x})", seed);
    }
    for d in stats.known_overflow.iter().take(10) {
        println!(
            "  known-overflow seed: {0} (0x{0:x}): {1}",
            d.seed,
            d.message.lines().next().unwrap_or("")
        );
    }
    for d in stats.divergences.iter().take(20) {
        println!(
            "  divergence seed: {0} (0x{0:x}): {1}",
            d.seed,
            d.message.lines().next().unwrap_or("")
        );
    }

    if !stats.divergences.is_empty() {
        std::process::exit(1);
    }
}
