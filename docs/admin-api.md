# Admin API

The router exposes administrative endpoints for operational tasks like
graceful worker drain and hot configuration reload.

## Authentication

Admin endpoints can be protected with a static API key. Set `admin_api_key` in
the YAML config or pass `--admin-api-key` via CLI:

```yaml
# config.yaml
admin_api_key: "my-secret-admin-key"
```

```bash
# or via CLI
vllm-router --admin-api-key my-secret-admin-key ...
```

When set, all `/admin/*` requests must include the key as a Bearer token or
via the `X-Admin-Key` header:

```bash
# Standard Bearer token
curl -X POST http://router:3001/admin/reload \
  -H 'Authorization: Bearer my-secret-admin-key'

# Alternative header (useful behind k8s service proxy)
curl -X POST http://router:3001/admin/reload \
  -H 'X-Admin-Key: my-secret-admin-key'
```

Similarly, the `inbound_api_key` for inference endpoints (`/v1/*`, `/workers`,
`/add_worker`) accepts both `Authorization: Bearer <key>` and `X-Router-Key`:

```bash
curl http://router:3001/workers \
  -H 'X-Router-Key: my-inbound-key'
```

> **Why alternative headers?** When the router sits behind a Kubernetes service
> proxy, the API server consumes the `Authorization` header for its own auth and
> does not forward it to the backend service. Custom headers like `X-Router-Key`
> and `X-Admin-Key` pass through unmodified.

Requests without the key (or with a wrong key) get `401 Unauthorized`.

If `admin_api_key` is **not** set, admin endpoints fall back to the same
authentication mechanism as other endpoints (`api_key_validation_urls`). If
neither is configured, admin endpoints are open.

## Graceful Worker Drain

### `POST /admin/drain`

Mark a worker as **draining**: the router immediately stops sending new requests
to the worker while letting in-flight requests finish. Once the worker's load
drops to zero it is automatically removed from the pool.

**Request body:**

```json
{
  "url": "http://worker1:8080",
  "timeout_secs": 300
}
```

| Field          | Type   | Default | Description                                      |
|----------------|--------|---------|--------------------------------------------------|
| `url`          | string | —       | Worker URL to drain (required)                   |
| `timeout_secs` | u64    | 300     | Max seconds to wait before force-removing worker |

**Response:** `202 Accepted`

```json
{
  "status": "draining",
  "url": "http://worker1:8080",
  "timeout_secs": 300
}
```

### `GET /admin/drain/status?url=<worker_url>`

Check the drain status of a worker.

**Response:** `200 OK`

```json
{
  "url": "http://worker1:8080",
  "draining": true,
  "current_load": 3,
  "healthy": true
}
```

If the worker has already been removed (drain completed), returns `404`.

## Hot Configuration Reload

### `POST /admin/reload`

Re-read the YAML configuration file and apply changes without restarting the
router. Currently supports:

- **API key changes** — global `api_key` and per-worker `worker_api_keys`
- **Worker list changes** — new workers are added, removed workers are drained

**Prerequisites:** The router must have been started with `--config-file`.
CLI-only setups will return `400 Bad Request`.

**Request:** No body required.

**Response:** `200 OK`

```json
{
  "status": "ok",
  "reload": "Config reloaded: api_key, worker_api_keys updated",
  "workers_added": ["http://new-worker:8080"],
  "workers_drained": ["http://old-worker:8080"]
}
```

## Active Configuration

### `GET /admin/config`

Returns the running configuration as JSON. Sensitive fields (`api_key`, `admin_api_key`, `inbound_api_key`, `worker_api_keys` values, `embeddings_api_key`) are redacted as `"***"`.

```bash
curl -H 'Authorization: Bearer my-secret-admin-key' \
  http://router:3000/admin/config | jq .policy
```

## Stats Snapshot

### `GET /admin/stats`

Returns a snapshot of internal state: cache stats, worker health, policy assignments, uptime.

```json
{
  "uptime_secs": 3600,
  "cache": {
    "backend": "memory",
    "exact_entries": 142,
    "semantic_entries": 0
  },
  "workers": { "total": 4, "healthy": 3, "draining": 1 },
  "policies": {
    "default": "round_robin",
    "per_model": { "llama-3": "cache_aware" }
  },
  "decisions_logged": 582
}
```

## Recent Routing Decisions

### `GET /admin/decisions?limit=50`

Returns the last N routing decisions from an in-memory ring buffer (max 1000 entries). Each entry records how a request was routed: method, policy, worker, cache status, latency.

```bash
curl -H 'Authorization: Bearer my-secret-admin-key' \
  'http://router:3000/admin/decisions?limit=5' | jq .
```

```json
{
  "decisions": [
    {
      "timestamp": "2026-03-20T18:30:00.123Z",
      "route": "/v1/chat/completions",
      "model": "llama-3",
      "method": "policy",
      "policy": "round_robin",
      "worker": "http://localhost:8010",
      "cache_status": "miss",
      "status": 200,
      "duration_ms": 85,
      "tenant": "ml-team"
    }
  ]
}
```

The `tenant` field is only present when multi-tenant API keys are configured.

## Tenant Management

### `GET /admin/tenants`

List all configured tenants with live status (requires multi-tenant `api_keys` to be configured):

```bash
curl -H 'Authorization: Bearer my-secret-admin-key' \
  http://router:3000/admin/tenants | jq .
```

```json
{
  "tenants": [
    {
      "name": "ml-team",
      "enabled": true,
      "rate_limit_rps": 100,
      "max_concurrent": 50,
      "allowed_models": ["*"],
      "total_requests": 58423,
      "total_rate_limited": 17,
      "metadata": { "org": "ml-research" }
    }
  ]
}
```

## Worker List

The `GET /workers` endpoint now includes a `draining` field for each worker:

```json
{
  "workers": [
    {
      "url": "http://worker1:8080",
      "model_id": "meta-llama/Llama-3.2-1B",
      "is_healthy": true,
      "draining": false,
      "load": 5,
      ...
    }
  ]
}
```

## Typical Workflows

### Rolling GPU maintenance

```bash
# 1. Drain worker before maintenance
curl -X POST http://router:3001/admin/drain \
  -H 'Authorization: Bearer my-secret-admin-key' \
  -H 'Content-Type: application/json' \
  -d '{"url": "http://gpu-node1:8080", "timeout_secs": 120}'

# 2. Monitor drain progress
curl -H 'Authorization: Bearer my-secret-admin-key' \
  'http://router:3001/admin/drain/status?url=http://gpu-node1:8080'

# 3. After maintenance, add worker back
curl -X POST 'http://router:3001/add_worker?url=http://gpu-node1:8080'

# Unix socket workers use url-encoded unix:// URLs
curl -X POST 'http://router:3001/add_worker?url=unix:///tmp/vllm.sock'
```

### Rotating API keys

```yaml
# config.yaml — update the keys
api_key: "sk-new-global-key"
worker_api_keys:
  "http://node1:8080": "sk-node1-rotated"
```

```bash
# Apply without restart
curl -X POST http://router:3001/admin/reload \
  -H 'Authorization: Bearer my-secret-admin-key'
```
