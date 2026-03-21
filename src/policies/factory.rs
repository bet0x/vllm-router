//! Factory and registry for creating load balancing policies.
//!
//! Builtin policies are registered at startup. External code can register
//! additional policies by name before the server starts, making the factory
//! extensible without modifying this file.

use super::{
    CacheAwareConfig, CacheAwarePolicy, ConsistentHashPolicy, LMCacheAwareConfig,
    LMCacheAwarePolicy, LoadBalancingPolicy, PowerOfTwoPolicy, RandomPolicy, RoundRobinPolicy,
};
use crate::config::PolicyConfig;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Type alias for a policy constructor function.
/// Receives no config (for `create_by_name` / dynamic creation).
type PolicyConstructor = Box<dyn Fn() -> Arc<dyn LoadBalancingPolicy> + Send + Sync>;

/// Factory for creating policy instances.
///
/// Maintains a registry of name → constructor mappings. Builtin policies are
/// registered automatically; external policies can be added via [`register`].
pub struct PolicyFactory {
    registry: RwLock<HashMap<String, PolicyConstructor>>,
}

impl PolicyFactory {
    /// Create a new factory with all builtin policies registered.
    pub fn new() -> Self {
        let factory = Self {
            registry: RwLock::new(HashMap::new()),
        };
        factory.register_builtins();
        factory
    }

    /// Register a policy constructor by name.
    ///
    /// Multiple names can map to the same constructor (e.g. `"round_robin"` and
    /// `"roundrobin"`). Names are normalized to lowercase for lookup.
    pub fn register<F>(&self, name: &str, constructor: F)
    where
        F: Fn() -> Arc<dyn LoadBalancingPolicy> + Send + Sync + 'static,
    {
        self.registry
            .write()
            .insert(name.to_lowercase(), Box::new(constructor));
    }

    /// Create a policy by name (for dynamic loading / policy hints from workers).
    ///
    /// Name lookup is case-insensitive.
    pub fn create_by_name(&self, name: &str) -> Option<Arc<dyn LoadBalancingPolicy>> {
        let registry = self.registry.read();
        registry.get(&name.to_lowercase()).map(|ctor| ctor())
    }

    /// Create a policy from a typed [`PolicyConfig`] enum.
    ///
    /// This handles config-specific parameters (thresholds, intervals, etc.)
    /// that the simple name-based constructors don't support.
    pub fn create_from_config(&self, config: &PolicyConfig) -> Arc<dyn LoadBalancingPolicy> {
        match config {
            PolicyConfig::Random => Arc::new(RandomPolicy::new()),
            PolicyConfig::RoundRobin => Arc::new(RoundRobinPolicy::new()),
            PolicyConfig::PowerOfTwo { .. } => Arc::new(PowerOfTwoPolicy::new()),
            PolicyConfig::CacheAware {
                cache_threshold,
                balance_abs_threshold,
                balance_rel_threshold,
                eviction_interval_secs,
                max_tree_size,
            } => {
                let config = CacheAwareConfig {
                    cache_threshold: *cache_threshold,
                    balance_abs_threshold: *balance_abs_threshold,
                    balance_rel_threshold: *balance_rel_threshold,
                    eviction_interval_secs: *eviction_interval_secs,
                    max_tree_size: *max_tree_size,
                };
                Arc::new(CacheAwarePolicy::with_config(config))
            }
            PolicyConfig::ConsistentHash { virtual_nodes: _ } => {
                Arc::new(ConsistentHashPolicy::new())
            }
            PolicyConfig::LMCacheAware {
                controller_url,
                poll_interval_secs,
                cache_weight,
                fallback_policy,
                controller_timeout_ms,
                lookup_mode,
                controller_api_key,
                lmcache_worker_map,
            } => {
                let fallback = self
                    .create_by_name(fallback_policy)
                    .unwrap_or_else(|| Arc::new(PowerOfTwoPolicy::new()));
                let config = LMCacheAwareConfig {
                    controller_url: controller_url.clone(),
                    poll_interval_secs: *poll_interval_secs,
                    cache_weight: *cache_weight,
                    fallback_policy_name: fallback_policy.clone(),
                    controller_timeout_ms: *controller_timeout_ms,
                    lookup_mode: lookup_mode.clone(),
                    controller_api_key: controller_api_key.clone(),
                    lmcache_worker_map: lmcache_worker_map.clone(),
                };
                Arc::new(LMCacheAwarePolicy::new(config, fallback))
            }
        }
    }

