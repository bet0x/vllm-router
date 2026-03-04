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
  virtual_nodes: 160     # consistent_hash only

admin_api_key: "my-secret-admin-key"  # optional: protect /admin/* endpoints

semantic_cluster:        # optional
  embeddings_url: "http://embeddings:8030"
  embeddings_model: "BAAI/bge-small-en-v1.5"
  threshold: 0.70
  embedding_timeout_ms: 2000
  clusters:
    - name: my-cluster
      examples: ["example prompt 1", "example prompt 2"]
      workers: ["http://worker1:8000"]
```

See individual files in `configs/` for per-policy documentation and inline comments.

## Authentication

```yaml
# Global backend key (sent to all workers)
api_key: "sk-global-secret"

# Per-worker keys (override global per worker URL)
worker_api_keys:
  "http://node1:8080": "sk-node1-secret"
  "http://node2:8080": "sk-node2-secret"

# Inbound client validation
api_key_validation_urls:
  - "https://your-auth-server/validate"

# Protect /admin/* endpoints (drain, reload) with a static key
admin_api_key: "my-secret-admin-key"
```

Priority for outbound requests: `worker_api_keys` → `api_key` → `OPENAI_API_KEY` env var.

See [authentication.md](authentication.md) for the full guide including PD disaggregation, security considerations, and key rotation.

Or via environment variable:

```bash
# .env
API_KEY_VALIDATION_URLS=https://your-auth-server/validate
```

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

## Prometheus metrics

```yaml
prometheus_host: "0.0.0.0"
prometheus_port: 9000       # default: 29000
```

See [metrics.md](metrics.md) for the full metrics reference.
