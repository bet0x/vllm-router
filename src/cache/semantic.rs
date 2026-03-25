//! Semantic similarity response cache (T-12).
//!
//! Complements the exact-match [`super::ResponseCache`] by finding semantically
//! similar previous requests using embedding vectors.  When an incoming request
//! misses the exact-match cache, the router can compute its embedding (via an
//! external `/v1/embeddings` endpoint), search for the nearest cached embedding
//! using cosine similarity, and return the stored response if the similarity
//! exceeds a configurable threshold.
//!
//! # Thread safety
//! All mutations are protected by a [`parking_lot::RwLock`]-guarded `Vec`.
//! Reads (similarity search) take a shared lock; writes take an exclusive lock.

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::RwLock;
use std::time::{Duration, Instant};

use crate::cache::traits::SemanticCacheBackend;

/// A single entry stored in the semantic cache.
#[derive(Debug)]
struct SemanticEntry {
    embedding: Vec<f32>,
    body: Bytes,
    content_type: Option<String>,
    created_at: Instant,
}

/// In-memory semantic similarity cache backed by brute-force cosine search.
#[derive(Debug)]
///
/// For workloads with a small number of distinct queries (< a few thousand)
/// O(n) linear scan over stored embeddings is fast enough.  Entries expire
/// after the configured TTL and are lazily removed during insertion.
pub struct SemanticCache {
    entries: RwLock<Vec<SemanticEntry>>,
    ttl: Duration,
    max_entries: usize,
    /// Minimum cosine similarity required to declare a cache hit (0.0–1.0).
    pub threshold: f32,
}

impl SemanticCache {
    /// Create a new semantic cache.
    ///
    /// # Arguments
    /// * `max_entries` — maximum number of entries before eviction kicks in
    /// * `ttl_secs`    — time-to-live for each entry in seconds
    /// * `threshold`   — cosine similarity threshold for a cache hit (0.0–1.0)
    pub fn new(max_entries: usize, ttl_secs: u64, threshold: f32) -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            ttl: Duration::from_secs(ttl_secs),
            max_entries,
            threshold,
        }
    }

    /// Cosine similarity between two equal-length vectors.
    ///
    /// Returns `0.0` if either vector is empty or has zero magnitude.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot / (norm_a * norm_b)
        }
    }

    /// Find the most similar non-expired entry whose cosine similarity to
    /// `query` is ≥ `self.threshold`.
    ///
    /// Returns `None` if no matching entry exists.
    pub fn find_similar(&self, query: &[f32]) -> Option<(Bytes, Option<String>, f32)> {
        let entries = self.entries.read();
        let now = Instant::now();
        let mut best_sim = self.threshold;
        let mut best: Option<(Bytes, Option<String>, f32)> = None;

        for entry in entries.iter() {
            if now.duration_since(entry.created_at) >= self.ttl {
                continue; // expired — skip (will be cleaned on next insert)
            }
            let sim = Self::cosine_similarity(query, &entry.embedding);
            if sim >= best_sim {
                best_sim = sim;
                best = Some((entry.body.clone(), entry.content_type.clone(), sim));
            }
        }
        best
    }

    /// Store a new embedding → response mapping.
    ///
    /// If the cache is at capacity, expired entries are evicted first.
    /// If still at capacity after eviction, the oldest ~10 % are dropped.
    pub fn insert(&self, embedding: Vec<f32>, body: Bytes, content_type: Option<String>) {
        let mut entries = self.entries.write();
        if entries.len() >= self.max_entries {
            let ttl = self.ttl;
            let now = Instant::now();
            entries.retain(|e| now.duration_since(e.created_at) < ttl);
            // If still at capacity after TTL eviction, drop oldest ~10 %.
            if entries.len() >= self.max_entries {
                let to_drop = (self.max_entries / 10).max(1);
                let len = entries.len();
                entries.drain(0..to_drop.min(len));
            }
        }
        entries.push(SemanticEntry {
            embedding,
            body,
            content_type,
            created_at: Instant::now(),
        });
    }

    /// Remove all expired entries.  Can be called periodically in a background
    /// task to free memory without waiting for the next insertion.
    pub fn evict_expired(&self) {
        let mut entries = self.entries.write();
        let ttl = self.ttl;
        let now = Instant::now();
        entries.retain(|e| now.duration_since(e.created_at) < ttl);
    }

    /// Number of entries currently in the cache (including expired ones not yet
    /// evicted).
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Returns `true` if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }
}

