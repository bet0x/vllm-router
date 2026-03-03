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

## Behaviour

- Streaming requests (`"stream": true`) bypass both caches entirely.
- On a cache hit (exact or semantic), the cached response is returned immediately without contacting any backend worker.
- On a miss, the response from the backend is stored in both the exact-match and semantic caches for future requests.
- Entries are evicted after `ttl_secs` or when `max_entries` is reached (LRU).
