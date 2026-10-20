---
title: Embedded Search (Experimental)
description: Namespace-scoped lexical search and cached AI embeddings in the nulang-ai Rust library.
---

# Embedded Search

The `nulang-ai` Rust crate includes an **experimental, dependency-free**
`EmbeddedSearch` backend with an inverted index, BM25 relevance ranking,
upsert/delete, exact attribute filters, facets, and versioned snapshots.

This implementation borrows indexing principles from Tantivy, but **does not
currently embed Tantivy**. It has not been benchmarked against Tantivy,
Meilisearch, or OpenSearch. It is not a managed Nulang Cloud service or a
distributed index. No new Nulang language syntax or `nlc.search` package
is introduced by this initial slice.

## Rust integration

```rust
use nulang_ai::{EmbeddedSearch, SearchDocument, SearchQuery};
use std::collections::BTreeMap;

let mut index = EmbeddedSearch::new();
index.upsert(SearchDocument {
    namespace: "my-workspace".into(),
    id: "article-1".into(),
    text: "Nulang actors exchange typed messages".into(),
    attributes: BTreeMap::from([("type".into(), "article".into())]),
})?;

let mut query = SearchQuery::new("my-workspace", "typed actors");
query.filters.insert("type".into(), "article".into());
for hit in index.search(&query) {
    println!("{}: {}", hit.document.id, hit.score);
}
```

`namespace` is mandatory on ingestion, querying, facet counts, and
deletion. Callers should still authorize the requested namespace using the
authenticated principal; an arbitrary client-supplied namespace is not an
authorization mechanism. The `attributes` values are exact-match strings.

`SearchQuery::limit` defaults to 20. Empty or whitespace-only queries
return no hits. Unicode alphanumeric tokens are lowercased, but there is no
language-specific stemming, typo tolerance, phrase query, or fuzzy matching.

## Snapshots and consistency

```rust
let bytes = serde_json::to_vec(&index.snapshot())?;
// Persist bytes atomically using your application's durable storage.
// Later:
let restored = EmbeddedSearch::from_snapshot(
    serde_json::from_slice(&bytes)?
)?;
```

Snapshots contain source documents, **not** posting lists. Restore
rebuilds the index, verifies the version, and rejects duplicate document
identities. Writes become visible immediately in the same instance. This
module does not own storage durability, concurrency control, disk quotas,
or cross-process replication. Recovery of writes made after the most recent
snapshot requires rebuilding from an authoritative event log or database.

Search relevancy (BM25) considers all documents in the selected namespace;
filters restrict the returned hits. Facet counts apply namespace and exact
filters, but not the full-text query.

## Provider-independent Rust API

The `SearchBackend` trait supplies the common synchronous operations:
`index_document`, `remove_document`, `query`, and `facets`.
`EmbeddedSearch` implements this trait today. `query` returns owned
`OwnedSearchHit` values; scores are provider-specific and not comparable
between engines. Future Tantivy backends may implement the same trait without
introducing search keywords into the language core.

## Meilisearch remote adapter

The Rust crate also exposes an async Meilisearch client using the existing
reqwest dependency. This adapter is distinct from the synchronous
SearchBackend trait. It is an **experimental integration**, not hosted
Nulang Cloud search.

```rust
use nulang_ai::{MeilisearchClient, SearchDocument, SearchQuery};
use std::collections::BTreeMap;

let client = MeilisearchClient::new(
    "https://search.example.com",
    std::env::var("MEILISEARCH_TENANT_API_KEY")?,
    "workspace_a",
)?;
let update = client.upsert(&SearchDocument {
    namespace: "workspace_a".into(),
    id: "article_1".into(),
    text: "Nulang durable actors".into(),
    attributes: BTreeMap::from([("category".into(), "docs".into())]),
}).await?;

// Write APIs queue tasks asynchronously. Check that this task is marked
// "succeeded" before expecting the newly indexed document to be visible.
let state = client.task_status(update.task_uid).await?;
if state.status != "succeeded" {
    // Handle enqueued, processing, or failed states.
}

let hits = client.search(&SearchQuery::new("workspace_a", "durable")).await?;
```

