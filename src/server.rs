use crate::{
    auth::{TenantInfo, TenantRegistry},
    config::{ConnectionMode, HistoryBackend, RouterConfig},
    core::{WorkerRegistry, WorkerType},
    data_connector::{MemoryResponseStorage, NoOpResponseStorage, SharedResponseStorage},
    logging::{self, LoggingConfig},
    metrics::{self, PrometheusConfig},
    middleware::{self, QueuedRequest, TokenBucket},
    policies::PolicyRegistry,
    protocols::{
        anthropic::{from_openai_response, translate_sse_chunk, MessagesRequest, SseState},
        spec::{
            ChatCompletionRequest, ChatCompletionResponse, CompletionRequest, EmbeddingRequest,
            GenerateRequest, RerankRequest, V1RerankReqInput,
        },
        worker_spec::{WorkerApiResponse, WorkerConfigRequest, WorkerErrorResponse},
    },
    routers::{
        router_manager::{RouterId, RouterManager},
        RouterFactory, RouterTrait,
    },
    service_discovery::{start_service_discovery, ServiceDiscoveryConfig},
    tokenizer::{factory as tokenizer_factory, traits::Tokenizer},
};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    serve, Json, Router,
};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, signal, spawn, sync::RwLock};
use tracing::{error, info, warn, Level};

#[derive(Clone)]
pub struct AppContext {
    pub client: Client,
    pub router_config: RouterConfig,
    pub rate_limiter: Arc<TokenBucket>,
    pub tokenizer: Option<Arc<dyn Tokenizer>>,
    pub worker_registry: Arc<WorkerRegistry>,
    pub policy_registry: Arc<PolicyRegistry>,
    pub router_manager: Option<Arc<RouterManager>>,
    pub response_storage: SharedResponseStorage,
    pub api_key_cache: Arc<RwLock<HashMap<String, bool>>>,
    pub api_key_validation_urls: Arc<Vec<String>>,
    /// Static API key for admin endpoints. None = admin endpoints use the same auth as everything else.
    pub admin_api_key: Option<String>,
    /// Static API key for inference endpoints. None = no static key auth (falls through to api_key_validation_urls).
    pub inbound_api_key: Option<String>,
    /// Path to the YAML config file (for hot reload). None when started via CLI flags.
    pub config_file_path: Option<String>,
    /// Ring buffer of recent routing decisions for /admin/decisions.
    pub decision_log: Arc<crate::admin::DecisionLog>,
    /// Multi-tenant API key registry. None when `api_keys` is empty (single-key mode).
    /// Wrapped in RwLock for hot reload support.
    pub tenant_registry: Arc<RwLock<Option<Arc<TenantRegistry>>>>,
}

impl AppContext {
    pub fn new(
        router_config: RouterConfig,
        client: Client,
        max_concurrent_requests: usize,
        rate_limit_tokens_per_second: Option<usize>,
        api_key_validation_urls: Vec<String>,
        config_file_path: Option<String>,
    ) -> Result<Self, String> {
        let rate_limit_tokens = rate_limit_tokens_per_second.unwrap_or(max_concurrent_requests);
        let rate_limiter = Arc::new(TokenBucket::new(max_concurrent_requests, rate_limit_tokens));

        // Initialize gRPC-specific components only when in gRPC mode
        let tokenizer = if router_config.connection_mode == ConnectionMode::Grpc {
            // Get tokenizer path (required for gRPC mode)
            let tokenizer_path = router_config
                .tokenizer_path
                .clone()
                .or_else(|| router_config.model_path.clone())
                .ok_or_else(|| {
                    "gRPC mode requires either --tokenizer-path or --model-path to be specified"
                        .to_string()
                })?;

            // Initialize tokenizer (use model map if configured)
            Some(
                tokenizer_factory::create_tokenizer_with_map(
                    &tokenizer_path,
                    &router_config.tokenizer_model_map,
                )
                .map_err(|e| format!("Failed to create tokenizer: {e}"))?,
            )
        } else {
            // HTTP mode doesn't need tokenizer
            None
        };

        let worker_registry = Arc::new(WorkerRegistry::new());
        let policy_registry = Arc::new(PolicyRegistry::new(router_config.policy.clone()));

        let router_manager = None;

        // Initialize response storage based on configuration
        let response_storage: SharedResponseStorage = match router_config.history_backend {
            HistoryBackend::Memory => Arc::new(MemoryResponseStorage::new()),
            HistoryBackend::None => Arc::new(NoOpResponseStorage::new()),
        };

        let admin_api_key = router_config.admin_api_key.clone();
        let inbound_api_key = router_config.inbound_api_key.clone();

        let tenant_registry = if router_config.api_keys.is_empty() {
            Arc::new(RwLock::new(None))
        } else {
            info!(
                "Multi-tenant mode: {} API keys configured",
                router_config.api_keys.len()
            );
            Arc::new(RwLock::new(Some(Arc::new(TenantRegistry::from_config(
                &router_config.api_keys,
            )))))
        };

        Ok(Self {
            client,
            router_config,
            rate_limiter,
            tokenizer,
            worker_registry,
            policy_registry,
            router_manager,
            response_storage,
            api_key_cache: Arc::new(RwLock::new(HashMap::new())),
            api_key_validation_urls: Arc::new(api_key_validation_urls),
            admin_api_key,
            inbound_api_key,
            config_file_path,
            decision_log: Arc::new(crate::admin::DecisionLog::new(1000)),
            tenant_registry,
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    pub router: Arc<dyn RouterTrait>,
    pub context: Arc<AppContext>,
    pub concurrency_queue_tx: Option<tokio::sync::mpsc::Sender<QueuedRequest>>,
    pub router_manager: Option<Arc<RouterManager>>,
    pub start_time: std::time::Instant,
}

// Fallback handler for unmatched routes
async fn sink_handler() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// Transparent proxy handler for unmatched routes
/// Routes requests through the router's route_transparent method
async fn transparent_proxy_handler(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let mut headers = req.headers().clone();

    // Check authorization
    let tenant = match authorize_request(&state, &headers).await {
        Ok(t) => t,
        Err(response) => return response,
    };
    inject_tenant(&mut headers, &tenant);

    // Extract path and method
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // Read body
    let body_bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Failed to read request body: {}", e),
            )
                .into_response()
        }
    };

