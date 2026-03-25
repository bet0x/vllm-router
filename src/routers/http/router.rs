use crate::config::types::RetryConfig;
use crate::core::{
    is_retryable_status, BasicWorker, CircuitBreakerConfig, DPAwareWorker, HealthConfig,
    RetryExecutor, Worker, WorkerRegistry, WorkerType,
};
use crate::metrics::RouterMetrics;
use crate::policies::{LoadBalancingPolicy, PolicyRegistry};
use crate::protocols::spec::{
    ChatCompletionRequest, CompletionRequest, EmbeddingRequest, GenerateRequest, GenerationRequest,
    RerankRequest, RerankResponse, RerankResult, ResponsesRequest,
};
use crate::routers::header_utils;
use crate::routers::http::dp_utils;
use crate::routers::{RouterTrait, WorkerManagement};
use axum::body::to_bytes;
use axum::{
    body::Body,
    extract::Request,
    http::{
        header::CONTENT_LENGTH, header::CONTENT_TYPE, HeaderMap, HeaderValue, Method, StatusCode,
    },
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, field::Empty, info, warn};

// ── Routing decision headers ────────────────────────────────────────────────
//
// These header names are part of the public contract when
// `expose_routing_headers` is enabled. Changing a name is a breaking change.

/// Worker URL that handled the request.
pub const HEADER_WORKER: &str = "x-vllm-router-worker";
/// How the worker was selected: `policy`, `cluster`, `lmcache-prefix`, `cache-hit`, `semantic-hit`.
pub const HEADER_METHOD: &str = "x-vllm-router-method";
/// Load-balancing policy name (only set when method=policy).
pub const HEADER_POLICY: &str = "x-vllm-router-policy";
/// Semantic cluster that matched (only set when method=cluster).
pub const HEADER_CLUSTER: &str = "x-vllm-router-cluster";
/// Resolved model name.
pub const HEADER_MODEL: &str = "x-vllm-router-model";
/// Cache status: `exact-hit`, `semantic-hit`, or `miss`.
pub const HEADER_CACHE_STATUS: &str = "x-vllm-router-cache-status";
/// Comma-separated list of hooks that executed.
pub const HEADER_HOOKS: &str = "x-vllm-router-hooks";

/// All explainability header names, for use in contract tests.
pub const ALL_EXPLAINABILITY_HEADERS: &[&str] = &[
    HEADER_WORKER,
    HEADER_METHOD,
    HEADER_POLICY,
    HEADER_CLUSTER,
    HEADER_MODEL,
    HEADER_CACHE_STATUS,
    HEADER_HOOKS,
];

/// Metadata accumulated during the routing pipeline, injected as response
/// headers when `expose_routing_headers` is enabled.
#[derive(Debug, Clone, Default)]
pub(crate) struct RoutingDecision {
    pub(crate) worker: Option<String>,
    pub(crate) method: Option<&'static str>,
    pub(crate) policy: Option<String>,
    pub(crate) cluster: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) cache_status: Option<&'static str>,
    pub(crate) hooks_ran: Vec<String>,
    pub(crate) request_text: Option<String>,
    pub(crate) tenant: Option<String>,
}

impl RoutingDecision {
    fn to_record(
        &self,
        request_id: Option<&str>,
        route: &str,
        status: u16,
        duration_ms: u64,
    ) -> crate::admin::DecisionRecord {
        crate::admin::DecisionRecord {
            schema_version: crate::admin::decision_log::DECISION_SCHEMA_VERSION,
            timestamp: crate::admin::decision_log::now_iso8601(),
            request_id: request_id.map(|s| s.to_string()),
            route: route.to_string(),
            model: self.model.clone(),
            method: self.method.map(|s| s.to_string()),
            policy: self.policy.clone(),
            cluster: self.cluster.clone(),
            worker: self.worker.clone(),
            cache_status: self.cache_status.map(|s| s.to_string()),
            status,
            duration_ms,
            hooks_ran: self.hooks_ran.clone(),
            request_text: self.request_text.clone(),
            tenant: self.tenant.clone(),
        }
    }

    fn inject_headers(&self, resp: &mut Response) {
        let h = resp.headers_mut();
        if let Some(ref w) = self.worker {
            if let Ok(v) = HeaderValue::from_str(w) {
                h.insert(HEADER_WORKER, v);
            }
        }
        if let Some(m) = self.method {
            h.insert(HEADER_METHOD, HeaderValue::from_static(m));
        }
        if let Some(ref p) = self.policy {
            if let Ok(v) = HeaderValue::from_str(p) {
                h.insert(HEADER_POLICY, v);
            }
        }
        if let Some(ref c) = self.cluster {
            if let Ok(v) = HeaderValue::from_str(c) {
                h.insert(HEADER_CLUSTER, v);
            }
        }
        if let Some(ref model) = self.model {
            if let Ok(v) = HeaderValue::from_str(model) {
                h.insert(HEADER_MODEL, v);
            }
        }
        if let Some(cs) = self.cache_status {
            h.insert(HEADER_CACHE_STATUS, HeaderValue::from_static(cs));
        }
        if !self.hooks_ran.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&self.hooks_ran.join(",")) {
                h.insert(HEADER_HOOKS, v);
            }
        }
    }
}

/// Regular router that uses injected load balancing policies
#[derive(Debug)]
pub struct Router {
    worker_registry: Arc<WorkerRegistry>,
    policy_registry: Arc<PolicyRegistry>,
    client: Client,
    worker_startup_timeout_secs: u64,
    worker_startup_check_interval_secs: u64,
    intra_node_data_parallel_size: usize,
    api_key: Arc<tokio::sync::RwLock<Option<String>>>,
    /// Per-worker API keys (url → key). Populated from RouterConfig.worker_api_keys.
    worker_api_keys: Arc<tokio::sync::RwLock<std::collections::HashMap<String, String>>>,
    retry_config: RetryConfig,
    circuit_breaker_config: CircuitBreakerConfig,
    _worker_loads: Arc<tokio::sync::watch::Receiver<HashMap<String, isize>>>,
    _load_monitor_handle: Option<Arc<tokio::task::JoinHandle<()>>>,
    /// Exact-match response cache for non-streaming requests.
    response_cache: Arc<dyn crate::cache::traits::ExactMatchCache>,
    /// Semantic similarity cache (T-12).  `None` when the feature is disabled.
    semantic_cache: Option<Arc<dyn crate::cache::traits::SemanticCacheBackend>>,
    /// Base URL of the embeddings endpoint used for semantic caching/cluster routing.
    embeddings_url: Option<String>,
    /// API key for the embeddings endpoint (sent as Bearer token).
    embeddings_api_key: Option<String>,
    /// Model name sent in embedding requests.
    embeddings_model: String,
    /// Timeout in milliseconds for each embedding HTTP call.
    embedding_timeout_ms: u64,
    /// Semantic cluster routing policy.  `None` when cluster routing is disabled.
    semantic_cluster: Option<Arc<crate::policies::SemanticClusterPolicy>>,
    /// Whether to inject `x-vllm-router-*` decision headers into responses.
    expose_routing_headers: bool,
    /// Ring buffer of recent routing decisions for admin visibility.
    decision_log: Arc<crate::admin::DecisionLog>,
    /// Model name rewrite rules (alias, fallback chains).
    model_rules: Vec<crate::model_rules::ModelRule>,
    /// Pre-routing hooks configuration.
    pre_routing_hooks: Vec<crate::hooks::PreRoutingHook>,
    /// Whether to capture request text in decision records (for replay).
    include_request_text: bool,
    /// Token ID cache for LMCache prefix lookup optimization. None = no cache.
    token_cache_memory: Option<Arc<crate::cache::token_cache::TokenCache>>,
    /// Redis token cache (behind feature flag).
    #[cfg(feature = "redis-cache")]
    token_cache_redis: Option<Arc<crate::cache::token_cache::redis_backend::RedisTokenCache>>,
}

