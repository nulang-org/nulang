//! Reproducible, opt-in embedded search microbenchmark.
//!
//! Run (not part of normal CI):
//!   NULANG_SEARCH_BENCH_DOCS=100000 cargo test -p nulang-ai \
//!       --test search_benchmark --locked -- --ignored --nocapture
//!
//! Set NULANG_SEARCH_BENCH_DOCS up to 1,000,000 and
//! NULANG_SEARCH_BENCH_QUERIES up to 10,000 to study larger corpora.
//! Reports measurements, not pass/fail performance thresholds.

use nulang_ai::{EmbeddedSearch, SearchDocument, SearchQuery};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn environment_count(key: &str, default: usize, limit: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map_or(default, |value| value.min(limit))
}

fn percentile(samples: &[Duration], numerator: usize, denominator: usize) -> Duration {
    let index = ((samples.len() - 1) * numerator) / denominator;
    samples[index]
}

#[test]
#[ignore = "large deterministic microbenchmark; run manually with --ignored"]
fn embedded_bm25_index_and_query_performance() {
    let count = environment_count("NULANG_SEARCH_BENCH_DOCS", 10_000, 1_000_000);
    let query_count = environment_count("NULANG_SEARCH_BENCH_QUERIES", 200, 10_000);
    let namespaces = 10;
    let mut index = EmbeddedSearch::new();

    let start = Instant::now();
    for i in 0..count {
        let tenant = format!("namespace_{}", i % namespaces);
        let content = format!(
            "Nulang durable actors search benchmark document {} category {} \
             distributed messaging and search indexing",
            i,
            i % 20
        );
        index
            .upsert(SearchDocument {
                namespace: tenant,
                id: format!("document_{i}"),
                text: content,
                attributes: BTreeMap::from([(
                    "category".to_owned(),
                    format!("c{}", i % 20),
                )]),
            })
            .expect("benchmark must index valid documents");
    }
    let index_time = start.elapsed();

    let mut latencies = Vec::with_capacity(query_count);
    let mut total_hits = 0usize;
    for i in 0..query_count {
        let mut query = SearchQuery::new(
            format!("namespace_{}", i % namespaces),
            if i % 2 == 0 {
                "durable distributed"
            } else {
                "benchmark indexing"
            },
        );
        query.limit = 20;

        let started = Instant::now();
        let hits = index.search(&query);
        latencies.push(started.elapsed());
        total_hits += hits.len();
    }

    latencies.sort();
    let query_total = latencies.iter().copied().sum::<Duration>();
    let per_second = count as f64 / index_time.as_secs_f64().max(1e-9);

    println!("search documents={count} namespaces={namespaces} queries={query_count}");
    println!(
        "indexing time_ms={:.2} throughput_docs_per_second={:.2}",
        index_time.as_secs_f64() * 1000.0,
        per_second
    );
    println!(
        "query p50_ms={:.4} p95_ms={:.4} average_ms={:.4} returned_hits={}",
        percentile(&latencies, 50, 100).as_secs_f64() * 1000.0,
        percentile(&latencies, 95, 100).as_secs_f64() * 1000.0,
        query_total.as_secs_f64() * 1000.0 / query_count as f64,
        total_hits
    );

    assert_eq!(index.document_count("namespace_0"), (count + namespaces - 1) / namespaces);
    assert!(total_hits > 0, "generated benchmark corpus must yield results");
}