    // Parse body as JSON
    let body_json: serde_json::Value = if body_bytes.is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_slice(&body_bytes) {
            Ok(json) => json,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, format!("Invalid JSON body: {}", e))
                    .into_response()
            }
        }
    };

    // Route through transparent proxy
    state
        .router
        .route_transparent(Some(&headers), &path, &method, body_json)
        .await
}

// Health check endpoints — exempt from auth so K8s probes work
async fn liveness(State(state): State<Arc<AppState>>, _req: Request) -> Response {
    state.router.liveness()
}

async fn readiness(State(state): State<Arc<AppState>>, _req: Request) -> Response {
    state.router.readiness()
}

async fn health(State(state): State<Arc<AppState>>, req: Request) -> Response {
    state.router.health(req).await
}

async fn health_generate(State(state): State<Arc<AppState>>, req: Request) -> Response {
    state.router.health_generate(req).await
}

async fn get_server_info(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let headers = req.headers().clone();
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }
    state.router.get_server_info(req).await
}

async fn v1_models(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let headers = req.headers().clone();
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }
    state.router.get_models(req).await
}

async fn get_model_info(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let headers = req.headers().clone();
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }
    state.router.get_model_info(req).await
}

// Generation endpoints
// The RouterTrait now accepts optional headers and typed body directly
async fn generate(
    State(state): State<Arc<AppState>>,
    mut headers: http::HeaderMap,
    Json(body): Json<GenerateRequest>,
) -> Response {
    let tenant = match authorize_request(&state, &headers).await {
        Ok(t) => t,
        Err(response) => return response,
    };
    inject_tenant(&mut headers, &tenant);

    state
        .router
        .route_generate(Some(&headers), &body, None)
        .await
}

async fn v1_chat_completions(
    State(state): State<Arc<AppState>>,
    mut headers: http::HeaderMap,
    Json(body): Json<ChatCompletionRequest>,
) -> Response {
    let tenant = match authorize_request(&state, &headers).await {
        Ok(t) => t,
        Err(response) => return response,
    };
    if let Err(response) = check_tenant_model_access(&tenant, body.model.as_deref()) {
        return response;
    }
    inject_tenant(&mut headers, &tenant);

    state
        .router
        .route_chat(Some(&headers), &body, body.model.as_deref())
        .await
}

async fn v1_completions(
    State(state): State<Arc<AppState>>,
    mut headers: http::HeaderMap,
    Json(body): Json<CompletionRequest>,
) -> Response {
    let tenant = match authorize_request(&state, &headers).await {
        Ok(t) => t,
        Err(response) => return response,
    };
    if let Err(response) = check_tenant_model_access(&tenant, body.model.as_deref()) {
        return response;
    }
    inject_tenant(&mut headers, &tenant);

    state
        .router
        .route_completion(Some(&headers), &body, body.model.as_deref())
        .await
}

async fn rerank(
    State(state): State<Arc<AppState>>,
    mut headers: http::HeaderMap,
    Json(body): Json<RerankRequest>,
) -> Response {
    let tenant = match authorize_request(&state, &headers).await {
        Ok(t) => t,
        Err(response) => return response,
    };
    inject_tenant(&mut headers, &tenant);

    state
        .router
        .route_rerank(Some(&headers), &body, Some(&body.model))
        .await
}

async fn v1_rerank(
    State(state): State<Arc<AppState>>,
    mut headers: http::HeaderMap,
    Json(body): Json<V1RerankReqInput>,
) -> Response {
    let tenant = match authorize_request(&state, &headers).await {
        Ok(t) => t,
        Err(response) => return response,
    };
    inject_tenant(&mut headers, &tenant);

    state
        .router
        .route_rerank(Some(&headers), &body.into(), None)
        .await
}

async fn v1_responses(
    State(state): State<Arc<AppState>>,
    mut headers: http::HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let tenant = match authorize_request(&state, &headers).await {
        Ok(t) => t,
        Err(response) => return response,
    };
    let model = body.get("model").and_then(|v| v.as_str());
    if let Err(response) = check_tenant_model_access(&tenant, model) {
        return response;
    }
    inject_tenant(&mut headers, &tenant);

    state
        .router
        .route_transparent(Some(&headers), "/v1/responses", &http::Method::POST, body)
        .await
}

async fn v1_embeddings(
    State(state): State<Arc<AppState>>,
    mut headers: http::HeaderMap,
    Json(body): Json<EmbeddingRequest>,
) -> Response {
    let tenant = match authorize_request(&state, &headers).await {
        Ok(t) => t,
        Err(response) => return response,
    };
    if let Err(response) = check_tenant_model_access(&tenant, body.model.as_deref()) {
        return response;
    }
    inject_tenant(&mut headers, &tenant);

    state
        .router
        .route_embeddings(Some(&headers), &body, body.model.as_deref())
        .await
}