impl Router {
    /// Create a new router with injected policy and client
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        worker_urls: Vec<String>,
        ctx: &Arc<crate::server::AppContext>,
    ) -> Result<Self, String> {
        // Update active workers gauge
        RouterMetrics::set_active_workers(worker_urls.len());

        // Wait for workers to be healthy (skip if empty - for service discovery mode)
        if !worker_urls.is_empty() {
            Self::wait_for_healthy_workers(
                &worker_urls,
                ctx.router_config.worker_startup_timeout_secs,
                ctx.router_config.worker_startup_check_interval_secs,
            )
            .await?;
        }

        // Automatically expand to DP-aware workers when intra_node_data_parallel_size > 1
        let worker_urls = if ctx.router_config.intra_node_data_parallel_size > 1 {
            // worker address now in the format of "http://host:port@dp_rank"
            dp_utils::get_dp_aware_workers(
                &worker_urls,
                &ctx.router_config.api_key,
                ctx.router_config.intra_node_data_parallel_size,
            )
            .await
            .map_err(|e| format!("Failed to get dp-aware workers: {}", e))?
        } else {
            worker_urls
        };

        // Convert config CircuitBreakerConfig to core CircuitBreakerConfig
        let circuit_breaker_config = ctx.router_config.effective_circuit_breaker_config();
        let core_cb_config = CircuitBreakerConfig {
            failure_threshold: circuit_breaker_config.failure_threshold,
            success_threshold: circuit_breaker_config.success_threshold,
            timeout_duration: Duration::from_secs(circuit_breaker_config.timeout_duration_secs),
            window_duration: Duration::from_secs(circuit_breaker_config.window_duration_secs),
        };

        // Register workers in the registry, fetching model_id from each worker.
        let dp_size = ctx.router_config.intra_node_data_parallel_size;
        let health_config = HealthConfig {
            timeout_secs: ctx.router_config.health_check.timeout_secs,
            check_interval_secs: ctx.router_config.health_check.check_interval_secs,
            endpoint: ctx.router_config.health_check.endpoint.clone(),
            failure_threshold: ctx.router_config.health_check.failure_threshold,
            success_threshold: ctx.router_config.health_check.success_threshold,
        };
        for url in &worker_urls {
            // Strip @rank suffix (DP-aware) to look up the base URL in worker_api_keys
            let base_url_for_lookup = url.split('@').next().unwrap_or(url);
            let worker_api_key = ctx
                .router_config
                .worker_api_keys
                .get(base_url_for_lookup)
                .cloned();
            let model_id =
                Self::fetch_model_id_from_server(&ctx.client, url, worker_api_key.as_deref())
                    .await;
            let mut labels = std::collections::HashMap::new();
            labels.insert("model_id".to_string(), model_id);

            let worker_arc: Arc<dyn Worker> = if dp_size > 1 {
                let (base_url, dp_rank) = dp_utils::parse_worker_url(url);
                Arc::new(
                    DPAwareWorker::new(base_url, dp_rank.unwrap_or(0), dp_size, WorkerType::Regular)
                        .with_labels(labels)
                        .with_api_key(worker_api_key)
                        .with_circuit_breaker_config(core_cb_config.clone())
                        .with_health_config(health_config.clone()),
                )
            } else {
                Arc::new(
                    BasicWorker::new(url.clone(), WorkerType::Regular)
                        .with_labels(labels)
                        .with_api_key(worker_api_key)
                        .with_circuit_breaker_config(core_cb_config.clone())
                        .with_health_config(health_config.clone()),
                )
            };
            ctx.worker_registry.register(worker_arc.clone());

            // Notify PolicyRegistry about the new worker
            let model_id = worker_arc.model_id();
            let policy = ctx.policy_registry.on_worker_added(model_id, None);

            // If this is a cache-aware policy and it's the first worker for this model,
            // initialize it with the worker
            if policy.name() == "cache_aware" {
                if let Some(cache_aware) = policy
                    .as_any()
                    .downcast_ref::<crate::policies::CacheAwarePolicy>()
                {
                    let worker_dyn: Arc<dyn Worker> = worker_arc.clone();
                    cache_aware.init_workers(std::slice::from_ref(&worker_dyn));
                }
            }
        }

        // Configure shared prefix routing on cache_aware policies
        if let Some(ref spr_config) = ctx.router_config.shared_prefix_routing {
            let default_policy = ctx.policy_registry.get_default_policy();
            if default_policy.name() == "cache_aware" {
                if let Some(cache_aware) = default_policy
                    .as_any()
                    .downcast_ref::<crate::policies::CacheAwarePolicy>()
                {
                    cache_aware.set_shared_prefix_config(spr_config);
                }
            }
        }

        // Setup load monitoring for PowerOfTwo policy
        let (tx, rx) = tokio::sync::watch::channel(HashMap::new());
        let worker_loads = Arc::new(rx);

        // Check if default policy is power_of_two for load monitoring
        let default_policy = ctx.policy_registry.get_default_policy();
        let load_monitor_handle = if default_policy.name() == "power_of_two" {
            let monitor_urls = worker_urls.clone();
            let monitor_interval = ctx.router_config.worker_startup_check_interval_secs;
            let policy_clone = default_policy.clone();
            let client_clone = ctx.client.clone();

            Some(Arc::new(tokio::spawn(async move {
                Self::monitor_worker_loads(
                    monitor_urls,
                    tx,
                    monitor_interval,
                    policy_clone,
                    client_clone,
                )
                .await;
            })))
        } else {
            None
        };

        // Resolve embeddings URL and model: prefer semantic_cache config, fall back to
        // semantic_cluster config if present.
        let embeddings_url = ctx
            .router_config
            .semantic_cache
            .as_ref()
            .and_then(|sc| sc.embeddings_url.clone())
            .or_else(|| {
                ctx.router_config
                    .semantic_cluster
                    .as_ref()
                    .and_then(|sc| sc.embeddings_url.clone())
            });

        let embeddings_model = ctx
            .router_config
            .semantic_cache
            .as_ref()
            .map(|sc| sc.embeddings_model.clone())
            .unwrap_or_else(|| {
                ctx.router_config
                    .semantic_cluster
                    .as_ref()
                    .map(|sc| sc.embeddings_model.clone())
                    .unwrap_or_else(|| "default".to_string())
            });

        let embedding_timeout_ms = ctx
            .router_config
            .semantic_cache
            .as_ref()
            .map(|sc| sc.embedding_timeout_ms)
            .unwrap_or_else(|| {
                ctx.router_config
                    .semantic_cluster
                    .as_ref()
                    .map(|sc| sc.embedding_timeout_ms)
                    .unwrap_or(500)
            });

        // Resolve embeddings API key: prefer semantic_cache, fall back to semantic_cluster.
        let embeddings_api_key = ctx
            .router_config
            .semantic_cache
            .as_ref()
            .and_then(|sc| sc.embeddings_api_key.clone())
            .or_else(|| {
                ctx.router_config
                    .semantic_cluster
                    .as_ref()
                    .and_then(|sc| sc.embeddings_api_key.clone())
            });

        // Build semantic cluster policy (computes centroids via the embeddings endpoint).
        let semantic_cluster = if let Some(sc_config) = &ctx.router_config.semantic_cluster {
            let url = sc_config
                .embeddings_url
                .as_deref()
                .or(embeddings_url.as_deref());
            match url {
                Some(url) => {
                    Self::build_semantic_clusters(sc_config, url, embeddings_api_key.as_deref(), &ctx.client).await
                }
                None => {
                    warn!(
                        "semantic_cluster config present but no embeddings_url — cluster routing disabled"
                    );
                    None
                }
            }
        } else {
            None
        };

        let router = Router {
            worker_registry: ctx.worker_registry.clone(),
            policy_registry: ctx.policy_registry.clone(),
            client: ctx.client.clone(),
            worker_startup_timeout_secs: ctx.router_config.worker_startup_timeout_secs,
            worker_startup_check_interval_secs: ctx
                .router_config
                .worker_startup_check_interval_secs,
            intra_node_data_parallel_size: ctx.router_config.intra_node_data_parallel_size,
            api_key: Arc::new(tokio::sync::RwLock::new(ctx.router_config.api_key.clone())),
            worker_api_keys: Arc::new(tokio::sync::RwLock::new(
                ctx.router_config.worker_api_keys.clone(),
            )),
            retry_config: ctx.router_config.effective_retry_config(),
            circuit_breaker_config: core_cb_config,
            _worker_loads: worker_loads,
            _load_monitor_handle: load_monitor_handle,
            response_cache: Self::build_exact_cache(&ctx.router_config)?,
            semantic_cache: Self::build_semantic_cache(&ctx.router_config)?,
            embeddings_url: embeddings_url.clone(),
            embeddings_api_key: embeddings_api_key.clone(),
            embeddings_model: embeddings_model.clone(),
            embedding_timeout_ms,
            semantic_cluster,
            expose_routing_headers: ctx.router_config.expose_routing_headers,
            decision_log: ctx.decision_log.clone(),
            model_rules: ctx.router_config.model_rules.clone(),
            pre_routing_hooks: ctx.router_config.pre_routing_hooks.clone(),
            include_request_text: ctx.router_config.decision_log.as_ref().map(|d| d.include_request_text).unwrap_or(false),
            token_cache_memory: Self::build_token_cache_memory(&ctx.router_config),
            #[cfg(feature = "redis-cache")]
            token_cache_redis: Self::build_token_cache_redis(&ctx.router_config),
        };

        if let Some(ref url) = embeddings_url {
            info!(embeddings_url = url, embeddings_model = %embeddings_model, "embeddings endpoint configured");
        }

        Ok(router)
    }

    /// Update authentication config (hot reload)
    pub async fn update_auth_config(
        &self,
        api_key: Option<String>,
        worker_api_keys: HashMap<String, String>,
    ) {
        *self.api_key.write().await = api_key;
        *self.worker_api_keys.write().await = worker_api_keys;
    }

    /// Build a [`SemanticClusterPolicy`] by fetching embeddings for each cluster's
    /// example prompts and averaging them into a normalised centroid vector.
    async fn build_semantic_clusters(
        config: &crate::config::types::SemanticClusterConfig,
        embeddings_url: &str,
        embeddings_api_key: Option<&str>,
        client: &Client,
    ) -> Option<Arc<crate::policies::SemanticClusterPolicy>> {
        use std::time::Duration;

        let timeout = Duration::from_millis(config.embedding_timeout_ms);
        let endpoint = format!("{}/v1/embeddings", embeddings_url.trim_end_matches('/'));
        let mut parts: Vec<(String, Vec<f32>, Vec<String>)> = Vec::new();

        for cluster_def in &config.clusters {
            let mut embeddings: Vec<Vec<f32>> = Vec::new();

            for example in &cluster_def.examples {
                let mut req = client
                    .post(&endpoint)
                    .json(&serde_json::json!({
                        "model": config.embeddings_model,
                        "input": example,
                    }))
                    .timeout(timeout);
                if let Some(key) = embeddings_api_key {
                    req = req.bearer_auth(key);
                }
                let resp = req.send().await;

                let emb = match resp {
                    Ok(r) if r.status().is_success() => {
                        r.json::<serde_json::Value>().await.ok().and_then(|j| {
                            let arr = j.get("data")?.get(0)?.get("embedding")?.as_array()?;
                            let v: Vec<f32> = arr
                                .iter()
                                .filter_map(|x| x.as_f64().map(|f| f as f32))
                                .collect();
                            if v.is_empty() { None } else { Some(v) }
                        })
                    }
                    _ => None,
                };

                match emb {
                    Some(v) => embeddings.push(v),
                    None => {
                        warn!(
                            "Cluster '{}': failed to embed example '{}'",
                            cluster_def.name, example
                        );
                    }
                }
            }

            if embeddings.is_empty() {
                warn!(
                    "Cluster '{}' has no valid embeddings — skipping",
                    cluster_def.name
                );
                continue;
            }

            // Average and L2-normalise → centroid.
            let dim = embeddings[0].len();
            let mut centroid = vec![0.0f32; dim];
            for emb in &embeddings {
                for (c, v) in centroid.iter_mut().zip(emb.iter()) {
                    *c += v;
                }
            }
            let n = embeddings.len() as f32;
            for c in centroid.iter_mut() {
                *c /= n;
            }
            let norm: f32 = centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for c in centroid.iter_mut() {
                    *c /= norm;
                }
            }

            info!(
                "Semantic cluster '{}' ready — {} examples → {} workers",
                cluster_def.name,
                embeddings.len(),
                cluster_def.workers.len()
            );
            parts.push((
                cluster_def.name.clone(),
                centroid,
                cluster_def.workers.clone(),
            ));
        }

        if parts.is_empty() {
            warn!("No semantic clusters could be initialised — cluster routing disabled");
            return None;
        }

        Some(Arc::new(
            crate::policies::SemanticClusterPolicy::from_parts(parts, config.threshold),
        ))
    }

    /// Get cached token IDs (checks memory first, then Redis).
    async fn token_cache_get(&self, key: u64) -> Option<Option<Vec<i64>>> {
        // Check memory cache first
        if let Some(ref mc) = self.token_cache_memory {
            if let Some(tokens) = mc.get(key) {
                return Some(Some(tokens));
            }
        }
        // Check Redis cache
        #[cfg(feature = "redis-cache")]
        if let Some(ref rc) = self.token_cache_redis {
            if let Some(tokens) = rc.get(key).await {
                return Some(Some(tokens));
            }
        }
        Some(None)
    }

    /// Store token IDs in the cache.
    async fn token_cache_insert(&self, key: u64, tokens: &[i64]) {
        if let Some(ref mc) = self.token_cache_memory {
            mc.insert(key, tokens.to_vec());
        }
        #[cfg(feature = "redis-cache")]
        if let Some(ref rc) = self.token_cache_redis {
            rc.insert(key, tokens).await;
        }
    }

    fn build_token_cache_memory(
        config: &crate::config::types::RouterConfig,
    ) -> Option<Arc<crate::cache::token_cache::TokenCache>> {
        let pc = config.prompt_cache.as_ref()?;
        if pc.backend != crate::config::types::CacheBackend::Memory {
            return None;
        }
        info!(max_entries = pc.max_entries, ttl_secs = pc.ttl_secs, "token cache: in-memory");
        Some(Arc::new(crate::cache::token_cache::TokenCache::new(pc.max_entries, pc.ttl_secs)))
    }

    #[cfg(feature = "redis-cache")]
    fn build_token_cache_redis(
        config: &crate::config::types::RouterConfig,
    ) -> Option<Arc<crate::cache::token_cache::redis_backend::RedisTokenCache>> {
        let pc = config.prompt_cache.as_ref()?;
        if pc.backend != crate::config::types::CacheBackend::Redis {
            return None;
        }
        // Use prompt_cache.redis, fall back to cache.redis
        let redis_config = pc.redis.clone()
            .or_else(|| config.cache.as_ref().and_then(|c| c.redis.clone()))
            .unwrap_or_default();
        info!(url = %redis_config.url, ttl_secs = pc.ttl_secs, "token cache: redis");
        match crate::cache::token_cache::redis_backend::RedisTokenCache::new(&redis_config, pc.ttl_secs) {
            Ok(cache) => Some(Arc::new(cache)),
            Err(e) => {
                warn!(error = %e, "token cache: failed to create Redis cache, falling back to none");
                None
            }
        }
    }


    /// Resolve model rules: alias, fallback, wildcard rewrite.
    /// Returns the (possibly rewritten) model name.
    fn resolve_model<'a>(&self, model_id: Option<&'a str>, decision: &mut RoutingDecision) -> Option<String> {
        if self.model_rules.is_empty() {
            return model_id.map(|s| s.to_string());
        }
        let result = crate::model_rules::resolve(model_id, &self.model_rules, &self.worker_registry)?;
        if let Some(ref orig) = result.original {
            decision.model = Some(result.model.clone());
            decision.method = None; // Will be set by the routing pipeline
            // Store original for logging (method field reused below)
            debug!(from = orig.as_str(), to = result.model.as_str(), "model resolved");
        }
        Some(result.model)
    }

    /// Build the exact-match response cache from config.
    ///
    /// Uses Redis when `cache.backend == redis` and the `redis-cache` feature is
    /// enabled; otherwise falls back to the in-memory implementation.
    fn build_exact_cache(
        config: &crate::config::types::RouterConfig,
    ) -> Result<Arc<dyn crate::cache::traits::ExactMatchCache>, String> {
        let (max_entries, ttl_secs, backend) = match &config.cache {
            Some(cc) => (cc.max_entries, cc.ttl_secs, &cc.backend),
            None => (1024, 60, &crate::config::types::CacheBackend::Memory),
        };

        match backend {
            crate::config::types::CacheBackend::Memory => {
                info!(max_entries, ttl_secs, "exact-match cache: in-memory");
                Ok(Arc::new(crate::cache::ResponseCache::new(max_entries, ttl_secs)))
            }
            crate::config::types::CacheBackend::Redis => {
                #[cfg(feature = "redis-cache")]
                {
                    let redis_config = config
                        .cache
                        .as_ref()
                        .and_then(|cc| cc.redis.clone())
                        .unwrap_or_default();
                    info!(url = %redis_config.url, max_entries, ttl_secs, "exact-match cache: redis");
                    let cache = crate::cache::redis_backend::RedisExactCache::new(
                        &redis_config,
                        max_entries,
                        ttl_secs,
                    )?;
                    Ok(Arc::new(cache))
                }
                #[cfg(not(feature = "redis-cache"))]
                {
                    Err("cache.backend is 'redis' but the binary was compiled without the 'redis-cache' feature".to_string())
                }
            }
        }
    }

    /// Build the semantic similarity cache from config.
    ///
    /// Returns `None` when no embeddings URL is configured. Uses Redis when
    /// `cache.backend == redis` and the `redis-cache` feature is enabled.
    fn build_semantic_cache(
        config: &crate::config::types::RouterConfig,
    ) -> Result<Option<Arc<dyn crate::cache::traits::SemanticCacheBackend>>, String> {
        let sc = match config.semantic_cache.as_ref() {
            Some(sc) if sc.embeddings_url.is_some() => sc,
            _ => return Ok(None),
        };

        let backend = config
            .cache
            .as_ref()
            .map(|cc| &cc.backend)
            .unwrap_or(&crate::config::types::CacheBackend::Memory);

        match backend {
            crate::config::types::CacheBackend::Memory => {
                info!(
                    max_entries = sc.max_entries,
                    ttl_secs = sc.ttl_secs,
                    threshold = sc.threshold,
                    "semantic cache: in-memory"
                );
                Ok(Some(Arc::new(crate::cache::semantic::SemanticCache::new(
                    sc.max_entries,
                    sc.ttl_secs,
                    sc.threshold,
                ))))
            }
            crate::config::types::CacheBackend::Redis => {
                #[cfg(feature = "redis-cache")]
                {
                    let redis_config = config
                        .cache
                        .as_ref()
                        .and_then(|cc| cc.redis.clone())
                        .unwrap_or_default();
                    info!(
                        url = %redis_config.url,
                        max_entries = sc.max_entries,
                        ttl_secs = sc.ttl_secs,
                        threshold = sc.threshold,
                        "semantic cache: redis"
                    );
                    let cache = crate::cache::redis_backend::RedisSemanticCache::new(
                        &redis_config,
                        sc.max_entries,
                        sc.ttl_secs,
                        sc.threshold,
                    )?;
                    Ok(Some(Arc::new(cache)))
                }
                #[cfg(not(feature = "redis-cache"))]
                {
                    Err("cache.backend is 'redis' but the binary was compiled without the 'redis-cache' feature".to_string())
                }
            }
        }
    }

    /// Extract a plain-text representation of the request body for embedding.
    ///
    /// For chat requests the concatenated message contents are returned.
    /// For completion requests the `prompt` field is used.
    /// Falls back to the canonical JSON string when neither is available.
    fn extract_request_text(body: &serde_json::Value) -> String {
        // Chat messages: concatenate role+content pairs
        if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
            let text: String = messages
                .iter()
                .filter_map(|m| {
                    // content may be a string or an array of content parts
                    let content = m.get("content")?;
                    if let Some(s) = content.as_str() {
                        return Some(s.to_string());
                    }
                    if let Some(parts) = content.as_array() {
                        let combined: String = parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !combined.is_empty() {
                            return Some(combined);
                        }
                    }
                    None
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                return text;
            }
        }
        // Completion prompt
        if let Some(prompt) = body.get("prompt").and_then(|p| p.as_str()) {
            return prompt.to_string();
        }
        // Last resort: full canonical JSON
        serde_json::to_string(body).unwrap_or_default()
    }

    /// Call the configured embeddings endpoint and return the embedding vector.
    ///
    /// Returns `None` when the semantic cache is disabled, the HTTP call fails,
    /// or the response cannot be parsed.
    async fn get_embedding(&self, text: &str) -> Option<Vec<f32>> {
        let url = self.embeddings_url.as_ref()?;
        let endpoint = format!("{}/v1/embeddings", url.trim_end_matches('/'));
        let mut req = self
            .client
            .post(&endpoint)
            .json(&serde_json::json!({
                "model": self.embeddings_model,
                "input": text,
            }))
            .timeout(Duration::from_millis(self.embedding_timeout_ms));
        if let Some(ref key) = self.embeddings_api_key {
            req = req.bearer_auth(key);
        }
        let resp = match req.send().await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(endpoint, error = %e, "embedding request failed");
                return None;
            }
        };

        if !resp.status().is_success() {
            warn!(endpoint, status = resp.status().as_u16(), "embedding request returned error status");
            return None;
        }
        let json: serde_json::Value = resp.json().await.ok()?;
        let embedding: Vec<f32> = json
            .get("data")?
            .get(0)?
            .get("embedding")?
            .as_array()?
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();

        if embedding.is_empty() {
            None
        } else {
            Some(embedding)
        }
    }

    /// Fetch model_id from a worker. Tries two endpoints in order:
    ///   1. /get_server_info  — llm-d / SGLang-based workers
    ///   2. /v1/models        — standard vLLM (OpenAI-compatible)
    /// Returns "unknown" if both fail or neither contains a model ID.
    async fn fetch_model_id_from_server(
        client: &reqwest::Client,
        url: &str,
        api_key: Option<&str>,
    ) -> String {
        let base = url.trim_end_matches('/');

        // 1. Try /get_server_info (llm-d / SGLang)
        let info_url = format!("{}/get_server_info", base);
        let mut req = client.get(&info_url).timeout(Duration::from_secs(5));
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        if let Ok(resp) = req.send().await
        {
            if resp.status().is_success() {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    let model_id = json
                        .get("model_id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            json.get("model_path")
                                .and_then(|v| v.as_str())
                                .and_then(|p| p.split('/').next_back())
                        })
                        .map(|s| s.to_string());
                    if let Some(id) = model_id {
                        return id;
                    }
                }
            }
        }

        // 2. Fall back to /v1/models (standard vLLM)
        let models_url = format!("{}/v1/models", base);
        let mut req = client.get(&models_url).timeout(Duration::from_secs(5));
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        if let Ok(resp) = req.send().await {
            if resp.status().is_success() {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(id) = json
                        .get("data")
                        .and_then(|d| d.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|m| m.get("id"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        return id.to_string();
                    }
                }
            }
        }

        "unknown".to_string()
    }

    /// Get the current list of worker URLs
    pub fn get_worker_urls(&self) -> Vec<String> {
        self.worker_registry.get_all_urls()
    }

    /// Get worker URLs for a specific model
    pub fn get_worker_urls_for_model(&self, model_id: Option<&str>) -> Vec<String> {
        let workers = match model_id {
            Some(model) => self.worker_registry.get_by_model_fast(model),
            None => self.worker_registry.get_all(),
        };
        workers.iter().map(|w| w.url().to_string()).collect()
    }

    pub async fn wait_for_healthy_workers(
        worker_urls: &[String],
        worker_startup_timeout_secs: u64,
        worker_startup_check_interval_secs: u64,
    ) -> Result<(), String> {
        if worker_urls.is_empty() {
            return Err(
                "Timeout waiting for workers to become healthy: no workers provided".to_string(),
            );
        }

        // Perform health check asynchronously
        Self::wait_for_healthy_workers_async(
            worker_urls,
            worker_startup_timeout_secs,
            worker_startup_check_interval_secs,
        )
        .await
    }

    async fn wait_for_healthy_workers_async(
        worker_urls: &[String],
        worker_startup_timeout_secs: u64,
        worker_startup_check_interval_secs: u64,
    ) -> Result<(), String> {
        // Extract unique base URLs (hosts) for health checks
        // This deduplicates DP-aware URLs like http://host:8081@0, @1, @2, @3
        // to only check http://host:8081 once
        use std::collections::HashSet;
        let mut unique_hosts = HashSet::new();
        let mut host_to_workers: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();

        for url in worker_urls {
            // Extract base URL by removing @rank suffix if present
            let base_url = if let Some(at_pos) = url.rfind('@') {
                url[..at_pos].to_string()
            } else {
                url.clone()
            };

            unique_hosts.insert(base_url.clone());
            host_to_workers
                .entry(base_url)
                .or_default()
                .push(url.clone());
        }

        let unique_hosts_vec: Vec<String> = unique_hosts.into_iter().collect();

        info!(
            "Waiting for {} unique hosts (representing {} workers) to become healthy (timeout: {}s)",
            unique_hosts_vec.len(),
            worker_urls.len(),
            worker_startup_timeout_secs
        );

        let start_time = std::time::Instant::now();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

        loop {
            if start_time.elapsed() > Duration::from_secs(worker_startup_timeout_secs) {
                error!(
                    "Timeout {}s waiting for hosts {:?} to become healthy. Please set --router-worker-startup-timeout-secs (vllm_router.launch_server) or --worker-startup-timeout-secs (vllm_worker.router) to a larger value",
                    worker_startup_timeout_secs, unique_hosts_vec
                );
                return Err(format!(
                    "Timeout {}s waiting for hosts {:?} to become healthy. Please set --router-worker-startup-timeout-secs (vllm_router.launch_server) or --worker-startup-timeout-secs (vllm_worker.router) to a larger value",
                    worker_startup_timeout_secs, unique_hosts_vec
                ));
            }

            // Perform health checks only on unique hosts (not per DP rank)
            let mut health_checks = Vec::new();
            for base_url in &unique_hosts_vec {
                let client_clone = client.clone();
                let url_clone = base_url.clone();

                let check_health = tokio::spawn(async move {
                    let health_url = format!("{}/health", url_clone);
                    match client_clone.get(&health_url).send().await {
                        Ok(res) => {
                            if res.status().is_success() {
                                None
                            } else {
                                Some((url_clone, format!("status: {}", res.status())))
                            }
                        }
                        Err(_) => Some((url_clone, "not ready".to_string())),
                    }
                });

                health_checks.push(check_health);
            }

            // Wait for all health checks to complete
            let results = futures::future::join_all(health_checks).await;

            let mut unhealthy_hosts = Vec::new();
            let mut all_healthy = true;

            for result in results {
                match result {
                    Ok(None) => {
                        // Host is healthy
                    }
                    Ok(Some((url, reason))) => {
                        all_healthy = false;
                        unhealthy_hosts.push((url, reason));
                    }
                    Err(e) => {
                        all_healthy = false;
                        unhealthy_hosts.push(("unknown".to_string(), format!("task error: {}", e)));
                    }
                }
            }

            if all_healthy {
                info!(
                    "All {} unique hosts are healthy (representing {} workers)",
                    unique_hosts_vec.len(),
                    worker_urls.len()
                );
                return Ok(());
            } else {
                debug!(
                    "Waiting for {} unique hosts to become healthy ({} unhealthy: {:?})",
                    unique_hosts_vec.len(),
                    unhealthy_hosts.len(),
                    unhealthy_hosts
                );
                tokio::time::sleep(Duration::from_secs(worker_startup_check_interval_secs)).await;
            }
        }
    }

    fn select_first_worker(&self) -> Result<String, String> {
        let workers = self.worker_registry.get_all();
        if workers.is_empty() {
            Err("No workers are available".to_string())
        } else {
            Ok(workers[0].url().to_string())
        }
    }

    #[allow(dead_code)]
    fn select_first_worker_for_model(&self, model_id: Option<&str>) -> Result<String, String> {
        let workers = match model_id {
            Some(model) => self.worker_registry.get_by_model_fast(model),
            None => self.worker_registry.get_all(),
        };
        if workers.is_empty() {
            Err(format!(
                "No workers are available for model: {:?}",
                model_id
            ))
        } else {
            Ok(workers[0].url().to_string())
        }
    }

    pub async fn send_health_check(&self, worker_url: &str) -> Response {
        let health_url = if self.intra_node_data_parallel_size > 1 {
            // Need to extract the URL from "http://host:port@dp_rank"
            match dp_utils::extract_dp_rank(worker_url) {
                Ok((worker_url_prefix, _dp_rank)) => worker_url_prefix,
                Err(e) => {
                    error!("Failed to extract dp_rank for health check: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to extract dp_rank: {}", e),
                    )
                        .into_response();
                }
            }
        } else {
            worker_url
        };

        let request_builder = self.client.get(format!("{}/health", health_url));

        let response = match request_builder.send().await {
            Ok(res) => {
                let status = StatusCode::from_u16(res.status().as_u16())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

                match res.bytes().await {
                    Ok(body) => (status, body).into_response(),
                    Err(e) => {
                        error!(
                            worker_url = %health_url,
                            error = %e,
                            "Failed to read health response body"
                        );
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Failed to read response body: {}", e),
                        )
                            .into_response()
                    }
                }
            }
            Err(e) => {
                error!(
                    worker_url = %health_url,
                    error = %e,
                    "Failed to send health request to worker"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to send request to worker {}: {}", health_url, e),
                )
                    .into_response()
            }
        };

        // Don't record metrics for health checks
        response
    }

    // Helper method to proxy GET requests to the first available worker
    async fn proxy_get_request(&self, req: Request<Body>, endpoint: &str) -> Response {
        let headers = header_utils::copy_request_headers(&req);

        match self.select_first_worker() {
            Ok(worker_url) => {
                let mut request_builder = self.client.get(format!("{}/{}", worker_url, endpoint));
                for (name, value) in headers {
                    let name_lc = name.to_lowercase();
                    if name_lc != "content-type" && name_lc != "content-length" {
                        request_builder = request_builder.header(name, value);
                    }
                }

                match request_builder.send().await {
                    Ok(res) => {
                        let status = StatusCode::from_u16(res.status().as_u16())
                            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

                        // Preserve headers from backend
                        let response_headers =
                            header_utils::preserve_response_headers(res.headers());

                        match res.bytes().await {
                            Ok(body) => {
                                let mut response = Response::new(axum::body::Body::from(body));
                                *response.status_mut() = status;
                                *response.headers_mut() = response_headers;
                                response
                            }
                            Err(e) => (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("Failed to read response: {}", e),
                            )
                                .into_response(),
                        }
                    }
                    Err(e) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Request failed: {}", e),
                    )
                        .into_response(),
                }
            }
            Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
        }
    }

    /// Convert axum HeaderMap to policy RequestHeaders (HashMap<String, String>)
    fn headers_to_request_headers(
        headers: Option<&HeaderMap>,
    ) -> Option<crate::policies::RequestHeaders> {
        headers.map(|h| {
            h.iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|v| (name.as_str().to_lowercase(), v.to_string()))
                })
                .collect()
        })
    }

    /// Select worker for a specific model considering circuit breaker state.
    ///
    /// When `preferred` is `Some` and still available (healthy + circuit closed),
    /// it is returned immediately without consulting the load-balancing policy.
    /// This allows semantic cluster routing to steer requests to a specific worker
    /// while transparently falling back to the regular policy when that worker is
    /// unhealthy or the circuit is open.
    fn select_worker_for_model(
        &self,
        model_id: Option<&str>,
        text: Option<&str>,
        headers: Option<&HeaderMap>,
        preferred: Option<&Arc<dyn Worker>>,
    ) -> Option<Arc<dyn Worker>> {
        // Fast-path: use the cluster-selected worker if it is still available.
        if let Some(pw) = preferred {
            if pw.is_available() {
                return Some(pw.clone());
            }
        }

        // Get workers for the specified model (O(1) lookup if model_id is provided).
        // Fall back to all workers when no workers are registered under that model_id
        // — this handles the common case where workers report "unknown" during startup
        // or where the client sends a model name that doesn't exactly match the registry.
        let workers = match model_id {
            Some(model) => {
                let by_model = self.worker_registry.get_by_model_fast(model);
                if by_model.is_empty() {
                    self.worker_registry.get_all()
                } else {
                    by_model
                }
            }
            None => self.worker_registry.get_all(),
        };

        let available: Vec<Arc<dyn Worker>> = workers
            .iter()
            .filter(|w| w.is_available())
            .cloned()
            .collect();
        if available.is_empty() {
            return None;
        }

        // Get the appropriate policy for this model
        let policy = match model_id {
            Some(model) => self.policy_registry.get_policy_or_default(model),
            None => self.policy_registry.get_default_policy(),
        };

        // Convert headers for policies that need them (e.g., consistent_hash)
        let request_headers = Self::headers_to_request_headers(headers);

        let idx = policy.select_worker_with_headers(&available, text, request_headers.as_ref())?;
        Some(available[idx].clone())
    }

    /// If the default policy is `lmcache_aware` in `prefix_lookup` mode,
    /// tokenize the request via a healthy worker and query the LMCache
    /// controller for the worker with the longest cached prefix.
    ///
    /// Passes the full request body (with `messages`) to `/tokenize` so
    /// vLLM applies the chat template — critical because LMCache stores
    /// KV cache keyed by template-applied token IDs.
    ///
    /// Returns `Some(worker)` when a match is found, `None` otherwise
    /// (caller falls back to normal policy selection).
    async fn lmcache_prefix_lookup_preferred<T: serde::Serialize>(
        &self,
        model_id: Option<&str>,
        typed_req: &T,
    ) -> Option<Arc<dyn Worker>> {
        use crate::policies::LMCacheAwarePolicy;

        // Get the policy and downcast to LMCacheAwarePolicy
        let policy = match model_id {
            Some(model) => self.policy_registry.get_policy_or_default(model),
            None => self.policy_registry.get_default_policy(),
        };
        let lmcache_policy = policy.as_any().downcast_ref::<LMCacheAwarePolicy>()?;

        if !lmcache_policy.is_prefix_lookup_mode() {
            return None;
        }

        // Pick any healthy worker for tokenization (stateless operation)
        let workers = self.worker_registry.get_all();
        let healthy_worker_url = workers.iter().find(|w| w.is_available())?.url().to_string();

        let timeout = std::time::Duration::from_millis(100);

        // Serialize the request to JSON — /tokenize accepts the same body
        // format as /v1/chat/completions (with messages + model)
        let request_body = serde_json::to_value(typed_req).ok()?;

        // Step 1: Tokenize — check token cache first, fall back to HTTP
        let cache_key = crate::cache::token_cache::TokenCache::compute_key(&request_body);
        let tokens = self.token_cache_get(cache_key).await.unwrap_or_else(|| None);

        let tokens = if let Some(cached_tokens) = tokens {
            RouterMetrics::record_token_cache_hit();
            RouterMetrics::record_tokenize_duration(std::time::Duration::from_millis(0), "cache");
            cached_tokens
        } else {
            RouterMetrics::record_token_cache_miss();
            let start = std::time::Instant::now();
            let fresh_tokens = LMCacheAwarePolicy::tokenize_via_worker(
                &healthy_worker_url,
                &request_body,
                timeout,
            )
            .await?;
            RouterMetrics::record_tokenize_duration(start.elapsed(), "worker");

            // Store in cache (fire-and-forget)
            self.token_cache_insert(cache_key, &fresh_tokens).await;
            if let Some(ref mc) = self.token_cache_memory {
                RouterMetrics::set_token_cache_entries(mc.len());
            }

            fresh_tokens
        };

        // Step 2: POST /lookup to controller
        let preferred_url = lmcache_policy.prefix_lookup(&tokens).await?;

        // Step 3: Find the worker Arc in the registry
        workers
            .iter()
            .find(|w| w.url() == preferred_url)
            .cloned()
    }

    /// Route a request through the exact-match cache for non-streaming requests.
    ///
    /// - If `is_stream` is true, bypasses the cache entirely (streaming responses
    ///   cannot be buffered and replayed).
    /// - On a cache hit, returns the stored body immediately.
    /// - On a cache miss, delegates to `route_typed_request`. If the response is
    ///   successful (2xx), the response body is buffered, stored in the cache, and
    ///   a new Response is constructed from the bytes.
    async fn route_with_cache<T: GenerationRequest + serde::Serialize + serde::de::DeserializeOwned + Clone>(
        &self,
        headers: Option<&HeaderMap>,
        typed_req: &T,
        route: &str,
        model_id: Option<&str>,
        is_stream: bool,
    ) -> Response {
        use axum::http::header::CONTENT_TYPE;

        let expose = self.expose_routing_headers;
        // Extract tenant name from internal header (set by authorize_request)
        let tenant = headers
            .and_then(|h| h.get(crate::server::TENANT_HEADER))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let mut decision = RoutingDecision {
            tenant,
            ..Default::default()
        };

        // Resolve model rules (alias, fallback) before anything else.
        let resolved_model = self.resolve_model(model_id, &mut decision);
        let model_id = resolved_model.as_deref();
        decision.model = model_id.map(|s| s.to_string());

        // Execute pre-routing hooks (safety checks, PII masking, etc.).
        let _hooks_span = if crate::otel_trace::is_otel_enabled() && !self.pre_routing_hooks.is_empty() {
            Some(tracing::info_span!(
                target: "otel_trace",
                "hooks.pipeline",
                otel.name = "pre-routing hooks",
                hooks.count = self.pre_routing_hooks.len(),
                hooks.outcome = Empty,
            ))
        } else {
            None
        };
        let (typed_req, hooks_ran) = if !self.pre_routing_hooks.is_empty() {
            let body_json = match serde_json::to_value(typed_req) {
                Ok(j) => j,
                Err(_) => {
                    return (StatusCode::BAD_REQUEST, "Failed to serialize request for hooks")
                        .into_response();
                }
            };
            match crate::hooks::execute(&self.pre_routing_hooks, &self.client, body_json).await {
                crate::hooks::HookOutcome::Allow { body, hooks_ran } => {
                    if let Some(ref s) = _hooks_span { s.record("hooks.outcome", "allow"); }
                    // Re-deserialize in case a hook transformed the body
                    match serde_json::from_value::<T>(body) {
                        Ok(new_req) => (std::borrow::Cow::Owned(new_req), hooks_ran),
                        Err(e) => {
                            warn!(error = %e, "hook transformed body is not valid for this request type");
                            return (StatusCode::BAD_REQUEST, "Hook produced invalid request body")
                                .into_response();
                        }
                    }
                }
                crate::hooks::HookOutcome::Reject(resp) => {
                    if let Some(ref s) = _hooks_span { s.record("hooks.outcome", "reject"); }
                    return resp;
                }
            }
        } else {
            (std::borrow::Cow::Borrowed(typed_req), Vec::new())
        };
        let typed_req = typed_req.as_ref();
        decision.hooks_ran = hooks_ran;
        drop(_hooks_span);

        if !is_stream {
            if let Ok(json) = serde_json::to_value(typed_req) {
                let cache_key = crate::cache::ResponseCache::compute_key(&json);

                // 1. Exact-match cache hit — return immediately without touching a worker
                let _cache_span = if crate::otel_trace::is_otel_enabled() {
                    Some(tracing::info_span!(
                        target: "otel_trace",
                        "cache.exact_lookup",
                        otel.name = "exact-match cache lookup",
                        cache.status = Empty,
                    ))
                } else {
                    None
                };
                if let Some((cached_body, ct)) = self.response_cache.get(cache_key).await {
                    if let Some(ref s) = _cache_span { s.record("cache.status", "hit"); }
                    let mut resp = Response::new(axum::body::Body::from(cached_body));
                    *resp.status_mut() = StatusCode::OK;
                    if let Some(ct_str) = ct {
                        if let Ok(val) = ct_str.parse::<axum::http::HeaderValue>() {
                            resp.headers_mut().insert(CONTENT_TYPE, val);
                        }
                    }
                    decision.method = Some("cache-hit");
                    decision.cache_status = Some("exact-hit");
                    RouterMetrics::record_cache_hit();
                    if expose { decision.inject_headers(&mut resp); }
                    self.decision_log.push(decision.to_record(None, route, 200, 0));
                    return resp;
                }
                if let Some(ref s) = _cache_span { s.record("cache.status", "miss"); }
                drop(_cache_span);

                // 2. Compute embedding when semantic cache or cluster routing is active.
                // The same vector is reused for both purposes to avoid double-fetching.
                let mut query_embedding: Option<Vec<f32>> = None;
                if self.semantic_cache.is_some() || self.semantic_cluster.is_some() {
                    let _emb_span = if crate::otel_trace::is_otel_enabled() {
                        Some(tracing::info_span!(
                            target: "otel_trace",
                            "embedding.fetch",
                            otel.name = "fetch embedding",
                            embedding.success = Empty,
                        ))
                    } else {
                        None
                    };
                    let text = Self::extract_request_text(&json);
                    debug!(text_len = text.len(), has_cluster = self.semantic_cluster.is_some(), "fetching embedding for routing");
                    query_embedding = self.get_embedding(&text).await;
                    if let Some(ref s) = _emb_span {
                        s.record("embedding.success", query_embedding.is_some());
                    }
                    debug!(got_embedding = query_embedding.is_some(), "embedding fetch done");
                }

                // 2a. Semantic similarity cache lookup (T-12).
                let _sem_span = if crate::otel_trace::is_otel_enabled() && query_embedding.is_some() && self.semantic_cache.is_some() {
                    Some(tracing::info_span!(
                        target: "otel_trace",
                        "cache.semantic_lookup",
                        otel.name = "semantic cache lookup",
                        cache.status = Empty,
                    ))
                } else {
                    None
                };
                if let (Some(emb), Some(sem_cache)) =
                    (query_embedding.as_ref(), &self.semantic_cache)
                {
                    if let Some((cached_body, ct)) = sem_cache.find_similar(emb).await {
                        if let Some(ref s) = _sem_span { s.record("cache.status", "hit"); }
                        debug!("Semantic cache hit (similarity ≥ {})", sem_cache.threshold());
                        let mut resp = Response::new(axum::body::Body::from(cached_body));
                        *resp.status_mut() = StatusCode::OK;
                        if let Some(ct_str) = ct {
                            if let Ok(val) = ct_str.parse::<axum::http::HeaderValue>() {
                                resp.headers_mut().insert(CONTENT_TYPE, val);
                            }
                        }
                        decision.method = Some("semantic-hit");
                        decision.cache_status = Some("semantic-hit");
                        RouterMetrics::record_cache_hit();
                        if expose { decision.inject_headers(&mut resp); }
                        self.decision_log.push(decision.to_record(None, route, 200, 0));
                        return resp;
                    }
                }

                if let Some(ref s) = _sem_span { s.record("cache.status", "miss"); }
                drop(_sem_span);
                decision.cache_status = Some("miss");
                RouterMetrics::record_cache_miss();

                // 2b. Semantic cluster routing — pick the best-matching cluster worker.
                // Returns (worker, cluster_name) so the cluster name can be logged + metered.
                let _cluster_span = if crate::otel_trace::is_otel_enabled() && query_embedding.is_some() && self.semantic_cluster.is_some() {
                    Some(tracing::info_span!(
                        target: "otel_trace",
                        "routing.cluster",
                        otel.name = "semantic cluster routing",
                        routing.cluster = Empty,
                        routing.worker = Empty,
                    ))
                } else {
                    None
                };
                let cluster_hit: Option<(Arc<dyn Worker>, &str)> =
                    if let (Some(emb), Some(sc)) =
                        (query_embedding.as_ref(), &self.semantic_cluster)
                    {
                        let all = match model_id {
                            Some(m) => {
                                let by_model = self.worker_registry.get_by_model_fast(m);
                                if by_model.is_empty() {
                                    self.worker_registry.get_all()
                                } else {
                                    by_model
                                }
                            }
                            None => self.worker_registry.get_all(),
                        };
                        let available: Vec<Arc<dyn Worker>> =
                            all.iter().filter(|w| w.is_available()).cloned().collect();
                        sc.route(emb, &available).inspect(|(w, cname)| {
                            debug!(cluster = cname, worker = w.url(), "semantic cluster selected");
                            if let Some(ref s) = _cluster_span {
                                s.record("routing.cluster", *cname);
                                s.record("routing.worker", w.url());
                            }
                        })
                    } else {
                        None
                    };

                let cluster_worker = cluster_hit.as_ref().map(|(w, _)| w.clone());

                // 3. Both caches missed — forward to backend
                let mut upstream = self
                    .route_typed_request(
                        headers,
                        typed_req,
                        route,
                        model_id,
                        cluster_worker,
                        cluster_hit.as_ref().map(|(_, name)| *name),
                        &mut decision,
                    )
                    .await;

                if upstream.status().is_success() {
                    let (parts, body) = upstream.into_parts();
                    let ct = parts
                        .headers
                        .get(CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());

                    // Buffer the body (cap at 8 MB to avoid huge responses filling the cache)
                    const MAX_CACHE_BODY: usize = 8 * 1024 * 1024;
                    match axum::body::to_bytes(body, MAX_CACHE_BODY).await {
                        Ok(bytes) => {
                            // Store in exact-match cache
                            self.response_cache
                                .insert(cache_key, bytes.clone(), ct.clone())
                                .await;
                            // Also store in semantic cache when an embedding is available
                            if let (Some(sem_cache), Some(emb)) =
                                (&self.semantic_cache, query_embedding)
                            {
                                sem_cache.insert(emb, bytes.clone(), ct).await;
                            }
                            // Rebuild the response from the buffered bytes
                            let mut resp =
                                Response::from_parts(parts, axum::body::Body::from(bytes));
                            if expose { decision.inject_headers(&mut resp); }
                            return resp;
                        }
                        Err(_) => {
                            // Body too large or read error — rebuild a minimal error response
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "Failed to buffer response for caching",
                            )
                                .into_response();
                        }
                    }
                }

                if expose { decision.inject_headers(&mut upstream); }
                return upstream;
            }
        }

        // Streaming or JSON serialisation failure — skip cache (no cluster hint)
        let mut resp = self
            .route_typed_request(headers, typed_req, route, model_id, None, None, &mut decision)
            .await;
        if expose { decision.inject_headers(&mut resp); }
        resp
    }

    pub(crate) async fn route_typed_request<T: GenerationRequest + serde::Serialize + Clone>(
        &self,
        headers: Option<&HeaderMap>,
        typed_req: &T,
        route: &str,
        model_id: Option<&str>,
        // Optional cluster-selected worker. Passed to select_worker_for_model as a
        // "preferred" hint — used when available, falls back to policy otherwise.
        preferred_worker: Option<Arc<dyn Worker>>,
        // Name of the semantic cluster that selected `preferred_worker`, if any.
        // Used for logging and Prometheus metrics.
        cluster_name: Option<&str>,
        // Accumulates routing metadata for explainability headers.
        decision: &mut RoutingDecision,
    ) -> Response {
        let _fwd_span = if crate::otel_trace::is_otel_enabled() {
            Some(tracing::info_span!(
                target: "otel_trace",
                "worker.forward",
                otel.name = %format_args!("forward {}", route),
                routing.route = route,
                routing.model = model_id.unwrap_or(""),
                routing.method = Empty,
                routing.policy = Empty,
                routing.worker = Empty,
                http.response.status_code = Empty,
                worker.duration_ms = Empty,
            ))
        } else {
            None
        };
        let start = Instant::now();
        let is_stream = typed_req.is_stream();
        let text = typed_req.extract_text_for_routing();

        if self.include_request_text {
            decision.request_text = Some(text.clone());
        }

        // ── LMCache prefix lookup pre-step (Phase 2) ──────────────────
        // When the policy is lmcache_aware in prefix_lookup mode and no
        // cluster-preferred worker is already set, perform an async lookup
        // to find which worker has the longest cached prefix for this prompt.
        let preferred_worker = if preferred_worker.is_none() {
            self.lmcache_prefix_lookup_preferred(model_id, typed_req)
                .await
                .or(preferred_worker)
        } else {
            preferred_worker
        };

        // Pre-fill decision metadata that is known before the retry loop.
        {
            let via_preferred = preferred_worker.is_some();
            if via_preferred && cluster_name.is_some() {
                decision.method = Some("cluster");
                decision.cluster = cluster_name.map(|s| s.to_string());
            } else if via_preferred {
                decision.method = Some("lmcache-prefix");
            } else {
                decision.method = Some("policy");
                let policy = match model_id {
                    Some(model) => self.policy_registry.get_policy_or_default(model),
                    None => self.policy_registry.get_default_policy(),
                };
                decision.policy = Some(policy.name().to_string());
            }
        }
        // Record routing decision on the OTel span.
        if let Some(ref s) = _fwd_span {
            if let Some(m) = decision.method { s.record("routing.method", m); }
            if let Some(ref p) = decision.policy { s.record("routing.policy", p.as_str()); }
        }

        // Shared slot so the retry closure can report the final worker URL.
        let last_worker_url: Arc<parking_lot::Mutex<Option<String>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let last_worker_url_inner = last_worker_url.clone();

        let response = RetryExecutor::execute_response_with_retry(
            &self.retry_config,
            // operation per attempt
            |_: u32| async {
                let worker = match self.select_worker_for_model(
                    model_id,
                    Some(&text),
                    headers,
                    preferred_worker.as_ref(),
                ) {
                    Some(w) => w,
                    None => {
                        RouterMetrics::record_request_error(route, "no_available_workers");
                        warn!(route, model = model_id.unwrap_or("?"), "No available workers");
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "No available workers (all circuits open or unhealthy)",
                        )
                            .into_response();
                    }
                };

                // Record the worker URL for explainability headers.
                *last_worker_url_inner.lock() = Some(worker.url().to_string());

                let via_preferred = preferred_worker
                    .as_ref()
                    .map(|pw| pw.url() == worker.url())
                    .unwrap_or(false);
                let routing_method = if via_preferred {
                    if cluster_name.is_some() { "cluster" } else { "lmcache_prefix" }
                } else {
                    "policy"
                };

                // Log the forwarding decision
                if via_preferred && cluster_name.is_some() {
                    let cname = cluster_name.unwrap_or("?");
                    info!(
                        route,
                        model = model_id.unwrap_or("?"),
                        worker = worker.url(),
                        cluster = cname,
                        "→ cluster routing"
                    );
                    RouterMetrics::record_cluster_request(cname, worker.url());
                } else if via_preferred {
                    info!(
                        route,
                        model = model_id.unwrap_or("?"),
                        worker = worker.url(),
                        routing = routing_method,
                        "→ lmcache prefix routing"
                    );
                } else {
                    info!(
                        route,
                        model = model_id.unwrap_or("?"),
                        worker = worker.url(),
                        routing = routing_method,
                        "→ policy routing"
                    );
                    // If a cluster was configured but this request fell back to policy,
                    // record the fallback (cluster_name is Some when threshold was not met).
                    if cluster_name.is_some() {
                        RouterMetrics::record_cluster_fallback(route);
                    }
                }
                RouterMetrics::record_worker_request(route, worker.url(), routing_method);

                // Optional load tracking for cache-aware policy
                // Get the policy for this model to check if it's cache-aware
                let policy = match model_id {
                    Some(model) => self.policy_registry.get_policy_or_default(model),
                    None => self.policy_registry.get_default_policy(),
                };

                let load_incremented = if policy.name() == "cache_aware" {
                    worker.increment_load();
                    RouterMetrics::set_running_requests(worker.url(), worker.load());
                    true
                } else {
                    false
                };

                // Keep a clone for potential cleanup on retry
                let worker_for_cleanup = if load_incremented {
                    Some(worker.clone())
                } else {
                    None
                };

                let response = self
                    .send_typed_request(
                        headers,
                        typed_req,
                        route,
                        worker.url(),
                        is_stream,
                        load_incremented,
                    )
                    .await;

                // Client errors (4xx) are not worker failures - only server errors (5xx)
                // should count against the circuit breaker. This matches pd_router.rs behavior.
                let status = response.status();
                worker.record_outcome(status.is_success() || status.is_client_error());

                // For retryable failures, we need to decrement load since send_typed_request
                // won't have done it (it only decrements on success or non-retryable failures)
                if is_retryable_status(response.status()) && load_incremented {
                    if let Some(cleanup_worker) = worker_for_cleanup {
                        cleanup_worker.decrement_load();
                        RouterMetrics::set_running_requests(
                            cleanup_worker.url(),
                            cleanup_worker.load(),
                        );
                    }
                }

                response
            },
            // should_retry predicate
            |res, _attempt| is_retryable_status(res.status()),
            // on_backoff hook
            |delay, attempt| {
                RouterMetrics::record_retry(route);
                RouterMetrics::record_retry_backoff_duration(delay, attempt);
            },
            // on_exhausted hook
            || RouterMetrics::record_retries_exhausted(route),
        )
        .await;

        // Populate the final worker URL from the last retry attempt.
        decision.worker = last_worker_url.lock().take();

        let duration = start.elapsed();
        let status = response.status();

        // Record final outcome on the OTel span.
        if let Some(ref s) = _fwd_span {
            if let Some(ref w) = decision.worker { s.record("routing.worker", w.as_str()); }
            s.record("http.response.status_code", status.as_u16());
            s.record("worker.duration_ms", duration.as_millis() as u64);
        }
        if status.is_success() {
            RouterMetrics::record_request(route);
            RouterMetrics::record_generate_duration(duration);
            info!(
                route,
                status = status.as_u16(),
                duration_ms = duration.as_millis(),
                "← completed"
            );
        } else {
            if !is_retryable_status(status) {
                RouterMetrics::record_request_error(route, "non_retryable_error");
            }
            warn!(
                route,
                status = status.as_u16(),
                duration_ms = duration.as_millis(),
                "← failed"
            );
        }

        // Record the decision for /admin/decisions.
        self.decision_log.push(decision.to_record(
            None, // request_id not available here; set by middleware
            route,
            status.as_u16(),
            duration.as_millis() as u64,
        ));

        // Emit per-tenant metrics if a tenant was identified.
        if let Some(ref tenant_name) = decision.tenant {
            RouterMetrics::record_tenant_request(tenant_name, route);
            RouterMetrics::record_tenant_request_duration(tenant_name, route, duration);
            if !status.is_success() {
                RouterMetrics::record_tenant_error(tenant_name, route, status.as_str());
            }
        }

        response
    }

    // Helper: return base worker URL (strips DP suffix when enabled)
    fn worker_base_url(&self, worker_url: &str) -> String {
        if self.intra_node_data_parallel_size > 1 {
            if let Ok((prefix, _)) = dp_utils::extract_dp_rank(worker_url) {
                return prefix.to_string();
            }
        }
        worker_url.to_string()
    }

    // Generic simple routing for GET/POST without JSON body
    async fn route_simple_request(
        &self,
        headers: Option<&HeaderMap>,
        endpoint: &str,
        method: Method,
    ) -> Response {
        // TODO: currently the vllm worker is using in-memory state management, so this implementation has to fan out to all workers.
        // Eventually, we need to have router to manage the chat history with a proper database, will update this implementation accordingly.
        let worker_urls = self.get_worker_urls();
        if worker_urls.is_empty() {
            return (StatusCode::SERVICE_UNAVAILABLE, "No available workers").into_response();
        }

        let mut last_response: Option<Response> = None;
        for worker_url in worker_urls {
            let base = self.worker_base_url(&worker_url);

            let url = format!("{}/{}", base, endpoint);
            let mut request_builder = match method {
                Method::GET => self.client.get(url),
                Method::POST => self.client.post(url),
                _ => {
                    return (
                        StatusCode::METHOD_NOT_ALLOWED,
                        "Unsupported method for simple routing",
                    )
                        .into_response()
                }
            };

            if let Some(hdrs) = headers {
                for (name, value) in hdrs {
                    let name_lc = name.as_str().to_lowercase();
                    if name_lc != "content-type" && name_lc != "content-length" {
                        request_builder = request_builder.header(name, value);
                    }
                }
            }

            match request_builder.send().await {
                Ok(res) => {
                    let status = StatusCode::from_u16(res.status().as_u16())
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                    let response_headers = header_utils::preserve_response_headers(res.headers());
                    match res.bytes().await {
                        Ok(body) => {
                            let mut response = Response::new(axum::body::Body::from(body));
                            *response.status_mut() = status;
                            *response.headers_mut() = response_headers;
                            if status.is_success() {
                                return response;
                            }
                            last_response = Some(response);
                        }
                        Err(e) => {
                            last_response = Some(
                                (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    format!("Failed to read response: {}", e),
                                )
                                    .into_response(),
                            );
                        }
                    }
                }
                Err(e) => {
                    last_response = Some(
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Request failed: {}", e),
                        )
                            .into_response(),
                    );
                }
            }
        }

        last_response
            .unwrap_or_else(|| (StatusCode::BAD_GATEWAY, "No worker response").into_response())
    }

    // Route a GET request with provided headers to a specific endpoint
    async fn route_get_request(&self, headers: Option<&HeaderMap>, endpoint: &str) -> Response {
        self.route_simple_request(headers, endpoint, Method::GET)
            .await
    }

    // Route a POST request with empty body to a specific endpoint
    async fn route_post_empty_request(
        &self,
        headers: Option<&HeaderMap>,
        endpoint: &str,
    ) -> Response {
        self.route_simple_request(headers, endpoint, Method::POST)
            .await
    }

    // Send typed request directly without conversion
    async fn send_typed_request<T: serde::Serialize>(
        &self,
        headers: Option<&HeaderMap>,
        typed_req: &T,
        route: &str,
        worker_url: &str,
        is_stream: bool,
        load_incremented: bool, // Whether load was incremented for this request
    ) -> Response {
        let (mut request_builder, extracted_dp_rank) = if self.intra_node_data_parallel_size > 1 {
            let (worker_url_prefix, dp_rank) = match dp_utils::extract_dp_rank(worker_url) {
                Ok(tup) => tup,
                Err(e) => {
                    error!("Failed to extract dp_rank: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to extract dp_rank: {}", e),
                    )
                        .into_response();
                }
            };

            // Parse the request body
            let json_val = match serde_json::to_value(typed_req) {
                Ok(j) => j,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        format!("Convert into serde_json::Value failed: {}", e),
                    )
                        .into_response();
                }
            };

            // Use the original json_val without modification

            (
                self.client
                    .post(format!("{}{}", worker_url_prefix, route))
                    .json(&json_val),
                Some(dp_rank),
            )
        } else {
            (
                self.client
                    .post(format!("{}{}", worker_url, route))
                    .json(typed_req),
                None,
            ) // Use json() directly with typed request
        };

        // Copy all headers from original request if provided (except auth — handled below)
        if let Some(headers) = headers {
            for (name, value) in headers {
                // Skip Content-Type, Content-Length (.json() sets them) and Authorization
                // (per-worker key injected below)
                if *name != CONTENT_TYPE
                    && *name != CONTENT_LENGTH
                    && *name != http::header::AUTHORIZATION
                {
                    request_builder = request_builder.header(name, value);
                }
            }
        }

        // Add authorization: use per-worker key if set, fall back to global api_key
        {
            let base_url = worker_url.split('@').next().unwrap_or(worker_url);
            let worker = self.worker_registry.get_by_url(base_url);
            let api_key_guard = self.api_key.read().await;
            let effective_key = worker
                .as_ref()
                .and_then(|w| w.api_key().map(|s| s.to_string()))
                .or_else(|| api_key_guard.as_ref().cloned());
            if let Some(key) = effective_key {
                request_builder =
                    request_builder.header("Authorization", format!("Bearer {}", key));
            }
        }

        // Add X-data-parallel-rank header for DP-aware routing
        if let Some(dp_rank) = extracted_dp_rank {
            request_builder = request_builder.header("X-data-parallel-rank", dp_rank.to_string());
        }

        let res = match request_builder.send().await {
            Ok(res) => res,
            Err(e) => {
                error!(
                    "Failed to send typed request worker_url={} route={} error={}",
                    worker_url, route, e
                );

                // Decrement load on error if it was incremented
                if load_incremented {
                    if let Some(worker) = self.worker_registry.get_by_url(worker_url) {
                        worker.decrement_load();
                        RouterMetrics::set_running_requests(worker_url, worker.load());
                    }
                }

                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Request failed: {}", e),
                )
                    .into_response();
            }
        };

        let status = StatusCode::from_u16(res.status().as_u16())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        if !is_stream {
            // For non-streaming requests, preserve headers
            let response_headers = header_utils::preserve_response_headers(res.headers());

            let response = match res.bytes().await {
                Ok(body) => {
                    let mut response = Response::new(axum::body::Body::from(body));
                    *response.status_mut() = status;
                    *response.headers_mut() = response_headers;
                    response
                }
                Err(e) => {
                    // IMPORTANT: Decrement load on error before returning
                    if load_incremented {
                        if let Some(worker) = self.worker_registry.get_by_url(worker_url) {
                            worker.decrement_load();
                            RouterMetrics::set_running_requests(worker_url, worker.load());
                        }
                    }

                    let error_msg = format!("Failed to get response body: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, error_msg).into_response()
                }
            };

            // Decrement load counter for non-streaming requests if it was incremented
            if load_incremented {
                if let Some(worker) = self.worker_registry.get_by_url(worker_url) {
                    worker.decrement_load();
                    RouterMetrics::set_running_requests(worker_url, worker.load());
                }
            }

            response
        } else if load_incremented {
            // For streaming with load tracking, we need to manually decrement when done
            let registry = Arc::clone(&self.worker_registry);
            let worker_url = worker_url.to_string();

            // Preserve headers for streaming response
            let mut response_headers = header_utils::preserve_response_headers(res.headers());
            // Ensure we set the correct content-type for SSE
            response_headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));

            let stream = res.bytes_stream();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

            // Spawn task to forward stream and detect completion
            tokio::spawn(async move {
                let mut stream = stream;
                let mut decremented = false;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            // Check for stream end marker
                            if bytes
                                .as_ref()
                                .windows(12)
                                .any(|window| window == b"data: [DONE]")
                            {
                                if let Some(worker) = registry.get_by_url(&worker_url) {
                                    worker.decrement_load();
                                    RouterMetrics::set_running_requests(&worker_url, worker.load());
                                    decremented = true;
                                }
                            }
                            if tx.send(Ok(bytes)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(format!("Stream error: {}", e)));
                            break;
                        }
                    }
                }
                if !decremented {
                    if let Some(worker) = registry.get_by_url(&worker_url) {
                        worker.decrement_load();
                        RouterMetrics::set_running_requests(&worker_url, worker.load());
                    }
                }
            });

            let stream = UnboundedReceiverStream::new(rx);
            let body = Body::from_stream(stream);

            let mut response = Response::new(body);
            *response.status_mut() = status;
            *response.headers_mut() = response_headers;
            response
        } else {
            // For requests without load tracking, just stream
            // Preserve headers for streaming response
            let mut response_headers = header_utils::preserve_response_headers(res.headers());
            // Ensure we set the correct content-type for SSE
            response_headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));

            let stream = res.bytes_stream();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

            // Spawn task to forward stream
            tokio::spawn(async move {
                let mut stream = stream;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if tx.send(Ok(bytes)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(format!("Stream error: {}", e)));
                            break;
                        }
                    }
                }
            });

            let stream = UnboundedReceiverStream::new(rx);
            let body = Body::from_stream(stream);

            let mut response = Response::new(body);
            *response.status_mut() = status;
            *response.headers_mut() = response_headers;
            response
        }
    }

    pub async fn add_worker(&self, worker_url: &str) -> Result<String, String> {
        let start_time = std::time::Instant::now();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.worker_startup_timeout_secs))
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

        loop {
            if start_time.elapsed() > Duration::from_secs(self.worker_startup_timeout_secs) {
                error!(
                    "Timeout {}s waiting for worker {} to become healthy. Please set --router-worker-startup-timeout-secs (vllm_router.launch_server) or --worker-startup-timeout-secs (vllm_worker.router) to a larger value",
                    self.worker_startup_timeout_secs, worker_url
                );
                return Err(format!(
                    "Timeout {}s waiting for worker {} to become healthy. Please set --router-worker-startup-timeout-secs (vllm_router.launch_server) or --worker-startup-timeout-secs (vllm_worker.router) to a larger value",
                    self.worker_startup_timeout_secs, worker_url
                ));
            }

            match client.get(format!("{}/health", worker_url)).send().await {
                Ok(res) => {
                    if res.status().is_success() {
                        if self.intra_node_data_parallel_size > 1 {
                            // Expand worker URL into multiple DP-aware URLs based on configured intra_node_data_parallel_size
                            // (e.g., "http://host:8000" → "http://host:8000@0", "@1", etc.)
                            // without querying the worker
                            let url_vec = vec![String::from(worker_url)];
                            let api_key_guard = self.api_key.read().await;
                            let dp_url_vec = dp_utils::get_dp_aware_workers(
                                &url_vec,
                                &api_key_guard,
                                self.intra_node_data_parallel_size,
                            )
                            .await
                            .map_err(|e| format!("Failed to get dp-aware workers: {}", e))?;
                            let mut worker_added: bool = false;
                            for dp_url in &dp_url_vec {
                                if self.worker_registry.get_by_url(dp_url).is_some() {
                                    warn!("Worker {} already exists", dp_url);
                                    continue;
                                }
                                info!("Added worker: {}", dp_url);
                                let base_url_for_lookup =
                                    dp_url.split('@').next().unwrap_or(dp_url);
                                let worker_api_keys_guard = self.worker_api_keys.read().await;
                                let worker_api_key = worker_api_keys_guard
                                    .get(base_url_for_lookup)
                                    .cloned();
                                drop(worker_api_keys_guard);
                                let model_id = Self::fetch_model_id_from_server(
                                    &client,
                                    dp_url,
                                    worker_api_key.as_deref(),
                                )
                                .await;
                                let mut labels = std::collections::HashMap::new();
                                labels.insert("model_id".to_string(), model_id);
                                let (base_url, dp_rank) = dp_utils::parse_worker_url(dp_url);
                                let new_worker =
                                    DPAwareWorker::new(
                                        base_url,
                                        dp_rank.unwrap_or(0),
                                        self.intra_node_data_parallel_size,
                                        WorkerType::Regular,
                                    )
                                        .with_labels(labels)
                                        .with_api_key(worker_api_key)
                                        .with_circuit_breaker_config(
                                            self.circuit_breaker_config.clone(),
                                        );

                                let worker_arc: Arc<dyn Worker> = Arc::new(new_worker);
                                self.worker_registry.register(worker_arc.clone());

                                // Notify PolicyRegistry about the new worker
                                let model_id = worker_arc.model_id();
                                let policy = self.policy_registry.on_worker_added(model_id, None);

                                // If this is a cache-aware policy, update it with all workers for this model
                                if policy.name() == "cache_aware" {
                                    if let Some(cache_aware) = policy
                                        .as_any()
                                        .downcast_ref::<crate::policies::CacheAwarePolicy>(
                                    ) {
                                        let model_workers =
                                            self.worker_registry.get_by_model_fast(model_id);
                                        cache_aware.init_workers(&model_workers);
                                    }
                                }

                                worker_added = true;
                            }
                            if !worker_added {
                                return Err(format!("No worker added for {}", worker_url));
                            }
                        } else {
                            if self.worker_registry.get_by_url(worker_url).is_some() {
                                return Err(format!("Worker {} already exists", worker_url));
                            }
                            info!("Added worker: {}", worker_url);
                            let worker_api_keys_guard = self.worker_api_keys.read().await;
                            let worker_api_key = worker_api_keys_guard.get(worker_url).cloned();
                            drop(worker_api_keys_guard);
                            let model_id = Self::fetch_model_id_from_server(
                                &client,
                                worker_url,
                                worker_api_key.as_deref(),
                            )
                            .await;
                            let mut labels = std::collections::HashMap::new();
                            labels.insert("model_id".to_string(), model_id);
                            let new_worker =
                                BasicWorker::new(worker_url.to_string(), WorkerType::Regular)
                                    .with_labels(labels)
                                    .with_circuit_breaker_config(
                                        self.circuit_breaker_config.clone(),
                                    );

                            let worker_arc = Arc::new(new_worker);
                            self.worker_registry.register(worker_arc.clone());

                            // Notify PolicyRegistry about the new worker
                            let model_id = worker_arc.model_id();
                            let policy = self.policy_registry.on_worker_added(model_id, None);

                            // If this is a cache-aware policy, add this worker to it
                            if policy.name() == "cache_aware" {
                                if let Some(cache_aware) = policy
                                    .as_any()
                                    .downcast_ref::<crate::policies::CacheAwarePolicy>(
                                ) {
                                    // Get all workers for this model
                                    let model_workers =
                                        self.worker_registry.get_by_model_fast(model_id);
                                    cache_aware.init_workers(&model_workers);
                                }
                            }
                        }

                        RouterMetrics::set_active_workers(self.worker_registry.get_all().len());

                        return Ok(format!("Successfully added worker: {}", worker_url));
                    } else {
                        debug!(
                            "Worker {} health check pending - status: {}",
                            worker_url,
                            res.status()
                        );
                        // if the url does not have http or https prefix, warn users
                        if !worker_url.starts_with("http://") && !worker_url.starts_with("https://")
                        {
                            warn!("The worker url {} does not have http or https prefix. Please add the prefix to the url.", worker_url);
                        }

                        tokio::time::sleep(Duration::from_secs(
                            self.worker_startup_check_interval_secs,
                        ))
                        .await;
                        continue;
                    }
                }
                Err(e) => {
                    debug!("Worker {} health check pending - error: {}", worker_url, e);

                    // if the url does not have http or https prefix, warn users
                    if !worker_url.starts_with("http://") && !worker_url.starts_with("https://") {
                        warn!("The worker url {} does not have http or https prefix. Please add the prefix to the url.", worker_url);
                    }

                    tokio::time::sleep(Duration::from_secs(
                        self.worker_startup_check_interval_secs,
                    ))
                    .await;
                    continue;
                }
            }
        }
    }

    pub fn remove_worker(&self, worker_url: &str) {
        if self.intra_node_data_parallel_size > 1 {
            // remove dp-aware workers in a prefix-matching fashion
            // without contacting the remote worker
            let mut removed_workers: Vec<String> = Vec::new();
            let worker_url_prefix = format!("{}@", worker_url);

            // Find and remove all workers with matching prefix
            let all_workers = self.worker_registry.get_all();
            for w in all_workers.iter() {
                if w.url().starts_with(&worker_url_prefix) {
                    // Get model_id before removing
                    let model_id = w.model_id().to_string();

                    if self.worker_registry.remove_by_url(w.url()).is_some() {
                        info!("Removed worker: {}", w.url());
                        removed_workers.push(w.url().to_string());

                        // Notify PolicyRegistry about the removed worker
                        self.policy_registry.on_worker_removed(&model_id);
                    } else {
                        warn!("Worker {} not found, skipping removal", w.url());
                    }
                }
            }

            RouterMetrics::set_active_workers(self.worker_registry.get_all().len());

            // If any models are using cache aware policy, remove the workers from the tree
            // Check each removed worker's model and get its policy
            for dp_url in removed_workers.iter() {
                if let Some(worker) = self.worker_registry.get_by_url(dp_url) {
                    let model_id = worker.model_id();
                    if let Some(policy) = self.policy_registry.get_policy(model_id) {
                        if let Some(cache_aware) = policy
                            .as_any()
                            .downcast_ref::<crate::policies::CacheAwarePolicy>()
                        {
                            cache_aware.remove_worker_by_url(dp_url);
                            info!("Removed worker from cache-aware tree: {}", dp_url);
                        }
                    }
                }
            }
        } else {
            // Get the worker first to extract model_id
            let model_id = if let Some(worker) = self.worker_registry.get_by_url(worker_url) {
                worker.model_id().to_string()
            } else {
                warn!("Worker {} not found, skipping removal", worker_url);
                return;
            };

            if self.worker_registry.remove_by_url(worker_url).is_some() {
                info!("Removed worker: {}", worker_url);

                // Notify PolicyRegistry about the removed worker
                self.policy_registry.on_worker_removed(&model_id);

                RouterMetrics::set_active_workers(self.worker_registry.get_all().len());
            }

            // If the model is using cache aware policy, remove the worker from the tree
            if let Some(policy) = self.policy_registry.get_policy(&model_id) {
                if let Some(cache_aware) = policy
                    .as_any()
                    .downcast_ref::<crate::policies::CacheAwarePolicy>()
                {
                    cache_aware.remove_worker_by_url(worker_url);
                    info!("Removed worker from cache-aware tree: {}", worker_url);
                }
            }
        }
    }

    async fn get_worker_load(&self, worker_url: &str) -> Option<isize> {
        let worker_url = if self.intra_node_data_parallel_size > 1 {
            // Need to extract the URL from "http://host:port@dp_rank"
            let (worker_url_prefix, _dp_rank) = match dp_utils::extract_dp_rank(worker_url) {
                Ok(tup) => tup,
                Err(e) => {
                    error!("Failed to extract dp_rank: {}", e);
                    return None;
                }
            };
            worker_url_prefix
        } else {
            worker_url
        };

        match self
            .client
            .get(format!("{}/get_load", worker_url))
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => match res.bytes().await {
                Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(data) => data
                        .get("load")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as isize),
                    Err(e) => {
                        debug!("Failed to parse load response from {}: {}", worker_url, e);
                        None
                    }
                },
                Err(e) => {
                    debug!("Failed to read load response from {}: {}", worker_url, e);
                    None
                }
            },
            Ok(res) => {
                debug!(
                    "Worker {} returned non-success status: {}",
                    worker_url,
                    res.status()
                );
                None
            }
            Err(e) => {
                debug!("Failed to get load from {}: {}", worker_url, e);
                None
            }
        }
    }

    // Background task to monitor worker loads
    async fn monitor_worker_loads(
        worker_urls: Vec<String>,
        tx: tokio::sync::watch::Sender<HashMap<String, isize>>,
        interval_secs: u64,
        policy: Arc<dyn LoadBalancingPolicy>,
        client: Client,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

        loop {
            interval.tick().await;

            let mut loads = HashMap::new();
            for url in &worker_urls {
                if let Some(load) = Self::get_worker_load_static(&client, url).await {
                    loads.insert(url.clone(), load);
                }
            }

            if !loads.is_empty() {
                // Update policy with new loads
                policy.update_loads(&loads);

                // Send to watchers
                if let Err(e) = tx.send(loads) {
                    error!("Failed to send load update: {}", e);
                }
            }
        }
    }

    // Static version of get_worker_load for use in monitoring task
    async fn get_worker_load_static(client: &reqwest::Client, worker_url: &str) -> Option<isize> {
        let worker_url = if worker_url.contains("@") {
            // Need to extract the URL from "http://host:port@dp_rank"
            let (worker_url_prefix, _dp_rank) = match dp_utils::extract_dp_rank(worker_url) {
                Ok(tup) => tup,
                Err(e) => {
                    debug!("Failed to extract dp_rank: {}", e);
                    return None;
                }
            };
            worker_url_prefix
        } else {
            worker_url
        };

        match client.get(format!("{}/get_load", worker_url)).send().await {
            Ok(res) if res.status().is_success() => match res.bytes().await {
                Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                    Ok(data) => data
                        .get("load")
                        .and_then(|v| v.as_i64())
                        .map(|v| v as isize),
                    Err(e) => {
                        debug!("Failed to parse load response from {}: {}", worker_url, e);
                        None
                    }
                },
                Err(e) => {
                    debug!("Failed to read load response from {}: {}", worker_url, e);
                    None
                }
            },
            Ok(res) => {
                debug!(
                    "Worker {} returned non-success status: {}",
                    worker_url,
                    res.status()
                );
                None
            }
            Err(e) => {
                debug!("Failed to get load from {}: {}", worker_url, e);
                None
            }
        }
    }

    async fn build_rerank_response(
        req: &RerankRequest,
        response: Response,
    ) -> anyhow::Result<Response> {
        let (_, response_body) = response.into_parts();
        let body_bytes = to_bytes(response_body, usize::MAX).await?;
        let rerank_results = serde_json::from_slice::<Vec<RerankResult>>(&body_bytes)?;
        let mut rerank_response =
            RerankResponse::new(rerank_results, req.model.clone(), req.rid.clone());
        rerank_response.sort_by_score();
        if let Some(top_k) = req.top_k {
            rerank_response.apply_top_k(top_k);
        }
        if !req.return_documents {
            rerank_response.drop_documents();
        }
        Ok(Json(rerank_response).into_response())
    }
}

