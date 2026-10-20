//! Semantic memory for long-term fact retrieval.
//!
//! `SemanticMemory` stores [`Document`] values and retrieves the most relevant
//! ones for a query using cosine similarity over deterministic embeddings.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A single fact stored in semantic memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// Stable identifier derived from the content hash.
    pub id: String,
    /// The fact or passage text.
    pub content: String,
    /// Arbitrary string metadata associated with the fact.
    pub metadata: HashMap<String, String>,
}

/// Function pointer type for producing a vector embedding from a text string.
pub type EmbeddingFn = fn(&str) -> Vec<f32>;

/// Identifies which embedding algorithm produced the cached vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum EmbeddingSource {
    #[default]
    Deterministic,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticMemoryError {
    MissingEmbeddingProvider,
    EmbeddingDimensionMismatch { expected: usize, actual: usize },
    InvalidEmbeddingValue,
}

impl std::fmt::Display for SemanticMemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEmbeddingProvider => {
                write!(f, "custom embedding provider must be reattached after restore")
            }
            Self::EmbeddingDimensionMismatch { expected, actual } => {
                write!(f, "invalid embedding dimension: expected {expected}, got {actual}")
            }
            Self::InvalidEmbeddingValue => {
                write!(f, "embedding contains a non-finite value")
            }
        }
    }
}

impl std::error::Error for SemanticMemoryError {}

/// Long-term semantic memory backed by a brute-force vector search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticMemory {
    /// Stored documents.
    pub documents: Vec<Document>,
    /// Dimensionality of the embedding vectors.
    pub dimensions: usize,
    /// Precomputed document vectors. Legacy snapshots without this field are
    /// still readable and are embedded on demand.
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
    /// Provenance guard prevents mixing cached custom vectors with default
    /// hash-derived query embeddings after deserialization.
    #[serde(default)]
    embedding_source: EmbeddingSource,
    /// Optional custom embedding function. Defaults to a deterministic hash-based
    /// embedding when `None`.
    #[serde(skip)]
    embedding_fn: Option<EmbeddingFn>,
}

impl PartialEq for SemanticMemory {
    fn eq(&self, other: &Self) -> bool {
        self.documents == other.documents && self.dimensions == other.dimensions
    }
}

impl SemanticMemory {
    /// Create an empty semantic memory with the given vector dimensionality.
    ///
    /// If `embedding_fn` is `None`, a built-in deterministic embedding is used.
    pub fn new(dimensions: usize, embedding_fn: Option<EmbeddingFn>) -> Self {
        Self {
            documents: Vec::new(),
            dimensions,
            embeddings: Vec::new(),
            embedding_source: if embedding_fn.is_some() {
                EmbeddingSource::Custom
            } else {
                EmbeddingSource::Deterministic
            },
            embedding_fn,
        }
    }

    /// Attach an embedding provider after restore, or change providers.
    ///
    /// Document vectors are rebuilt because provider functions are not
    /// serialized. Call this before querying a restored memory that was
    /// created with a custom embedding function.
    pub fn set_embedding_fn(&mut self, embedding_fn: Option<EmbeddingFn>) {
        self.embedding_fn = embedding_fn;
        self.embedding_source = if embedding_fn.is_some() {
            EmbeddingSource::Custom
        } else {
            EmbeddingSource::Deterministic
        };
        self.embeddings = self
            .documents
            .iter()
            .map(|doc| self.embed(&doc.content))
            .collect();
    }

    /// Store a fact and return its deterministic document id.
    pub fn store(
        &mut self,
        content: impl Into<String>,
        metadata: HashMap<String, String>,
    ) -> String {
        let content = content.into();
        let id = deterministic_id(&content);
        // When an older snapshot is loaded, populate its missing vectors before
        // appending so document offsets and cached vectors remain aligned.
        if self.embeddings.len() != self.documents.len() {
            self.embeddings = if self.embedding_source == EmbeddingSource::Custom
                && self.embedding_fn.is_none()
            {
                vec![Vec::new(); self.documents.len()]
            } else {
                self.documents
                    .iter()
                    .map(|doc| self.embed(&doc.content))
                    .collect()
            };
        }
        // Preserve a placeholder rather than silently producing incompatible
        // hash embeddings when restoring memory from a custom provider.
        let embedding = if self.embedding_source == EmbeddingSource::Custom
            && self.embedding_fn.is_none()
        {
            Vec::new()
        } else {
            self.embed(&content)
        };
        self.documents.push(Document {
            id: id.clone(),
            content,
            metadata,
        });
        self.embeddings.push(embedding);
        id
    }