/// POST /v1/messages — Anthropic Messages API (T-24)
///
/// For non-streaming requests: translate Anthropic → OpenAI, forward to route_chat,
/// translate the OpenAI response back to Anthropic format.
///
/// For streaming requests: forward to route_chat with stream=true, then transform
/// each OpenAI SSE chunk into one or more Anthropic SSE events.
async fn v1_messages(
    State(state): State<Arc<AppState>>,
    mut headers: http::HeaderMap,
    Json(body): Json<MessagesRequest>,
) -> Response {
    let tenant = match authorize_request(&state, &headers).await {
        Ok(t) => t,
        Err(response) => return response,
    };
    if let Err(response) = check_tenant_model_access(&tenant, Some(&body.model)) {
        return response;
    }
    inject_tenant(&mut headers, &tenant);

    let original_model = body.model.clone();
    let is_stream = body.stream;

    // Translate to OpenAI chat request
    let oai_req = body.to_openai_chat();

    if !is_stream {
        // Non-streaming path: call route_chat, collect body, translate back
        let upstream = state
            .router
            .route_chat(Some(&headers), &oai_req, Some(&original_model))
            .await;

        if !upstream.status().is_success() {
            return upstream;
        }

        let (parts, up_body) = upstream.into_parts();
        let bytes = match axum::body::to_bytes(up_body, 32 * 1024 * 1024).await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("Failed to read upstream body: {e}"),
                )
                    .into_response()
            }
        };

        match serde_json::from_slice::<ChatCompletionResponse>(&bytes) {
            Ok(oai_resp) => {
                let anthropic_resp = from_openai_response(&oai_resp, &original_model);
                let mut response = Json(anthropic_resp).into_response();
                // Forward status code from upstream
                *response.status_mut() = parts.status;
                response
            }
            Err(_) => {
                // Could not parse as ChatCompletionResponse — return raw upstream body
                Response::from_parts(parts, Body::from(bytes))
            }
        }
    } else {
        // Streaming path: route_chat with stream=true, translate each SSE chunk
        let upstream = state
            .router
            .route_chat(Some(&headers), &oai_req, Some(&original_model))
            .await;

        if !upstream.status().is_success() {
            return upstream;
        }

        let (mut parts, up_body) = upstream.into_parts();

        // Override content-type to Anthropic's event-stream
        parts.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );

        // Spawn a task that reads the upstream SSE stream and translates each chunk
        let msg_id = uuid::Uuid::new_v4().simple().to_string();
        let model = original_model.clone();
        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);

        tokio::spawn(async move {
            use futures_util::StreamExt;
            let mut upstream_stream = up_body.into_data_stream();
            let mut state_machine = SseState::default();
            let mut buffer = String::new();

            while let Some(chunk) = upstream_stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(_) => break,
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // Process complete lines from the buffer
                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                    buffer = buffer[newline_pos + 1..].to_string();

                    if let Some(data) = line.strip_prefix("data: ") {
                        let events =
                            translate_sse_chunk(data, &model, &msg_id, &mut state_machine);
                        for event in events {
                            if tx
                                .send(Ok(bytes::Bytes::from(event)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        });

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        let body = Body::from_stream(stream);
        Response::from_parts(parts, body)
    }
}

async fn v1_responses_get(
    State(state): State<Arc<AppState>>,
    Path(response_id): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }
    state.router.get_response(Some(&headers), &response_id).await
}

async fn v1_responses_cancel(
    State(state): State<Arc<AppState>>,
    Path(response_id): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }
    state.router.cancel_response(Some(&headers), &response_id).await
}

async fn v1_responses_delete(
    State(state): State<Arc<AppState>>,
    Path(response_id): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }
    state.router.delete_response(Some(&headers), &response_id).await
}

async fn v1_responses_list_input_items(
    State(state): State<Arc<AppState>>,
    Path(response_id): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }
    state.router.list_response_input_items(Some(&headers), &response_id).await
}

// ---------- Admin endpoints (drain + hot reload) ----------

#[derive(Deserialize)]
struct DrainRequest {
    url: String,
    #[serde(default = "default_drain_timeout")]
    timeout_secs: u64,
}

fn default_drain_timeout() -> u64 {
    300
}

#[derive(Deserialize)]
struct DrainStatusQuery {
    url: String,
}

/// Authorize an admin request.
/// If `admin_api_key` is configured, check the Bearer token against it.
/// Otherwise fall back to `authorize_request` (external validation URLs).
#[allow(clippy::result_large_err)]
async fn authorize_admin_request(
    state: &Arc<AppState>,
    headers: &http::HeaderMap,
) -> Result<(), Response> {
    // Accepts: Authorization: Bearer <key> OR X-Admin-Key: <key> (for k8s proxy)
    if let Some(ref expected) = state.context.admin_api_key {
        let token = headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(str::trim)
            .or_else(|| headers.get("X-Admin-Key").and_then(|v| v.to_str().ok()).map(str::trim));

        if token == Some(expected.as_str()) {
            return Ok(());
        }
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Invalid or missing admin API key" })),
        )
            .into_response());
    }

    // No static admin key configured — fall back to the general auth mechanism
    authorize_request(state, headers).await.map(|_| ())
}

/// POST /admin/drain — mark a worker as draining, then remove it once load hits 0
async fn admin_drain(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Json(body): Json<DrainRequest>,
) -> Response {
    if let Err(response) = authorize_admin_request(&state, &headers).await {
        return response;
    }

    let url = body.url.clone();
    let timeout_secs = body.timeout_secs;

    // Mark worker as draining
    if let Err(e) = state.router.drain_worker(&url) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response();
    }

    info!("Draining worker {} (timeout {}s)", url, timeout_secs);

    // Spawn background task to wait for load to reach 0
    let router = state.router.clone();
    let registry = state.context.worker_registry.clone();
    let drain_url = url.clone();
    tokio::spawn(async move {
        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

            let load = registry
                .get_by_url(&drain_url)
                .map(|w| w.load())
                .unwrap_or(0);

            if load == 0 {
                info!(
                    "Worker {} drained (load=0), removing from pool",
                    drain_url
                );
                router.remove_worker(&drain_url);
                return;
            }

            if tokio::time::Instant::now() >= deadline {
                warn!(
                    "Drain timeout for {} (load={}), force-removing",
                    drain_url, load
                );
                router.remove_worker(&drain_url);
                return;
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "draining",
            "url": url,
            "timeout_secs": timeout_secs,
        })),
    )
        .into_response()
}

