# Changelog

All notable changes to this fork are documented here.
Upstream: [vllm-project/router](https://github.com/vllm-project/router) | Fork: [bet0x/vllm-router](https://github.com/bet0x/vllm-router)

---

## [Unreleased]

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

## [Unreleased] — fork additions on top of upstream main

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