use async_trait::async_trait;

#[async_trait]
impl WorkerManagement for Router {
    async fn add_worker(&self, worker_url: &str) -> Result<String, String> {
        Router::add_worker(self, worker_url).await
    }

    fn remove_worker(&self, worker_url: &str) {
        Router::remove_worker(self, worker_url)
    }

    fn get_worker_urls(&self) -> Vec<String> {
        Router::get_worker_urls(self)
    }

    fn drain_worker(&self, worker_url: &str) -> Result<(), String> {
        let worker = self
            .worker_registry
            .get_by_url(worker_url)
            .ok_or_else(|| format!("Worker {} not found", worker_url))?;
        worker.set_draining(true);
        Ok(())
    }

    async fn reload_config(
        &self,
        config: &crate::config::RouterConfig,
    ) -> Result<String, String> {
        self.update_auth_config(config.api_key.clone(), config.worker_api_keys.clone())
            .await;
        Ok("Config reloaded: api_key, worker_api_keys updated".to_string())
    }
}

impl Router {
    async fn aggregate_models(&self) -> Response {
        let worker_urls = self.worker_registry.get_all_urls();

        if worker_urls.is_empty() {
            return (StatusCode::SERVICE_UNAVAILABLE, "No workers available").into_response();
        }

        let client = &self.client;
        let worker_api_keys = self.worker_api_keys.read().await.clone();
        let futures: Vec<_> = worker_urls
            .into_iter()
            .map(|worker_url| {
                let worker_api_keys = &worker_api_keys;
                async move {
                let url = format!("{}/v1/models", worker_url.trim_end_matches('/'));
                let base_url = worker_url.split('@').next().unwrap_or(&worker_url);
                let mut req = client.get(&url);
                if let Some(key) = worker_api_keys.get(base_url) {
                    req = req.bearer_auth(key);
                }
                match req.send().await {
                    Ok(res) => {
                        if res.status().is_success() {
                            match res.json::<serde_json::Value>().await {
                                Ok(json) => Some(json),
                                Err(e) => {
                                    warn!("Failed to parse models from {}: {}", worker_url, e);
                                    None
                                }
                            }
                        } else {
                            warn!("Worker {} returned status {}", worker_url, res.status());
                            None
                        }
                    }
                    Err(e) => {
                        warn!("Failed to fetch models from {}: {}", worker_url, e);
                        None
                    }
                }
            }})
            .collect();

        let results = futures_util::future::join_all(futures).await;

        let mut all_models: Vec<serde_json::Value> = Vec::new();
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

        for result in results.into_iter().flatten() {
            if let Some(data) = result.get("data").and_then(|d| d.as_array()) {
                for model in data {
                    if let Some(id) = model.get("id").and_then(|i| i.as_str()) {
                        if seen_ids.insert(id.to_string()) {
                            all_models.push(model.clone());
                        }
                    }
                }
            }
        }

        if all_models.is_empty() {
            (StatusCode::SERVICE_UNAVAILABLE, "No models available from workers").into_response()
        } else {
            Json(serde_json::json!({
                "object": "list",
                "data": all_models
            }))
            .into_response()
        }
    }
}