/// GET /admin/drain/status?url=... — check drain status of a worker
async fn admin_drain_status(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Query(query): Query<DrainStatusQuery>,
) -> Response {
    if let Err(response) = authorize_admin_request(&state, &headers).await {
        return response;
    }

    match state.context.worker_registry.get_by_url(&query.url) {
        Some(worker) => Json(json!({
            "url": query.url,
            "draining": worker.is_draining(),
            "current_load": worker.load(),
            "healthy": worker.is_healthy(),
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Worker {} not found (may already be removed)", query.url) })),
        )
            .into_response(),
    }
}

/// POST /admin/reload — re-read YAML config file and apply changes
async fn admin_reload(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin_request(&state, &headers).await {
        return response;
    }
    let config_path = match &state.context.config_file_path {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "No config file path configured (started via CLI flags?)" })),
            )
                .into_response();
        }
    };

    // Read and parse config
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Failed to read config file: {}", e) })),
            )
                .into_response();
        }
    };

    let new_config: RouterConfig = match serde_yaml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("Failed to parse config: {}", e) })),
            )
                .into_response();
        }
    };

    // Validate
    if let Err(e) = new_config.validate() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("Config validation failed: {}", e) })),
        )
            .into_response();
    }

    // Apply auth config reload
    let reload_msg = match state.router.reload_config(&new_config).await {
        Ok(msg) => msg,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Reload failed: {}", e) })),
            )
                .into_response();
        }
    };

    // Reload tenant registry if api_keys changed
    {
        let new_registry = if new_config.api_keys.is_empty() {
            None
        } else {
            Some(Arc::new(TenantRegistry::from_config(&new_config.api_keys)))
        };
        *state.context.tenant_registry.write().await = new_registry;
        info!("Tenant registry reloaded ({} keys)", new_config.api_keys.len());
    }

    // Sync workers: detect new and removed workers
    let current_urls: std::collections::HashSet<String> =
        state.router.get_worker_urls().into_iter().collect();
    let new_urls: std::collections::HashSet<String> =
        new_config.mode.all_worker_urls().into_iter().collect();

    let mut added = Vec::new();
    let mut drained = Vec::new();

    // Add new workers
    for url in new_urls.difference(&current_urls) {
        match state.router.add_worker(url).await {
            Ok(_) => added.push(url.clone()),
            Err(e) => warn!("Failed to add worker {} during reload: {}", url, e),
        }
    }

    // Drain removed workers
    for url in current_urls.difference(&new_urls) {
        match state.router.drain_worker(url) {
            Ok(_) => {
                drained.push(url.clone());
                // Spawn background removal (same as admin_drain)
                let router = state.router.clone();
                let registry = state.context.worker_registry.clone();
                let drain_url = url.clone();
                tokio::spawn(async move {
                    let deadline = tokio::time::Instant::now()
                        + tokio::time::Duration::from_secs(300);
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        let load = registry
                            .get_by_url(&drain_url)
                            .map(|w| w.load())
                            .unwrap_or(0);
                        if load == 0 || tokio::time::Instant::now() >= deadline {
                            router.remove_worker(&drain_url);
                            return;
                        }
                    }
                });
            }
            Err(e) => warn!("Failed to drain worker {} during reload: {}", url, e),
        }
    }

    info!(
        "Config reloaded: {} | added: {:?} | drained: {:?}",
        reload_msg, added, drained
    );

    Json(json!({
        "status": "ok",
        "reload": reload_msg,
        "workers_added": added,
        "workers_drained": drained,
    }))
    .into_response()
}

/// GET /admin/config — return the active (redacted) configuration
async fn admin_config(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin_request(&state, &headers).await {
        return response;
    }
    let mut config = serde_json::to_value(&state.context.router_config).unwrap_or_default();
    // Redact sensitive fields
    if let Some(obj) = config.as_object_mut() {
        for key in &["api_key", "admin_api_key", "inbound_api_key"] {
            if obj.get(*key).and_then(|v| v.as_str()).is_some() {
                obj.insert(key.to_string(), json!("***"));
            }
        }
        if let Some(wk) = obj.get_mut("worker_api_keys").and_then(|v| v.as_object_mut()) {
            for val in wk.values_mut() {
                *val = json!("***");
            }
        }
        // Redact api_keys entries (show name only, not the key)
        if let Some(keys) = obj.get_mut("api_keys").and_then(|v| v.as_array_mut()) {
            for entry in keys.iter_mut() {
                if let Some(obj) = entry.as_object_mut() {
                    if obj.contains_key("key") {
                        obj.insert("key".to_string(), json!("***"));
                    }
                }
            }
        }
        // Redact embeddings_api_key inside semantic_cache and semantic_cluster
        for section in &["semantic_cache", "semantic_cluster"] {
            if let Some(sc) = obj.get_mut(*section).and_then(|v| v.as_object_mut()) {
                if sc.get("embeddings_api_key").and_then(|v| v.as_str()).is_some() {
                    sc.insert("embeddings_api_key".to_string(), json!("***"));
                }
            }
        }
    }
    Json(config).into_response()
}

/// GET /admin/stats — snapshot of internal state
async fn admin_stats(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin_request(&state, &headers).await {
        return response;
    }

    let workers = state.context.worker_registry.get_all();
    let healthy = workers.iter().filter(|w| w.is_healthy()).count();
    let draining = workers.iter().filter(|w| w.is_draining()).count();

    let exact_entries = state.router.cache_len().await;
    let semantic_entries = state.router.semantic_cache_len().await;

    let cache_backend = state
        .context
        .router_config
        .cache
        .as_ref()
        .map(|c| format!("{:?}", c.backend).to_lowercase())
        .unwrap_or_else(|| "memory".to_string());

    // Policy assignments per model
    let per_model = state.context.policy_registry.model_policies();

    let default_policy = state.context.policy_registry.get_default_policy();

    let mut stats = json!({
        "uptime_secs": state.start_time.elapsed().as_secs(),
        "cache": {
            "backend": cache_backend,
            "exact_entries": exact_entries,
            "semantic_entries": semantic_entries,
        },
        "workers": {
            "total": workers.len(),
            "healthy": healthy,
            "draining": draining,
        },
        "policies": {
            "default": default_policy.name(),
            "per_model": per_model,
        },
        "decisions_logged": state.context.decision_log.len(),
    });

    // Add tenant summary if multi-tenant mode is active
    let tenant_reg = state.context.tenant_registry.read().await;
    if let Some(ref registry) = *tenant_reg {
        let tenants = registry.list_tenants();
        stats["tenants"] = json!({
            "count": tenants.len(),
            "entries": tenants,
        });
    }

    Json(stats).into_response()
}

#[derive(Deserialize)]
struct DecisionsQuery {
    #[serde(default = "default_decisions_limit")]
    limit: usize,
}
fn default_decisions_limit() -> usize {
    50
}

/// GET /admin/tenants — list all configured tenants with live status
async fn admin_tenants(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin_request(&state, &headers).await {
        return response;
    }

    let tenant_reg = state.context.tenant_registry.read().await;
    match &*tenant_reg {
        Some(registry) => {
            let tenants = registry.list_tenants();
            Json(json!({ "tenants": tenants })).into_response()
        }
        None => Json(json!({
            "tenants": [],
            "message": "Multi-tenant API keys not configured. Set api_keys in config."
        }))
        .into_response(),
    }
}

/// GET /admin/decisions?limit=50 — recent routing decisions
async fn admin_decisions(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DecisionsQuery>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin_request(&state, &headers).await {
        return response;
    }
    let decisions = state.context.decision_log.recent(query.limit);
    Json(json!({ "decisions": decisions })).into_response()
}

const AUTH_FAILURE_MESSAGE: &str =
    "You must provide a valid API key. Obtain one from the panel or contact your administrator.";

/// Internal header used to propagate tenant name from auth to routing pipeline.
pub const TENANT_HEADER: &str = "x-vllm-tenant";