    /// List all registered policy names.
    pub fn registered_names(&self) -> Vec<String> {
        self.registry.read().keys().cloned().collect()
    }

    fn register_builtins(&self) {
        self.register("random", || Arc::new(RandomPolicy::new()));
        self.register("round_robin", || Arc::new(RoundRobinPolicy::new()));
        self.register("roundrobin", || Arc::new(RoundRobinPolicy::new()));
        self.register("power_of_two", || Arc::new(PowerOfTwoPolicy::new()));
        self.register("poweroftwo", || Arc::new(PowerOfTwoPolicy::new()));
        self.register("cache_aware", || Arc::new(CacheAwarePolicy::new()));
        self.register("cacheaware", || Arc::new(CacheAwarePolicy::new()));
        self.register("consistent_hash", || Arc::new(ConsistentHashPolicy::new()));
        self.register("consistenthash", || Arc::new(ConsistentHashPolicy::new()));
        self.register("lmcache_aware", || {
            Arc::new(LMCacheAwarePolicy::with_defaults())
        });
        self.register("lmcacheaware", || {
            Arc::new(LMCacheAwarePolicy::with_defaults())
        });
    }
}

/// Global shared factory instance. Initialized once, used by PolicyRegistry
/// and anywhere else that needs to create policies by name.
static FACTORY: std::sync::LazyLock<PolicyFactory> = std::sync::LazyLock::new(PolicyFactory::new);

/// Get a reference to the global policy factory.
pub fn global_factory() -> &'static PolicyFactory {
    &FACTORY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_from_config() {
        let factory = PolicyFactory::new();

        let policy = factory.create_from_config(&PolicyConfig::Random);
        assert_eq!(policy.name(), "random");

        let policy = factory.create_from_config(&PolicyConfig::RoundRobin);
        assert_eq!(policy.name(), "round_robin");

        let policy = factory.create_from_config(&PolicyConfig::PowerOfTwo {
            load_check_interval_secs: 60,
        });
        assert_eq!(policy.name(), "power_of_two");

        let policy = factory.create_from_config(&PolicyConfig::CacheAware {
            cache_threshold: 0.7,
            balance_abs_threshold: 10,
            balance_rel_threshold: 1.5,
            eviction_interval_secs: 30,
            max_tree_size: 1000,
        });
        assert_eq!(policy.name(), "cache_aware");

        let policy =
            factory.create_from_config(&PolicyConfig::ConsistentHash { virtual_nodes: 160 });
        assert_eq!(policy.name(), "consistent_hash");
    }

    #[test]
    fn test_create_by_name() {
        let factory = PolicyFactory::new();

        assert!(factory.create_by_name("random").is_some());
        assert!(factory.create_by_name("RANDOM").is_some());
        assert!(factory.create_by_name("round_robin").is_some());
        assert!(factory.create_by_name("RoundRobin").is_some());
        assert!(factory.create_by_name("power_of_two").is_some());
        assert!(factory.create_by_name("PowerOfTwo").is_some());
        assert!(factory.create_by_name("cache_aware").is_some());
        assert!(factory.create_by_name("CacheAware").is_some());
        assert!(factory.create_by_name("consistent_hash").is_some());
        assert!(factory.create_by_name("ConsistentHash").is_some());
        assert!(factory.create_by_name("lmcache_aware").is_some());
        assert!(factory.create_by_name("unknown").is_none());
    }

    #[test]
    fn test_custom_registration() {
        let factory = PolicyFactory::new();

        // Register a custom policy (reusing RandomPolicy as a stand-in)
        factory.register("my_custom_policy", || Arc::new(RandomPolicy::new()));

        let policy = factory.create_by_name("my_custom_policy");
        assert!(policy.is_some());
        assert_eq!(policy.unwrap().name(), "random");
    }

    #[test]
    fn test_registered_names() {
        let factory = PolicyFactory::new();
        let names = factory.registered_names();
        assert!(names.contains(&"random".to_string()));
        assert!(names.contains(&"round_robin".to_string()));
        assert!(names.contains(&"cache_aware".to_string()));
    }
}