#[async_trait]
impl RouterTrait for Router {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn health(&self, _req: Request<Body>) -> Response {
        let workers = self.worker_registry.get_all();
        let unhealthy_servers: Vec<_> = workers
            .iter()
            .filter(|w| !w.is_healthy())
            .map(|w| w.url().to_string())
            .collect();

        if unhealthy_servers.is_empty() {
            (StatusCode::OK, "All servers healthy").into_response()
        } else {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Unhealthy servers: {:?}", unhealthy_servers),
            )
                .into_response()
        }
    }

    async fn health_generate(&self, req: Request<Body>) -> Response {
        self.proxy_get_request(req, "health_generate").await
    }

    async fn get_server_info(&self, req: Request<Body>) -> Response {
        self.proxy_get_request(req, "get_server_info").await
    }

    async fn get_models(&self, _req: Request<Body>) -> Response {
        self.aggregate_models().await
    }

    async fn get_model_info(&self, req: Request<Body>) -> Response {
        self.proxy_get_request(req, "get_model_info").await
    }

    async fn route_generate(
        &self,
        headers: Option<&HeaderMap>,
        body: &GenerateRequest,
        model_id: Option<&str>,
    ) -> Response {
        let mut decision = RoutingDecision::default();
        let resolved = self.resolve_model(model_id, &mut decision);
        let model_id = resolved.as_deref();
        decision.model = model_id.map(|s| s.to_string());
        let mut resp = self.route_typed_request(headers, body, "/generate", model_id, None, None, &mut decision).await;
        if self.expose_routing_headers { decision.inject_headers(&mut resp); }
        resp
    }

    async fn route_chat(
        &self,
        headers: Option<&HeaderMap>,
        body: &ChatCompletionRequest,
        model_id: Option<&str>,
    ) -> Response {
        self.route_with_cache(headers, body, "/v1/chat/completions", model_id, body.stream)
            .await
    }

    async fn route_completion(
        &self,
        headers: Option<&HeaderMap>,
        body: &CompletionRequest,
        model_id: Option<&str>,
    ) -> Response {
        self.route_with_cache(headers, body, "/v1/completions", model_id, body.stream)
            .await
    }

    async fn route_responses(
        &self,
        headers: Option<&HeaderMap>,
        body: &ResponsesRequest,
        model_id: Option<&str>,
    ) -> Response {
        let mut decision = RoutingDecision::default();
        let resolved = self.resolve_model(model_id, &mut decision);
        let model_id = resolved.as_deref();
        decision.model = model_id.map(|s| s.to_string());
        let mut resp = self.route_typed_request(headers, body, "/v1/responses", model_id, None, None, &mut decision).await;
        if self.expose_routing_headers { decision.inject_headers(&mut resp); }
        resp
    }

    async fn get_response(&self, headers: Option<&HeaderMap>, response_id: &str) -> Response {
        let endpoint = format!("v1/responses/{}", response_id);
        self.route_get_request(headers, &endpoint).await
    }

    async fn cancel_response(&self, headers: Option<&HeaderMap>, response_id: &str) -> Response {
        let endpoint = format!("v1/responses/{}/cancel", response_id);
        self.route_post_empty_request(headers, &endpoint).await
    }

    async fn route_embeddings(
        &self,
        headers: Option<&HeaderMap>,
        body: &EmbeddingRequest,
        model_id: Option<&str>,
    ) -> Response {
        // Record embeddings-specific metrics in addition to general request metrics
        let start = Instant::now();
        let mut decision = RoutingDecision::default();
        let resolved = self.resolve_model(model_id, &mut decision);
        let model_id = resolved.as_deref();
        decision.model = model_id.map(|s| s.to_string());
        let mut res = self
            .route_typed_request(headers, body, "/v1/embeddings", model_id, None, None, &mut decision)
            .await;
        if self.expose_routing_headers { decision.inject_headers(&mut res); }

        // Embedding specific metrics
        if res.status().is_success() {
            RouterMetrics::record_embeddings_request();
            RouterMetrics::record_embeddings_duration(start.elapsed());
        } else {
            let error_type = format!("http_{}", res.status().as_u16());
            RouterMetrics::record_embeddings_error(&error_type);
        }

        res
    }

    async fn route_rerank(
        &self,
        headers: Option<&HeaderMap>,
        body: &RerankRequest,
        model_id: Option<&str>,
    ) -> Response {
        if let Err(e) = body.validate() {
            return (StatusCode::BAD_REQUEST, e).into_response();
        }
        let mut decision = RoutingDecision::default();
        let resolved = self.resolve_model(model_id, &mut decision);
        let model_id = resolved.as_deref();
        decision.model = model_id.map(|s| s.to_string());
        let response = self
            .route_typed_request(headers, body, "/v1/rerank", model_id, None, None, &mut decision)
            .await;
        let mut final_resp = if response.status().is_success() {
            match Self::build_rerank_response(body, response).await {
                Ok(rerank_response) => rerank_response,
                Err(e) => {
                    error!("Failed to build rerank response: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to build rerank response".to_string(),
                    )
                        .into_response();
                }
            }
        } else {
            response
        };
        if self.expose_routing_headers { decision.inject_headers(&mut final_resp); }
        final_resp
    }

    async fn cache_len(&self) -> usize {
        self.response_cache.len().await
    }

    async fn semantic_cache_len(&self) -> usize {
        match &self.semantic_cache {
            Some(sc) => sc.len().await,
            None => 0,
        }
    }

    async fn flush_cache(&self) -> Response {
        // Get all worker URLs
        let worker_urls = self.get_worker_urls();

        // Send requests to all workers concurrently without headers
        let mut tasks = Vec::new();
        for worker_url in &worker_urls {
            let worker_url = if self.intra_node_data_parallel_size > 1 {
                // Need to extract the URL from "http://host:port@dp_rank"
                let (worker_url_prefix, _dp_rank) = match dp_utils::extract_dp_rank(worker_url) {
                    Ok(tup) => tup,
                    Err(e) => {
                        error!("Failed to extract dp_rank: {}", e);
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("Failed to extract dp_rank: {}", e),
                        )
                            .into_response();
                    }
                };
                worker_url_prefix
            } else {
                worker_url
            };
            let request_builder = self.client.post(format!("{}/flush_cache", worker_url));
            tasks.push(request_builder.send());
        }

        // Wait for all responses
        let results = futures_util::future::join_all(tasks).await;

        // Check if all succeeded
        let all_success = results.iter().all(|r| {
            r.as_ref()
                .map(|res| res.status().is_success())
                .unwrap_or(false)
        });

        if all_success {
            (StatusCode::OK, "Cache flushed on all servers").into_response()
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cache flush failed on one or more servers",
            )
                .into_response()
        }
    }

    async fn get_worker_loads(&self) -> Response {
        let urls = self.get_worker_urls();
        let mut loads = Vec::new();

        // Get loads from all workers
        for url in &urls {
            let load = self.get_worker_load(url).await.unwrap_or(-1);
            loads.push(serde_json::json!({
                "worker": url,
                "load": load
            }));
        }

        Json(serde_json::json!({
            "workers": loads
        }))
        .into_response()
    }

    fn router_type(&self) -> &'static str {
        "regular"
    }

    fn readiness(&self) -> Response {
        // Regular router is ready if it has at least one healthy worker
        let workers = self.worker_registry.get_all();
        let healthy_count = workers.iter().filter(|w| w.is_healthy()).count();
        let total_workers = workers.len();

        if healthy_count > 0 {
            Json(serde_json::json!({
                "status": "ready",
                "healthy_workers": healthy_count,
                "total_workers": total_workers
            }))
            .into_response()
        } else {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "status": "not_ready",
                    "reason": "no healthy workers available",
                    "total_workers": total_workers
                })),
            )
                .into_response()
        }
    }

    /// Route a transparent proxy request to a backend worker
    /// Forwards the request as-is to a selected worker
    async fn route_transparent(
        &self,
        headers: Option<&HeaderMap>,
        path: &str,
        method: &Method,
        body: serde_json::Value,
    ) -> Response {
        debug!("Transparent proxy: routing {} {} to backend", method, path);

        // Select a worker (filter by availability like select_worker_for_model)
        let all_workers = self.worker_registry.get_all();
        let workers: Vec<Arc<dyn Worker>> = all_workers
            .iter()
            .filter(|w| w.is_available())
            .cloned()
            .collect();
        if workers.is_empty() {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "No available workers".to_string(),
            )
                .into_response();
        }

        let policy = self.policy_registry.get_default_policy();
        let request_text = serde_json::to_string(&body).ok();
        let request_headers = Self::headers_to_request_headers(headers);
        let worker_idx = match policy.select_worker_with_headers(
            &workers,
            request_text.as_deref(),
            request_headers.as_ref(),
        ) {
            Some(idx) => idx,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Failed to select a worker".to_string(),
                )
                    .into_response();
            }
        };

        let worker: &dyn Worker = workers[worker_idx].as_ref();
        let url = worker.endpoint_url(path);

        debug!("Transparent proxy: forwarding to {}", url);

        // Build the request
        let mut request_builder = match *method {
            Method::GET => self.client.get(&url),
            Method::POST => self.client.post(&url),
            Method::PUT => self.client.put(&url),
            Method::DELETE => self.client.delete(&url),
            Method::PATCH => self.client.patch(&url),
            Method::HEAD => self.client.head(&url),
            _ => {
                return (
                    StatusCode::METHOD_NOT_ALLOWED,
                    format!("Method {} not supported", method),
                )
                    .into_response();
            }
        };

        // Add X-data-parallel-rank header for DP-aware routing
        request_builder = dp_utils::add_dp_rank_header(request_builder, worker.dp_rank());

        // Propagate trace + Semantic Router metadata headers to the worker
        request_builder = header_utils::propagate_all_routing_headers(request_builder, headers);

        // Add JSON body if not null/empty
        if !body.is_null() {
            request_builder = request_builder.json(&body);
        }

        // Add authorization: use per-worker key if set, fall back to global api_key
        let api_key_guard = self.api_key.read().await;
        let effective_key = worker.api_key().or(api_key_guard.as_deref());
        if let Some(key) = effective_key {
            request_builder = request_builder.header("Authorization", format!("Bearer {}", key));
        }

        // Send request
        match request_builder.send().await {
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();

                // Stream the response body
                let body = Body::from_stream(response.bytes_stream());
                let mut response_builder = Response::builder().status(status.as_u16());

                for (name, value) in headers.iter() {
                    if name != "transfer-encoding" && name != "content-length" {
                        response_builder = response_builder.header(name, value);
                    }
                }

                match response_builder.body(body) {
                    Ok(response) => response,
                    Err(e) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to build response: {}", e),
                    )
                        .into_response(),
                }
            }
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                format!("Backend request failed: {}", e),
            )
                .into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn create_test_regular_router() -> Router {
        // Create registries
        let worker_registry = Arc::new(WorkerRegistry::new());
        let policy_registry = Arc::new(PolicyRegistry::new(
            crate::config::types::PolicyConfig::RoundRobin,
        ));

        // Register test workers
        let worker1 = BasicWorker::new("http://worker1:8080".to_string(), WorkerType::Regular);
        let worker2 = BasicWorker::new("http://worker2:8080".to_string(), WorkerType::Regular);
        worker_registry.register(Arc::new(worker1));
        worker_registry.register(Arc::new(worker2));

        let (_, rx) = tokio::sync::watch::channel(HashMap::new());
        Router {
            worker_registry,
            policy_registry,
            worker_startup_timeout_secs: 5,
            worker_startup_check_interval_secs: 1,
            intra_node_data_parallel_size: 1,
            api_key: Arc::new(tokio::sync::RwLock::new(None)),
            worker_api_keys: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            client: Client::new(),
            retry_config: RetryConfig::default(),
            circuit_breaker_config: CircuitBreakerConfig::default(),
            _worker_loads: Arc::new(rx),
            _load_monitor_handle: None,
            response_cache: Arc::new(crate::cache::ResponseCache::new(128, 60)) as Arc<dyn crate::cache::traits::ExactMatchCache>,
            semantic_cache: None,
            embeddings_url: None,
            embeddings_api_key: None,
            embeddings_model: "default".to_string(),
            embedding_timeout_ms: 500,
            semantic_cluster: None,
            expose_routing_headers: false,
            decision_log: Arc::new(crate::admin::DecisionLog::new(10)),
            model_rules: Vec::new(),
            pre_routing_hooks: Vec::new(),
            include_request_text: false,
            token_cache_memory: None,
            #[cfg(feature = "redis-cache")]
            token_cache_redis: None,
        }
    }

    #[test]
    fn test_router_get_worker_urls_regular() {
        let router = create_test_regular_router();
        let urls = router.get_worker_urls();

        assert_eq!(urls.len(), 2);
        assert!(urls.contains(&"http://worker1:8080".to_string()));
        assert!(urls.contains(&"http://worker2:8080".to_string()));
    }

    #[test]
    fn test_select_first_worker_regular() {
        let router = create_test_regular_router();
        let result = router.select_first_worker();

        assert!(result.is_ok());
        let url = result.unwrap();
        // DashMap doesn't guarantee order, so just check we get one of the workers
        assert!(url == "http://worker1:8080" || url == "http://worker2:8080");
    }

    #[tokio::test]
    async fn test_wait_for_healthy_workers_empty_list() {
        // Empty list will return error immediately
        let result = Router::wait_for_healthy_workers(&[], 1, 1).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no workers provided"));
    }

    #[tokio::test]
    async fn test_wait_for_healthy_workers_invalid_urls() {
        // This test will timeout quickly since the URLs are invalid
        let result =
            Router::wait_for_healthy_workers(&["http://nonexistent:8080".to_string()], 1, 1).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Timeout"));
    }

    // =============================
    // Tests for transparent proxy header/availability fixes
    // =============================

    /// Create a test router with ConsistentHash policy instead of RoundRobin
    fn create_test_consistent_hash_router() -> Router {
        let worker_registry = Arc::new(WorkerRegistry::new());
        let policy_registry = Arc::new(PolicyRegistry::new(
            crate::config::types::PolicyConfig::ConsistentHash { virtual_nodes: 100 },
        ));

        let worker1 = BasicWorker::new("http://worker1:8080".to_string(), WorkerType::Regular);
        let worker2 = BasicWorker::new("http://worker2:8080".to_string(), WorkerType::Regular);
        let worker3 = BasicWorker::new("http://worker3:8080".to_string(), WorkerType::Regular);
        worker_registry.register(Arc::new(worker1));
        worker_registry.register(Arc::new(worker2));
        worker_registry.register(Arc::new(worker3));

        let (_, rx) = tokio::sync::watch::channel(HashMap::new());
        Router {
            worker_registry,
            policy_registry,
            worker_startup_timeout_secs: 5,
            worker_startup_check_interval_secs: 1,
            intra_node_data_parallel_size: 1,
            api_key: Arc::new(tokio::sync::RwLock::new(None)),
            worker_api_keys: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
            client: Client::new(),
            retry_config: RetryConfig::default(),
            circuit_breaker_config: CircuitBreakerConfig::default(),
            _worker_loads: Arc::new(rx),
            _load_monitor_handle: None,
            response_cache: Arc::new(crate::cache::ResponseCache::new(128, 60)) as Arc<dyn crate::cache::traits::ExactMatchCache>,
            semantic_cache: None,
            embeddings_url: None,
            embeddings_api_key: None,
            embeddings_model: "default".to_string(),
            embedding_timeout_ms: 500,
            semantic_cluster: None,
            expose_routing_headers: false,
            decision_log: Arc::new(crate::admin::DecisionLog::new(10)),
            model_rules: Vec::new(),
            pre_routing_hooks: Vec::new(),
            include_request_text: false,
            token_cache_memory: None,
            #[cfg(feature = "redis-cache")]
            token_cache_redis: None,
        }
    }

    #[test]
    fn test_headers_to_request_headers_basic() {
        // Test that headers_to_request_headers correctly converts HeaderMap to HashMap
        let mut header_map = HeaderMap::new();
        header_map.insert("x-session-id", HeaderValue::from_static("session-123"));
        header_map.insert("content-type", HeaderValue::from_static("application/json"));
        header_map.insert("X-Custom-Header", HeaderValue::from_static("custom-value"));

        let result = Router::headers_to_request_headers(Some(&header_map));
        assert!(result.is_some());
        let headers = result.unwrap();

        // All keys should be lowercased
        assert_eq!(headers.get("x-session-id").unwrap(), "session-123");
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("x-custom-header").unwrap(), "custom-value");
    }

    #[test]
    fn test_headers_to_request_headers_none() {
        // Test that None headers produce None output
        let result = Router::headers_to_request_headers(None);
        assert!(result.is_none());
    }

    #[test]
    fn test_headers_to_request_headers_empty() {
        // Test that empty HeaderMap produces empty HashMap
        let header_map = HeaderMap::new();
        let result = Router::headers_to_request_headers(Some(&header_map));
        assert!(result.is_some());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_select_worker_for_model_with_consistent_hash_uses_headers() {
        // Verify that select_worker_for_model passes headers through to the policy,
        // producing consistent routing for the same session ID
        let router = create_test_consistent_hash_router();

        let mut header_map = HeaderMap::new();
        header_map.insert("x-session-id", HeaderValue::from_static("sticky-session-1"));

        // Make multiple selections with the same headers - should all pick the same worker
        let mut selected_urls: Vec<String> = Vec::new();
        for _ in 0..10 {
            let worker = router
                .select_worker_for_model(None, Some(r#"{"prompt": "test"}"#), Some(&header_map), None)
                .expect("Should select a worker");
            selected_urls.push(worker.url().to_string());
        }

        // All selections should go to the same worker (sticky routing)
        let first = &selected_urls[0];
        for (i, url) in selected_urls.iter().enumerate() {
            assert_eq!(
                url, first,
                "Request {} routed to {}, expected {} (session stickiness broken)",
                i, url, first
            );
        }
    }

    #[test]
    fn test_select_worker_for_model_filters_unavailable_workers() {
        // Verify that select_worker_for_model skips unhealthy workers
        let router = create_test_consistent_hash_router();

        // Mark worker1 and worker2 as unhealthy, leaving only worker3
        let all_workers = router.worker_registry.get_all();
        for w in &all_workers {
            if w.url() == "http://worker1:8080" || w.url() == "http://worker2:8080" {
                w.set_healthy(false);
            }
        }

        let worker = router
            .select_worker_for_model(None, Some(r#"{"prompt": "test"}"#), None, None)
            .expect("Should select the remaining healthy worker");

        assert_eq!(
            worker.url(),
            "http://worker3:8080",
            "Should only select the healthy worker"
        );
    }

    #[test]
    fn test_select_worker_for_model_returns_none_when_all_unavailable() {
        // Verify that when all workers are unhealthy, None is returned
        let router = create_test_consistent_hash_router();

        // Mark all workers as unhealthy
        let all_workers = router.worker_registry.get_all();
        for w in &all_workers {
            w.set_healthy(false);
        }

        let result = router.select_worker_for_model(None, Some(r#"{"prompt": "test"}"#), None, None);
        assert!(
            result.is_none(),
            "Should return None when all workers are unavailable"
        );
    }

    #[test]
    fn test_consistent_hash_different_sessions_can_route_differently() {
        // Verify that different session IDs can route to different workers
        let router = create_test_consistent_hash_router();

        let mut worker_urls_seen = std::collections::HashSet::new();
        for i in 0..50 {
            let mut header_map = HeaderMap::new();
            let session_id = format!("session-{}", i);
            header_map.insert("x-session-id", HeaderValue::from_str(&session_id).unwrap());

            if let Some(worker) = router.select_worker_for_model(
                None,
                Some(r#"{"prompt": "test"}"#),
                Some(&header_map),
                None,
            ) {
                worker_urls_seen.insert(worker.url().to_string());
            }
        }

        // With 50 different sessions and 3 workers, we should see at least 2 workers used
        assert!(
            worker_urls_seen.len() >= 2,
            "Expected distribution across workers, only used: {:?}",
            worker_urls_seen
        );
    }

    #[test]
    fn test_inline_header_conversion_matches_headers_to_request_headers() {
        // Verify that the inline header conversion pattern used in pd_router and
        // vllm_pd_router produces the same result as Router::headers_to_request_headers.
        // This ensures consistency across all three router implementations.
        let mut header_map = HeaderMap::new();
        header_map.insert("X-Session-Id", HeaderValue::from_static("session-abc"));
        header_map.insert("Content-Type", HeaderValue::from_static("application/json"));
        header_map.insert("x-user-id", HeaderValue::from_static("user-42"));

        // Method 1: Router::headers_to_request_headers (used in router.rs)
        let method1 = Router::headers_to_request_headers(Some(&header_map)).unwrap();

        // Method 2: Inline conversion (used in pd_router.rs and vllm_pd_router.rs)
        let method2: HashMap<String, String> = header_map
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_lowercase(), v.to_string()))
            })
            .collect();

        assert_eq!(
            method1, method2,
            "Both header conversion methods should produce identical results"
        );
    }

    /// Helper: start a minimal mock server that responds 200 on /health.
    async fn start_healthy_mock_server() -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::get, Router as AxumRouter};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = AxumRouter::new().route("/health", get(|| async { "ok" }));
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        (format!("http://{}", addr), handle)
    }

    #[tokio::test]
    async fn test_wait_for_healthy_workers_all_healthy() {
        let (url, _handle) = start_healthy_mock_server().await;
        let result = Router::wait_for_healthy_workers(&[url], 5, 1).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_wait_for_healthy_workers_partial_health() {
        // One healthy server + one unreachable URL.
        // All hosts must be healthy, so this should time out and return Err.
        let (healthy_url, _handle) = start_healthy_mock_server().await;
        let unreachable_url = "http://127.0.0.1:1".to_string(); // port 1 is unreachable

        // max_retries=2, interval=1s — fast timeout
        let result =
            Router::wait_for_healthy_workers(&[healthy_url, unreachable_url], 2, 1).await;
        assert!(result.is_err());
    }

    /// Helper: start a mock server that returns 503 on /health for a given
    /// duration, then switches to 200. Simulates a worker with a slow startup.
    async fn start_delayed_healthy_mock_server(
        delay: std::time::Duration,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{extract::State, http::StatusCode, routing::get, Router as AxumRouter};
        use std::sync::Arc;
        use tokio::net::TcpListener;

        let start = std::time::Instant::now();
        let ready_after = Arc::new(delay);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = AxumRouter::new()
            .route(
                "/health",
                get(
                    move |State((start, ready_after)): State<(
                        std::time::Instant,
                        Arc<std::time::Duration>,
                    )>| async move {
                        if start.elapsed() >= *ready_after {
                            StatusCode::OK
                        } else {
                            StatusCode::SERVICE_UNAVAILABLE
                        }
                    },
                ),
            )
            .with_state((start, ready_after));

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        (format!("http://{}", addr), handle)
    }

    #[tokio::test]
    async fn test_wait_for_healthy_workers_dp_aware_dedup() {
        // DP-aware URLs like http://host:port@0, @1, @2 should be deduplicated
        // to a single /health check on http://host:port.
        let (base_url, _handle) = start_healthy_mock_server().await;
        let dp_urls: Vec<String> = (0..4)
            .map(|rank| format!("{}@{}", base_url, rank))
            .collect();

        let result = Router::wait_for_healthy_workers(&dp_urls, 5, 1).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_delayed_worker_becomes_routable_via_health_checker() {
        use crate::core::{BasicWorker, HealthConfig, WorkerRegistry, WorkerType};
        use std::sync::Arc;

        // Two workers: one immediately healthy, one delayed (503 for 2s, then 200).
        let (healthy_url, _h1) = start_healthy_mock_server().await;
        let (delayed_url, _h2) =
            start_delayed_healthy_mock_server(std::time::Duration::from_secs(2)).await;

        // Verify the delayed worker is genuinely unhealthy right now.
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/health", &delayed_url))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 503);

        // ── Step 1: wait_for_healthy_workers (mirrors PDRouter::new startup) ──
        // This succeeds because the healthy worker responds immediately,
        // even though the delayed worker is still returning 503.
        let result =
            Router::wait_for_healthy_workers(&[healthy_url.clone(), delayed_url.clone()], 10, 1)
                .await;
        assert!(
            result.is_ok(),
            "Startup should succeed with at least one healthy worker"
        );

        // ── Step 2: register workers in the registry (mirrors PDRouter::new) ──
        let registry = Arc::new(WorkerRegistry::new());

        let healthy_worker = Arc::new(
            BasicWorker::new(healthy_url, WorkerType::Decode).with_health_config(HealthConfig {
                timeout_secs: 2,
                check_interval_secs: 1,
                endpoint: "/health".to_string(),
                failure_threshold: 3,
                success_threshold: 1,
            }),
        );
        registry.register(healthy_worker);

        let delayed_worker = Arc::new(
            BasicWorker::new(delayed_url, WorkerType::Decode).with_health_config(HealthConfig {
                timeout_secs: 2,
                check_interval_secs: 1,
                endpoint: "/health".to_string(),
                failure_threshold: 3,
                success_threshold: 1,
            }),
        );
        delayed_worker.set_healthy(false); // starts unhealthy
        registry.register(delayed_worker.clone());

        // Only the immediately-healthy worker should be available for routing.
        let healthy = registry.get_workers_filtered(None, None, None, true);
        assert_eq!(
            healthy.len(),
            1,
            "Only 1 worker should be healthy initially, got {}",
            healthy.len()
        );

        // ── Step 3: start background health checker (mirrors PDRouter::new) ──
        let health_checker = registry.start_health_checker(1);

        // ── Step 4: wait for delayed worker to recover ──
        // The mock server switches to 200 at t≈2s. With a 1s check interval
        // and success_threshold=1, the worker should be healthy by t≈3-4s.
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;

        // Both workers should now be available for routing.
        let healthy = registry.get_workers_filtered(None, None, None, true);
        assert_eq!(
            healthy.len(),
            2,
            "Both workers should be healthy after recovery, got {}",
            healthy.len()
        );
        assert!(
            delayed_worker.is_healthy(),
            "Delayed worker should have transitioned to healthy via health checker"
        );

        health_checker.shutdown().await;
    }

    // ── Explainability header contract tests ──────────────────────────────

    #[test]
    fn header_constants_are_valid_http_names() {
        for name in ALL_EXPLAINABILITY_HEADERS {
            assert!(
                axum::http::HeaderName::from_bytes(name.as_bytes()).is_ok(),
                "{name} is not a valid HTTP header name"
            );
        }
    }

    #[test]
    fn header_constants_all_start_with_x_vllm_router() {
        for name in ALL_EXPLAINABILITY_HEADERS {
            assert!(
                name.starts_with("x-vllm-router-"),
                "header {name} must start with x-vllm-router-"
            );
        }
    }

    #[test]
    fn inject_headers_policy_route() {
        let decision = RoutingDecision {
            worker: Some("http://w1:8000".to_string()),
            method: Some("policy"),
            policy: Some("round_robin".to_string()),
            cluster: None,
            model: Some("llama-3".to_string()),
            cache_status: Some("miss"),
            hooks_ran: vec!["safety".to_string()],
            request_text: None,
            tenant: Some("ml-team".to_string()),
        };
        let mut resp = Response::new(Body::empty());
        decision.inject_headers(&mut resp);
        let h = resp.headers();
        assert_eq!(h.get(HEADER_WORKER).unwrap(), "http://w1:8000");
        assert_eq!(h.get(HEADER_METHOD).unwrap(), "policy");
        assert_eq!(h.get(HEADER_POLICY).unwrap(), "round_robin");
        assert!(h.get(HEADER_CLUSTER).is_none(), "cluster should be absent");
        assert_eq!(h.get(HEADER_MODEL).unwrap(), "llama-3");
        assert_eq!(h.get(HEADER_CACHE_STATUS).unwrap(), "miss");
        assert_eq!(h.get(HEADER_HOOKS).unwrap(), "safety");
    }

    #[test]
    fn inject_headers_cache_hit() {
        let decision = RoutingDecision {
            worker: None,
            method: Some("cache-hit"),
            policy: None,
            cluster: None,
            model: Some("gpt-4".to_string()),
            cache_status: Some("exact-hit"),
            hooks_ran: vec![],
            request_text: None,
            tenant: None,
        };
        let mut resp = Response::new(Body::empty());
        decision.inject_headers(&mut resp);
        let h = resp.headers();
        assert!(h.get(HEADER_WORKER).is_none(), "no worker on cache hit");
        assert_eq!(h.get(HEADER_METHOD).unwrap(), "cache-hit");
        assert!(h.get(HEADER_POLICY).is_none());
        assert_eq!(h.get(HEADER_CACHE_STATUS).unwrap(), "exact-hit");
        assert!(h.get(HEADER_HOOKS).is_none(), "no hooks header when empty");
    }

    #[test]
    fn inject_headers_cluster_route() {
        let decision = RoutingDecision {
            worker: Some("http://w-math:8000".to_string()),
            method: Some("cluster"),
            policy: None,
            cluster: Some("math".to_string()),
            model: None,
            cache_status: Some("miss"),
            hooks_ran: vec!["pii".to_string(), "safety:timeout".to_string()],
            request_text: None,
            tenant: None,
        };
        let mut resp = Response::new(Body::empty());
        decision.inject_headers(&mut resp);
        let h = resp.headers();
        assert_eq!(h.get(HEADER_METHOD).unwrap(), "cluster");
        assert_eq!(h.get(HEADER_CLUSTER).unwrap(), "math");
        assert_eq!(h.get(HEADER_HOOKS).unwrap(), "pii,safety:timeout");
        assert!(h.get(HEADER_MODEL).is_none());
    }

    #[test]
    fn inject_headers_minimal_decision() {
        let decision = RoutingDecision::default();
        let mut resp = Response::new(Body::empty());
        decision.inject_headers(&mut resp);
        // Default decision has nothing set — no headers should be injected
        for name in ALL_EXPLAINABILITY_HEADERS {
            assert!(
                resp.headers().get(*name).is_none(),
                "header {name} should not appear for empty decision"
            );
        }
    }
}
