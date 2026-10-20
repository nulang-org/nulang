//! Dependency-free embedded lexical index and typed API for Nulang AI.
//!
//! This first backend uses a per-namespace inverted index with BM25 ranking.
//! It is an experimental in-process index, NOT a replacement for Tantivy or a
//! distributed engine. Callers own snapshot durability and admission controls.

use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

const SNAPSHOT_VERSION: u32 = 1;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// A document's namespace is part of its identity and mandatory on every read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchDocument {
    pub namespace: String,
    pub id: String,
    pub text: String,
    /// Exact-match string attributes; not full-text indexed.
    pub attributes: BTreeMap<String, String>,
}

/// A namespace-scoped lexical query. An empty query returns no hits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub namespace: String,
    pub text: String,
    pub filters: BTreeMap<String, String>,
    pub limit: usize,
}

impl SearchQuery {
    pub fn new(namespace: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            text: text.into(),
            filters: BTreeMap::new(),
            limit: 20,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SearchHit<'a> {
    pub document: &'a SearchDocument,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    EmptyNamespace,
    EmptyDocumentId,
    UnsupportedSnapshotVersion(u32),
    DuplicateSnapshotDocument { namespace: String, id: String },
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyNamespace => write!(f, "search namespace must not be empty"),
            Self::EmptyDocumentId => write!(f, "search document id must not be empty"),
            Self::UnsupportedSnapshotVersion(version) => {
                write!(f, "unsupported search snapshot version: {version}")
            }
            Self::DuplicateSnapshotDocument { namespace, id } => {
                write!(f, "duplicate snapshot document {namespace}/{id}")
            }
        }
    }
}

impl std::error::Error for SearchError {}

/// Serializable source documents. The inverted index is rebuilt on restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchSnapshot {
    pub version: u32,
    pub documents: Vec<SearchDocument>,
}

struct IndexedDocument {
    document: SearchDocument,
    token_count: usize,
}

/// An in-process namespace-isolated inverted index.
///
/// Not thread-safe by itself; synchronize externally if shared between tasks.
/// Updates are immediately visible to reads in the same instance. For durable
/// usage, persist snapshots atomically or rebuild from an authoritative log.
#[derive(Default)]
pub struct EmbeddedSearch {
    documents: HashMap<(String, String), IndexedDocument>,
    // (namespace, term) -> (document ID -> term frequency).
    postings: HashMap<(String, String), HashMap<String, usize>>,
}

impl EmbeddedSearch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&mut self, document: SearchDocument) -> Result<(), SearchError> {
        if document.namespace.trim().is_empty() {
            return Err(SearchError::EmptyNamespace);
        }
        if document.id.trim().is_empty() {
            return Err(SearchError::EmptyDocumentId);
        }

        let key = (document.namespace.clone(), document.id.clone());
        if let Some(previous) = self.documents.remove(&key) {
            for term in term_frequencies(&previous.document.text).into_keys() {
                match self.postings.entry((previous.document.namespace.clone(), term)) {
                    Entry::Occupied(mut entry) => {
                        entry.get_mut().remove(&previous.document.id);
                        if entry.get().is_empty() {
                            entry.remove();
                        }
                    }
                    Entry::Vacant(_) => {}
                }
            }
        }