    /// Search stored documents by cosine similarity to the query embedding.
    ///
    /// Returns up to `top_k` results sorted by descending similarity score.
    pub fn search(&self, query: &str, top_k: usize) -> Vec<(&Document, f32)> {
        // Historical infallible interface: fail closed on missing providers.
        // New callers should use search_checked() to propagate the error.
        self.search_checked(query, top_k).unwrap_or_default()
    }

    /// Fallible search that rejects a missing custom embedding provider.
    pub fn search_checked(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<(&Document, f32)>, SemanticMemoryError> {
        if self.embedding_source == EmbeddingSource::Custom && self.embedding_fn.is_none() {
            return Err(SemanticMemoryError::MissingEmbeddingProvider);
        }
        if top_k == 0 || self.documents.is_empty() {
            return Ok(Vec::new());
        }

        let query_embedding = self.embed(query);
        validate_embedding(&query_embedding, self.dimensions)?;
        let mut results: Vec<(&Document, f32)> = Vec::with_capacity(self.documents.len());

        for (index, doc) in self.documents.iter().enumerate() {
            // Legacy snapshots without embeddings are re-embedded on demand.
            // Invalid *present* embeddings fail closed rather than silently
            // comparing truncated vectors or falling back to another provider.
            let fallback;
            let doc_embedding = match self.embeddings.get(index) {
                Some(embedding) => embedding,
                None => {
                    fallback = self.embed(&doc.content);
                    &fallback
                }
            };
            validate_embedding(doc_embedding, self.dimensions)?;
            let score = cosine_similarity(&query_embedding, doc_embedding);
            results.push((doc, score));
        }

        results.sort_by(|a, b| b.1.total_cmp(&a.1));
        results.truncate(top_k);
        Ok(results)
    }

    /// Remove the document with the given id.
    ///
    /// Returns `true` if a document was removed.
    pub fn delete(&mut self, id: &str) -> bool {
        if let Some(pos) = self.documents.iter().position(|doc| doc.id == id) {
            if self.embeddings.len() == self.documents.len() {
                self.embeddings.remove(pos);
            } else {
                // Partial/legacy caches cannot safely retain positional entries.
                self.embeddings.clear();
            }
            self.documents.remove(pos);
            true
        } else {
            false
        }
    }

    /// Return the number of stored documents.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Return whether the memory contains no documents.
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        match self.embedding_fn {
            Some(f) => f(text),
            None => default_embed(text, self.dimensions),
        }
    }
}

// -----------------------------------------------------------------------------
// Deterministic embedding
// -----------------------------------------------------------------------------

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn deterministic_id(content: &str) -> String {
    format!("{:016x}", fnv1a(content.as_bytes()))
}

/// Deterministic embedding function.
///
/// The text is tokenized on whitespace. Each token is hashed with a stable
/// FNV-1a function and the resulting hash is projected into `dimensions`
/// values in `[-1, 1]`. Token vectors are averaged and L2-normalized.
fn default_embed(input: &str, dimensions: usize) -> Vec<f32> {
    if dimensions == 0 {
        return Vec::new();
    }

    let tokens: Vec<&str> = input.split_whitespace().collect();
    if tokens.is_empty() {
        return vec![0.0; dimensions];
    }

    let mut accumulator = vec![0.0f32; dimensions];
    for token in &tokens {
        let base_hash = fnv1a(token.as_bytes());
        let mut token_vec = vec![0.0f32; dimensions];
        let mut state = base_hash;
        for (i, value) in token_vec.iter_mut().enumerate() {
            state = state.wrapping_add((i as u64).wrapping_mul(0x9e3779b97f4a7c15));
            let bits = (splitmix64(state) >> 32) as u32;
            *value = (bits as f32 / u32::MAX as f32) * 2.0 - 1.0;
        }
        l2_normalize(&mut token_vec);
        for (acc, v) in accumulator.iter_mut().zip(token_vec.iter()) {
            *acc += *v;
        }
    }

    l2_normalize(&mut accumulator);
    accumulator
}

