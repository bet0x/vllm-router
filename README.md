# vLLM Router — Extended Fork

> **Fork of [vllm-project/router](https://github.com/vllm-project/router)**
> This fork extends the upstream router with response caching, semantic cluster routing, Anthropic API support, and several production-readiness improvements. See [CHANGELOG.md](CHANGELOG.md) for the full list of additions.

A high-performance, lightweight request forwarding system for vLLM large-scale deployments, providing advanced load balancing, prefill/decode disaggregation, and semantic-aware routing.

---

## What this fork adds

| Feature | Upstream | This fork |
|---------|----------|-----------|
| Config file (`--config-file`) | ❌ | ✅ YAML config for all settings |
| Exact-match response cache | ❌ | ✅ FNV-1a, DashMap, TTL |
| Semantic similarity cache | ❌ | ✅ Cosine similarity + embeddings endpoint |
| Semantic cluster routing | ❌ | ✅ Route by prompt content to worker clusters |
| Anthropic Messages API | ❌ | ✅ `POST /v1/messages` with streaming |
| Sticky sessions + graceful failover | ❌ | ✅ DashMap TTL + ring walk on failure |
| vLLM Semantic Router header propagation | ❌ | ✅ `x-semantic-*` headers forwarded to workers |
| SentencePiece tokenizer | ❌ | ✅ Via system `libsentencepiece` |
| Model→tokenizer mapping | ❌ | ✅ `--tokenizer-model-map` substring match |
| `POST /v1/completions` (OpenAI backend) | ❌ | ✅ Proxy + streaming SSE |
| `POST /v1/embeddings` (OpenAI backend) | ❌ | ✅ Proxy |
| `POST /v1/rerank` (OpenAI backend) | ❌ | ✅ Proxy |
| gRPC `SessionParams` / `ModelInfo` proto | ❌ | ✅ In `vllm_scheduler.proto` |
| Per-routing-decision Prometheus metrics | ❌ | ✅ Worker, cluster, and fallback counters |
| INFO-level routing logs | ❌ | ✅ Model, worker, method, status, duration |

---

## Quick Start

### Prerequisites

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env

# System deps (Ubuntu/Debian)
sudo apt-get install -y protobuf-compiler libprotobuf-dev libsentencepiece-dev
```

### Build

```bash
cargo build --release
```

### Run with a config file

The easiest way to start the router is with a YAML config file. Sample configs for every policy are in `configs/`:

```bash
# Round-robin across two workers
cargo run -- --config-file configs/round-robin.yaml

# Cache-aware routing (reduces TTFT by reusing vLLM's KV cache)
cargo run -- --config-file configs/cache-aware.yaml

# Session affinity (same user always goes to same worker)
cargo run -- --config-file configs/consistent-hash.yaml

# Semantic cluster routing (routes by prompt content)
cargo run -- --config-file configs/test-semantic-cluster.yaml
```

### Run with CLI flags

```bash
./target/release/vllm-router \
    --worker-urls http://worker1:8000 http://worker2:8000 \
    --policy consistent_hash \
    --host 0.0.0.0 \
    --port 3000
```

---

## Load Balancing Policies

| Policy | Description | Session Affinity | Best For |
|--------|-------------|-----------------|----------|
| `round_robin` | Strict sequential rotation | No | Uniform workloads |
| `random` | Random worker per request | No | Simple deployments |
| `consistent_hash` | Same session → same worker | Yes | Multi-turn chat |
| `power_of_two` | Least loaded of 2 random workers | No | Variable-length requests |
| `cache_aware` | Worker with most cached prompt prefix | Yes | Repeated prompts, few-shot |

### Policy by use case

| Use case | Recommended policy | Why |
|----------|--------------------|-----|
| Multi-turn chat (strict affinity) | `consistent_hash` | Pins each `session_id` / `user_id` to one worker for the lifetime of the session. vLLM's KV cache is preserved across turns. If the worker dies, the next healthy worker in the ring is used automatically. |
| Multi-turn chat (fault-tolerant) | `cache_aware` | Every turn sends the full conversation history in the request body. The router picks the worker that already has the most of that prefix cached, so vLLM can rebuild the KV cache even after a failure. |
| Batch inference / one-shot completions | `power_of_two` | No session state needed; picks the least loaded of two random workers, avoiding hot-spots under variable request durations. |
| Repeated prompts / few-shot templates | `cache_aware` | Maximises KV cache reuse when many requests share a long common prefix (system prompt, few-shot examples). |
| Simple scaling, homogeneous workers | `round_robin` | Predictable, zero overhead, works well when all workers are equivalent and request durations are similar. |
| Routing by prompt content (topics / domains) | `consistent_hash` + semantic clusters | Requests are embedded and matched to the nearest cluster centroid; workers within that cluster are then chosen by consistent hash for KV cache affinity. |
| Multi-tenant API (per-customer isolation) | `consistent_hash` | `x-user-id` or `x-tenant-id` header pins each tenant to a dedicated worker, preventing cross-tenant cache pollution. |

### Multi-turn chat: `consistent_hash` vs `cache_aware`

```
Turn 1: session-123 → consistent_hash → Worker A  ✓ (KV cache built on A)
Turn 2: session-123 → sticky map     → Worker A  ✓ (KV cache reused)
Turn 3: Worker A fails ✗
Turn 4: session-123 → next in ring   → Worker B  ✓ (KV cache lost, rebuilt from history)

                 vs.

Turn 1: session-123 → cache_aware → Worker A  ✓ (A has no cache yet, routed by load)
Turn 2: session-123 → cache_aware → Worker A  ✓ (A has 100% prefix match)
Turn 3: Worker A fails ✗
Turn 4: session-123 → cache_aware → Worker B  ✓ (full history in body, B rebuilds cache)
```

**Rule of thumb:** use `consistent_hash` when minimising latency on the first token is critical and your workers are stable. Use `cache_aware` when you need automatic recovery without manual session management.

Session key extraction order for `consistent_hash`:
1. `x-semantic-cluster-id` (vLLM Semantic Router cluster)
2. `x-session-id` / `x-user-id` / `x-tenant-id`
3. `body.session_params.session_id` / `body.user`
4. Full body hash (not sticky)

---

## Semantic Cluster Routing

Routes requests to the worker cluster whose centroid is most similar to the request embedding. Cluster centroids are computed at startup from example prompts.

```yaml
# configs/test-semantic-cluster.yaml (excerpt)
policy:
  type: consistent_hash

semantic_cluster:
  embeddings_url: "http://localhost:8030"
  embeddings_model: "BAAI/bge-small-en-v1.5"
  threshold: 0.70
  clusters:
    - name: coding
      examples:
        - "Write a Python function to sort a list"
        - "Debug this Rust borrow checker error"
      workers:
        - "http://localhost:8010"
    - name: general
      examples:
        - "What is the capital of France?"
        - "Explain quantum entanglement"
      workers:
        - "http://localhost:8020"
```

---

## Response Caching

Two-level cache pipeline on every non-streaming request:

1. **Exact-match** — FNV-1a hash of canonical JSON (strips `stream`/`user`/`request_id`)
2. **Semantic** — cosine similarity against stored embeddings (configurable threshold)
3. **Backend** — on miss, response is stored in both caches

```bash
cargo run -- \
    --config-file configs/round-robin.yaml \
    --semantic-cache-embeddings-url http://localhost:8030 \
    --semantic-cache-embeddings-model BAAI/bge-small-en-v1.5 \
    --semantic-cache-threshold 0.95 \
    --semantic-cache-ttl-secs 300
```

---

## Anthropic Messages API

The router natively accepts the Anthropic Messages API format and translates it internally to OpenAI Chat Completions:

```bash
curl http://localhost:3000/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model": "my-model",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

Streaming is fully supported. The response follows the Anthropic SSE event format (`message_start`, `content_block_delta`, `message_stop`).

---

## Configuration Reference

### Full YAML config structure

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

See individual files in `configs/` for per-policy documentation.

### Authentication

```bash
# .env
API_KEY_VALIDATION_URLS=https://your-auth-server/validate

# or CLI
vllm-router --api-key-validation-urls https://your-auth-server/validate
```

### Metrics

Prometheus endpoint at `127.0.0.1:29000` by default.

```bash
vllm-router --prometheus-host 0.0.0.0 --prometheus-port 9000
```

Key metrics added in this fork:
- `vllm_router_worker_requests_total{route, worker, routing}` — per-worker counts by routing method
- `vllm_router_cluster_requests_total{cluster, worker}` — semantic cluster hits
- `vllm_router_cluster_fallback_total{route}` — cluster misses falling back to policy

### Retries and Circuit Breakers

```bash
vllm-router \
  --retry-max-retries 3 \
  --retry-initial-backoff-ms 100 \
  --retry-max-backoff-ms 10000 \
  --cb-failure-threshold 5 \
  --cb-success-threshold 2 \
  --cb-timeout-duration-secs 30
```

### Tokenizer mapping

```bash
vllm-router \
  --tokenizer-model-map "llama=meta-llama/Llama-3.2-1B" \
  --tokenizer-model-map "mistral=mistral-community/Mistral-7B-v0.1"
```

Supports: `tiktoken`, `tiktoken:<model>`, local `.model` (SentencePiece), or HuggingFace model ID.

---

## Prefill-Decode Disaggregation

```bash
cargo run --release -- \
    --policy consistent_hash \
    --vllm-pd-disaggregation \
    --prefill http://127.0.0.1:8081 \
    --prefill http://127.0.0.1:8082 \
    --decode http://127.0.0.1:8083 \
    --decode http://127.0.0.1:8084 \
    --host 127.0.0.1 \
    --port 8090
```

---

## Kubernetes Service Discovery

```bash
vllm-router \
    --service-discovery \
    --selector app=vllm-worker role=inference \
    --service-discovery-namespace default
```

---

## Development

```bash
# Run all tests
cargo test --lib

# Lint (requires nightly)
cargo +nightly clippy -- -D warnings

# Start local test workers (requires vLLM)
./scripts/start_test_workers.sh         # chat workers on :8010 and :8020
./scripts/start_test_workers.sh --all   # + BAAI/bge-small-en-v1.5 embeddings on :8030
```

---

## Acknowledgements

This project is a fork of [vllm-project/router](https://github.com/vllm-project/router), which is itself a fork of [SGLang Model Gateway](https://github.com/sgl-project/sglang/tree/main/sgl-model-gateway). We thank the original authors for their work.
