//! Response caching subsystem.
//!
//! * [`ResponseCache`] — fast exact-match cache (FNV-1a keyed by canonical JSON).
//! * [`semantic::SemanticCache`] — semantic similarity cache (cosine search over
//!   embeddings).  Enabled when an embeddings endpoint is configured.
//!
//! Caches complete response bodies keyed by a canonical hash of the request body.
//! Only non-streaming requests are cached. Entries expire after a configurable TTL.
//! When the store exceeds `max_entries`, expired entries are evicted first; if still
//! over capacity, the oldest entries are removed (approximate LRU).

pub mod prefix_table;
pub mod semantic;
pub mod token_cache;
pub mod traits;

#[cfg(feature = "redis-cache")]
pub mod redis_backend;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use std::time::{Duration, Instant};

use crate::cache::traits::ExactMatchCache;

/// A single cached response entry.
#[derive(Debug)]
struct CacheEntry {
    body: Bytes,
    content_type: Option<String>,
    created_at: Instant,
}

/// Exact-match in-memory response cache.
#[derive(Debug)]
pub struct ResponseCache {
    store: DashMap<u64, CacheEntry>,
    ttl: Duration,
    max_entries: usize,
}

impl ResponseCache {
    /// Create a new cache with the given capacity and TTL.
    pub fn new(max_entries: usize, ttl_secs: u64) -> Self {
        Self {
            store: DashMap::new(),
            ttl: Duration::from_secs(ttl_secs),
            max_entries,
        }
    }

    /// Compute a stable 64-bit cache key from a request body JSON value.
    ///
    /// Fields that do not affect the model output are removed before hashing:
    /// `stream`, `user`, `request_id` — so that a streaming and a non-streaming
    /// variant of the same request share the same cache entry.
    pub fn compute_key(body: &serde_json::Value) -> u64 {
        let mut normalized = body.clone();
        if let Some(obj) = normalized.as_object_mut() {
            for field in ["stream", "user", "request_id"] {
                obj.remove(field);
            }
        }
        let canonical = Self::canonical_json(&normalized);
        // FNV-1a 64-bit hash — no external dep needed
        let mut hash: u64 = 14695981039346656037;
        for byte in canonical.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(1099511628211);
        }
        hash
    }

    /// Retrieve a cached response. Returns `None` if the key is absent or expired.
    pub fn get(&self, key: u64) -> Option<(Bytes, Option<String>)> {
        // Fast path: check existence before acquiring write lock
        if let Some(entry) = self.store.get(&key) {
            if entry.created_at.elapsed() < self.ttl {
                return Some((entry.body.clone(), entry.content_type.clone()));
            }
            // Expired — drop the read guard before removing
            drop(entry);
        }
        self.store.remove(&key);
        None
    }

    /// Insert a response into the cache.
    pub fn insert(&self, key: u64, body: Bytes, content_type: Option<String>) {
        if self.store.len() >= self.max_entries {
            self.evict_expired();
            // If still at or above capacity, drop ~10% oldest entries (approximate)
            if self.store.len() >= self.max_entries {
                let to_drop = (self.max_entries / 10).max(1);
                let keys: Vec<u64> = self.store.iter().take(to_drop).map(|e| *e.key()).collect();
                for k in keys {
                    self.store.remove(&k);
                }
            }
        }
        self.store.insert(
            key,
            CacheEntry {
                body,
                content_type,
                created_at: Instant::now(),
            },
        );
    }

    /// Number of entries currently in the cache (including expired ones not yet evicted).
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// Returns true if the cache has no entries.
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    /// Remove all expired entries.
    pub fn evict_expired(&self) {
        let ttl = self.ttl;
        self.store.retain(|_, v| v.created_at.elapsed() < ttl);
    }

    /// Produce a canonical (key-sorted) JSON string for stable hashing.
    fn canonical_json(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Object(map) => {
                let mut pairs: Vec<(&str, String)> = map
                    .iter()
                    .map(|(k, v)| (k.as_str(), Self::canonical_json(v)))
                    .collect();
                pairs.sort_by_key(|(k, _)| *k);
                let inner: Vec<String> = pairs
                    .into_iter()
                    .map(|(k, v)| format!("\"{}\":{}", k, v))
                    .collect();
                format!("{{{}}}", inner.join(","))
            }
            serde_json::Value::Array(arr) => {
                let items: Vec<String> = arr.iter().map(Self::canonical_json).collect();
                format!("[{}]", items.join(","))
            }
            other => other.to_string(),
        }
    }
}

#[async_trait]
impl ExactMatchCache for ResponseCache {
    async fn get(&self, key: u64) -> Option<(Bytes, Option<String>)> {
        self.get(key)
    }

    async fn insert(&self, key: u64, body: Bytes, content_type: Option<String>) {
        self.insert(key, body, content_type);
    }

    async fn len(&self) -> usize {
        self.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_hit_and_miss() {
        let cache = ResponseCache::new(100, 60);
        let body = serde_json::json!({"model": "llama-3", "messages": [{"role": "user", "content": "hi"}]});
        let key = ResponseCache::compute_key(&body);

        assert!(cache.get(key).is_none(), "should miss on empty cache");

        let data = Bytes::from_static(b"{\"id\":\"chatcmpl-1\"}");
        cache.insert(key, data.clone(), Some("application/json".to_string()));

        let (got_body, got_ct) = cache.get(key).expect("should hit after insert");
        assert_eq!(got_body, data);
        assert_eq!(got_ct.as_deref(), Some("application/json"));
    }

    #[test]
    fn test_stream_field_ignored_in_key() {
        let a = serde_json::json!({"model": "m", "messages": [], "stream": false});
        let b = serde_json::json!({"model": "m", "messages": [], "stream": true});
        assert_eq!(ResponseCache::compute_key(&a), ResponseCache::compute_key(&b));
    }

    #[test]
    fn test_user_field_ignored_in_key() {
        let a = serde_json::json!({"model": "m", "messages": [], "user": "alice"});
        let b = serde_json::json!({"model": "m", "messages": []});
        assert_eq!(ResponseCache::compute_key(&a), ResponseCache::compute_key(&b));
    }

    #[test]
    fn test_different_bodies_produce_different_keys() {
        let a = serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "hello"}]});
        let b = serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "world"}]});
        assert_ne!(ResponseCache::compute_key(&a), ResponseCache::compute_key(&b));
    }

    #[test]
    fn test_key_order_independent() {
        // Canonical JSON should produce the same key regardless of field order.
        // serde_json preserves insertion order, so we build them in different orders.
        let a = serde_json::json!({"model": "m", "temperature": 0.0, "messages": []});
        let b = serde_json::json!({"temperature": 0.0, "messages": [], "model": "m"});
        assert_eq!(ResponseCache::compute_key(&a), ResponseCache::compute_key(&b));
    }

    #[test]
    fn test_eviction_on_capacity() {
        let cache = ResponseCache::new(5, 3600);
        for i in 0u64..7 {
            let key = i;
            cache.insert(key, Bytes::from_static(b"x"), None);
        }
        // After inserting 7 into a cache of capacity 5, some entries were evicted.
        assert!(cache.len() <= 5);
    }

    #[test]
    fn test_ttl_expiry() {
        let cache = ResponseCache::new(100, 0); // 0s TTL → expires immediately
        let key = ResponseCache::compute_key(&serde_json::json!({"model": "m"}));
        cache.insert(key, Bytes::from_static(b"x"), None);
        // TTL of 0s means duration 0 → elapsed() > 0 → expired
        assert!(cache.get(key).is_none(), "entry with TTL=0 should expire immediately");
    }
}
