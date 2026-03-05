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
| LMCache-aware routing (`lmcache_aware`) | ❌ | ✅ Real KV cache state from LMCache controller |
| Per-routing-decision Prometheus metrics | ❌ | ✅ Worker, cluster, and fallback counters |
| INFO-level routing logs | ❌ | ✅ Model, worker, method, status, duration |
| Per-worker API keys | ❌ | ✅ Each backend can have its own `Authorization: Bearer` key |
| Embeddings endpoint auth | ❌ | ✅ `embeddings_api_key` for semantic cache and cluster routing |
| Graceful worker drain | ❌ | ✅ `POST /admin/drain` — stop traffic, wait for in-flight, then remove |
| Hot config reload | ❌ | ✅ `POST /admin/reload` — re-read YAML, swap keys & workers without restart |

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

### Run

The easiest way to start the router is with a YAML config file. Sample configs for every policy are in `configs/`:

```bash
# Round-robin across two workers
vllm-router --config-file configs/round-robin.yaml

# Cache-aware routing (reduces TTFT by reusing vLLM's KV cache)
vllm-router --config-file configs/cache-aware.yaml

# Session affinity (same user always goes to same worker)
vllm-router --config-file configs/consistent-hash.yaml

# Semantic cluster routing (routes by prompt content)
vllm-router --config-file configs/test-semantic-cluster.yaml

# LMCache-aware routing (real KV cache state from controller)
vllm-router --config-file configs/lmcache-aware.yaml
```

---

## Documentation

Detailed guides are in the [`docs/`](docs/) folder:

| Guide | Description |
|-------|-------------|
| [Architecture](docs/architecture.md) | When to use the router, separation of concerns with vLLM/LMCache, caching layers |
| [Configuration](docs/configuration.md) | Full YAML reference, CLI flags, authentication, retries, circuit breakers, tokenizer mapping |
| [Authentication](docs/authentication.md) | Inbound client validation, per-worker backend API keys, embeddings endpoint auth, health probe exemptions |
| [Load Balancing](docs/load-balancing.md) | Policy overview with defaults, use-case recommendations, multi-turn routing, per-policy details |
| [Semantic Routing](docs/semantic-routing.md) | Cluster routing by prompt content with embeddings, API key support |
| [Caching](docs/caching.md) | Exact-match and semantic response cache pipeline, Redis backend |
| [Anthropic API](docs/anthropic-api.md) | Anthropic Messages API support and streaming |
| [PD Disaggregation](docs/pd-disaggregation.md) | Prefill-Decode split inference, multi-turn with PD |
| [Metrics](docs/metrics.md) | Full Prometheus metrics reference |
| [Admin API](docs/admin-api.md) | Graceful worker drain and hot configuration reload |
| [LMCache Integration](docs/lmcache-integration.md) | LMCache controller-driven cache-aware routing |
| [Kubernetes](docs/kubernetes.md) | Kubernetes service discovery setup |

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

This project is a fork of [vllm-project/router](https://github.com/vllm-project/router) (original author: Byron Hsu), which is itself a fork of [SGLang Model Gateway](https://github.com/sgl-project/sglang/tree/main/sgl-model-gateway). We thank the original authors for their work.

Maintained by [Alberto Ferrer](https://github.com/bet0x).
