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
