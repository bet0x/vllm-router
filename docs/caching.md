# Response Caching

Two-level cache pipeline on every non-streaming request:

1. **Exact-match** — FNV-1a hash of canonical JSON (strips `stream`/`user`/`request_id`)
2. **Semantic** — cosine similarity against stored embeddings (configurable threshold)
3. **Backend** — on miss, response is stored in both caches

## Configuration

Configure via YAML (see `configs/test-semantic-cluster.yaml` for a full example):

```yaml
host: "0.0.0.0"
port: 3000
log_level: info

mode:
  type: regular
  worker_urls:
    - "http://localhost:8010"
    - "http://localhost:8020"

policy:
  type: round_robin

semantic_cache:
  embeddings_url: "http://localhost:8030"
  embeddings_model: "BAAI/bge-small-en-v1.5"
  embeddings_api_key: "sk-embed-secret"  # optional: Bearer token for the embeddings endpoint
  threshold: 0.95          # cosine similarity required for a cache hit
  max_entries: 256
  ttl_secs: 300
  embedding_timeout_ms: 500
```

## Cache keys

The exact-match cache normalises the request body before hashing:
- Removes `stream`, `user`, and `request_id` fields
- Serialises the remaining JSON canonically
- Hashes with FNV-1a for fast lookups

The semantic cache computes an embedding of the prompt and compares it (cosine similarity) against all stored entries. A hit requires similarity >= `threshold`.

## Redis backend

By default both caches live in-memory. To share cached responses across multiple router instances or persist them across restarts, enable the Redis backend.

### Build with Redis support

```bash
cargo build --release --features redis-cache
```

### Configuration

Add a `cache` section to your YAML config:

```yaml
cache:
  backend: redis          # "memory" (default) or "redis"
  max_entries: 2048       # exact-match cache capacity (default: 1024)
  ttl_secs: 120           # time-to-live per entry (default: 60)
  redis:
    url: "redis://127.0.0.1:6379/0"
    pool_size: 8
    key_prefix: "vllm-router:"
    connection_timeout_ms: 3000
    command_timeout_ms: 500
```

When `cache.backend` is `memory` (or the `cache` section is absent), behaviour is identical to previous releases — no Redis dependency required.

### Shared cache across instances

With Redis, every router instance pointing to the same Redis server shares the same response cache. A cache hit on one instance benefits all others immediately.

```
Client → Router A ──┐
                     ├──→ Redis (shared response cache)
Client → Router B ──┘
```

### Graceful degradation

Redis errors never fail a request. On timeout or connection error the cache returns a miss and logs a warning. The router continues forwarding requests to backend workers.

## Behaviour

- Streaming requests (`"stream": true`) bypass both caches entirely.
- On a cache hit (exact or semantic), the cached response is returned immediately without contacting any backend worker.
- On a miss, the response from the backend is stored in both the exact-match and semantic caches for future requests.
- Entries are evicted after `ttl_secs` or when `max_entries` is reached (LRU).
