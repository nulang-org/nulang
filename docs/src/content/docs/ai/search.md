---
title: Embedded Search (Experimental)
description: Namespace-scoped lexical search and cached AI embeddings in the nulang-ai Rust library.
---

# Embedded Search

The \`nulang-ai\` Rust crate includes an **experimental, dependency-free**
\`EmbeddedSearch\` backend with an inverted index, BM25 relevance ranking,
upsert/delete, exact attribute filters, facets, and versioned snapshots.

This implementation borrows indexing principles from Tantivy, but **does not
currently embed Tantivy**. It has not been benchmarked against Tantivy,
Meilisearch, or OpenSearch. It is not a managed Nulang Cloud service or a
distributed index. No new Nulang language syntax or \`nlc.search\` package
is introduced by this initial slice.

## Rust integration

\`\`\`rust
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
\`\`\`

\`namespace\` is mandatory on ingestion, querying, facet counts, and
deletion. Callers should still authorize the requested namespace using the
authenticated principal; an arbitrary client-supplied namespace is not an
authorization mechanism. The \`attributes\` values are exact-match strings.

\`SearchQuery::limit\` defaults to 20. Empty or whitespace-only queries
return no hits. Unicode alphanumeric tokens are lowercased, but there is no
language-specific stemming, typo tolerance, phrase query, or fuzzy matching.

## Snapshots and consistency

\`\`\`rust
let bytes = serde_json::to_vec(&index.snapshot())?;
// Persist bytes atomically using your application's durable storage.
// Later:
let restored = EmbeddedSearch::from_snapshot(
    serde_json::from_slice(&bytes)?
)?;
\`\`\`

Snapshots contain source documents, **not** posting lists. Restore
rebuilds the index, verifies the version, and rejects duplicate document
identities. Writes become visible immediately in the same instance. This
module does not own storage durability, concurrency control, disk quotas,
or cross-process replication. Recovery of writes made after the most recent
snapshot requires rebuilding from an authoritative event log or database.

Search relevancy (BM25) considers all documents in the selected namespace;
filters restrict the returned hits. Facet counts apply namespace and exact
filters, but not the full-text query.

## AI semantic-memory improvement

\`SemanticMemory\` now caches document embeddings on store and persists
them in serialized memory. Existing snapshots without the \`embeddings\`
field remain readable using an on-demand fallback. Vector retrieval
remains a linear scan; the default embeddings remain deterministic
hash-derived features, **not trained semantic embeddings**. Custom embedding
functions are not serialized and must be reattached by application code
after restoration if the same embedding model is required.

## Follow-up work

1. Benchmark a Tantivy backend and add it as an optional Cargo feature
   when the corresponding Cargo.lock can be regenerated and checked in.
2. Add a stable, capability-checked \`nlc.search\` package facade backed by
   the Rust implementation.
3. Add language-sensitive analyzers and Meilisearch/OpenSearch adapters.
4. Add durable ingestion cursors, tenant authorization, cost limits and
   consistency contracts before offering managed Nulang Cloud search.