/// SplitMix64 pseudo-random number generator.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

fn l2_normalize(vec: &mut [f32]) {
    let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in vec.iter_mut() {
            *v /= norm;
        }
    }
}

fn validate_embedding(
    embedding: &[f32],
    expected: usize,
) -> Result<(), SemanticMemoryError> {
    if embedding.len() != expected {
        return Err(SemanticMemoryError::EmbeddingDimensionMismatch {
            expected,
            actual: embedding.len(),
        });
    }
    if !embedding.iter().all(|component| component.is_finite()) {
        return Err(SemanticMemoryError::InvalidEmbeddingValue);
    }
    Ok(())
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for i in 0..len {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semantic_memory_store_and_search() {
        let mut memory = SemanticMemory::new(64, None);
        memory.store(
            "The quick brown fox jumps over the lazy dog",
            HashMap::new(),
        );
        memory.store(
            "Rust is a systems programming language with fearless concurrency",
            HashMap::new(),
        );

        let results = memory.search("programming in Rust", 2);
        assert_eq!(results.len(), 2);
        assert!(
            results[0].0.content.contains("Rust"),
            "expected Rust document to rank first, got {:?}",
            results
        );
        assert!(results[0].1 > results[1].1);
    }

    #[test]
    fn test_semantic_memory_deterministic_embeddings() {
        let mut memory1 = SemanticMemory::new(64, None);
        let mut memory2 = SemanticMemory::new(64, None);

        let id1 = memory1.store("deterministic test content", HashMap::new());
        let id2 = memory2.store("deterministic test content", HashMap::new());
        assert_eq!(id1, id2);

        let query = "deterministic test";
        let score1 = memory1.search(query, 1)[0].1;
        let score2 = memory2.search(query, 1)[0].1;
        assert!((score1 - score2).abs() < f32::EPSILON);
    }

    #[test]
    fn test_semantic_memory_serialization_roundtrip() {
        let mut memory = SemanticMemory::new(32, None);
        let id1 = memory.store("alpha fact", HashMap::new());

        let mut meta = HashMap::new();
        meta.insert("source".to_string(), "unit-test".to_string());
        let id2 = memory.store("beta fact", meta);

        let json = serde_json::to_string(&memory).unwrap();
        let restored: SemanticMemory = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 2);
        assert_eq!(restored.documents[0].id, id1);
        assert_eq!(restored.documents[1].id, id2);
        assert_eq!(
            restored.documents[1].metadata.get("source").unwrap(),
            "unit-test"
        );
        assert_eq!(restored.dimensions, 32);

        let results = restored.search("alpha", 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.content, "alpha fact");
    }

    #[test]
    fn stored_embeddings_are_not_recomputed_for_every_query() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn counting_embedding(text: &str) -> Vec<f32> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            if text.contains("rust") {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            }
        }

        CALLS.store(0, Ordering::SeqCst);
        let mut memory = SemanticMemory::new(2, Some(counting_embedding));
        memory.store("rust", HashMap::new());
        memory.store("database", HashMap::new());
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);

        assert_eq!(memory.search("rust", 1)[0].0.content, "rust");
        assert_eq!(memory.search("rust", 1)[0].0.content, "rust");
        // Two writes and two query embeddings, not six total embeddings.
        assert_eq!(CALLS.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn restored_custom_memory_can_reattach_its_embedding_provider() {
        fn embed(text: &str) -> Vec<f32> {
            if text.contains("rust") {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            }
        }

        let mut memory = SemanticMemory::new(2, Some(embed));
        memory.store("rust", HashMap::new());
        memory.store("database", HashMap::new());
        let mut restored: SemanticMemory =
            serde_json::from_str(&serde_json::to_string(&memory).unwrap()).unwrap();

        restored.set_embedding_fn(Some(embed));
        assert_eq!(restored.search("rust", 1)[0].0.content, "rust");
    }

    #[test]
    fn legacy_snapshots_without_cached_embeddings_still_search() {
        let mut memory = SemanticMemory::new(16, None);
        memory.store("recovery token", HashMap::new());

        let mut legacy = serde_json::to_value(&memory).unwrap();
        legacy.as_object_mut().unwrap().remove("embeddings");
        let restored: SemanticMemory = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.search("recovery", 1)[0].0.content, "recovery token");
    }

    #[test]
    fn deletions_keep_cached_embeddings_aligned_with_documents() {
        let mut memory = SemanticMemory::new(32, None);
        let old = memory.store("obsolete record", HashMap::new());
        memory.store("remaining record", HashMap::new());
        assert!(memory.delete(&old));
        assert_eq!(memory.len(), 1);
        assert_eq!(memory.search("remaining", 1)[0].0.content, "remaining record");
    }

    #[test]
    fn restored_custom_embeddings_fail_closed_until_provider_attached() {
        fn custom(text: &str) -> Vec<f32> {
            if text.contains("rust") {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            }
        }
        let mut memory = SemanticMemory::new(2, Some(custom));
        memory.store("rust fact", HashMap::new());
        memory.store("database fact", HashMap::new());

        let serialized = serde_json::to_string(&memory).unwrap();
        let mut restored: SemanticMemory = serde_json::from_str(&serialized).unwrap();

        assert_eq!(
            restored.search_checked("rust", 1).unwrap_err(),
            SemanticMemoryError::MissingEmbeddingProvider
        );
        assert!(restored.search("rust", 1).is_empty());

        restored.set_embedding_fn(Some(custom));
        assert_eq!(restored.search_checked("rust", 1).unwrap()[0].0.content, "rust fact");
    }

    #[test]
    fn custom_memory_after_restore_does_not_generate_mixed_vectors() {
        fn custom(text: &str) -> Vec<f32> {
            if text.contains("rust") { vec![1.0, 0.0] } else { vec![0.0, 1.0] }
        }
        let mut memory = SemanticMemory::new(2, Some(custom));
        memory.store("rust", HashMap::new());

        let mut restored: SemanticMemory =
            serde_json::from_str(&serde_json::to_string(&memory).unwrap()).unwrap();
        restored.store("database", HashMap::new());
        assert_eq!(
            restored.search_checked("rust", 1).unwrap_err(),
            SemanticMemoryError::MissingEmbeddingProvider
        );
        restored.set_embedding_fn(Some(custom));
        assert_eq!(restored.search_checked("database", 1).unwrap()[0].0.content, "database");
    }

    #[test]
    fn checked_search_rejects_wrong_embedding_dimensions() {
        fn bad_provider(_: &str) -> Vec<f32> { vec![1.0] }
        let mut memory = SemanticMemory::new(2, Some(bad_provider));
        memory.store("sample", HashMap::new());
        assert_eq!(
            memory.search_checked("sample", 1).unwrap_err(),
            SemanticMemoryError::EmbeddingDimensionMismatch {
                expected: 2,
                actual: 1,
            }
        );
        assert!(memory.search("sample", 1).is_empty());
    }

    #[test]
    fn checked_search_rejects_corrupted_cached_embedding_dimensions() {
        let mut memory = SemanticMemory::new(2, None);
        memory.store("sample", HashMap::new());
        let mut value = serde_json::to_value(&memory).unwrap();
        value["embeddings"][0] = serde_json::json!([1.0]);
        let memory: SemanticMemory = serde_json::from_value(value).unwrap();
        assert_eq!(
            memory.search_checked("sample", 1).unwrap_err(),
            SemanticMemoryError::EmbeddingDimensionMismatch {
                expected: 2,
                actual: 1,
            }
        );
    }

    #[test]
    fn checked_search_rejects_nonfinite_custom_embeddings() {
        fn nonfinite(_: &str) -> Vec<f32> { vec![f32::NAN, 0.0] }
        let mut memory = SemanticMemory::new(2, Some(nonfinite));
        memory.store("sample", HashMap::new());
        assert_eq!(
            memory.search_checked("sample", 1).unwrap_err(),
            SemanticMemoryError::InvalidEmbeddingValue
        );
    }

}
