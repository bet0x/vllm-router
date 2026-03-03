//! Redis-backed response cache backends.
//!
//! Provides [`RedisExactCache`] and [`RedisSemanticCache`] that implement the
//! [`ExactMatchCache`] and [`SemanticCacheBackend`] traits respectively using
//! Redis as the backing store via `deadpool-redis`.
//!
//! ## Graceful degradation
//!
//! All Redis operations are wrapped in timeouts. On error or timeout the cache
//! returns `None` (get) or silently skips the write (insert) and logs a warning.
//! The router continues functioning without cache — Redis failures never block
//! or fail a request.

use async_trait::async_trait;
use bytes::Bytes;
use deadpool_redis::{Config as PoolConfig, Pool, Runtime};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::warn;

use crate::cache::traits::{ExactMatchCache, SemanticCacheBackend};
use crate::config::types::RedisCacheConfig;

/// MessagePack-encoded value for exact-match cache entries.
#[derive(Serialize, Deserialize)]
struct CachedResponse {
    body: Vec<u8>,
    content_type: Option<String>,
}

/// MessagePack-encoded value for semantic cache entries.
#[derive(Serialize, Deserialize)]
struct SemanticEntry {
    embedding: Vec<f32>,
    body: Vec<u8>,
    content_type: Option<String>,
}

// ---------------------------------------------------------------------------
// RedisExactCache
// ---------------------------------------------------------------------------

/// Redis-backed exact-match response cache.
///
/// Keys: `{prefix}exact:{hex_key}` → MessagePack-encoded `CachedResponse`
/// TTL is set via Redis EXPIRE on each insert.
#[derive(Debug)]
pub struct RedisExactCache {
    pool: Pool,
    key_prefix: String,
    ttl_secs: u64,
    max_entries: usize,
    command_timeout: Duration,
}

impl RedisExactCache {
    /// Create a new Redis exact-match cache.
    pub fn new(
        redis_config: &RedisCacheConfig,
        max_entries: usize,
        ttl_secs: u64,
    ) -> Result<Self, String> {
        let pool = create_pool(redis_config)?;
        Ok(Self {
            pool,
            key_prefix: redis_config.key_prefix.clone(),
            ttl_secs,
            max_entries,
            command_timeout: Duration::from_millis(redis_config.command_timeout_ms),
        })
    }

    fn make_key(&self, key: u64) -> String {
        format!("{}exact:{:016x}", self.key_prefix, key)
    }
}

#[async_trait]
impl ExactMatchCache for RedisExactCache {
    async fn get(&self, key: u64) -> Option<(Bytes, Option<String>)> {
        let redis_key = self.make_key(key);
        let result: Option<(Bytes, Option<String>)> =
            tokio::time::timeout(self.command_timeout, async {
                let mut conn = self.pool.get().await.map_err(|e| {
                    warn!(error = %e, "redis exact cache: pool error on get");
                    e
                }).ok()?;
                let raw: Option<Vec<u8>> = conn.get(&redis_key).await.map_err(|e| {
                    warn!(error = %e, key = %redis_key, "redis exact cache: GET failed");
                    e
                }).ok()?;
                let raw = raw?;
                let entry: CachedResponse = rmp_serde::from_slice(&raw).map_err(|e| {
                    warn!(error = %e, "redis exact cache: deserialize failed");
                    e
                }).ok()?;
                Some((Bytes::from(entry.body), entry.content_type))
            })
            .await
            .unwrap_or(None);
        result
    }