A client instance is bound to exactly one namespace and one derived index
UID, such as `nulang_workspace_a`. The credentials must be issued
and restricted **server-side** to that index. The client checks namespace
identity on writes, reads, and task status responses. Do not accept a user
chosen endpoint, API key, or namespace without authenticated authorization.

Non-loopback HTTP endpoints, empty keys, invalid index identifiers, and
unsafe document IDs are rejected before network access. Redirect following
is disabled. Query limits are capped at 200, and empty queries do not
fall through to Meilisearch's match-all behavior.

Exact attribute filters require corresponding Meilisearch
`filterableAttributes` configuration. The adapter does not
implicitly change index settings, because those require broader
administrative privileges. Its own index-specific API key should not
have cross-tenant permissions. Index creation may also need to be
provisioned separately using an administrative service.

Meilisearch relevance scores are not BM25 scores. They must not be
compared numerically with scores from the embedded backend.

The current Nulang-language `http-client` seed package does not
forward configured HTTP headers through its sender, so a secure native
Nulang-language Meilisearch package is not shipped in this change.
HTTP authorization capability and a language-level adapter remain follow-ups.

## AI semantic-memory improvement

`SemanticMemory` now caches document embeddings on store and persists
them in serialized memory. Existing snapshots without the `embeddings`
field remain readable using an on-demand fallback. Vector retrieval
remains a linear scan; the default embeddings remain deterministic
hash-derived features, **not trained semantic embeddings**. Custom embedding functions are not serialized. The serialized provider marker
now distinguishes custom embeddings from the default deterministic algorithm.
When a custom provider is unavailable after restoration, the original
`search` method returns no results rather than mixing incompatible vectors.
Use `search_checked` to receive an explicit `MissingEmbeddingProvider`
error, then call `set_embedding_fn` with the original provider before
searching. The checked API also rejects dimension mismatches and non-finite
embedding components. Storing additional documents before reattaching a custom provider
records a placeholder rather than an incompatible vector, and rebuilding
with `set_embedding_fn` makes those documents searchable.

## Reproducible performance baseline

The opt-in Rust integration benchmark is available at
`crates/nulang-ai/tests/search_benchmark.rs`. It generates
deterministic multi-namespace documents and reports indexing throughput and
query p50/p95/mean. It makes no claims about service-level latency.

```bash
cargo test -p nulang-ai --lib --locked
cargo fmt --all --check

# This performance test is ignored in routine CI.
NULANG_SEARCH_BENCH_DOCS=100000 \
NULANG_SEARCH_BENCH_QUERIES=500 \
cargo test -p nulang-ai --test search_benchmark --locked -- --ignored --nocapture
```

The benchmark defaults to 10,000 documents and 200 queries. Increasing the
document count up to 1,000,000 is supported but may require substantial RAM.
Compare Tantivy on identical documents, namespace cardinality, queries, hit
limits, hardware, and warmed/cold-cache conditions before making migration
decisions. Record indexing memory and size-on-disk independently; the current
embedded engine keeps everything in process and does not own a disk-backed
segment format.

## Search safety and data correctness

- The embedded engine caches namespace document counts and token totals on
  insert, replacement, and deletion; snapshot restore rebuilds those counters.
- The semantic-memory checked API validates all query/document vector
  dimensions and finite numeric values. Invalid or corrupted cached vectors
  are not silently truncated or mixed with another embedding provider.
- All engines still require application-level namespace authorization before
  running a query. This crate does not implement an identity provider,
  ACL-aware result filtering, billing quotas, or transactional ingestion.
- The benchmark and tests have no externally verified results until the
  Rust CI jobs complete successfully.

## Follow-up work

1. Benchmark a Tantivy backend and add it as an optional Cargo feature
   when the corresponding Cargo.lock can be regenerated and checked in.
2. Add a stable, capability-checked `nlc.search` package facade backed by
   the Rust implementation.
3. Add language-sensitive analyzers, OpenSearch adapters, and a Nulang-language search facade; harden and integration-test the existing Meilisearch Rust adapter.
4. Add durable ingestion cursors, tenant authorization, cost limits and
   consistency contracts before offering managed Nulang Cloud search.
