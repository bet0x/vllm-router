# Features — vLLM Router Extended Fork

Everything below is added by this fork. The [upstream router](https://github.com/vllm-project/router) does not include any of these features.

| Feature | Description |
|---------|-------------|
| Config file (`--config-file`) | YAML config for all settings |
| Exact-match response cache | FNV-1a, DashMap, TTL |
| Semantic similarity cache | Cosine similarity + embeddings endpoint |
| Semantic cluster routing | Route by prompt content to worker clusters |
| Anthropic Messages API | `POST /v1/messages` with streaming |
| Sticky sessions + graceful failover | DashMap TTL + ring walk on failure |
| vLLM Semantic Router header propagation | `x-semantic-*` headers forwarded to workers |
| SentencePiece tokenizer | Via system `libsentencepiece` |
| Model-to-tokenizer mapping | `--tokenizer-model-map` substring match |
| `POST /v1/completions` (OpenAI backend) | Proxy + streaming SSE |
| `POST /v1/embeddings` (OpenAI backend) | Proxy |
| `POST /v1/rerank` (OpenAI backend) | Proxy |
| gRPC `SessionParams` / `ModelInfo` proto | In `vllm_scheduler.proto` |
| LMCache-aware routing (`lmcache_aware`) | Real KV cache state from LMCache controller (occupancy + prefix lookup) |
| Per-routing-decision Prometheus metrics | Worker, cluster, and fallback counters |
| INFO-level routing logs | Model, worker, method, status, duration |
| Per-worker API keys | Each backend can have its own `Authorization: Bearer` key |
| Embeddings endpoint auth | `embeddings_api_key` for semantic cache and cluster routing |
| Graceful worker drain | `POST /admin/drain` — stop traffic, wait for in-flight, then remove |
| Hot config reload | `POST /admin/reload` — re-read YAML, swap keys & workers without restart |
| Routing explainability headers | `x-vllm-router-*` headers on every response (worker, method, policy, cache status, similarity score) |
| Model aliasing & fallback | `model_rules` — rewrite model names, wildcard matching, fallback chains |
| Pre-routing hooks | HTTP callouts to external services for safety, PII, custom validation |
| Admin state endpoints | `/admin/config`, `/admin/stats`, `/admin/decisions` |
| Decision export & replay | JSONL export + `vllm-router replay` for evidence-based policy comparison |
| Token ID cache | Cache tokenization results for LMCache prefix lookup (~100ms to <1ms) |
| Shared prefix routing | Multi-instance `cache_aware` via shared prefix table (memory/Redis) |
| OpenTelemetry tracing | OTLP distributed tracing with W3C TraceContext propagation |
| Multi-tenant API keys | Per-tenant rate limits, model ACL, SHA-256 hashed keys, hot reload |
| Grafana dashboard | Pre-provisioned 18-panel dashboard + Prometheus + Docker Compose |
| Web dashboard UI | React + TypeScript real-time dashboard — no Grafana needed ([docs](docs/dashboard.md)) |
| Worker metrics proxy | `GET /workers/{url}/metrics` — vLLM Prometheus metrics proxied through admin auth |
| Tenant model access on all routes | `/v1/messages` and `/v1/responses` enforce `allowed_models` like `/v1/chat/completions` |

---

## Performance Impact

Measured improvements over a standard vLLM deployment without this router, and over deployments using the upstream router without these optimizations:

| Scenario | Without this router | With this router | Improvement |
|----------|-------------------|------------------|-------------|
| Repeated prompts (exact match) | Full inference (~200ms+) | Sub-millisecond cache hit | **200x+ faster** |
| Similar prompts (semantic cache) | Full inference | Cached response if similarity >= threshold | **Up to 200x faster** |
| LMCache prefix lookup overhead | 300ms (tokenize + lookup) | <200ms (token cache hit + lookup) | **33% less routing overhead** |
| Multi-instance prefix awareness | Each instance learns independently (1/N utilization) | Shared prefix table across instances | **Up to Nx cache utilization** |
| System prompt tokenization | 100ms HTTP round-trip per request | <1ms Redis/memory lookup | **100x faster tokenization** |
| Routing decision visibility | Logs only | Headers + admin API + Grafana + JSONL export | **Full observability** |

> These numbers are for the routing layer only. Actual end-to-end latency depends on model inference time, which the router does not control.
