# Configuration Reference

The recommended way to configure the router is with a YAML config file. Sample configs for every policy are in `configs/`.

```bash
vllm-router --config-file configs/round-robin.yaml
```

## Full YAML config structure

```yaml
host: "0.0.0.0"
port: 3000
log_level: info          # trace | debug | info | warn | error

mode:
  type: regular          # regular | pd_disaggregation | igw
  worker_urls:
    - "http://worker1:8000"
    - "http://worker2:8000"

policy:
  type: consistent_hash  # round_robin | random | consistent_hash | power_of_two | cache_aware
  virtual_nodes: 160     # consistent_hash only (all policy fields have sensible defaults and can be omitted)

inbound_api_key: "sk-my-router-key"   # optional: protect all /v1/* inference endpoints
admin_api_key: "my-secret-admin-key"  # optional: protect /admin/* endpoints

metrics:                 # optional (default: 127.0.0.1:29000)
  host: "0.0.0.0"
  port: 29000

health_check:            # optional
  check_interval_secs: 60
  timeout_secs: 5
  failure_threshold: 3
  success_threshold: 2
  endpoint: /health

semantic_cluster:        # optional
  embeddings_url: "http://embeddings:8030"
  embeddings_model: "BAAI/bge-small-en-v1.5"
  embeddings_api_key: "sk-embed-secret"  # optional: Bearer token for embeddings endpoint
  threshold: 0.70
  embedding_timeout_ms: 2000
  clusters:
    - name: my-cluster
      examples: ["example prompt 1", "example prompt 2"]
      workers: ["http://worker1:8000"]
expose_routing_headers: true   # optional (default: true)

model_rules:                   # optional
  - match: "gpt-4"
    rewrite: "meta-llama/Llama-3.1-70B"
  - match: "openai/*"
    rewrite: "meta-llama/Llama-3.1-70B"
  - match: "llama-70b"
    fallback:
      - "meta-llama/Llama-3.1-70B-FP8"
      - "meta-llama/Llama-3.1-8B-FP8"

pre_routing_hooks:             # optional
  - name: "content-safety"
    url: "http://localhost:9001/check"
    timeout_ms: 200
    on_error: pass             # pass | block
    on_reject: block403        # block403 | block400 | pass
    transform: false

cache:                         # optional (default: in-memory)
  backend: redis
  max_entries: 2048
  ttl_secs: 120
  redis:
    url: "redis://127.0.0.1:6379/0"
    pool_size: 8
    key_prefix: "vllm-router:"

decision_log:                  # optional
  export_path: "/var/log/vllm-router/decisions.jsonl"
  export_interval_secs: 10
  include_request_text: false
```

See individual files in `configs/` for per-policy documentation and inline comments.

## Authentication

### Inbound (client → router)

```yaml
# Static API key for all inference endpoints (simplest option)
inbound_api_key: "sk-my-router-key"

# Or validate against an external auth server
api_key_validation_urls:
  - "https://your-auth-server/validate"

# Protect /admin/* endpoints (drain, reload) with a separate key
admin_api_key: "my-secret-admin-key"
```

Priority for inbound auth: `inbound_api_key` → `api_key_validation_urls` → allow all.

When `inbound_api_key` is set, clients must include `Authorization: Bearer <key>` in every request. This is the simplest way to protect your router endpoint without an external auth server.

**Exempt endpoints:** Health probes (`/health`, `/liveness`, `/readiness`, `/health/generate`) are always exempt from inbound authentication so that Kubernetes probes work without a Bearer token.

### Outbound (router → backends)

```yaml
# Global backend key (sent to all workers)
api_key: "sk-global-secret"

# Per-worker keys (override global per worker URL)
worker_api_keys:
  "http://node1:8080": "sk-node1-secret"
  "http://node2:8080": "sk-node2-secret"
```

Priority for outbound auth: `worker_api_keys` → `api_key` → `OPENAI_API_KEY` env var.

See [authentication.md](authentication.md) for the full guide including PD disaggregation, security considerations, and key rotation.

### Embeddings endpoint

When the embeddings server (used by semantic cache or semantic cluster routing) requires authentication, set `embeddings_api_key` in the relevant config section:

```yaml
semantic_cache:
  embeddings_url: "http://infinity:80"
  embeddings_model: "BAAI/bge-small-en-v1.5"
  embeddings_api_key: "sk-embed-secret"   # sent as Authorization: Bearer

semantic_cluster:
  embeddings_url: "http://infinity:80"
  embeddings_model: "BAAI/bge-small-en-v1.5"
  embeddings_api_key: "sk-embed-secret"   # sent as Authorization: Bearer
```

If both `semantic_cache` and `semantic_cluster` are configured, each can have its own key (or share the same one).

## Retries and Circuit Breakers

