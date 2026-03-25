# Authentication

The router handles authentication at two levels:

1. **Inbound** — validating API keys from clients calling the router
2. **Outbound** — authorizing requests the router sends to backend workers

---

## Multi-tenant API keys

For multi-user deployments, configure `api_keys` with per-tenant rate limits and model access control:

```yaml
api_keys:
  - key: "sk-team-ml-xxxxx"
    name: "ml-team"
    rate_limit_rps: 100        # requests/sec (own token bucket)
    max_concurrent: 50         # max concurrent requests
    allowed_models: ["*"]      # glob patterns for allowed models
    enabled: true
    metadata:
      org: "ml-research"

  - key: "sk-team-backend-yyyyy"
    name: "backend-team"
    rate_limit_rps: 50
    max_concurrent: 20
    allowed_models: ["Llama-3*"]
    enabled: true
    metadata:
      org: "product-eng"
```

When `api_keys` is configured, it takes priority over `inbound_api_key` and `api_key_validation_urls`.

**Features:**

- **Per-tenant rate limiting** — each key has its own token bucket. Exceeded limits return `429 Too Many Requests` with `X-RateLimit-Limit` and `Retry-After` headers.
- **Model access control** — `allowed_models` supports exact names and trailing wildcards (e.g. `Llama-3*`). Denied access returns `403 Forbidden`.
- **Disabled keys** — set `enabled: false` to revoke a key without removing it.
- **Observability** — tenant name appears in `/admin/decisions`, per-tenant Prometheus metrics (`vllm_router_tenant_*`), and `/admin/tenants`.
- **Hot reload** — `POST /admin/reload` reloads tenant keys from config without restart.
- **Security** — keys are stored as SHA-256 hashes in memory; the plaintext key is never kept after init.

**Priority order** (when multiple auth methods are configured):

```
1. api_keys           (multi-tenant, per-key rate limits)
2. inbound_api_key    (single static key)
3. api_key_validation_urls (external service)
4. open access        (if none configured)
```

---

## Inbound: client authentication

```yaml
# Validate incoming requests against an external auth server
api_key_validation_urls:
  - "https://your-auth-server/validate"
```

Or via environment variable:

```bash
API_KEY_VALIDATION_URLS=https://your-auth-server/validate
```

When set, every request received by the router must carry a valid `Authorization: Bearer <key>` header. The router forwards the key to each validation URL and rejects the request if any check fails.

**Exempt endpoints:** Health probes (`/health`, `/liveness`, `/readiness`, `/health/generate`) are always exempt from both `inbound_api_key` and `api_key_validation_urls` so that Kubernetes liveness/readiness probes work without a Bearer token.

---

## Outbound: backend worker authentication

The router sends `Authorization: Bearer <key>` to each backend worker. You can configure this globally or per-worker.

### Global key (all workers share the same key)

```yaml
api_key: "sk-global-secret"
```

This key is added to every outbound request regardless of which worker is selected.

### Per-worker keys

For multi-model or multi-provider deployments where each backend has its own credentials:

```yaml
worker_api_keys:
  "http://node1:8080": "sk-node1-secret"
  "http://node2:8080": "sk-node2-secret"
  # node3 has no entry → falls back to api_key global
```

The lookup key must match the worker URL exactly as declared in `worker_urls` / `prefill_urls` / `decode_urls`.

### Priority order

For each request the router resolves the authorization key in this order:

```
1. per-worker key from worker_api_keys  (highest priority)
2. global api_key
3. OPENAI_API_KEY environment variable  (PD disaggregation only, last resort)
4. no Authorization header sent
```

### Full example — mixed credentials

```yaml
host: "0.0.0.0"
port: 8090

mode:
  type: regular
  worker_urls:
    - "http://llama-node1:8080"    # uses sk-llama
    - "http://llama-node2:8080"    # uses sk-llama
    - "http://mistral-node:8081"   # uses sk-mistral
    - "http://internal-node:8082"  # no entry → uses global api_key

api_key: "sk-internal-fallback"

worker_api_keys:
  "http://llama-node1:8080": "sk-llama"
  "http://llama-node2:8080": "sk-llama"
  "http://mistral-node:8081": "sk-mistral"

policy:
  type: round_robin
```

### PD disaggregation with per-worker keys

Prefill and decode workers can have different API keys:

```yaml
mode:
  type: vllm_prefill_decode
  prefill_urls:
    - "http://prefill1:8081"
    - "http://prefill2:8081"
  decode_urls:
    - "http://decode1:8083"
    - "http://decode2:8083"

worker_api_keys:
  "http://prefill1:8081": "sk-prefill-cluster"
  "http://prefill2:8081": "sk-prefill-cluster"
  "http://decode1:8083":  "sk-decode-cluster"
  "http://decode2:8083":  "sk-decode-cluster"
```

---

## Header format

All outbound authorization uses the standard HTTP Bearer token format (RFC 6750):

```
Authorization: Bearer <api_key>
```

This is the same format used by OpenAI, Anthropic, and most LLM providers.

---

## Admin API authentication

The admin endpoints (`/admin/drain`, `/admin/reload`) can be protected with a
dedicated static API key, independent of inbound client authentication:

```yaml
admin_api_key: "my-secret-admin-key"
```

```bash
# or via CLI
vllm-router --admin-api-key my-secret-admin-key ...
```

When set, all `/admin/*` requests must include the key as a Bearer token:

```bash
curl -X POST http://router:3001/admin/reload \
  -H 'Authorization: Bearer my-secret-admin-key'
```

If `admin_api_key` is **not** set, admin endpoints fall back to the same
inbound authentication (`api_key_validation_urls`). If neither is configured,
admin endpoints are open.

See [admin-api.md](admin-api.md) for the full admin API reference.

---

## Embeddings endpoint authentication

When using semantic cache or semantic cluster routing with an embeddings server that requires authentication (e.g. Infinity), set `embeddings_api_key` in the relevant config section:

```yaml
semantic_cache:
  embeddings_url: "http://infinity:80"
  embeddings_model: "BAAI/bge-small-en-v1.5"
  embeddings_api_key: "sk-embed-secret"

semantic_cluster:
  embeddings_url: "http://infinity:80"
  embeddings_model: "BAAI/bge-small-en-v1.5"
  embeddings_api_key: "sk-embed-secret"
```

The key is sent as `Authorization: Bearer <embeddings_api_key>` on every request to the embeddings endpoint (both for computing cluster centroids at startup and for embedding incoming requests at runtime).

---

## Security considerations

- **Inbound and outbound keys are independent.** The key a client sends to the router is never forwarded to backend workers. The router uses the configured `worker_api_keys` / `api_key` instead.
- **Minimum privilege.** Assign each worker its own key so a compromised worker credential cannot access other workers.
- **Key rotation.** Update `worker_api_keys` in the config file and call `POST /admin/reload` to apply without restart. See [admin-api.md](admin-api.md).
- **Config file security.** Store the config file with restricted permissions (`chmod 600`) since it contains credentials in plaintext.
- **Environment variables.** For container deployments, inject sensitive keys via Kubernetes Secrets or similar rather than baking them into config files.