    async fn insert(&self, key: u64, body: Bytes, content_type: Option<String>) {
        let redis_key = self.make_key(key);
        let entry = CachedResponse {
            body: body.to_vec(),
            content_type,
        };
        let encoded = match rmp_serde::to_vec(&entry) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "redis exact cache: serialize failed");
                return;
            }
        };

        let ttl = self.ttl_secs;
        let _ = tokio::time::timeout(self.command_timeout, async {
            let mut conn = match self.pool.get().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "redis exact cache: pool error on insert");
                    return;
                }
            };
            let result: Result<(), redis::RedisError> =
                conn.set_ex(&redis_key, encoded, ttl).await;
            if let Err(e) = result {
                warn!(error = %e, key = %redis_key, "redis exact cache: SETEX failed");
            }
        })
        .await;
    }

    async fn len(&self) -> usize {
        // Counting keys in Redis is expensive; return 0 as an approximation.
        // The router only uses len() for logging/metrics, not correctness.
        let result = tokio::time::timeout(self.command_timeout, async {
            let mut conn = match self.pool.get().await {
                Ok(c) => c,
                Err(_) => return 0usize,
            };
            let pattern = format!("{}exact:*", self.key_prefix);
            let keys: Vec<String> = redis::cmd("KEYS")
                .arg(&pattern)
                .query_async(&mut *conn)
                .await
                .unwrap_or_default();
            keys.len().min(self.max_entries)
        })
        .await;
        result.unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// RedisSemanticCache
// ---------------------------------------------------------------------------

/// Redis-backed semantic similarity cache.
///
/// Each entry is stored as: `{prefix}sem:entry:{index}` → MessagePack-encoded
/// `SemanticEntry`.  An index counter at `{prefix}sem:count` tracks the next
/// entry ID.
///
/// Cosine similarity is computed client-side by fetching all entries. This is
/// acceptable because the semantic cache is small by design (max ~256 entries).
#[derive(Debug)]
pub struct RedisSemanticCache {
    pool: Pool,
    key_prefix: String,
    ttl_secs: u64,
    max_entries: usize,
    threshold: f32,
    command_timeout: Duration,
}

impl RedisSemanticCache {
    /// Create a new Redis semantic cache.
    pub fn new(
        redis_config: &RedisCacheConfig,
        max_entries: usize,
        ttl_secs: u64,
        threshold: f32,
    ) -> Result<Self, String> {
        let pool = create_pool(redis_config)?;
        Ok(Self {
            pool,
            key_prefix: redis_config.key_prefix.clone(),
            ttl_secs,
            max_entries,
            threshold,
            command_timeout: Duration::from_millis(redis_config.command_timeout_ms),
        })
    }

    fn entry_key(&self, index: u64) -> String {
        format!("{}sem:entry:{}", self.key_prefix, index)
    }

    fn counter_key(&self) -> String {
        format!("{}sem:count", self.key_prefix)
    }

    /// Cosine similarity between two equal-length vectors.
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
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
}

#[async_trait]
impl SemanticCacheBackend for RedisSemanticCache {
    async fn find_similar(&self, query: &[f32]) -> Option<(Bytes, Option<String>)> {
        let threshold = self.threshold;
        let result: Option<(Bytes, Option<String>)> =
            tokio::time::timeout(self.command_timeout, async {
                let mut conn = self.pool.get().await.map_err(|e| {
                    warn!(error = %e, "redis semantic cache: pool error on find_similar");
                    e
                }).ok()?;

                // Get the current entry count
                let count: u64 = conn.get(self.counter_key()).await.unwrap_or(0);
                if count == 0 {
                    return None;
                }

                // Scan entries for the best match
                let mut best_sim = threshold;
                let mut best: Option<(Bytes, Option<String>)> = None;

                // We only scan up to max_entries most recent entries
                let start = count.saturating_sub(self.max_entries as u64);
                for i in start..count {
                    let key = self.entry_key(i);
                    let raw: Option<Vec<u8>> = conn.get(&key).await.ok()?;
                    let raw = match raw {
                        Some(r) => r,
                        None => continue, // expired or deleted
                    };
                    let entry: SemanticEntry = match rmp_serde::from_slice(&raw) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let sim = Self::cosine_similarity(query, &entry.embedding);
                    if sim >= best_sim {
                        best_sim = sim;
                        best = Some((Bytes::from(entry.body), entry.content_type));
                    }
                }
                best
            })
            .await
            .unwrap_or(None);
        result
    }

    async fn insert(&self, embedding: Vec<f32>, body: Bytes, content_type: Option<String>) {
        let entry = SemanticEntry {
            embedding,
            body: body.to_vec(),
            content_type,
        };
        let encoded = match rmp_serde::to_vec(&entry) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "redis semantic cache: serialize failed");
                return;
            }
        };

        let ttl = self.ttl_secs;
        let _ = tokio::time::timeout(self.command_timeout, async {
            let mut conn = match self.pool.get().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "redis semantic cache: pool error on insert");
                    return;
                }
            };

            // Atomically increment the entry counter
            let index: u64 = match conn.incr(self.counter_key(), 1i64).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "redis semantic cache: INCR failed");
                    return;
                }
            };
            // Counter returns value after increment, so entry index is (value - 1)
            let entry_index = index - 1;
            let key = self.entry_key(entry_index);

            let result: Result<(), redis::RedisError> =
                conn.set_ex(&key, encoded, ttl).await;
            if let Err(e) = result {
                warn!(error = %e, key = %key, "redis semantic cache: SETEX failed");
            }
        })
        .await;
    }

    fn threshold(&self) -> f32 {
        self.threshold
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a `deadpool-redis` connection pool from config.
fn create_pool(config: &RedisCacheConfig) -> Result<Pool, String> {
    let pool_config = PoolConfig::from_url(&config.url);
    let pool = pool_config
        .builder()
        .map_err(|e| format!("Failed to build Redis pool config: {}", e))?
        .max_size(config.pool_size)
        .wait_timeout(Some(Duration::from_millis(config.connection_timeout_ms)))
        .runtime(Runtime::Tokio1)
        .build()
        .map_err(|e| format!("Failed to create Redis pool: {}", e))?;
    Ok(pool)
}
