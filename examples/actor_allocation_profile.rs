//! Synthetic ORCA allocation-size distribution, NOT a production workload.
//!
//! Separate fresh heap per bucket. Report actual physical size-class extent,
//! not `heap.used()` (which excludes LOS objects and counts reused blocks).
use nulang::runtime::{ActorHeap, TypeTag};
use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: actor_allocation_profile <positive-allocation-count>");
        return ExitCode::from(2);
    }
    let Ok(count) = args[1].parse::<usize>() else {
        eprintln!("invalid allocation count");
        return ExitCode::from(2);
    };
    if count == 0 || count > 1_000_000 {
        eprintln!("count must be 1..=1000000");
        return ExitCode::from(2);
    }

    for payload in [8usize, 16, 32, 64, 128, 256] {
        let mut heap = ActorHeap::new(16 * 1024);
        heap.set_actor_id(1);
        let mut pointers = Vec::with_capacity(count);
        let mut physical_total = 0usize;
        for _ in 0..count {
            let Some(ptr) = heap.alloc(payload, TypeTag::Raw) else {
                eprintln!("ORCA allocation failed");
                return ExitCode::FAILURE;
            };
            pointers.push(ptr);
            // The allocator rounds total bytes to its physical size class.
            // Above 256 bytes its large-object space uses an exact extent.
            let total = ActorHeap::HEADER_SIZE + payload.next_multiple_of(8);
            let physical = match total {
                0..=32 => 32,
                33..=64 => 64,
                65..=128 => 128,
                129..=256 => 256,
                other => other,
            };
            physical_total += physical;
        }
        // Pointers remain live until after totals are emitted; no reuse.
        std::hint::black_box(&pointers);
        println!("{{\"payload_bytes\":{payload},\"allocations\":{count},\"payload_total_bytes\":{},\"physical_bytes\":{physical_total}}}", payload * count);
    }
    ExitCode::SUCCESS
}