        let frequencies = term_frequencies(&document.text);
        let token_count = frequencies.values().sum();
        for (term, count) in frequencies {
            self.postings
                .entry((document.namespace.clone(), term))
                .or_default()
                .insert(document.id.clone(), count);
        }
        self.documents.insert(
            key,
            IndexedDocument {
                document,
                token_count,
            },
        );
        Ok(())
    }

    pub fn delete(&mut self, namespace: &str, id: &str) -> bool {
        let Some(previous) = self.documents.remove(&(namespace.to_owned(), id.to_owned())) else {
            return false;
        };

        for term in term_frequencies(&previous.document.text).into_keys() {
            if let Entry::Occupied(mut entry) =
                self.postings.entry((namespace.to_owned(), term))
            {
                entry.get_mut().remove(id);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }
        true
    }

    pub fn document_count(&self, namespace: &str) -> usize {
        self.documents
            .values()
            .filter(|stored| stored.document.namespace == namespace)
            .count()
    }

    /// BM25 relevance, descending, breaking equal scores by document ID.
    /// Filtering is applied before any hits are returned.
    pub fn search(&self, query: &SearchQuery) -> Vec<SearchHit<'_>> {
        if query.namespace.trim().is_empty() || query.limit == 0 {
            return Vec::new();
        }
        let terms: BTreeSet<String> = tokenize(&query.text).into_iter().collect();
        if terms.is_empty() {
            return Vec::new();
        }

        let mut count = 0usize;
        let mut total_length = 0usize;
        for stored in self.documents.values() {
            if stored.document.namespace == query.namespace {
                count += 1;
                total_length += stored.token_count;
            }
        }
        if count == 0 {
            return Vec::new();
        }
        let avg_length = (total_length as f64 / count as f64).max(1.0);
        let mut scores: HashMap<&str, f64> = HashMap::new();

        for term in terms {
            let Some(postings) = self.postings.get(&(query.namespace.clone(), term)) else {
                continue;
            };
            let df = postings.len();
            let idf = (1.0 + (count as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln();

            for (doc_id, frequency) in postings {
                let Some(stored) = self
                    .documents
                    .get(&(query.namespace.clone(), doc_id.clone()))
                else {
                    continue;
                };
                if !matches_filters(&stored.document, &query.filters) {
                    continue;
                }
                let tf = *frequency as f64;
                let length_adjustment =
                    BM25_K1 * (1.0 - BM25_B + BM25_B * stored.token_count as f64 / avg_length);
                let score = idf * (tf * (BM25_K1 + 1.0)) / (tf + length_adjustment);
                *scores.entry(doc_id.as_str()).or_default() += score;
            }
        }

        let mut hits: Vec<SearchHit<'_>> = scores
            .into_iter()
            .filter_map(|(id, score)| {
                self.documents
                    .get(&(query.namespace.clone(), id.to_owned()))
                    .map(|stored| SearchHit {
                        document: &stored.document,
                        score,
                    })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.document.id.cmp(&b.document.id))
        });
        hits.truncate(query.limit);
        hits
    }

    /// Exact-match facet counts within the supplied namespace and filters.
    /// Unlike search(), facet_counts does not apply a free-text query.
    pub fn facet_counts(
        &self,
        namespace: &str,
        attribute: &str,
        filters: &BTreeMap<String, String>,
    ) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        if namespace.trim().is_empty() {
            return counts;
        }
        for stored in self.documents.values() {
            if stored.document.namespace != namespace
                || !matches_filters(&stored.document, filters)
            {
                continue;
            }
            if let Some(value) = stored.document.attributes.get(attribute) {
                *counts.entry(value.clone()).or_default() += 1;
            }
        }
        counts
    }

    pub fn snapshot(&self) -> SearchSnapshot {
        let mut documents: Vec<SearchDocument> = self
            .documents
            .values()
            .map(|stored| stored.document.clone())
            .collect();
        documents.sort_by(|a, b| {
            a.namespace.cmp(&b.namespace).then_with(|| a.id.cmp(&b.id))
        });
        SearchSnapshot {
            version: SNAPSHOT_VERSION,
            documents,
        }
    }

    pub fn from_snapshot(snapshot: SearchSnapshot) -> Result<Self, SearchError> {
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(SearchError::UnsupportedSnapshotVersion(snapshot.version));
        }
        let mut index = Self::new();
        for document in snapshot.documents {
            let key = (document.namespace.clone(), document.id.clone());
            if index.documents.contains_key(&key) {
                return Err(SearchError::DuplicateSnapshotDocument {
                    namespace: document.namespace,
                    id: document.id,
                });
            }
            index.upsert(document)?;
        }
        Ok(index)
    }
}

fn matches_filters(doc: &SearchDocument, filters: &BTreeMap<String, String>) -> bool {
    filters
        .iter()
        .all(|(name, value)| doc.attributes.get(name) == Some(value))
}

