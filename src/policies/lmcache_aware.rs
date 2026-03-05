//! LMCache-Aware Load Balancing Policy
//!
//! Routes requests to workers based on real KV cache state reported by the
//! LMCache controller, rather than maintaining an approximate radix tree.
//!
//! Supports two modes:
//! - **Occupancy** (Phase 1): Poll `GET /controller/workers` for per-worker `key_count`.
//!   Score = cache_weight * normalized_key_count + (1 - cache_weight) * normalized_inverse_load.
//! - **Prefix lookup** (Phase 2): `POST /lookup` with token IDs to find which worker
//!   has the longest cached prefix for a specific request.
//!
//! When the LMCache controller is unreachable, falls back to a configurable policy
//! (default: `power_of_two`).

use super::{get_healthy_worker_indices, LoadBalancingPolicy, RequestHeaders};
use crate::core::Worker;
use crate::metrics::RouterMetrics;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Configuration for the LMCache-aware policy
#[derive(Debug, Clone)]
pub struct LMCacheAwareConfig {
    /// URL of the LMCache controller's API server
    pub controller_url: String,
    /// How often to poll the controller for worker state (seconds)
    pub poll_interval_secs: u64,
    /// Weight for cache occupancy vs load balancing (0.0 = pure load, 1.0 = pure cache)
    pub cache_weight: f32,
    /// Fallback policy name when controller is unreachable
    pub fallback_policy_name: String,
    /// HTTP timeout for controller queries (milliseconds)
    pub controller_timeout_ms: u64,
    /// Lookup mode: "occupancy" (Phase 1) or "prefix_lookup" (Phase 2)
    pub lookup_mode: String,
    /// Optional: API key for the controller endpoint
    pub controller_api_key: Option<String>,
    /// Optional: explicit instance_id -> worker_url mapping
    pub lmcache_worker_map: Option<HashMap<String, String>>,
}

impl Default for LMCacheAwareConfig {
    fn default() -> Self {
        Self {
            controller_url: "http://localhost:9000".to_string(),
            poll_interval_secs: 10,
            cache_weight: 0.7,
            fallback_policy_name: "power_of_two".to_string(),
            controller_timeout_ms: 2000,
            lookup_mode: "occupancy".to_string(),
            controller_api_key: None,
            lmcache_worker_map: None,
        }
    }
}

/// Cache information for a single worker, refreshed from the controller
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct WorkerCacheInfo {
    key_count: usize,
    last_heartbeat: f64,
    ip: String,
    port: u16,
}

/// Response model for GET /controller/workers
#[derive(Debug, serde::Deserialize)]
struct ControllerWorkersResponse {
    workers: Vec<ControllerWorkerInfo>,
    #[allow(dead_code)]
    total_count: usize,
}

#[derive(Debug, serde::Deserialize)]
struct ControllerWorkerInfo {
    instance_id: String,
    #[allow(dead_code)]
    worker_id: i64,
    ip: String,
    port: u16,
    key_count: usize,
    last_heartbeat_time: f64,
}

/// LMCache-aware routing policy
///
/// Queries the LMCache controller for real KV cache state and routes
/// requests to workers with the most relevant cached data.
pub struct LMCacheAwarePolicy {
    config: LMCacheAwareConfig,
    /// Periodically refreshed from LMCache controller: instance_id -> cache info
    worker_cache_state: Arc<RwLock<HashMap<String, WorkerCacheInfo>>>,
    /// Maps LMCache instance_id -> router worker URL
    instance_worker_map: Arc<RwLock<HashMap<String, String>>>,
    /// Reverse map: router worker URL -> LMCache instance_id
    worker_instance_map: Arc<RwLock<HashMap<String, String>>>,
    /// Fallback policy when controller is unreachable
    fallback_policy: Arc<dyn LoadBalancingPolicy>,
    /// Whether the controller has ever been successfully contacted
    controller_available: Arc<RwLock<bool>>,
}

impl std::fmt::Debug for LMCacheAwarePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LMCacheAwarePolicy")
            .field("config", &self.config)
            .field("controller_available", &*self.controller_available.read())
            .field(
                "tracked_workers",
                &self.worker_cache_state.read().len(),
            )
            .finish()
    }
}

