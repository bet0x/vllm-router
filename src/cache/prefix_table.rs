//! Shared prefix routing table backed by Redis.
//!
//! Supplements the local in-memory radix tree in `cache_aware` policy.
//! When multiple router instances share a Redis, they share knowledge of
//! which worker has which prompt prefix cached.
//!
//! - Reads on local tree miss (fast-path optimization)
//! - Writes are async and probabilistic (configurable)
//! - TTL-based eviction (no manual cleanup)

use std::time::Duration;
#[cfg(feature = "redis-cache")]
use tracing::{debug, warn};

/// Compute a prefix hash from the first N characters of the request text.
pub fn prefix_hash(text: &str, prefix_chars: usize) -> u64 {
    let prefix: String = text.chars().take(prefix_chars).collect();
    // FNV-1a 64-bit
    let mut hash: u64 = 14695981039346656037;
    for byte in prefix.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

/// In-memory shared prefix table (for single-instance deployments).
/// Provides the same API as Redis but without cross-instance sharing.
#[derive(Debug)]
pub struct MemoryPrefixTable {
    store: dashmap::DashMap<String, (String, std::time::Instant)>,
    ttl: Duration,
    prefix_chars: usize,
}

impl MemoryPrefixTable {
    pub fn new(prefix_chars: usize, ttl_secs: u64) -> Self {
        Self {
            store: dashmap::DashMap::new(),
            ttl: Duration::from_secs(ttl_secs),
            prefix_chars,
        }
    }

    pub fn get(&self, model: &str, text: &str) -> Option<String> {
        let key = self.make_key(model, text);
        if let Some(entry) = self.store.get(&key) {
            if entry.1.elapsed() < self.ttl {
                return Some(entry.0.clone());
            }
            drop(entry);
            self.store.remove(&key);
        }
        None
    }

    pub fn insert(&self, model: &str, text: &str, worker_url: &str) {
        let key = self.make_key(model, text);
        self.store.insert(key, (worker_url.to_string(), std::time::Instant::now()));
    }

    fn make_key(&self, model: &str, text: &str) -> String {
        format!("{}:{:016x}", model, prefix_hash(text, self.prefix_chars))
    }

    pub fn len(&self) -> usize {
        self.store.len()
    }
}

/// Redis-backed shared prefix routing table.
#[cfg(feature = "redis-cache")]
pub mod redis_backend {
    use super::*;
    use deadpool_redis::{Config as PoolConfig, Pool, Runtime};
    use ::redis::AsyncCommands;

    #[derive(Debug)]
    pub struct RedisPrefixTable {
        pool: Pool,
        key_prefix: String,
        ttl_secs: u64,
        prefix_chars: usize,
        command_timeout: Duration,
    }

    impl RedisPrefixTable {
        pub fn new(
            redis_config: &crate::config::types::RedisCacheConfig,
            prefix_chars: usize,
            ttl_secs: u64,
        ) -> Result<Self, String> {
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
                key_prefix: format!("{}routing:", redis_config.key_prefix),
                ttl_secs,
                prefix_chars,
                command_timeout: Duration::from_millis(redis_config.command_timeout_ms),
            })
        }

        fn make_key(&self, model: &str, text: &str) -> String {
            format!("{}{}:{:016x}", self.key_prefix, model, prefix_hash(text, self.prefix_chars))
        }

        pub async fn get(&self, model: &str, text: &str) -> Option<String> {
            let key = self.make_key(model, text);
            tokio::time::timeout(self.command_timeout, async {
                let mut conn = self.pool.get().await.map_err(|e| {
                    warn!(error = %e, "prefix table: Redis pool error");
                    e
                }).ok()?;
                let url: Option<String> = conn.get(&key).await.ok()?;
                debug!(key = key.as_str(), hit = url.is_some(), "prefix table lookup");
                url
            })
            .await
            .unwrap_or(None)
        }

        pub async fn insert(&self, model: &str, text: &str, worker_url: &str) {
            let key = self.make_key(model, text);
            let ttl = self.ttl_secs;
            let _ = tokio::time::timeout(self.command_timeout, async {
                let mut conn = match self.pool.get().await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(error = %e, "prefix table: Redis pool error on insert");
                        return;
                    }
                };
                let result: Result<(), ::redis::RedisError> =
                    conn.set_ex(&key, worker_url, ttl).await;
                if let Err(e) = result {
                    warn!(error = %e, "prefix table: Redis SET error");
                }
            })
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prefix_hash_deterministic() {
        let h1 = prefix_hash("Hello world", 256);
        let h2 = prefix_hash("Hello world", 256);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_prefix_hash_different_inputs() {
        let h1 = prefix_hash("Hello world", 256);
        let h2 = prefix_hash("Goodbye world", 256);
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_prefix_hash_truncates() {
        let h1 = prefix_hash("Hello world, this is a long text", 5);
        let h2 = prefix_hash("Hello different ending", 5);
        assert_eq!(h1, h2, "same first 5 chars should produce same hash");
    }

    #[test]
    fn test_memory_prefix_table() {
        let table = MemoryPrefixTable::new(256, 3600);
        assert!(table.get("model", "prompt").is_none());
        table.insert("model", "prompt", "http://worker1:8000");
        assert_eq!(table.get("model", "prompt"), Some("http://worker1:8000".to_string()));
    }

    #[test]
    fn test_memory_prefix_table_different_models() {
        let table = MemoryPrefixTable::new(256, 3600);
        table.insert("model-a", "prompt", "http://worker1:8000");
        table.insert("model-b", "prompt", "http://worker2:8000");
        assert_eq!(table.get("model-a", "prompt"), Some("http://worker1:8000".to_string()));
        assert_eq!(table.get("model-b", "prompt"), Some("http://worker2:8000".to_string()));
    }
}
