# vLLM Router — Monitoring Stack

Pre-provisioned Grafana + Prometheus stack for the vLLM Router.

## Quick Start

```bash
docker compose up -d
```

- **Grafana**: http://localhost:3001 (admin / admin)
- **Prometheus**: http://localhost:9090

The vLLM Router dashboard loads automatically as the home page.

## Requirements

- Docker with Compose v2
- vLLM Router running on the host (metrics on `:29000`)

## Ports

| Service    | Port | Description |
|------------|------|-------------|
| Prometheus | 9090 | Metric storage and queries |
| Grafana    | 3001 | Dashboard UI |
| Router     | 29000 | Prometheus scrape target (not started by this stack) |

## Dashboard

18 panels organized in 6 sections:

1. **Overview** — Active workers, request rate, P99 latency, error rate, cache hit ratio, retries
2. **Request Traffic** — Requests/sec by route, latency percentiles (P50/P95/P99)
3. **Workers** — Per-worker request rate, processed requests, circuit breaker state
4. **Routing Decisions** — Routing method distribution, policy→worker mapping
5. **Cache & Prefix** — Response cache, token cache, shared prefix table
6. **Reliability** — Retries, circuit breaker outcomes, errors by type

Collapsed sections for **Semantic Cluster Routing** and **PD Disaggregation** (visible when those features are active).

## Customization

- Edit `prometheus.yml` to add more scrape targets or change intervals
- Edit `dashboards/vllm-router.json` to customize panels
- Add alerting rules to `prometheus.yml` (see [docs/monitoring.md](../docs/monitoring.md) for examples)
