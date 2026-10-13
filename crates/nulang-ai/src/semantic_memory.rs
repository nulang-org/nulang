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

/// Long-term semantic memory backed by a brute-force vector search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticMemory {
    /// Stored documents.
    pub documents: Vec<Document>,
    /// Dimensionality of the embedding vectors.
    pub dimensions: usize,
    /// Cached document embeddings, aligned by index with `documents`.
    ///
    /// Older serialized memories omit this field; searches remain compatible
    /// and the cache is rebuilt on the next mutation.
    #[serde(default)]
    pub embeddings: Vec<Vec<f32>>,
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
            embedding_fn,
        }
    }

    /// Store a fact and return its deterministic document id.
    pub fn store(
        &mut self,
        content: impl Into<String>,
        metadata: HashMap<String, String>,
    ) -> String {
        let content = content.into();
        if self.embeddings.len() != self.documents.len() {
            self.rebuild_embeddings();
        }
        let embedding = self.embed(&content);
        let id = deterministic_id(&content);
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
        if top_k == 0 || self.documents.is_empty() {
            return Vec::new();
        }

        let query_embedding = self.embed(query);
        let mut results: Vec<(&Document, f32)> = self
            .documents
            .iter()
            .enumerate()
            .map(|(index, doc)| {
                let score = match self.embeddings.get(index) {
                    Some(embedding) if embedding.len() == self.dimensions => {
                        cosine_similarity(&query_embedding, embedding)
                    }
                    _ => {
                        // Backward-compatible path for memories serialized
                        // before embedding caching was introduced.
                        let embedding = self.embed(&doc.content);
                        cosine_similarity(&query_embedding, &embedding)
                    }
                };
                (doc, score)
            })
            .collect();

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        results
    }

    /// Remove the document with the given id.
    ///
    /// Returns `true` if a document was removed.
    pub fn delete(&mut self, id: &str) -> bool {
        if let Some(pos) = self.documents.iter().position(|doc| doc.id == id) {
            self.documents.remove(pos);
            if pos < self.embeddings.len() {
                self.embeddings.remove(pos);
            }
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

    /// Recompute cached embeddings for all documents.
    ///
    /// This is useful after loading an older serialized memory that predates
    /// the embedding cache or after changing the embedding function.
    pub fn rebuild_embeddings(&mut self) {
        self.embeddings = self
            .documents
            .iter()
            .map(|document| self.embed(&document.content))
            .collect();
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
    fn test_semantic_memory_caches_document_embeddings() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static EMBED_CALLS: AtomicUsize = AtomicUsize::new(0);
        fn counting_embed(input: &str) -> Vec<f32> {
            EMBED_CALLS.fetch_add(1, Ordering::SeqCst);
            let mut out = vec![0.0; 4];
            for (i, byte) in input.bytes().enumerate() {
                out[i % 4] += byte as f32;
            }
            l2_normalize(&mut out);
            out
        }

        EMBED_CALLS.store(0, Ordering::SeqCst);
        let mut memory = SemanticMemory::new(4, Some(counting_embed));
        memory.store("cached document", HashMap::new());
        assert_eq!(EMBED_CALLS.load(Ordering::SeqCst), 1);

        let _ = memory.search("query", 1);
        assert_eq!(
            EMBED_CALLS.load(Ordering::SeqCst),
            2,
            "search should embed only the query, not every stored document"
        );
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
        assert_eq!(restored.embeddings.len(), 2);
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
}
