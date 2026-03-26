# Dashboard UI

A self-contained web dashboard for monitoring and managing vllm-router. No external dependencies (Grafana, Prometheus server, etc.) required — the dashboard talks directly to the router.

## Quick Start

```bash
# 1. Start the router
./target/release/vllm-router --config-file configs/round-robin.yaml

# 2. Start the dashboard (dev mode)
cd ui
npm install
npm run dev
```

Open http://localhost:5173 and enter the admin API key configured in `admin_api_key`.

## Production Build

```bash
cd ui
npm run build
```

The `dist/` folder contains static files that can be served by any HTTP server. Configure your reverse proxy to route `/api/*` to the router and `/metrics` to the Prometheus endpoint.

## Architecture

```
Browser (React SPA)
  │
  ├── /api/*     → Vite proxy → router :3000  (admin + worker endpoints)
  ├── /metrics   → Vite proxy → router :29000 (Prometheus metrics)
  └── /api/workers/{url}/metrics → router proxies to worker's /metrics
```

The dashboard polls two data sources every 5 seconds:

- **Router Prometheus metrics** (`:29000/metrics`) — request rates, latencies, cache stats, circuit breaker states, per-worker and per-tenant counters
- **Router REST API** (`:3000/admin/*`, `/workers`) — stats, decisions, tenants, config, worker list

## Authentication

The dashboard requires the `admin_api_key` configured in the router's YAML config. The key is sent as both `X-Admin-Key` (for admin endpoints) and `Authorization: Bearer` (for worker/inference endpoints).

When multi-tenant `api_keys` are configured, the admin key must also be registered as a tenant with `allowed_models: ["*"]` to access non-admin endpoints like `/workers`:

```yaml
admin_api_key: "my-secret-admin-key"
api_keys:
  - key: "my-secret-admin-key"
    name: "admin"
    rate_limit_rps: 1000
    max_concurrent: 100
    allowed_models: ["*"]
```

## Panels

### Overview

Summary cards showing: workers (healthy/total), total requests, total errors, live request rate, cache hit rate, P50/P95 latency, decisions logged. Includes throughput and latency percentile time-series charts (populated when traffic is flowing). Shows available models from `/v1/models`.

### Workers

Table of all workers with: URL, model ID, type (regular/prefill/decode), health status, circuit breaker state, running requests, total requests served, priority.

**Click a worker row** to see vLLM engine metrics proxied from the worker's `/metrics` endpoint:

- Requests running / waiting
- KV cache usage %
- Time to first token (TTFT) P50/P95
- Inter-token latency P50/P95
- End-to-end latency P50/P95
- Prompt / generation tokens processed
- Prefix cache hit rate
- Request outcomes (stop/length/abort/error)
- Process memory usage

### Requests

Request totals by route (bar chart), requests by worker (bar chart), policy decisions breakdown (grouped by policy with per-worker distribution), errors by type, live throughput chart. Summary cards for P50/P95/P99 latency and retry stats.

### Cache

Exact and semantic cache entry counts, hit/miss ratio (pie chart), cache totals. Flush cache action is in Settings > Actions.

### Tenants

Summary cards (tenant count, total requests, rate limited, errors). Bar charts for requests and rate-limited counts by tenant. Table with tenant config (status, rate limit, max concurrent, allowed models) merged with live Prometheus counters.

Only visible when `api_keys` is configured in the router.

### Decisions

Live feed of routing decisions from `/admin/decisions`. Table with: timestamp, route, model, routing method, policy, worker, cache status, HTTP status code, duration, tenant. Rows are color-coded by status (green=2xx, yellow=4xx, red=5xx).

### Settings

Tabbed settings panel with four sections:

**Configuration** — Active router configuration parsed into logical sections (Server, Routing, Workers, Authentication, Health & Resilience, Concurrency, Cache, Advanced). Secrets are redacted. Includes a Reload Config button.

**Worker Management** — Add worker (URL form), drain worker (with configurable timeout), remove worker. Drain status is polled live showing current load with an animated progress indicator.

**Actions** — Reload Config (re-read YAML and apply changes), Flush Cache (clear all caches on all workers).

**Connection** — Change API key, logout.

## Data Flow

```
Prometheus /metrics  ──parse──►  MetricStore (ring buffer, 120 points)
                                      │
                                      ▼
                              useMetrics hook (5s poll)
                                      │
                          ┌───────────┼───────────┐
                          ▼           ▼           ▼
                     Overview    Requests    Workers
                      Panel       Panel      Panel

REST /admin/*  ──JSON──►  usePolling hook (5s poll)
                                      │
                          ┌───────────┼───────────┐
                          ▼           ▼           ▼
                     Overview    Decisions   Tenants
                      Panel       Panel      Panel
```

- Counters are converted to rates (delta / interval) for RPS charts
- Histograms are interpolated to compute P50/P95/P99 percentiles
- Time-series data is kept in a browser-memory ring buffer (last ~10 minutes)
- Charts only render when there are 3+ data points to avoid single-dot display

## Tech Stack

| Layer | Choice |
|-------|--------|
| Bundler | Vite |
| Framework | React 19 |
| Language | TypeScript |
| Styling | Tailwind CSS 4 |
| Charts | Recharts |
| State | React hooks + polling |
| HTTP | fetch API |

## Vite Proxy Configuration

In development, `vite.config.ts` proxies:

- `/api/*` → `http://localhost:3000` (strips `/api` prefix)
- `/metrics` → `http://localhost:29000`

This avoids CORS issues without modifying the router.
