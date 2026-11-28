//! Deterministic equal-entry-capacity hit-ratio comparison, not a throughput benchmark.
use nulang::runtime::cache::{CacheLimits, CacheStore};
use std::collections::{BTreeSet, HashMap, VecDeque};

const REQUESTS: usize = 100_000;

struct Reference {
    capacity: usize,
    lru: bool,
    keys: HashMap<u64, usize>,
    order: BTreeSet<(usize, u64)>,
    fifo: VecDeque<u64>,
}

impl Reference {
    fn new(capacity: usize, lru: bool) -> Self {
        Self {
            capacity,
            lru,
            keys: HashMap::new(),
            order: BTreeSet::new(),
            fifo: VecDeque::new(),
        }
    }
    fn access(&mut self, key: u64, now: usize) -> bool {
        if let Some(old) = self.keys.get_mut(&key) {
            if self.lru {
                self.order.remove(&(*old, key));
                *old = now;
                self.order.insert((now, key));
            }
            return true;
        }
        if self.keys.len() == self.capacity {
            let victim = if self.lru {
                self.order.pop_first().unwrap().1
            } else {
                self.fifo.pop_front().unwrap()
            };
            assert!(self.keys.remove(&victim).is_some());
        }
        self.keys.insert(key, now);
        if self.lru {
            self.order.insert((now, key));
        } else {
            self.fifo.push_back(key);
        }
        false
    }
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn trace(kind: &str, seed: u64, capacity: usize) {
    let mut state = seed;
    let mut cache = CacheStore::with_s3fifo(CacheLimits {
        max_entries: capacity,
        max_arena_bytes: 0,
    });
    let mut fifo = Reference::new(capacity, false);
    let mut lru = Reference::new(capacity, true);
    let mut hits = [0usize; 3];
    for i in 0..REQUESTS {
        let random = next(&mut state);
        let key = match kind {
            // A hot set interleaved with scans larger than the entire cache.
            "scan" => {
                if i % 10 < 6 {
                    random % 128
                } else {
                    1_000_000 + i as u64
                }
            }
            // Abrupt hot-set shifts every 20k references.
            "shift" => {
                if i % 10 < 8 {
                    (i / 20_000) as u64 * 1024 + random % 128
                } else {
                    1_000_000 + i as u64
                }
            }
            "uniform" => random % 4096,
            _ => unreachable!(),
        };
        let bytes = key.to_le_bytes();
        if cache.get(&bytes, 0).is_some() {
            hits[0] += 1;
        } else {
            cache.set_integer(&bytes, 1, None, 0).unwrap();
        }
        hits[1] += usize::from(fifo.access(key, i));
        hits[2] += usize::from(lru.access(key, i));
        assert!(cache.len() <= capacity);
    }
    let stats = cache.stats();
    assert_eq!(stats.hits, hits[0] as u64);
    assert_eq!(stats.hits + stats.misses, REQUESTS as u64);
    println!(
        "{kind},{seed},{capacity},{REQUESTS},{},{},{},{},{}",
        hits[0],
        hits[1],
        hits[2],
        stats.evictions,
        cache.memory_stats().retained_capacity_bytes
    );
}

fn main() {
    println!("workload,seed,capacity,requests,s3fifo_hits,fifo_hits,lru_hits,s3fifo_evictions,s3fifo_retained_capacity_bytes");
    for capacity in [256, 1024] {
        for kind in ["scan", "shift", "uniform"] {
            for seed in [1, 42, 2026] {
                trace(kind, seed, capacity);
            }
        }
    }
}