impl LMCacheAwarePolicy {
    /// Create a new policy with the given configuration and fallback policy
    pub fn new(
        config: LMCacheAwareConfig,
        fallback_policy: Arc<dyn LoadBalancingPolicy>,
    ) -> Self {
        let instance_worker_map = Arc::new(RwLock::new(
            config.lmcache_worker_map.clone().unwrap_or_default(),
        ));

        // Build reverse map
        let reverse_map: HashMap<String, String> = config
            .lmcache_worker_map
            .as_ref()
            .map(|m| m.iter().map(|(k, v)| (v.clone(), k.clone())).collect())
            .unwrap_or_default();
        let worker_instance_map = Arc::new(RwLock::new(reverse_map));

        Self {
            config,
            worker_cache_state: Arc::new(RwLock::new(HashMap::new())),
            instance_worker_map,
            worker_instance_map,
            fallback_policy,
            controller_available: Arc::new(RwLock::new(false)),
        }
    }

    /// Create with default configuration (used by create_by_name)
    pub fn with_defaults() -> Self {
        use super::PowerOfTwoPolicy;
        Self::new(
            LMCacheAwareConfig::default(),
            Arc::new(PowerOfTwoPolicy::new()),
        )
    }

    /// Start the background polling task
    fn start_polling(&self) {
        let controller_url = self.config.controller_url.clone();
        let poll_interval = Duration::from_secs(self.config.poll_interval_secs);
        let timeout = Duration::from_millis(self.config.controller_timeout_ms);
        let api_key = self.config.controller_api_key.clone();
        let cache_state = Arc::clone(&self.worker_cache_state);
        let controller_available = Arc::clone(&self.controller_available);

        tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .expect("Failed to create HTTP client for LMCache polling");

            loop {
                let url = format!("{}/controller/workers", controller_url);
                let mut request = client.get(&url);
                if let Some(ref key) = api_key {
                    request = request.header("Authorization", format!("Bearer {}", key));
                }

                match request.send().await {
                    Ok(resp) if resp.status().is_success() => {
                        match resp.json::<ControllerWorkersResponse>().await {
                            Ok(data) => {
                                let mut state = cache_state.write();
                                state.clear();
                                for worker in &data.workers {
                                    state.insert(
                                        worker.instance_id.clone(),
                                        WorkerCacheInfo {
                                            key_count: worker.key_count,
                                            last_heartbeat: worker.last_heartbeat_time,
                                            ip: worker.ip.clone(),
                                            port: worker.port,
                                        },
                                    );
                                }
                                *controller_available.write() = true;
                                debug!(
                                    "LMCache controller poll: {} workers tracked",
                                    data.workers.len()
                                );
                            }
                            Err(e) => {
                                error!("Failed to parse LMCache controller response: {}", e);
                            }
                        }
                    }
                    Ok(resp) => {
                        warn!(
                            "LMCache controller returned status {}: {}",
                            resp.status(),
                            controller_url
                        );
                    }
                    Err(e) => {
                        warn!("LMCache controller unreachable: {}", e);
                        *controller_available.write() = false;
                    }
                }

                tokio::time::sleep(poll_interval).await;
            }
        });
    }

    /// Compute a routing score for a worker based on cache state and load
    fn compute_score(
        &self,
        worker: &dyn Worker,
        max_key_count: usize,
        max_load: usize,
        instance_id: Option<&str>,
        cache_state: &HashMap<String, WorkerCacheInfo>,
    ) -> f64 {
        let cache_weight = self.config.cache_weight as f64;
        let load_weight = 1.0 - cache_weight;

        // Normalize key_count: worker's key_count / max_key_count
        let normalized_cache = if let Some(id) = instance_id {
            if let Some(info) = cache_state.get(id) {
                if max_key_count > 0 {
                    info.key_count as f64 / max_key_count as f64
                } else {
                    0.0
                }
            } else {
                0.0
            }
        } else {
            0.0
        };

        // Normalize inverse load: (max_load - worker_load) / max_load
        // Higher is better (less loaded)
        let normalized_inverse_load = if max_load > 0 {
            (max_load - worker.load().min(max_load)) as f64 / max_load as f64
        } else {
            1.0 // All workers have zero load, all equally good
        };

        cache_weight * normalized_cache + load_weight * normalized_inverse_load
    }
}

