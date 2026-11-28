//! Compare the actual cache under fixed arena pressure across revisions.
//! This is a deterministic admission/hit-ratio test, not a speed benchmark.
use nulang::runtime::cache::{CacheLimits, CacheStore, CacheValueView};

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn trace(seed: u64, arena: usize, mixed: bool) {
    let mut state = seed;
    let mut cache = CacheStore::with_s3fifo(CacheLimits {
        max_entries: 1024,
        max_arena_bytes: arena,
    });
    let mut rejected = 0u64;
    let mut inserted = 0u64;
    let requests = 100_000;
    let mut peak_entries = 0;
    for i in 0..requests {
        let random = next(&mut state);
        let id = if i % 10 < 6 {
            random % 128
        } else {
            1_000_000 + i
        };
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&id.to_le_bytes());
        let key = if mixed && id % 7 == 0 {
            &key[..]
        } else {
            &key[..8]
        };
        let size = if mixed {
            [32, 64, 128, 256][(id % 4) as usize]
        } else {
            64
        };
        let value = [id as u8; 256];
        let value = &value[..size];
        if let Some(hit) = cache.get(key, 0) {
            assert_eq!(hit, CacheValueView::Bytes(value));
        } else if cache.set_bytes(key, value, None, 0).is_ok() {
            inserted += 1;
        } else {
            rejected += 1;
        }
        assert!(cache.len() <= 1024);
        peak_entries = peak_entries.max(cache.len());
    }
    let stats = cache.stats();
    let memory = cache.memory_stats();
    assert_eq!(stats.hits + stats.misses, requests);
    assert_eq!(inserted + rejected, stats.misses);
    assert!(memory.arena_reserved_bytes <= arena);
    println!(
        "{seed},{arena},{mixed},{requests},{},{inserted},{rejected},{},{peak_entries},{},{}",
        stats.hits, stats.evictions, memory.arena_reserved_bytes, memory.retained_capacity_bytes
    );
}

fn main() {
    println!("seed,arena_limit_bytes,mixed,requests,hits,inserted,rejected,evictions,peak_entries,arena_extent_bytes,retained_capacity_bytes");
    for arena in [8192, 32768] {
        for mixed in [false, true] {
            for seed in [1, 42, 2026] {
                trace(seed, arena, mixed);
            }
        }
    }
}
