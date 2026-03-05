# Changelog

All notable changes to this fork are documented here.
Upstream: [vllm-project/router](https://github.com/vllm-project/router) | Fork: [bet0x/vllm-router](https://github.com/bet0x/vllm-router)

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
