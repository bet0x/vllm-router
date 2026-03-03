//! Async cache traits for pluggable response cache backends.
//!
//! Both the in-memory and Redis backends implement these traits so the router
//! can use them interchangeably via trait objects.

use async_trait::async_trait;
use bytes::Bytes;

/// Async exact-match response cache.
///
/// Implementations store full HTTP response bodies keyed by a 64-bit FNV-1a
/// hash of the canonical request JSON.
#[async_trait]
pub trait ExactMatchCache: Send + Sync + std::fmt::Debug {
    /// Retrieve a cached response by key.
    /// Returns `None` if absent or expired.
    async fn get(&self, key: u64) -> Option<(Bytes, Option<String>)>;

    /// Store a response body with optional content-type.
    async fn insert(&self, key: u64, body: Bytes, content_type: Option<String>);

    /// Number of entries currently in the cache.
    async fn len(&self) -> usize;
}

/// Async semantic similarity cache backend.
///
/// Implementations search stored embeddings for the nearest match (cosine
/// similarity ≥ threshold) and return the associated response.
#[async_trait]
pub trait SemanticCacheBackend: Send + Sync + std::fmt::Debug {
    /// Find the best-matching cached response for `query` embedding.
    /// Returns `None` if no entry meets the similarity threshold.
    async fn find_similar(&self, query: &[f32]) -> Option<(Bytes, Option<String>)>;

    /// Store a new embedding→response mapping.
    async fn insert(&self, embedding: Vec<f32>, body: Bytes, content_type: Option<String>);

    /// The cosine similarity threshold for this cache instance.
    fn threshold(&self) -> f32;
}
