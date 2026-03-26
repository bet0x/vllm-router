# Changelog

All notable changes to this fork are documented here.
Upstream: [vllm-project/router](https://github.com/vllm-project/router) | Fork: [bet0x/vllm-router](https://github.com/bet0x/vllm-router)

---

## [0.10.0] — 2026-03-26

### Added
- **Golden tests for public API routes** — baseline integration tests covering `POST /v1/chat/completions` (non-stream + stream), `POST /v1/messages` (non-stream + stream), `POST /v1/responses`, `GET /v1/responses/{id}`, and `POST /v1/responses/{id}/cancel`. Uses mock workers + `tower::ServiceExt::oneshot` to validate response structure and key fields. Fixture files in `tests/fixtures/golden/`.
- **Tenant model access checks for `/v1/messages` and `/v1/responses`** — both endpoints now enforce `check_tenant_model_access` before routing, matching the existing behavior of `/v1/chat/completions`, `/v1/completions`, and `/v1/embeddings`. Previously these two routes skipped the check, allowing tenants with restricted `allowed_models` to bypass model access control.

---

## [0.9.1] — 2026-03-25

### Added
- **Cache similarity score header** (`x-vllm-router-cache-similarity`) — exposes the cosine similarity score from semantic cache lookups as a response header (when `expose_routing_headers` is enabled). Set on semantic cache hits, enabling operators to tune similarity thresholds, debug unexpected matches/misses, and build cache effectiveness dashboards.
- **`vllm_router_cache_similarity` Prometheus histogram** — records the cosine similarity score on every semantic cache hit for observability and threshold calibration.

### Changed
- `SemanticCacheBackend::find_similar()` now returns the similarity score alongside the cached response (`Option<(Bytes, Option<String>, f32)>`).

---

## [0.9.0] — 2026-03-24

### Added
- **Multi-tenant API keys** (`api_keys` config) — per-tenant authentication with independent rate limits, model access control, and observability. Each key is stored as a SHA-256 hash (plaintext never kept in memory after init). Features:
  - **Per-tenant rate limiting** — each key has its own token bucket (`rate_limit_rps`, `max_concurrent`). Exceeded limits return `429 Too Many Requests` with `X-RateLimit-Limit` and `Retry-After` headers.
  - **Model access control** — `allowed_models` supports exact names and trailing wildcards (e.g. `Llama-3*`). Denied access returns `403 Forbidden`.
  - **Disabled keys** — `enabled: false` revokes a key without removing it from config.
  - **Hot reload** — `POST /admin/reload` reloads tenant keys from config without restart.
  - **Observability** — tenant name in `/admin/decisions`, 5 new per-tenant Prometheus metrics (`vllm_router_tenant_requests_total`, `_request_duration_seconds`, `_errors_total`, `_rate_limited_total`, `_tokens_total`), `GET /admin/tenants` endpoint.
  - **Priority order**: `api_keys` → `inbound_api_key` → `api_key_validation_urls` → open access. Backward compatible: empty `api_keys` preserves existing auth behavior.
- **Decision log schema v3** — added `tenant` field to `DecisionRecord`.
- `GET /admin/tenants` — list all tenants with live request/rate-limit counters.
- `api_keys` example in all 13 `configs/*.yaml` files.
- Documentation: `docs/authentication.md` (multi-tenant section), `docs/admin-api.md` (tenants endpoint), `docs/metrics.md` (per-tenant metrics).

---

## [0.8.0] — 2026-03-22

### Added
- **Token ID cache** (`prompt_cache` config) — caches tokenization results in memory or Redis. Eliminates the 100ms `POST /tokenize` HTTP round-trip for repeated system prompts in the LMCache prefix lookup path. 100x faster tokenization on cache hit.
- **Shared prefix routing table** (`shared_prefix_routing` config) — supplements the local radix tree in `cache_aware` policy with a cross-instance shared table. Enables multi-instance cache-aware routing where N router instances share prefix→worker knowledge instead of learning independently. Up to Nx improvement in KV cache utilization.
- **Grafana + Prometheus monitoring stack** — pre-provisioned 18-panel dashboard with Docker Compose. Covers request traffic, latency percentiles, worker load, circuit breakers, cache hit ratios, routing decisions, errors, and retry pressure. `cd monitoring && docker compose up -d`.
- **Monitoring documentation** (`docs/monitoring.md`) — dashboard panel reference, Prometheus metrics catalog, SLO alerting rules (error rate, latency, cache hit ratio, circuit breaker state).
- 8 new Prometheus metrics: `vllm_router_token_cache_{hits,misses}_total`, `vllm_router_token_cache_entries`, `vllm_router_tokenize_duration_seconds`, `vllm_router_shared_prefix_{hits,misses,writes,stale}_total`.

### Fixed
- **Cache hit/miss Prometheus metrics never recorded** — `vllm_router_cache_hits_total` and `vllm_router_cache_misses_total` were defined but never emitted from `route_with_cache`. Now properly recorded on every cache lookup.

---

## [0.7.3] — 2026-03-22

### Added
- **Token ID cache** (`prompt_cache` config) — caches `hash(request_body) → token_ids` in memory or Redis. Eliminates the `POST /tokenize` HTTP round-trip (~100ms) for repeated system prompts in the LMCache prefix lookup path. Metrics: `vllm_router_token_cache_hits_total`, `_misses_total`, `_entries`, `vllm_router_tokenize_duration_seconds`.
- **Shared prefix routing table** (`shared_prefix_routing` config) — supplements the local radix tree in `cache_aware` policy with a shared prefix→worker table. Enables multi-instance cache-aware routing. Reads on local miss, probabilistic writes (default 10%), TTL-based eviction. Metrics: `vllm_router_shared_prefix_hits_total`, `_misses_total`, `_writes_total`, `_stale_total`.

---

## [0.7.2] — 2026-03-20

### Added
- **End-to-end OTel child spans** — 7 new spans in the routing pipeline: `hooks.pipeline`, `hook.execute`, `cache.exact_lookup`, `cache.semantic_lookup`, `embedding.fetch`, `routing.cluster`, `worker.forward`. Each with typed attributes (outcome, status, worker, policy, duration). Guarded by `is_otel_enabled()` for zero overhead when disabled.
- **Span name contract** — `otel_trace::SPAN_*` constants and `ALL_SPAN_NAMES` slice define stable span names that external tooling can depend on.
- **Hook contract tests** — 18 tests covering every hook outcome: allow, reject (403/400/pass), transform (with/without body, disabled), timeout, non-200, invalid JSON, connection refused, unknown action, multi-hook chain ordering.
- **Decision log schema v2** — `DECISION_SCHEMA_VERSION = 2` with `schema_version` and `hooks_ran` fields. Backward-compatible: v1 records without `schema_version` deserialize with default `1`. `REQUIRED_FIELDS` and `ALL_FIELDS` constants for external validation. 10 contract tests.
- **Explainability header contract** — `HEADER_*` constants and `ALL_EXPLAINABILITY_HEADERS` slice. 6 contract tests verifying header injection for policy, cache-hit, cluster, and minimal decision scenarios.

---

## [0.7.1] — 2026-03-20

### Added
- **OpenTelemetry distributed tracing** (backport upstream #122, Andrew Bennett/Meta) — opt-in OTLP tracing via `trace_config` YAML section. Core init/layer integrated into logging subscriber; full request-level span instrumentation to follow incrementally.
- **OTel tests and benchmark** — `otel_disabled_path_test` (3 tests), `otel_disabled_path` benchmark, `mock_worker` capture functions, `test_app` with tracing toggle.

### Fixed
- **Consistent hash header priority** (backport upstream PR #125) — `x-correlation-id` now checked before `x-request-id`, fixing prefix cache affinity in multi-turn conversations.

---

## [0.7.0] — 2026-03-20

### Added
- **Routing explainability headers** — every response includes `x-vllm-router-worker`, `x-vllm-router-method`, `x-vllm-router-policy`, `x-vllm-router-cluster`, `x-vllm-router-model`, and `x-vllm-router-cache-status`. Controlled via `expose_routing_headers` config (default: true).
- **Admin state endpoints** — `GET /admin/config` (redacted active config), `GET /admin/stats` (cache entries, worker health, policy assignments, uptime), `GET /admin/decisions?limit=N` (recent routing decisions from in-memory ring buffer).
- **Model aliasing and fallback** — `model_rules` config section: rewrite model names before routing (exact match, wildcard `openai/*`, fallback chains that try models in order and pick the first with healthy workers).
- **Pre-routing hooks** — `pre_routing_hooks` config section: ordered HTTP callouts to external services (safety, PII, custom validation) before routing. Hooks can allow, reject (403/400), or transform request bodies. Graceful degradation on timeout/error.
- **Decision export and replay** — `decision_log` config section exports routing decisions to JSONL. `vllm-router replay --decisions file.jsonl --config new-config.yaml` compares routing strategies against historical traffic.
- Sample config: `configs/round-robin-with-hooks.yaml`

### Changed
- **Policy factory uses registry pattern** — `PolicyFactory` now uses a `HashMap<String, Constructor>` instead of hardcoded match statements. External code can register custom policies via `global_factory().register()` before server start.
- `PolicyRegistry` delegates to the global factory instead of duplicating match logic.

---

## [0.6.11] — 2026-03-06

### Added
- **LMCache prefix lookup routing (Phase 2)** — `lookup_mode: prefix_lookup` in `lmcache_aware` policy. Per-request `POST /lookup` to the LMCache controller finds the worker with the longest cached KV prefix. Tokenizes via vLLM's `/tokenize` endpoint (with chat template) to produce matching token IDs. Falls back to occupancy scoring when lookup fails or no prefix is cached. See `configs/lmcache-prefix-lookup-local.yaml`.

---

## [0.6.10] — 2026-03-06

### Added
- **`DPAwareWorker` for PD and regular routers** — backport from upstream (#104). Use `DPAwareWorker` instead of `BasicWorker` when `intra_node_data_parallel_size > 1`, fixing URL corruption with `@rank` suffix in DP mode.
- **`DefaultBodyLimit` for axum Json extractors** — backport from upstream (#109). Multimodal requests with base64 images exceeding 2MB no longer get 413 errors.
- **`/v1/responses` routed via transparent proxy in PD mode** — backport from upstream (#99). Avoids 422 deserialization errors by forwarding raw JSON.
- **Tool message `content` field as `Value`** — backport from upstream (#108). Preserves array content in tool messages instead of coercing to string.

### Fixed
- Integration tests now compile with all custom fields (`inbound_api_key`, `admin_api_key`, `worker_api_keys`, `cache`, `semantic_cache`, `semantic_cluster`, `tokenizer_model_map`).

---

## [0.6.9] — 2026-03-06

### Added
- **Alternative auth headers for k8s proxy** — `X-Router-Key` header accepted as fallback for `inbound_api_key` auth, and `X-Admin-Key` for admin endpoints. Allows authentication when the router is accessed through the Kubernetes service proxy, which strips the `Authorization` header.

---

## [0.6.8] — 2026-03-06

### Changed
- Auth failure message now directs users to the panel or administrator instead of external URL.

---

## [0.6.7] — 2026-03-05

### Changed
- Version bump to trigger CI release build (no code changes from 0.6.6)

---

## [0.6.6] — 2026-03-05

### Added
- **`lmcache_aware` routing policy** — new policy that queries the LMCache controller for real KV cache state instead of maintaining an approximate radix tree. Routes requests to workers with the most cached data using a configurable score: `cache_weight * normalized_key_count + (1 - cache_weight) * normalized_inverse_load`. Falls back to a configurable policy (default: `power_of_two`) when the controller is unreachable.
- **LMCache CLI flags** — `--lmcache-controller-url`, `--lmcache-poll-interval`, `--lmcache-cache-weight`, `--lmcache-lookup-mode`, `--lmcache-controller-timeout-ms` for configuring the policy via command line.
- **Config examples** — `configs/lmcache-aware.yaml` (regular mode) and `configs/pd-lmcache-aware.yaml` (PD disaggregation with lmcache_aware prefill + consistent_hash decode).
- **LMCache integration docs** — `docs/lmcache-integration.md` with architecture, prerequisites, configuration reference, and troubleshooting.

---

## [0.6.5] — 2026-03-05

### Fixed
- **`/v1/models` returns 503 when backends require auth** — `aggregate_models` now sends `worker_api_keys` as Bearer tokens when fetching models from workers.
- **Worker `model_id` shows "unknown" when backends require auth** — `fetch_model_id_from_server` now receives and uses the per-worker API key.

---

## [0.6.4] — 2026-03-05

### Added
- **Embeddings API key authentication** — new `embeddings_api_key` field in `semantic_cache` and `semantic_cluster` config sections. When set, the router sends a `Authorization: Bearer <key>` header to the embeddings endpoint (e.g. Infinity).

### Fixed
- **Nightly-only `is_multiple_of`** — replaced unstable `unsigned_is_multiple_of` usage in `worker.rs` and `worker_registry.rs` with stable `%` operator to build on stable Rust.

---

## [0.6.2] — 2026-03-04

### Fixed
- **Health endpoints exempt from `inbound_api_key`** — `/health`, `/liveness`, `/readiness`, and `/health/generate` are now exempt from inbound API key authentication, preventing Kubernetes probes from failing with 401.
- **Serde defaults for PolicyConfig** — all policy variant fields now have `#[serde(default)]`, so configs like `policy: { type: consistent_hash }` work without specifying every field (e.g. `virtual_nodes` defaults to 160).

---

## [0.6.1] — 2026-03-04

### Fixed
- **Metrics config from YAML ignored** — when using `--config-file`, the `metrics` section (host/port) was ignored and CLI defaults (`127.0.0.1:29000`) were always used. Now `metrics` in YAML takes precedence, fixing Prometheus scraping in Kubernetes.

---

## [0.6.0] — 2026-03-04

### Added
- **Inbound API key authentication** — new `inbound_api_key` config field (YAML or `--inbound-api-key` CLI) to protect all inference endpoints (`/v1/*`) with a static Bearer token. Simpler alternative to `api_key_validation_urls` when no external auth server is needed.

---

## [0.5.1] — 2026-03-04

### Fixed
- **Per-worker API keys not applied to typed routes** — `send_typed_request` (used by `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`, `/v1/rerank`) was forwarding the client's `Authorization` header instead of injecting the per-worker key from `worker_api_keys`. Now uses the same priority as `route_simple_request`: `worker_api_keys` → `api_key` → skip.

---

## [0.5.0] — 2026-03-03

### Added
- **Graceful worker drain** — `POST /admin/drain` marks a worker as draining (stops new requests, waits for in-flight to finish, then auto-removes); `GET /admin/drain/status` to monitor progress
- **Hot configuration reload** — `POST /admin/reload` re-reads the YAML config and applies API key and worker list changes without restarting the router
- **Admin API key authentication** — optional `admin_api_key` (YAML or `--admin-api-key` CLI) to protect `/admin/*` endpoints with a static Bearer token; falls back to `api_key_validation_urls` if not set
- `GET /workers` now includes a `draining` field per worker
- `RoutingMode::all_worker_urls()` helper for worker list diff during reload
- `admin_api_key` example in all `configs/*.yaml` files
- `docs/admin-api.md` — full admin API reference

### Changed
- `Router.api_key` and `Router.worker_api_keys` are now behind `Arc<RwLock<>>` for hot-swap during reload

### Removed
- `examples/` directory (legacy JSON configs from upstream SGLang, superseded by `configs/`)

---

## [0.4.0] — 2026-03-03

### Added
- **Per-worker API keys** — each backend worker can now have its own `Authorization: Bearer` key
  - New `worker_api_keys: HashMap<String, String>` field in `RouterConfig` (url → key)
  - Priority order: per-worker key → global `api_key` → `OPENAI_API_KEY` env var (PD mode only)
  - Applies to all router modes: regular, PD disaggregation (prefill + decode), OpenAI proxy
  - `VllmPDRouter`: fixed hardcoded `OPENAI_API_KEY` env var; now uses proper priority chain
  - Documentation: `docs/authentication.md`

---

## [0.3.0] — 2026-03-03

### Added
- **Redis-backed response cache** — optional Redis backend for both exact-match and semantic response caches, enabled via `--features redis-cache` Cargo feature flag
  - `CacheConfig` YAML section: `cache.backend` (`memory` or `redis`), `cache.max_entries`, `cache.ttl_secs`, `cache.redis.*`
  - `RedisExactCache` and `RedisSemanticCache` implementations using `deadpool-redis` connection pooling
  - Graceful degradation: Redis errors/timeouts return cache miss + warn log, never block requests
  - Shared cache across multiple router instances pointing to the same Redis
  - Sample config: `configs/round-robin-redis-cache.yaml`
- **Async cache traits** (`ExactMatchCache`, `SemanticCacheBackend`) for pluggable cache backends
- **Architecture documentation** (`docs/architecture.md`) — when to use the router, two-level caching diagram, separation of concerns with vLLM/LMCache

### Changed
- Response cache and semantic cache now use trait objects (`Arc<dyn ExactMatchCache>`, `Arc<dyn SemanticCacheBackend>`) instead of concrete types
- Cache `max_entries` and `ttl_secs` are now configurable via YAML `cache:` section (previously hardcoded to 1024/60s)
- `start_test_workers.sh`: updated venv path, removed `--disable-log-requests` flag

---

## [0.2.0] — fork additions

### Added

#### Config file support
- `--config-file <path>` CLI flag to load the full router configuration from a YAML file
- Sample configs for every load balancing policy: `configs/round-robin.yaml`, `configs/random.yaml`, `configs/consistent-hash.yaml`, `configs/power-of-two.yaml`, `configs/cache-aware.yaml`
- `configs/test-semantic-cluster.yaml` — full example with semantic cluster routing and all tuneable parameters documented inline

#### Response caching
- **Exact-match cache** (`src/cache/mod.rs`) — FNV-1a keyed, DashMap-backed, TTL-controlled; strips non-deterministic fields (`stream`, `user`, `request_id`) before hashing; responses > 8 MB are not cached
- **Semantic similarity cache** (`src/cache/semantic.rs`) — cosine similarity search over stored embeddings, configurable threshold, LRU eviction; calls any `/v1/embeddings`-compatible endpoint
- Cache lookup pipeline in `route_with_cache()`: ① exact-match → ② semantic → ③ backend (stores in both caches on miss)
- New CLI flags: `--semantic-cache-embeddings-url`, `--semantic-cache-embeddings-model`, `--semantic-cache-threshold`, `--semantic-cache-max-entries`, `--semantic-cache-ttl-secs`, `--semantic-cache-embedding-timeout-ms`

#### Semantic cluster routing
- `SemanticClusterPolicy` (`src/policies/semantic_cluster.rs`) — routes requests to the worker cluster whose centroid is most similar to the request embedding; round-robin within the cluster
- Cluster centroids are computed at startup by averaging the embeddings of configured example prompts
- Configured via YAML `semantic_cluster:` block (supports multiple clusters with per-cluster worker lists)
- Falls back to regular policy routing when no cluster exceeds the similarity threshold

#### Anthropic Messages API
- `POST /v1/messages` handler — accepts the Anthropic Messages API format and translates it to/from OpenAI Chat Completions internally
- Full bidirectional translation: system prompt, tool definitions (`input_schema` → `parameters`), stop sequences, usage tokens, finish reason
- Streaming support via SSE state machine (`message_start` / `content_block_*` / `message_delta` / `message_stop`)
- `Backend::Anthropic` is now a first-class backend (no longer emits a warning on startup)

#### Session affinity improvements
- Sticky sessions in `ConsistentHashPolicy`: `DashMap<session_id, (worker_url, expiry)>` with 30-minute TTL
- On worker failure mid-session, re-pins to the next healthy worker automatically
- `find_next_healthy_in_ring()` walks the ring BTreeMap from the hash position — preserves maximum affinity instead of random fallback
- Session key extraction priority: `x-semantic-cluster-id` → `x-session-id` → `x-user-id` → `x-tenant-id` → body fields → body hash

#### vLLM Semantic Router integration
- `propagate_all_routing_headers()` forwards all vLLM Semantic Router headers (`x-semantic-*`, `x-routed-by`, `x-model-preference`) plus standard trace headers to backend workers
- `x-semantic-cluster-id` is the highest-priority session key in consistent hash — cluster assignments from an external Semantic Router are honoured

#### Multi-model routing
- `fetch_model_id_from_server()` queries `/get_server_info` when registering workers, so each worker is tagged with its actual model ID
- `select_router_for_request` in `RouterManager` scores routers by `load`, `priority`, and `cost`; respects `x-worker-priority` and `x-max-cost` headers
- Fallback from `get_by_model_fast()` to `get_all()` when workers are registered as `"unknown"` — prevents spurious 503s in single-model deployments

#### OpenAI-compatible backend
- `POST /v1/completions` — proxy with streaming SSE, circuit breaker, Authorization forwarding
- `POST /v1/embeddings` — proxy with circuit breaker
- `POST /v1/rerank` — proxy compatible with Cohere-compatible and vLLM backends

#### Tokenizer
- **SentencePiece** tokenizer support (`src/tokenizer/sentencepiece.rs`) via `sentencepiece` crate; links against system `libsentencepiece-dev`; activated by `.model` extension or `sentencepiece:<path>` spec
- **Model→tokenizer mapping** (`--tokenizer-model-map KEY=SPEC`) — substring case-insensitive match; supports `tiktoken`, `tiktoken:<model>`, local path, and HF model IDs

#### gRPC
- `SessionParams` and `ModelInfo` messages added to `vllm_scheduler.proto`; referenced in `GenerateRequest` and `EmbedRequest`
- IGW mode now registers `grpc-regular` and `grpc-pd` routers at startup alongside the HTTP routers

#### Observability
- INFO-level structured logs for every routing decision: route, model, worker URL, routing method (`policy` or `cluster`), HTTP status, and duration in ms
- New Prometheus counters:
  - `vllm_router_worker_requests_total{route, worker, routing}` — per-worker request counts tagged by routing method
  - `vllm_router_cluster_requests_total{cluster, worker}` — requests routed via semantic cluster
  - `vllm_router_cluster_fallback_total{route}` — cluster misses that fell back to policy routing

#### Developer tooling
- `scripts/start_test_workers.sh` — launches two vLLM chat workers (`:8010`, `:8020`); `--all` flag adds a BAAI/bge-small-en-v1.5 embeddings worker (`:8030`) for semantic cache/cluster testing