```yaml
retry:
  max_retries: 3
  initial_backoff_ms: 100
  max_backoff_ms: 10000

circuit_breaker:
  failure_threshold: 5
  success_threshold: 2
  timeout_duration_secs: 30
```

## Tokenizer mapping

```yaml
tokenizer_model_map:
  - "llama=meta-llama/Llama-3.2-1B"
  - "mistral=mistral-community/Mistral-7B-v0.1"
```

Supports: `tiktoken`, `tiktoken:<model>`, local `.model` (SentencePiece), or HuggingFace model ID.

## Prometheus Metrics

```yaml
# In config file:
metrics:
  host: "0.0.0.0"       # default: 127.0.0.1
  port: 29000            # default: 29000

# Or via CLI flags:
#   vllm-router --prometheus-host 0.0.0.0 --prometheus-port 29000
```

When using `--config-file`, the `metrics` section in the YAML takes precedence over CLI flags.

**Important:** For Kubernetes deployments, set `host: "0.0.0.0"` so Prometheus can scrape the pod. The default (`127.0.0.1`) only listens on localhost and is unreachable from outside the container.

## Routing Explainability Headers

Every response includes `x-vllm-router-*` headers showing how the routing decision was made:

| Header | Value | When |
|--------|-------|------|
| `x-vllm-router-worker` | Worker URL that handled the request | When routed to a worker |
| `x-vllm-router-method` | `cache-hit`, `semantic-hit`, `cluster`, `lmcache-prefix`, `policy` | Always |
| `x-vllm-router-policy` | Policy name (e.g. `round_robin`) | When method=policy |
| `x-vllm-router-cluster` | Semantic cluster name | When method=cluster |
| `x-vllm-router-model` | Model ID routed to | Always |
| `x-vllm-router-cache-status` | `exact-hit`, `semantic-hit`, `miss` | Non-streaming only |
| `x-vllm-router-hooks` | Comma-separated list of hooks that ran | When hooks are configured |

Disable with:

```yaml
expose_routing_headers: false
```

## Model Rules

Rewrite model names before routing. Useful for aliasing external model names to local models, or providing fallback chains when a model has no healthy workers.

```yaml
model_rules:
  # Simple alias
  - match: "gpt-4"
    rewrite: "meta-llama/Llama-3.1-70B"

  # Wildcard: anything starting with "openai/"
  - match: "openai/*"
    rewrite: "meta-llama/Llama-3.1-70B"

  # Fallback chain: try in order, use first with healthy workers
  - match: "llama-70b"
    fallback:
      - "meta-llama/Llama-3.1-70B-FP8"
      - "meta-llama/Llama-3.1-8B-FP8"
```

Rules are evaluated in order; first match wins. If no rule matches, the model name passes through unchanged.

Model rules run before cache key computation and before semantic cluster routing, so the rewritten model name is used throughout the entire pipeline.

## Pre-Routing Hooks

HTTP callouts to external services that run before routing. Use for safety checks, PII detection, content moderation, or custom validation.

```yaml
pre_routing_hooks:
  - name: "content-safety"
    url: "http://localhost:9001/check"
    timeout_ms: 200
    on_error: pass        # pass = skip hook on error; block = fail request
    on_reject: block403   # block403 | block400 | pass
  - name: "pii-mask"
    url: "http://localhost:9002/mask"
    timeout_ms: 100
    on_error: pass
    transform: true       # replace request body with hook's response body
```

Each hook receives the request body as JSON via POST and must respond with:

```json
{"action": "allow"}
{"action": "reject", "reason": "unsafe content"}
{"action": "transform", "body": {"model": "...", "messages": [...]}}
```

Hooks run in order. If a hook rejects, the request is blocked immediately. If a hook transforms and `transform: true` is set, the modified body is used for subsequent hooks and routing.

## Decision Log Export

Export routing decisions to a JSONL file for analysis or replay.

```yaml
decision_log:
  export_path: "/var/log/vllm-router/decisions.jsonl"
  export_interval_secs: 10
  include_request_text: false   # set true for content-aware replay
```

Each line in the JSONL file is a routing decision with timestamp, model, method, worker, cache status, and latency.

Use `vllm-router replay` to compare routing strategies against exported decisions:

```bash
vllm-router replay --decisions decisions.jsonl --config configs/power-of-two.yaml
```

See [metrics.md](metrics.md) for the full metrics reference.

## Health Checks

```yaml
health_check:
  check_interval_secs: 60   # how often to check each worker (seconds)
  timeout_secs: 5            # per-check timeout
  failure_threshold: 3       # consecutive failures before marking unhealthy
  success_threshold: 2       # consecutive successes to mark healthy again
  endpoint: /health          # health check endpoint on each worker
```

Health checks run in a background loop. State changes are logged:
- `info` when a worker goes from unhealthy → healthy
- `warn` when a worker goes from healthy → unhealthy
- `debug` when a worker remains unhealthy (set `RUST_LOG=debug` to see these)
