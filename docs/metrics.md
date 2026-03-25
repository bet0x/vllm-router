# Prometheus Metrics Reference

Prometheus endpoint at `127.0.0.1:29000` by default. Override in YAML:

```yaml
prometheus_host: "0.0.0.0"
prometheus_port: 9000
```

## Request metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `vllm_router_requests_total` | counter | `route` | Total requests received by the router |
| `vllm_router_request_duration_seconds` | histogram | `route` | End-to-end request latency |
| `vllm_router_request_errors_total` | counter | `route`, `error_type` | Requests that returned an error |
| `vllm_router_retries_total` | counter | `route` | Retry attempts triggered |
| `vllm_router_retries_exhausted_total` | counter | `route` | Requests that exhausted all retries |

## Worker metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `vllm_router_active_workers` | gauge | — | Number of registered workers |
| `vllm_router_worker_health` | gauge | `worker` | Health status per worker (1=healthy, 0=unhealthy) |
| `vllm_router_worker_load` | gauge | `worker` | Current in-flight request count per worker |
| `vllm_router_processed_requests_total` | counter | `worker` | Requests completed per worker |
| `vllm_router_worker_requests_total` | counter | `route`, `worker`, `routing` | Requests forwarded per worker, tagged by routing method (`cluster` or `policy`) |
| `vllm_router_worker_request_duration_seconds` | histogram | `route`, `worker` | Latency per worker and route |

## Routing metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `vllm_router_policy_decisions_total` | counter | `policy`, `worker` | Decisions made by each load-balancing policy |
| `vllm_router_cluster_requests_total` | counter | `cluster`, `worker` | Requests routed via semantic cluster matching |
| `vllm_router_cluster_fallback_total` | counter | `route` | Requests that fell below the similarity threshold and used the default policy |
| `vllm_router_cache_hits_total` | counter | — | Exact-match cache hits |
| `vllm_router_cache_misses_total` | counter | — | Exact-match cache misses |
| `vllm_router_running_requests` | gauge | `worker` | Running requests per worker (used by `cache_aware` policy) |
| `vllm_router_tree_size` | gauge | `worker` | Prefix tree size per worker (`cache_aware` policy) |
| `vllm_router_load_balancing_events_total` | counter | — | Load-balancing override events |
| `vllm_router_max_load` / `vllm_router_min_load` | gauge | — | Max/min load across all workers |

## Circuit breaker metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `vllm_router_cb_state` | gauge | `worker` | Circuit breaker state (0=closed, 1=open, 2=half_open) |
| `vllm_router_cb_state_transitions_total` | counter | `worker` | State transitions per worker |
| `vllm_router_cb_outcomes_total` | counter | `worker`, `outcome` | Outcomes recorded by the circuit breaker (`success`/`failure`) |

## PD disaggregation metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `vllm_router_pd_requests_total` | counter | `route` | Total two-stage PD requests |
| `vllm_router_pd_prefill_requests_total` | counter | `worker` | Requests sent to each prefill worker |
| `vllm_router_pd_decode_requests_total` | counter | `worker` | Requests sent to each decode worker |
| `vllm_router_pd_request_duration_seconds` | histogram | `route` | End-to-end duration of PD requests |
| `vllm_router_pd_errors_total` | counter | `error_type` | PD routing errors |
| `vllm_router_pd_prefill_errors_total` | counter | `worker` | Prefill stage errors |
| `vllm_router_pd_decode_errors_total` | counter | `worker` | Decode stage errors |
| `vllm_router_pd_stream_errors_total` | counter | `worker` | Streaming errors |

## Service discovery metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `vllm_router_discovery_updates_total` | counter | — | Service discovery update events |
| `vllm_router_discovery_workers_added` | gauge | — | Workers added in the last discovery update |
| `vllm_router_discovery_workers_removed` | gauge | — | Workers removed in the last discovery update |

## Tokenizer metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `vllm_tokenizer_encode_duration_seconds` | histogram | — | Time to encode text to tokens |
| `vllm_tokenizer_decode_duration_seconds` | histogram | — | Time to decode tokens to text |
| `vllm_tokenizer_encode_requests_total` | counter | `tokenizer_type` | Encode requests by tokenizer |
| `vllm_tokenizer_factory_loads_total` | counter | `file_type` | Tokenizer load events |
| `vllm_tokenizer_vocab_size` | gauge | — | Vocabulary size of the loaded tokenizer |

## Per-tenant metrics

These metrics are only emitted when multi-tenant API keys (`api_keys`) are configured.

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `vllm_router_tenant_requests_total` | counter | `tenant`, `route` | Request volume per tenant |
| `vllm_router_tenant_request_duration_seconds` | histogram | `tenant`, `route` | Latency per tenant |
| `vllm_router_tenant_errors_total` | counter | `tenant`, `route`, `error_type` | Error rate per tenant |
| `vllm_router_tenant_rate_limited_total` | counter | `tenant` | 429 responses due to per-tenant rate limiting |
| `vllm_router_tenant_tokens_total` | counter | `tenant` | Token usage per tenant (from response body) |