/// Check if a tenant is allowed to access the requested model. Returns 403 if denied.
#[allow(clippy::result_large_err)]
fn check_tenant_model_access(
    tenant: &Option<TenantInfo>,
    model: Option<&str>,
) -> Result<(), Response> {
    if let Some(ref info) = tenant {
        let model_name = match model {
            Some(m) => m,
            None => return Ok(()),
        };
        let allowed = info.allowed_models.iter().any(|pattern| {
            if pattern == "*" {
                return true;
            }
            if let Some(prefix) = pattern.strip_suffix('*') {
                model_name.starts_with(prefix)
            } else {
                pattern == model_name
            }
        });
        if !allowed {
            return Err((
                StatusCode::FORBIDDEN,
                format!("Model '{model_name}' is not allowed for this API key."),
            )
                .into_response());
        }
    }
    Ok(())
}

/// Inject tenant name into headers so the routing pipeline can read it.
fn inject_tenant(headers: &mut http::HeaderMap, tenant: &Option<TenantInfo>) {
    if let Some(ref info) = tenant {
        if let Ok(v) = http::HeaderValue::from_str(&info.name) {
            headers.insert(TENANT_HEADER, v);
        }
    }
}

// ---------- Worker management endpoints (Legacy) ----------

#[derive(Deserialize)]
struct UrlQuery {
    url: String,
}

/// Authenticate an incoming request.
///
/// Returns `Ok(Some(TenantInfo))` when multi-tenant API keys are configured and the
/// key matched a tenant.  Returns `Ok(None)` when using single-key or open auth
/// (no tenant context available).
async fn authorize_request(
    state: &Arc<AppState>,
    headers: &http::HeaderMap,
) -> Result<Option<TenantInfo>, Response> {
    // Extract token from Authorization: Bearer <key> OR X-Router-Key: <key>
    let token = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .or_else(|| headers.get("X-Router-Key").and_then(|v| v.to_str().ok()).map(str::trim));

    // --- Multi-tenant API keys (highest priority) ---
    let tenant_reg = state.context.tenant_registry.read().await;
    if let Some(ref registry) = *tenant_reg {
        let raw_key = token
            .ok_or_else(|| (StatusCode::UNAUTHORIZED, AUTH_FAILURE_MESSAGE).into_response())?;

        let tenant = registry.lookup(raw_key).ok_or_else(|| {
            (StatusCode::UNAUTHORIZED, AUTH_FAILURE_MESSAGE).into_response()
        })?;

        if !tenant.enabled {
            return Err((
                StatusCode::FORBIDDEN,
                "API key is disabled. Contact your administrator.",
            )
                .into_response());
        }

        // Per-tenant rate limiting
        if tenant.rate_limiter.try_acquire(1.0).await.is_err() {
            tenant
                .total_rate_limited
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::metrics::RouterMetrics::record_tenant_rate_limited(&tenant.name);

            let mut resp = (
                StatusCode::TOO_MANY_REQUESTS,
                "Rate limit exceeded for this API key.",
            )
                .into_response();
            resp.headers_mut().insert(
                "X-RateLimit-Limit",
                http::HeaderValue::from_str(&tenant.rate_limit_rps.to_string()).unwrap(),
            );
            resp.headers_mut().insert(
                "Retry-After",
                http::HeaderValue::from_static("1"),
            );
            return Err(resp);
        }

        tenant
            .total_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        return Ok(Some(TenantInfo {
            name: tenant.name.clone(),
            allowed_models: tenant.allowed_models.clone(),
            metadata: tenant.metadata.clone(),
        }));
    }

    // --- Static inbound API key (single-key mode) ---
    if let Some(ref expected) = state.context.inbound_api_key {
        if token == Some(expected.as_str()) {
            return Ok(None);
        }
        return Err((StatusCode::UNAUTHORIZED, AUTH_FAILURE_MESSAGE).into_response());
    }

    // --- External validation URLs ---
    let validation_urls = state.context.api_key_validation_urls.as_ref();
    if validation_urls.is_empty() {
        return Ok(None); // Open access
    }

    let raw_token = token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, AUTH_FAILURE_MESSAGE).into_response())?;

    if let Some(valid) = state.context.api_key_cache.read().await.get(raw_token).copied() {
        if valid {
            return Ok(None);
        }
        return Err((StatusCode::UNAUTHORIZED, AUTH_FAILURE_MESSAGE).into_response());
    }

    let mut validated = false;
    for url in validation_urls {
        match state
            .context
            .client
            .get(url)
            .header(http::header::AUTHORIZATION, format!("Bearer {raw_token}"))
            .send()
            .await
        {
            Ok(response) if response.status() == StatusCode::OK => {
                validated = true;
                break;
            }
            Ok(_) => {
                continue;
            }
            Err(err) => {
                warn!("Failed to validate API key against {url}: {err}");
            }
        }
    }

    state
        .context
        .api_key_cache
        .write()
        .await
        .insert(raw_token.to_string(), validated);

    if validated {
        Ok(None)
    } else {
        Err((StatusCode::UNAUTHORIZED, AUTH_FAILURE_MESSAGE).into_response())
    }
}

async fn add_worker(
    State(state): State<Arc<AppState>>,
    Query(UrlQuery { url }): Query<UrlQuery>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }

    match state.router.add_worker(&url).await {
        Ok(message) => (StatusCode::OK, message).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
    }
}

async fn list_workers(State(state): State<Arc<AppState>>, headers: http::HeaderMap) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }

    let worker_list = state.router.get_worker_urls();
    Json(serde_json::json!({ "urls": worker_list })).into_response()
}

async fn remove_worker(
    State(state): State<Arc<AppState>>,
    Query(UrlQuery { url }): Query<UrlQuery>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }

    state.router.remove_worker(&url);
    (
        StatusCode::OK,
        format!("Successfully removed worker: {url}"),
    )
        .into_response()
}

async fn flush_cache(State(state): State<Arc<AppState>>, headers: http::HeaderMap) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }

    state.router.flush_cache().await
}

async fn get_loads(State(state): State<Arc<AppState>>, headers: http::HeaderMap) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }

    state.router.get_worker_loads().await
}

// ---------- Worker management endpoints (RESTful) ----------