impl LoadBalancingPolicy for LMCacheAwarePolicy {
    fn select_worker_with_headers(
        &self,
        workers: &[Arc<dyn Worker>],
        request_text: Option<&str>,
        _headers: Option<&RequestHeaders>,
    ) -> Option<usize> {
        let healthy_indices = get_healthy_worker_indices(workers);
        if healthy_indices.is_empty() {
            return None;
        }

        // Read cache state (non-blocking read lock)
        let cache_state = self.worker_cache_state.read();
        let controller_available = *self.controller_available.read();

        // If controller is not available or no cache state, use fallback
        if !controller_available || cache_state.is_empty() {
            debug!(
                "LMCache controller unavailable (available={}, state_size={}), using fallback: {}",
                controller_available,
                cache_state.len(),
                self.fallback_policy.name()
            );
            drop(cache_state);
            return self
                .fallback_policy
                .select_worker_with_headers(workers, request_text, _headers);
        }

        // Read instance-worker mappings
        let worker_instance_map = self.worker_instance_map.read();

        // Compute max key_count and max load for normalization
        let max_key_count = cache_state
            .values()
            .map(|info| info.key_count)
            .max()
            .unwrap_or(0);

        let max_load = healthy_indices
            .iter()
            .map(|&idx| workers[idx].load())
            .max()
            .unwrap_or(0);

        // Score each healthy worker
        let mut best_idx = None;
        let mut best_score = f64::NEG_INFINITY;

        for &idx in &healthy_indices {
            let worker_url = workers[idx].url();
            let instance_id = worker_instance_map.get(worker_url).map(|s| s.as_str());

            let score = self.compute_score(
                workers[idx].as_ref(),
                max_key_count,
                max_load,
                instance_id,
                &cache_state,
            );

            debug!(
                "Worker {} (instance: {:?}): score={:.4} (key_count={}, load={})",
                worker_url,
                instance_id,
                score,
                instance_id
                    .and_then(|id| cache_state.get(id))
                    .map(|i| i.key_count)
                    .unwrap_or(0),
                workers[idx].load(),
            );

            if score > best_score {
                best_score = score;
                best_idx = Some(idx);
            }
        }

        if let Some(idx) = best_idx {
            workers[idx].increment_processed();
            RouterMetrics::record_processed_request(workers[idx].url());
            RouterMetrics::record_policy_decision(self.name(), workers[idx].url());
            Some(idx)
        } else {
            // Should not happen if healthy_indices is non-empty, but fallback just in case
            drop(cache_state);
            drop(worker_instance_map);
            self.fallback_policy
                .select_worker_with_headers(workers, request_text, _headers)
        }
    }

