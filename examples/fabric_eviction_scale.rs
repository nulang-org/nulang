//! Bounded single-process cache-kernel scale qualification.
//! Run each policy in a separate process for comparable peak RSS.
use nulang::runtime::cache::{CacheAdmissionError, CacheLimits, CacheStore, CacheValueView};
use std::time::Instant;

fn rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|line| line.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        3,
        "usage: fabric_eviction_scale noeviction|s3fifo entries"
    );
    let policy = &args[1];
    let capacity: usize = args[2].parse().expect("positive entry capacity");
    assert!((100..=1_000_000).contains(&capacity));
    let arena = capacity.checked_mul(64).unwrap();
    let limits = CacheLimits {
        max_entries: capacity,
        max_arena_bytes: arena,
    };
    let mut cache = match policy.as_str() {
        "noeviction" => CacheStore::with_limits(limits),
        "s3fifo" => CacheStore::with_s3fifo(limits),
        _ => panic!("unknown policy"),
    };
    let fill = Instant::now();
    for id in 0..capacity as u64 {
        cache
            .set_bytes(&id.to_le_bytes(), &[id as u8; 64], None, 0)
            .unwrap();
    }
    let fill_seconds = fill.elapsed().as_secs_f64();
    let initial = cache.memory_stats();
    assert_eq!(initial.entries, capacity);
    assert_eq!(initial.arena_reserved_bytes, arena);
    let initial_rss = rss_kib();
    let hot = (capacity / 16).max(1) as u64;
    let mut hits = 0u64;
    let mut admitted = 0u64;
    let mut rejected = 0u64;
    let mut rejected_entry = 0u64;
    let mut rejected_arena = 0u64;
    let mut initial_rejection_prefix = 0u64;
    let scan = Instant::now();
    for round in 0..4u64 {
        // Two complete hot-set passes warm the probationary frequency
        // counters. Then scan half of the configured capacity in new keys.
        for _ in 0..2 {
            for id in 0..hot {
                if let Some(value) = cache.get(&id.to_le_bytes(), 0) {
                    assert_eq!(value, CacheValueView::Bytes(&[id as u8; 64]));
                    hits += 1;
                }
            }
        }
        for offset in 0..(capacity / 2) as u64 {
            let id = capacity as u64 + round * (capacity / 2) as u64 + offset;
            assert!(cache.get(&id.to_le_bytes(), 0).is_none());
            let evictions_before = cache.stats().evictions;
            match cache.set_bytes(&id.to_le_bytes(), &[id as u8; 64], None, 0) {
                Ok(()) => {
                    admitted += 1;
                    assert_eq!(
                        cache.get(&id.to_le_bytes(), 0),
                        Some(CacheValueView::Bytes(&[id as u8; 64]))
                    );
                }
                Err(error) => {
                    rejected += 1;
                    match error {
                        CacheAdmissionError::EntryLimit => rejected_entry += 1,
                        CacheAdmissionError::ArenaLimit => rejected_arena += 1,
                    }
                    assert_eq!(cache.stats().evictions, evictions_before);
                    assert_eq!(cache.len(), capacity);
                    assert!(!cache.exists(&id.to_le_bytes(), 0));
                    if round == 0 && offset == initial_rejection_prefix {
                        initial_rejection_prefix += 1;
                    }
                }
            }
        }
        for id in 0..hot {
            assert_eq!(
                cache.get(&id.to_le_bytes(), 0),
                Some(CacheValueView::Bytes(&[id as u8; 64])),
                "hot key {id} disappeared after scan round {round}"
            );
            hits += 1;
        }
        assert_eq!(cache.len(), capacity);
        assert_eq!(cache.memory_stats().arena_reserved_bytes, arena);
    }
    let scan_seconds = scan.elapsed().as_secs_f64();
    let memory = cache.memory_stats();
    let stats = cache.stats();
    let attempted = 4 * (capacity / 2) as u64;
    assert_eq!(hits, 12 * hot);
    assert_eq!(attempted, admitted + rejected);
    match policy.as_str() {
        "s3fifo" => {
            // The deliberately hot initial FIFO prefix can exhaust the
            // bounded scan while promoting entries. Record conservative
            // rejections, not retries or successful writes.
            assert_eq!(rejected, initial_rejection_prefix);
            assert_eq!(rejected_arena, 0);
            assert!(admitted > 0);
            assert_eq!(stats.evictions, admitted);
        }
        "noeviction" => {
            assert_eq!(rejected, attempted);
            assert_eq!(stats.evictions, 0);
        }
        _ => unreachable!(),
    }
    println!("policy,entries,arena_bytes,hot_keys,hot_verified_reads,scan_attempts,admitted,rejected,entry_rejected,arena_rejected,initial_rejection_prefix,evictions,fill_seconds,scan_seconds,initial_retained_bytes,final_retained_bytes,initial_peak_rss_kib,final_peak_rss_kib");
    println!("{policy},{capacity},{arena},{hot},{hits},{attempted},{admitted},{rejected},{rejected_entry},{rejected_arena},{initial_rejection_prefix},{},{fill_seconds:.6},{scan_seconds:.6},{},{},{},{}",
        stats.evictions, initial.retained_capacity_bytes, memory.retained_capacity_bytes,
        initial_rss.map_or(String::new(), |x| x.to_string()), rss_kib().map_or(String::new(), |x| x.to_string()));
}