/// POST /workers - Add a new worker with full configuration
async fn create_worker(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
    Json(config): Json<WorkerConfigRequest>,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }

    // Check if we have a RouterManager (enable_igw=true)
    if let Some(router_manager) = &state.router_manager {
        // Call RouterManager's add_worker method directly with the full config
        match router_manager.add_worker(config).await {
            Ok(response) => (StatusCode::OK, Json(response)).into_response(),
            Err(error) => (StatusCode::BAD_REQUEST, Json(error)).into_response(),
        }
    } else {
        // In single router mode, use the router's add_worker with basic config
        match state.router.add_worker(&config.url).await {
            Ok(message) => {
                let response = WorkerApiResponse {
                    success: true,
                    message,
                    worker: None,
                };
                (StatusCode::OK, Json(response)).into_response()
            }
            Err(error) => {
                let error_response = WorkerErrorResponse {
                    error,
                    code: "ADD_WORKER_FAILED".to_string(),
                };
                (StatusCode::BAD_REQUEST, Json(error_response)).into_response()
            }
        }
    }
}

async fn list_workers_rest(
    State(state): State<Arc<AppState>>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }

    if let Some(router_manager) = &state.router_manager {
        let response = router_manager.list_workers();
        Json(response).into_response()
    } else {
        // In single router mode, get detailed worker info from registry
        let workers = state.context.worker_registry.get_all();
        let response = serde_json::json!({
            "workers": workers.iter().map(|worker| {
                let mut worker_info = serde_json::json!({
                    "url": worker.url(),
                    "model_id": worker.model_id(),
                    "worker_type": match worker.worker_type() {
                        WorkerType::Regular => "regular",
                        WorkerType::Prefill { .. } => "prefill",
                        WorkerType::Decode => "decode",
                    },
                    "is_healthy": worker.is_healthy(),
                    "draining": worker.is_draining(),
                    "load": worker.load(),
                    "connection_mode": format!("{:?}", worker.connection_mode()),
                    "priority": worker.priority(),
                    "cost": worker.cost(),
                });

                // Add bootstrap_port for Prefill workers
                if let WorkerType::Prefill { bootstrap_port } = worker.worker_type() {
                    worker_info["bootstrap_port"] = serde_json::json!(bootstrap_port);
                }

                worker_info
            }).collect::<Vec<_>>(),
            "total": workers.len(),
            "stats": {
                "prefill_count": state.context.worker_registry.get_prefill_workers().len(),
                "decode_count": state.context.worker_registry.get_decode_workers().len(),
                "regular_count": state.context.worker_registry.get_by_type(&WorkerType::Regular).len(),
            }
        });
        Json(response).into_response()
    }
}

/// GET /workers/{url} - Get specific worker info
async fn get_worker(
    State(state): State<Arc<AppState>>,
    Path(url): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }

    if let Some(router_manager) = &state.router_manager {
        if let Some(worker) = router_manager.get_worker(&url) {
            Json(worker).into_response()
        } else {
            let error = WorkerErrorResponse {
                error: format!("Worker {url} not found"),
                code: "WORKER_NOT_FOUND".to_string(),
            };
            (StatusCode::NOT_FOUND, Json(error)).into_response()
        }
    } else {
        let workers = state.router.get_worker_urls();
        if workers.contains(&url) {
            Json(json!({
                "url": url,
                "model_id": "unknown",
                "is_healthy": true
            }))
            .into_response()
        } else {
            let error = WorkerErrorResponse {
                error: format!("Worker {url} not found"),
                code: "WORKER_NOT_FOUND".to_string(),
            };
            (StatusCode::NOT_FOUND, Json(error)).into_response()
        }
    }
}

/// DELETE /workers/{url} - Remove a worker
async fn delete_worker(
    State(state): State<Arc<AppState>>,
    Path(url): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_request(&state, &headers).await {
        return response;
    }

    if let Some(router_manager) = &state.router_manager {
        match router_manager.remove_worker_from_registry(&url) {
            Ok(response) => (StatusCode::OK, Json(response)).into_response(),
            Err(error) => (StatusCode::BAD_REQUEST, Json(error)).into_response(),
        }
    } else {
        // In single router mode, use router's remove_worker
        state.router.remove_worker(&url);
        let response = WorkerApiResponse {
            success: true,
            message: format!("Worker {url} removed successfully"),
            worker: None,
        };
        (StatusCode::OK, Json(response)).into_response()
    }
}

/// GET /workers/{url}/metrics — proxy the worker's Prometheus /metrics endpoint
async fn get_worker_metrics(
    State(state): State<Arc<AppState>>,
    Path(url): Path<String>,
    headers: http::HeaderMap,
) -> Response {
    if let Err(response) = authorize_admin_request(&state, &headers).await {
        return response;
    }

    // Verify the worker exists
    let worker_urls = state.router.get_worker_urls();
    if !worker_urls.contains(&url) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("Worker {} not found", url) })),
        )
            .into_response();
    }

    // Fetch /metrics from the worker (UDS-aware)
    let metrics_url = crate::transport::request_url(&url, "/metrics");
    let client = crate::transport::resolve_client(&url, &state.context.client)
        .unwrap_or_else(|_| state.context.client.clone());
    match client.get(&metrics_url).timeout(Duration::from_secs(5)).send().await {
        Ok(res) => {
            let status = StatusCode::from_u16(res.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let body = res.text().await.unwrap_or_default();
            let mut resp = Response::new(Body::from(body));
            *resp.status_mut() = status;
            resp.headers_mut().insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            resp
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("Failed to fetch worker metrics: {}", e) })),
        )
            .into_response(),
    }
}

pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub router_config: RouterConfig,
    pub max_payload_size: usize,
    pub log_dir: Option<String>,
    pub log_level: Option<String>,
    pub service_discovery_config: Option<ServiceDiscoveryConfig>,
    pub prometheus_config: Option<PrometheusConfig>,
    pub request_timeout_secs: u64,
    pub request_id_headers: Option<Vec<String>>,
    /// Path to the YAML config file (for hot reload)
    pub config_file_path: Option<String>,
    /// OpenTelemetry tracing configuration. None = tracing disabled.
    pub trace_config: Option<crate::config::TraceConfig>,
}

/// Build the Axum application with all routes and middleware.
///
/// Uses the current runtime OpenTelemetry state to decide whether to install
/// the request-level tracing middleware.
pub fn build_app(
    app_state: Arc<AppState>,
    max_payload_size: usize,
    request_id_headers: Vec<String>,
    cors_allowed_origins: Vec<String>,
    enable_transparent_proxy: bool,
) -> Router {
    build_app_with_request_tracing(
        app_state,
        max_payload_size,
        request_id_headers,
        cors_allowed_origins,
        enable_transparent_proxy,
        crate::otel_trace::is_otel_enabled(),
    )
}

