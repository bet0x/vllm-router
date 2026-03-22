//! Token ID cache for LMCache prefix lookup optimization.
//!
//! Caches the result of `POST /tokenize` (token IDs) keyed by the canonical
//! hash of the request body. Eliminates redundant tokenization HTTP calls for
//! repeated system prompts and few-shot examples.

use dashmap::DashMap;
use std::time::{Duration, Instant};
#[cfg(feature = "redis-cache")]
use tracing::warn;

/// Cached token IDs for a request body.
struct TokenEntry {
    tokens: Vec<i64>,
    created_at: Instant,
}

/// In-memory token ID cache with TTL and LRU eviction.
#[derive(Debug)]
pub struct TokenCache {
    store: DashMap<u64, TokenEntry>,
    ttl: Duration,
    max_entries: usize,
}

// DashMap entries aren't Debug
impl std::fmt::Debug for TokenEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenEntry")
            .field("token_count", &self.tokens.len())
            .finish()
    }
}

impl TokenCache {
    pub fn new(max_entries: usize, ttl_secs: u64) -> Self {
        Self {
            store: DashMap::new(),
            ttl: Duration::from_secs(ttl_secs),
            max_entries,
        }
    }

    /// Compute a cache key from a request body (same canonicalization as response cache).
    pub fn compute_key(body: &serde_json::Value) -> u64 {
        crate::cache::ResponseCache::compute_key(body)
    }

    /// Get cached token IDs for a request body hash.
    pub fn get(&self, key: u64) -> Option<Vec<i64>> {
        if let Some(entry) = self.store.get(&key) {
            if entry.created_at.elapsed() < self.ttl {
                return Some(entry.tokens.clone());
            }
            drop(entry);
            self.store.remove(&key);
        }
        None
    }

    /// Store token IDs in the cache.
    pub fn insert(&self, key: u64, tokens: Vec<i64>) {
        if self.store.len() >= self.max_entries {
            // Evict expired entries first
            let ttl = self.ttl;
            self.store.retain(|_, v| v.created_at.elapsed() < ttl);
            // If still full, drop ~10% oldest
            if self.store.len() >= self.max_entries {
                let to_drop = (self.max_entries / 10).max(1);
                let keys: Vec<u64> = self.store.iter().take(to_drop).map(|e| *e.key()).collect();
                for k in keys {
                    self.store.remove(&k);
                }
            }
        }
        self.store.insert(key, TokenEntry {
            tokens,
            created_at: Instant::now(),
        });
    }

    pub fn len(&self) -> usize {
        self.store.len()
    }
}

/// Redis-backed token ID cache.
#[cfg(feature = "redis-cache")]
pub mod redis_backend {
    use super::*;
    use deadpool_redis::{Config as PoolConfig, Pool, Runtime};
    use ::redis::AsyncCommands;

    #[derive(Debug)]
    pub struct RedisTokenCache {
        pool: Pool,
        key_prefix: String,
        ttl_secs: u64,
        command_timeout: Duration,
    }

    impl RedisTokenCache {
        pub fn new(redis_config: &crate::config::types::RedisCacheConfig, ttl_secs: u64) -> Result<Self, String> {
            let pool_config = PoolConfig::from_url(&redis_config.url);
            let pool = pool_config
                .builder()
                .map_err(|e| format!("Redis pool config error: {}", e))?
                .max_size(redis_config.pool_size)
                .wait_timeout(Some(Duration::from_millis(redis_config.connection_timeout_ms)))
                .runtime(Runtime::Tokio1)
                .build()
                .map_err(|e| format!("Redis pool error: {}", e))?;

            Ok(Self {
                pool,
                key_prefix: format!("{}tokens:", redis_config.key_prefix),
                ttl_secs,
                command_timeout: Duration::from_millis(redis_config.command_timeout_ms),
            })
        }

        fn make_key(&self, key: u64) -> String {
            format!("{}{:016x}", self.key_prefix, key)
        }

        pub async fn get(&self, key: u64) -> Option<Vec<i64>> {
            let redis_key = self.make_key(key);
            tokio::time::timeout(self.command_timeout, async {
                let mut conn = self.pool.get().await.map_err(|e| {
                    warn!(error = %e, "token cache: Redis pool error");
                    e
                }).ok()?;
                let raw: Option<Vec<u8>> = conn.get(&redis_key).await.ok()?;
                let raw = raw?;
                rmp_serde::from_slice(&raw).map_err(|e| {
                    warn!(error = %e, "token cache: deserialize error");
                    e
                }).ok()
            })
            .await
            .unwrap_or(None)
        }

        pub async fn insert(&self, key: u64, tokens: &[i64]) {
            let redis_key = self.make_key(key);
            let encoded = match rmp_serde::to_vec(tokens) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "token cache: serialize error");
                    return;
                }
            };
            let ttl = self.ttl_secs;
            let _ = tokio::time::timeout(self.command_timeout, async {
                let mut conn = match self.pool.get().await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(error = %e, "token cache: Redis pool error on insert");
                        return;
                    }
                };
                let result: Result<(), ::redis::RedisError> = conn.set_ex(&redis_key, encoded, ttl).await;
                if let Err(e) = result {
                    warn!(error = %e, "token cache: Redis SET error");
                }
            })
            .await;
        }
    }
}