fn tokenize(input: &str) -> Vec<String> {
    input
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn term_frequencies(input: &str) -> HashMap<String, usize> {
    let mut frequencies = HashMap::new();
    for term in tokenize(input) {
        *frequencies.entry(term).or_default() += 1;
    }
    frequencies
}


#[cfg(test)]
mod tests {
    use super::*;

    fn document(namespace: &str, id: &str, text: &str, category: &str) -> SearchDocument {
        SearchDocument {
            namespace: namespace.into(),
            id: id.into(),
            text: text.into(),
            attributes: BTreeMap::from([("category".into(), category.into())]),
        }
    }

    #[test]
    fn bm25_ranks_matching_documents_above_less_relevant_ones() {
        let mut index = EmbeddedSearch::new();
        index.upsert(document("tenant-a", "rust", "rust actors rust concurrency", "docs")).unwrap();
        index.upsert(document("tenant-a", "other", "actors scheduling", "docs")).unwrap();
        index.upsert(document("tenant-a", "unrelated", "database transactions", "docs")).unwrap();

        let hits = index.search(&SearchQuery::new("tenant-a", "rust actors"));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].document.id, "rust");
        assert!(hits[0].score > hits[1].score);
        assert!(hits[0].score.is_finite());
    }

    #[test]
    fn search_and_facets_never_cross_namespaces() {
        let mut index = EmbeddedSearch::new();
        index.upsert(document("alice", "1", "private invoice", "finance")).unwrap();
        index.upsert(document("bob", "1", "private invoice", "secrets")).unwrap();

        let hits = index.search(&SearchQuery::new("alice", "private"));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].document.namespace, "alice");

        let counts = index.facet_counts("alice", "category", &BTreeMap::new());
        assert_eq!(counts, BTreeMap::from([("finance".to_owned(), 1)]));
        assert_eq!(index.document_count("bob"), 1);
    }

    #[test]
    fn upsert_replaces_old_postings_and_metadata() {
        let mut index = EmbeddedSearch::new();
        index.upsert(document("a", "1", "obsolete term", "old")).unwrap();
        index.upsert(document("a", "1", "replacement text", "new")).unwrap();

        assert!(index.search(&SearchQuery::new("a", "obsolete")).is_empty());
        assert_eq!(index.search(&SearchQuery::new("a", "replacement")).len(), 1);
        assert_eq!(index.document_count("a"), 1);
        assert_eq!(index.facet_counts("a", "category", &BTreeMap::new()).get("new"), Some(&1));
    }

    #[test]
    fn deletion_removes_postings_without_affecting_other_namespaces() {
        let mut index = EmbeddedSearch::new();
        index.upsert(document("a", "1", "shared keyword", "docs")).unwrap();
        index.upsert(document("b", "1", "shared keyword", "docs")).unwrap();
        assert!(index.delete("a", "1"));
        assert!(!index.delete("a", "1"));
        assert!(index.search(&SearchQuery::new("a", "shared")).is_empty());
        assert_eq!(index.search(&SearchQuery::new("b", "shared")).len(), 1);
    }

    #[test]
    fn query_filters_and_facets_are_scoped() {
        let mut index = EmbeddedSearch::new();
        index.upsert(document("a", "1", "search term", "public")).unwrap();
        index.upsert(document("a", "2", "search term", "private")).unwrap();
        let filter = BTreeMap::from([("category".into(), "public".into())]);

        let mut query = SearchQuery::new("a", "search");
        query.filters = filter.clone();
        assert_eq!(index.search(&query).len(), 1);
        assert_eq!(index.search(&query)[0].document.id, "1");
        assert_eq!(index.facet_counts("a", "category", &filter), BTreeMap::from([("public".into(), 1)]));
    }

    #[test]
    fn invalid_ids_and_empty_queries_fail_closed() {
        let mut index = EmbeddedSearch::new();
        assert_eq!(index.upsert(document("", "1", "text", "docs")), Err(SearchError::EmptyNamespace));
        assert_eq!(index.upsert(document("a", " ", "text", "docs")), Err(SearchError::EmptyDocumentId));
        index.upsert(document("a", "1", "hello", "docs")).unwrap();
        assert!(index.search(&SearchQuery::new("", "hello")).is_empty());
        assert!(index.search(&SearchQuery::new("a", "")).is_empty());

        let mut query = SearchQuery::new("a", "hello");
        query.limit = 0;
        assert!(index.search(&query).is_empty());
    }

    #[test]
    fn ranking_is_stable_and_honors_limit() {
        let mut index = EmbeddedSearch::new();
        index.upsert(document("a", "b", "same text", "docs")).unwrap();
        index.upsert(document("a", "a", "same text", "docs")).unwrap();
        let mut query = SearchQuery::new("a", "same");
        query.limit = 1;
        let hits = index.search(&query);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].document.id, "a");
    }

    #[test]
    fn snapshots_rebuild_the_index_and_reject_unknown_versions() {
        let mut index = EmbeddedSearch::new();
        index.upsert(document("a", "1", "recoverable unicode Café", "docs")).unwrap();

        let encoded = serde_json::to_string(&index.snapshot()).unwrap();
        let snapshot: SearchSnapshot = serde_json::from_str(&encoded).unwrap();
        let restored = EmbeddedSearch::from_snapshot(snapshot.clone()).unwrap();
        assert_eq!(restored.search(&SearchQuery::new("a", "CAFÉ")).len(), 1);
        assert_eq!(restored.snapshot(), snapshot);
        assert!(matches!(
            EmbeddedSearch::from_snapshot(SearchSnapshot { version: 99, ..snapshot }),
            Err(SearchError::UnsupportedSnapshotVersion(99))
        ));
    }
    #[test]
    fn backend_trait_preserves_scoped_search_and_facet_contract() {
        let mut backend: Box<dyn SearchBackend> = Box::new(EmbeddedSearch::new());
        backend.index_document(document("alice", "1", "typed actors and messages", "docs")).unwrap();
        backend.index_document(document("bob", "1", "typed actors and messages", "secret")).unwrap();

        let alice = backend.query(&SearchQuery::new("alice", "actors")).unwrap();
        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].document.namespace, "alice");
        assert!(alice[0].score > 0.0);
        assert_eq!(
            backend.facets("alice", "category", &BTreeMap::new()).unwrap(),
            BTreeMap::from([("docs".to_string(), 1)])
        );

        assert!(backend.remove_document("alice", "1").unwrap());
        assert!(backend.query(&SearchQuery::new("alice", "actors")).unwrap().is_empty());
        assert_eq!(backend.query(&SearchQuery::new("bob", "actors")).unwrap().len(), 1);
    }

}