/// Build the Axum application with an explicit request-tracing toggle.
pub fn build_app_with_request_tracing(
    app_state: Arc<AppState>,
    max_payload_size: usize,
    request_id_headers: Vec<String>,
    cors_allowed_origins: Vec<String>,
    enable_transparent_proxy: bool,
    _enable_request_tracing: bool,
) -> Router {
    // Create routes
    let protected_routes = Router::new()
        .route("/generate", post(generate))
        .route("/v1/chat/completions", post(v1_chat_completions))
        .route("/v1/completions", post(v1_completions))
        .route("/rerank", post(rerank))
        .route("/v1/rerank", post(v1_rerank))
        .route("/v1/responses", post(v1_responses))
        .route("/v1/embeddings", post(v1_embeddings))
        .route("/v1/messages", post(v1_messages))
        .route("/v1/responses/{response_id}", get(v1_responses_get))
        .route(
            "/v1/responses/{response_id}/cancel",
            post(v1_responses_cancel),
        )
        .route("/v1/responses/{response_id}", delete(v1_responses_delete))
        .route(
            "/v1/responses/{response_id}/input",
            get(v1_responses_list_input_items),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            middleware::concurrency_limit_middleware,
        ));

    let public_routes = Router::new()
        .route("/liveness", get(liveness))
        .route("/readiness", get(readiness))
        .route("/health", get(health))
        .route("/health_generate", get(health_generate))
        .route("/v1/models", get(v1_models))
        .route("/get_model_info", get(get_model_info))
        .route("/get_server_info", get(get_server_info));

    let admin_routes = Router::new()
        .route("/add_worker", post(add_worker))
        .route("/remove_worker", post(remove_worker))
        .route("/list_workers", get(list_workers))
        .route("/flush_cache", post(flush_cache))
        .route("/get_loads", get(get_loads))
        .route("/admin/drain", post(admin_drain))
        .route("/admin/drain/status", get(admin_drain_status))
        .route("/admin/reload", post(admin_reload))
        .route("/admin/config", get(admin_config))
        .route("/admin/stats", get(admin_stats))
        .route("/admin/decisions", get(admin_decisions))
        .route("/admin/tenants", get(admin_tenants));

    // Worker management routes
    let worker_routes = Router::new()
        .route("/workers", post(create_worker))
        .route("/workers", get(list_workers_rest))
        .route("/workers/{url}", get(get_worker))
        .route("/workers/{url}", delete(delete_worker))
        .route("/workers/{url}/metrics", get(get_worker_metrics));

    // Build base app with all routes and middleware
    let base_app = Router::new()
        .merge(protected_routes)
        .merge(public_routes)
        .merge(admin_routes)
        .merge(worker_routes)
        // Request body size limiting
        .layer(DefaultBodyLimit::max(max_payload_size))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            max_payload_size,
        ))
        .layer(middleware::create_logging_layer())
        .layer(middleware::RequestIdLayer::new(request_id_headers))
        .layer(create_cors_layer(cors_allowed_origins));

    // Choose fallback based on transparent proxy mode
    if enable_transparent_proxy {
        base_app
            .fallback(transparent_proxy_handler)
            .with_state(app_state)
    } else {
        base_app.fallback(sink_handler).with_state(app_state)
    }
}

