# Monitoring & Observability

The router exposes 67+ Prometheus metrics on `:29000` and ships with a pre-provisioned Grafana dashboard.

## Quick Start

```bash
cd monitoring
docker compose up -d
```

- **Grafana**: http://localhost:3001 (admin / admin)
- **Prometheus**: http://localhost:9090

The dashboard auto-loads as the home page. No manual configuration needed.

## Dashboard Panels

### Overview (stat panels)

| Panel | Metric | Description |
|-------|--------|-------------|
| Active Workers | `vllm_router_active_workers` | Number of registered workers |
| Request Rate | `rate(vllm_router_requests_total[1m])` | Requests per second |
| P99 Latency | `histogram_quantile(0.99, ...)` | 99th percentile response time |
| Error Rate | `rate(vllm_router_request_errors_total[1m])` | Errors per second |
| Cache Hit Ratio | `hits / (hits + misses)` | Response cache effectiveness |
| Retries / min | `rate(vllm_router_retries_total[1m]) * 60` | Retry pressure |

### Request Traffic

- **Requests/sec by Route** — breakdown by endpoint (`/v1/chat/completions`, `/generate`, etc.)
- **Latency Percentiles** — P50, P95, P99 over time

### Workers

- **Requests/sec per Worker** — load distribution across backends
- **Processed Requests** — cumulative request count per worker
- **Circuit Breaker State** — Closed (green), Open (red), Half-Open (yellow)

### Routing Decisions

- **Routing Method** — distribution of `policy`, `cluster`, `lmcache-prefix`, `cache-hit`
- **Policy → Worker** — which policy routed to which worker

### Cache & Prefix

- **Response Cache** — exact-match hit/miss rate
- **Token Cache** — tokenization cache hit/miss (active when `prompt_cache` is configured)
- **Shared Prefix Table** — hits, misses, writes, stale entries (active when `shared_prefix_routing` is configured)

### Reliability

- **Retries** — retry count and exhausted retries per route
- **Circuit Breaker Outcomes** — success/failure per worker
- **Errors by Type** — `no_available_workers`, `non_retryable_error`, etc.

### Collapsed Sections

- **Semantic Cluster Routing** — cluster routing rate and fallbacks (active when `semantic_cluster` is configured)
- **PD Disaggregation** — prefill/decode request rates and errors (active in PD mode)

## Prometheus Metrics Reference

All metrics use the `vllm_router_` or `vllm_tokenizer_` prefix. Key metrics for SLO alerting:

### Request SLOs

```yaml
# Error rate above 1% for 5 minutes
- alert: HighErrorRate
  expr: sum(rate(vllm_router_request_errors_total[5m])) / sum(rate(vllm_router_requests_total[5m])) > 0.01
  for: 5m

# P99 latency above 5 seconds
- alert: HighLatency
  expr: histogram_quantile(0.99, sum(rate(vllm_router_generate_duration_seconds_bucket[5m])) by (le)) > 5
  for: 5m
```

### Cache SLOs

```yaml
# Cache hit ratio dropped below 50%
- alert: CacheHitRatioLow
  expr: sum(rate(vllm_router_cache_hits_total[5m])) / (sum(rate(vllm_router_cache_hits_total[5m])) + sum(rate(vllm_router_cache_misses_total[5m]))) < 0.5
  for: 5m

# Token cache hit ratio below 80% after warmup
- alert: TokenCacheHitRatioLow
  expr: sum(rate(vllm_router_token_cache_hits_total[5m])) / (sum(rate(vllm_router_token_cache_hits_total[5m])) + sum(rate(vllm_router_token_cache_misses_total[5m]))) < 0.8
  for: 10m
```

### Worker SLOs

```yaml
# Circuit breaker open for any worker
- alert: CircuitBreakerOpen
  expr: vllm_router_cb_state == 1
  for: 1m

# No healthy workers
- alert: NoHealthyWorkers
  expr: vllm_router_active_workers == 0
  for: 30s
```

## Custom Dashboards

All metrics are available in Prometheus for custom queries. Common patterns:

```promql
# Requests per second by model (via worker label)
sum(rate(vllm_router_worker_requests_total[1m])) by (worker)

# Cache-aware routing vs load balancing
sum(rate(vllm_router_worker_requests_total{routing="policy"}[1m]))
sum(rate(vllm_router_worker_requests_total{routing="cluster"}[1m]))

# Shared prefix table effectiveness
sum(rate(vllm_router_shared_prefix_hits_total[5m])) /
(sum(rate(vllm_router_shared_prefix_hits_total[5m])) + sum(rate(vllm_router_shared_prefix_misses_total[5m])))
```

## Architecture

```
┌──────────┐     :3000      ┌──────────────┐     :8010/:8020
│  Client   ├──────────────►│  vllm-router  ├──────────────►  vLLM workers
└──────────┘                └───────┬───────┘
                                    │ :29000
                            ┌───────▼───────┐
                            │  Prometheus    │ :9090
                            └───────┬───────┘
                            ┌───────▼───────┐
                            │   Grafana     │ :3001
                            └───────────────┘
```