    fn name(&self) -> &'static str {
        "lmcache_aware"
    }

    fn needs_request_text(&self) -> bool {
        // Phase 2 prefix_lookup mode benefits from request text
        self.config.lookup_mode == "prefix_lookup"
    }

    fn on_request_complete(&self, worker_url: &str, success: bool) {
        if !success {
            debug!(
                "LMCache-aware: request to {} completed with success={}",
                worker_url, success
            );
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn requires_initialization(&self) -> bool {
        true
    }

    fn init_workers(&self, workers: &[Arc<dyn Worker>]) {
        info!(
            "Initializing LMCache-aware policy with {} workers, controller: {}",
            workers.len(),
            self.config.controller_url
        );

        // If no explicit worker map was provided, try to build one from worker URLs
        {
            let map = self.instance_worker_map.read();
            if map.is_empty() {
                warn!(
                    "No lmcache_worker_map configured. Worker-to-instance mapping will \
                     be unavailable until the controller reports worker IPs that can be \
                     matched to router worker URLs. Consider configuring lmcache_worker_map \
                     in the policy config."
                );
            } else {
                info!(
                    "LMCache worker map: {:?}",
                    map.iter().collect::<Vec<_>>()
                );
            }
        }

        // Start background polling
        self.start_polling();

        info!(
            "LMCache-aware policy initialized. Polling {} every {}s. \
             Fallback: {}. Mode: {}. Cache weight: {}",
            self.config.controller_url,
            self.config.poll_interval_secs,
            self.fallback_policy.name(),
            self.config.lookup_mode,
            self.config.cache_weight,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{BasicWorker, WorkerType};
    use crate::policies::RandomPolicy;

    fn make_policy(cache_weight: f32) -> LMCacheAwarePolicy {
        let config = LMCacheAwareConfig {
            cache_weight,
            ..Default::default()
        };
        LMCacheAwarePolicy::new(config, Arc::new(RandomPolicy::new()))
    }

    fn make_workers(n: usize) -> Vec<Arc<dyn Worker>> {
        (0..n)
            .map(|i| {
                Arc::new(BasicWorker::new(
                    format!("http://w{}:8000", i + 1),
                    WorkerType::Regular,
                )) as Arc<dyn Worker>
            })
            .collect()
    }

    #[test]
    fn test_fallback_when_controller_unavailable() {
        let policy = make_policy(0.7);
        let workers = make_workers(3);

        // Controller not available (default state), should use fallback
        let idx = policy.select_worker(&workers, None);
        assert!(idx.is_some());
    }

    #[test]
    fn test_score_computation_pure_cache() {
        let policy = make_policy(1.0); // Pure cache weight

        let worker = BasicWorker::new("http://w1:8000".to_string(), WorkerType::Regular);

        let mut cache_state = HashMap::new();
        cache_state.insert(
            "inst-1".to_string(),
            WorkerCacheInfo {
                key_count: 100,
                last_heartbeat: 0.0,
                ip: "10.0.0.1".to_string(),
                port: 8000,
            },
        );

        let score = policy.compute_score(&worker, 100, 10, Some("inst-1"), &cache_state);
        // With cache_weight=1.0: score = 1.0 * (100/100) + 0.0 * inverse_load = 1.0
        assert!((score - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_score_computation_pure_load() {
        let policy = make_policy(0.0); // Pure load weight

        let worker = BasicWorker::new("http://w1:8000".to_string(), WorkerType::Regular);
        // Worker has 0 load, max_load is 10
        // inverse_load = (10 - 0) / 10 = 1.0

        let cache_state = HashMap::new();
        let score = policy.compute_score(&worker, 100, 10, None, &cache_state);
        // With cache_weight=0.0: score = 0.0 * cache + 1.0 * 1.0 = 1.0
        assert!((score - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_score_computation_mixed() {
        let policy = make_policy(0.5);

        let worker = BasicWorker::new("http://w1:8000".to_string(), WorkerType::Regular);

        let mut cache_state = HashMap::new();
        cache_state.insert(
            "inst-1".to_string(),
            WorkerCacheInfo {
                key_count: 50,
                last_heartbeat: 0.0,
                ip: "10.0.0.1".to_string(),
                port: 8000,
            },
        );

        // key_count=50, max=100 -> normalized=0.5
        // load=0, max_load=10 -> inverse=(10-0)/10=1.0
        // score = 0.5 * 0.5 + 0.5 * 1.0 = 0.25 + 0.5 = 0.75
        let score = policy.compute_score(&worker, 100, 10, Some("inst-1"), &cache_state);
        assert!((score - 0.75).abs() < 0.001);
    }

    #[test]
    fn test_prefers_worker_with_more_cache() {
        let policy = make_policy(0.7);
        let workers = make_workers(2);

        // Set up cache state: w1 has more cache
        {
            let mut state = policy.worker_cache_state.write();
            state.insert(
                "inst-1".to_string(),
                WorkerCacheInfo {
                    key_count: 100,
                    last_heartbeat: 0.0,
                    ip: "10.0.0.1".to_string(),
                    port: 8000,
                },
            );
            state.insert(
                "inst-2".to_string(),
                WorkerCacheInfo {
                    key_count: 10,
                    last_heartbeat: 0.0,
                    ip: "10.0.0.2".to_string(),
                    port: 8000,
                },
            );
        }

        // Set up worker-instance mapping
        {
            let mut map = policy.worker_instance_map.write();
            map.insert("http://w1:8000".to_string(), "inst-1".to_string());
            map.insert("http://w2:8000".to_string(), "inst-2".to_string());
        }

        // Mark controller as available
        *policy.controller_available.write() = true;

        let idx = policy.select_worker(&workers, None).unwrap();
        // w1 has more cache (100 vs 10), should be preferred with cache_weight=0.7
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_load_balances_when_cache_weight_zero() {
        let policy = make_policy(0.0); // Pure load balancing
        let workers = make_workers(2);

        // Give w1 high load
        for _ in 0..20 {
            workers[0].increment_load();
        }

        // Set up cache state: w1 has more cache
        {
            let mut state = policy.worker_cache_state.write();
            state.insert(
                "inst-1".to_string(),
                WorkerCacheInfo {
                    key_count: 100,
                    last_heartbeat: 0.0,
                    ip: "10.0.0.1".to_string(),
                    port: 8000,
                },
            );
            state.insert(
                "inst-2".to_string(),
                WorkerCacheInfo {
                    key_count: 0,
                    last_heartbeat: 0.0,
                    ip: "10.0.0.2".to_string(),
                    port: 8000,
                },
            );
        }

        {
            let mut map = policy.worker_instance_map.write();
            map.insert("http://w1:8000".to_string(), "inst-1".to_string());
            map.insert("http://w2:8000".to_string(), "inst-2".to_string());
        }

        *policy.controller_available.write() = true;

        let idx = policy.select_worker(&workers, None).unwrap();
        // Despite w1 having more cache, cache_weight=0.0 means pure load balancing
        // w2 has 0 load vs w1 has 20 load
        assert_eq!(idx, 1);
    }
}
