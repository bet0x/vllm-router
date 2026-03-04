# vLLM Router — Documentation

Detailed guides for configuring and operating the vLLM Router extended fork.

For a quick overview and getting started instructions, see the [main README](../README.md).

## Contents

| Guide | Description |
|-------|-------------|
| [Architecture](architecture.md) | When to use the router, separation of concerns with vLLM/LMCache, caching layers |
| [Configuration](configuration.md) | Full YAML reference, CLI flags, authentication, retries, circuit breakers, tokenizer mapping |
| [Authentication](authentication.md) | Inbound client validation, global and per-worker backend API keys, PD disaggregation credentials, security best practices |
| [Load Balancing](load-balancing.md) | Policy overview, use-case recommendations, multi-turn routing, per-policy details, decision tree |
| [Semantic Routing](semantic-routing.md) | Cluster routing by prompt content with embeddings |
| [Caching](caching.md) | Exact-match and semantic response cache pipeline |
| [Anthropic API](anthropic-api.md) | Anthropic Messages API support and streaming |
| [PD Disaggregation](pd-disaggregation.md) | Prefill-Decode split inference, multi-turn with PD |
| [Metrics](metrics.md) | Full Prometheus metrics reference |
| [Kubernetes](kubernetes.md) | Kubernetes service discovery setup |