#[async_trait]
impl SemanticCacheBackend for SemanticCache {
    async fn find_similar(&self, query: &[f32]) -> Option<(Bytes, Option<String>, f32)> {
        self.find_similar(query)
    }

    async fn insert(&self, embedding: Vec<f32>, body: Bytes, content_type: Option<String>) {
        self.insert(embedding, body, content_type);
    }

    async fn len(&self) -> usize {
        self.entries.read().len()
    }

    fn threshold(&self) -> f32 {
        self.threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cache(threshold: f32) -> SemanticCache {
        SemanticCache::new(64, 3600, threshold)
    }

    #[test]
    fn test_empty_cache_returns_none() {
        let c = make_cache(0.9);
        assert!(c.find_similar(&[1.0, 0.0]).is_none());
    }

    #[test]
    fn test_exact_match_hit() {
        let c = make_cache(0.9);
        let emb = vec![1.0_f32, 0.0, 0.0];
        let body = Bytes::from_static(b"response");
        c.insert(emb.clone(), body.clone(), None);
        let (got, _ct, score) = c.find_similar(&emb).expect("should hit identical embedding");
        assert_eq!(got, body);
        assert!((score - 1.0).abs() < 1e-6, "identical embedding should have score ~1.0");
    }

    #[test]
    fn test_similar_hit_above_threshold() {
        let c = make_cache(0.9);
        let stored = vec![1.0_f32, 0.1, 0.0];
        let query = vec![1.0_f32, 0.05, 0.0];
        c.insert(stored, Bytes::from_static(b"x"), Some("application/json".into()));
        // Cosine similarity of near-identical unit vectors should be > 0.9
        assert!(c.find_similar(&query).is_some());
    }

    #[test]
    fn test_orthogonal_miss() {
        let c = make_cache(0.9);
        c.insert(
            vec![1.0, 0.0],
            Bytes::from_static(b"x"),
            None,
        );
        // Orthogonal vector → similarity = 0.0 → miss
        assert!(c.find_similar(&[0.0, 1.0]).is_none());
    }

    #[test]
    fn test_cosine_similarity_basics() {
        // Identical unit vectors → 1.0
        assert!((SemanticCache::cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        // Orthogonal → 0.0
        assert!((SemanticCache::cosine_similarity(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
        // Opposite → -1.0
        assert!(
            (SemanticCache::cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6
        );
        // Zero vector → 0.0
        assert!((SemanticCache::cosine_similarity(&[0.0, 0.0], &[1.0, 0.0])).abs() < 1e-6);
        // Length mismatch → 0.0
        assert!((SemanticCache::cosine_similarity(&[1.0], &[1.0, 0.0])).abs() < 1e-6);
    }

    #[test]
    fn test_ttl_expiry() {
        // TTL = 0s → every entry is immediately expired
        let c = SemanticCache::new(64, 0, 0.5);
        c.insert(vec![1.0, 0.0], Bytes::from_static(b"x"), None);
        assert!(c.find_similar(&[1.0, 0.0]).is_none());
    }

    #[test]
    fn test_capacity_eviction() {
        let c = SemanticCache::new(5, 3600, 0.0);
        for i in 0..8u32 {
            let emb = vec![i as f32, 0.0];
            c.insert(emb, Bytes::from_static(b"x"), None);
        }
        assert!(c.len() <= 5);
    }

    #[test]
    fn test_returns_best_match() {
        let c = make_cache(0.5);
        // Insert two entries: one closer, one farther from [1,0]
        c.insert(vec![0.9_f32, 0.436], Bytes::from_static(b"far"), None);
        c.insert(vec![1.0_f32, 0.01], Bytes::from_static(b"close"), None);
        let (body, _, score) = c.find_similar(&[1.0, 0.0]).expect("should find a match");
        assert_eq!(body, Bytes::from_static(b"close"));
        assert!(score > 0.5, "best match score should exceed threshold");
    }

    #[test]
    fn test_evict_expired() {
        let c = SemanticCache::new(64, 0, 0.5);
        c.insert(vec![1.0, 0.0], Bytes::from_static(b"x"), None);
        assert_eq!(c.len(), 1);
        c.evict_expired();
        assert_eq!(c.len(), 0);
    }
}