pub async fn startup(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    println!("DEBUG: Server startup function called");

    // Only initialize logging if not already done (for Python bindings support)
    static LOGGING_INITIALIZED: AtomicBool = AtomicBool::new(false);

    println!("DEBUG: Initializing logging");
    let _log_guard = if !LOGGING_INITIALIZED.swap(true, Ordering::SeqCst) {
        Some(logging::init_logging(
            LoggingConfig {
                level: config
                    .log_level
                    .as_deref()
                    .and_then(|s| match s.to_uppercase().parse::<Level>() {
                        Ok(l) => Some(l),
                        Err(_) => {
                            warn!("Invalid log level string: '{s}'. Defaulting to INFO.");
                            None
                        }
                    })
                    .unwrap_or(Level::INFO),
                json_format: false,
                log_dir: config.log_dir.clone(),
                colorize: true,
                log_file_name: "vllm-router".to_string(),
                log_targets: None,
            },
            config.trace_config.clone(),
        ))
    } else {
        if config.trace_config.is_some() && !crate::otel_trace::is_otel_enabled() {
            warn!(
                "Tracing was requested after logging was already initialized; \
                 the existing subscriber will continue without enabling OpenTelemetry"
            );
        }
        None
    };
    println!("DEBUG: Logging initialized");

    // Initialize prometheus metrics exporter
    println!("DEBUG: Initializing Prometheus metrics");
    if let Some(prometheus_config) = config.prometheus_config {
        metrics::start_prometheus(prometheus_config);
    }
    println!("DEBUG: Prometheus metrics initialized");

    info!(
        "Starting router on {}:{} | mode: {:?} | policy: {:?} | max_payload: {}MB",
        config.host,
        config.port,
        config.router_config.mode,
        config.router_config.policy,
        config.max_payload_size / (1024 * 1024)
    );

    println!("DEBUG: Creating HTTP client");
    let client = Client::builder()
        .pool_idle_timeout(Some(Duration::from_secs(50)))
        .pool_max_idle_per_host(500)
        .timeout(Duration::from_secs(config.request_timeout_secs))
        .connect_timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .build()
        .expect("Failed to create HTTP client");
    println!("DEBUG: HTTP client created");

    // Create the application context with all dependencies
    println!("DEBUG: Creating AppContext");
    let app_context = AppContext::new(
        config.router_config.clone(),
        client.clone(),
        config.router_config.max_concurrent_requests,
        config.router_config.rate_limit_tokens_per_second,
        config.router_config.api_key_validation_urls.clone(),
        config.config_file_path.clone(),
    )?;
    println!("DEBUG: AppContext created");

    let app_context = Arc::new(app_context);

    // Create the appropriate router based on enable_igw flag
    let (router, router_manager): (Arc<dyn RouterTrait>, Option<Arc<RouterManager>>) =
        if config.router_config.enable_igw {
            info!("Multi-router mode enabled (enable_igw=true)");

            // Create RouterManager with shared registries from AppContext
            let router_manager = Arc::new(RouterManager::new(
                config.router_config.clone(),
                client.clone(),
                app_context.worker_registry.clone(),
                app_context.policy_registry.clone(),
            ));

            // 1. HTTP Regular Router
            match RouterFactory::create_regular_router(
                &[], // Empty worker list - workers added later
                &app_context,
            )
            .await
            {
                Ok(http_regular) => {
                    info!("Created HTTP Regular router");
                    router_manager.register_router(
                        RouterId::new("http-regular".to_string()),
                        Arc::from(http_regular),
                    );
                }
                Err(e) => {
                    warn!("Failed to create HTTP Regular router: {e}");
                }
            }

            // 2. HTTP PD Router
            match RouterFactory::create_pd_router(
                &[],
                &[],
                None,
                None,
                &config.router_config.policy,
                &app_context,
            )
            .await
            {
                Ok(http_pd) => {
                    info!("Created HTTP PD router");
                    router_manager
                        .register_router(RouterId::new("http-pd".to_string()), Arc::from(http_pd));
                }
                Err(e) => {
                    warn!("Failed to create HTTP PD router: {e}");
                }
            }

            // 3. gRPC Regular Router
            match RouterFactory::create_grpc_router(
                &[], // Empty worker list - workers added later via service discovery
                &app_context.router_config.policy,
                &app_context,
            )
            .await
            {
                Ok(grpc_regular) => {
                    info!("Created gRPC Regular router");
                    router_manager.register_router(
                        RouterId::new("grpc-regular".to_string()),
                        Arc::from(grpc_regular),
                    );
                }
                Err(e) => {
                    warn!("Failed to create gRPC Regular router: {e}");
                }
            }

            // 4. gRPC PD Router
            match RouterFactory::create_grpc_pd_router(
                &[], // Empty prefill list
                &[], // Empty decode list
                None,
                None,
                &app_context.router_config.policy,
                &app_context,
            )
            .await
            {
                Ok(grpc_pd) => {
                    info!("Created gRPC PD router");
                    router_manager.register_router(
                        RouterId::new("grpc-pd".to_string()),
                        Arc::from(grpc_pd),
                    );
                }
                Err(e) => {
                    warn!("Failed to create gRPC PD router: {e}");
                }
            }

            info!(
                "RouterManager initialized with {} routers",
                router_manager.router_count()
            );
            (
                router_manager.clone() as Arc<dyn RouterTrait>,
                Some(router_manager),
            )
        } else {
            info!("Single router mode (enable_igw=false)");
            // Create single router with the context
            (
                Arc::from(RouterFactory::create_router(&app_context).await?),
                None,
            )
        };

    // Start health checker for all workers in the registry
    let _health_checker = app_context
        .worker_registry
        .start_health_checker(config.router_config.health_check.check_interval_secs);
    info!(
        "Started health checker for workers with {}s interval",
        config.router_config.health_check.check_interval_secs
    );

    // Set up concurrency limiter with queue if configured
    let (limiter, processor) = middleware::ConcurrencyLimiter::new(
        app_context.rate_limiter.clone(),
        config.router_config.queue_size,
        Duration::from_secs(config.router_config.queue_timeout_secs),
    );

    // Start queue processor if enabled
    if let Some(processor) = processor {
        tokio::spawn(processor.run());
        info!(
            "Started request queue with size: {}, timeout: {}s",
            config.router_config.queue_size, config.router_config.queue_timeout_secs
        );
    }

    // Create app state with router and context
    let app_state = Arc::new(AppState {
        router,
        context: app_context.clone(),
        concurrency_queue_tx: limiter.queue_tx.clone(),
        router_manager,
        start_time: std::time::Instant::now(),
    });
    let router_arc = Arc::clone(&app_state.router);

    // Start decision log export if configured
    if let Some(ref dl_config) = config.router_config.decision_log {
        crate::admin::decision_log::spawn_export_task(
            app_context.decision_log.clone(),
            dl_config.export_path.clone(),
            dl_config.export_interval_secs,
        );
    }

    // Start the service discovery if enabled
    if let Some(service_discovery_config) = config.service_discovery_config {
        if service_discovery_config.enabled {
            match start_service_discovery(service_discovery_config, router_arc).await {
                Ok(handle) => {
                    info!("Service discovery started");
                    // Spawn a task to handle the service discovery thread
                    spawn(async move {
                        if let Err(e) = handle.await {
                            error!("Service discovery task failed: {:?}", e);
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to start service discovery: {e}");
                    warn!("Continuing without service discovery");
                }
            }
        }
    }

    info!(
        "Router ready | workers: {:?}",
        app_state.router.get_worker_urls()
    );

    let request_id_headers = config.request_id_headers.clone().unwrap_or_else(|| {
        vec![
            "x-request-id".to_string(),
            "x-correlation-id".to_string(),
            "x-trace-id".to_string(),
            "request-id".to_string(),
        ]
    });

    // Build the application
    // Enable transparent proxy for all routing modes
    let enable_transparent_proxy = true;

    let app = build_app(
        app_state,
        config.max_payload_size,
        request_id_headers,
        config.router_config.cors_allowed_origins.clone(),
        enable_transparent_proxy,
    );

    let addr = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&addr).await?;
    info!("Starting server on {}", addr);
    serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;

    Ok(())
}

// Graceful shutdown handler
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C, starting graceful shutdown");
        },
        _ = terminate => {
            info!("Received terminate signal, starting graceful shutdown");
        },
    }
}

// CORS Layer Creation
fn create_cors_layer(allowed_origins: Vec<String>) -> tower_http::cors::CorsLayer {
    use tower_http::cors::Any;

    let cors = if allowed_origins.is_empty() {
        // Allow all origins if none specified
        tower_http::cors::CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
            .expose_headers(Any)
    } else {
        // Restrict to specific origins
        let origins: Vec<http::HeaderValue> = allowed_origins
            .into_iter()
            .filter_map(|origin| origin.parse().ok())
            .collect();

        tower_http::cors::CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([http::Method::GET, http::Method::POST, http::Method::OPTIONS])
            .allow_headers([http::header::CONTENT_TYPE, http::header::AUTHORIZATION])
            .expose_headers([
                http::header::HeaderName::from_static("x-request-id"),
                http::header::HeaderName::from_static("x-vllm-router-worker"),
                http::header::HeaderName::from_static("x-vllm-router-method"),
                http::header::HeaderName::from_static("x-vllm-router-policy"),
                http::header::HeaderName::from_static("x-vllm-router-cluster"),
                http::header::HeaderName::from_static("x-vllm-router-model"),
                http::header::HeaderName::from_static("x-vllm-router-cache-status"),
            ])
    };

    cors.max_age(Duration::from_secs(3600))
}
