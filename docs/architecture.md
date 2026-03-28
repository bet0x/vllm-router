# Architecture & When to Use the Router

## What the router does

The vLLM Router is a **request-level load balancer** for vLLM deployments. It decides which worker handles each request and optionally caches responses to avoid redundant inference.

```
Clients → Router ─── TCP ──→ vLLM Workers (remote GPU inference)
                 └── UDS ──→ vLLM Worker  (same-host, no TCP overhead)
```

### Router responsibilities

| Responsibility | Description |
|----------------|-------------|
| Load balancing | Distribute requests across workers (`round_robin`, `power_of_two`, etc.) |
| Session affinity | Pin sessions to workers for latency optimization (`consistent_hash`) |
| Response caching | Cache full HTTP responses (exact-match + semantic) to avoid re-inference |
| Semantic cluster routing | Route by prompt content to specialized worker groups |
| Health checks & circuit breakers | Detect unhealthy workers and stop sending traffic |
| Retries with backoff | Retry failed requests on other workers |
| Anthropic API translation | Accept Anthropic Messages format, translate to OpenAI |
| Prometheus metrics | Expose routing, worker, and cache metrics |

### What the router does NOT do

| Not the router's job | Who handles it |
|----------------------|----------------|
| KV cache storage | vLLM (GPU memory) or [LMCache](https://docs.lmcache.ai) (Redis/S3) |
| KV cache transfer between workers | vLLM via NIXL connector (UCX/GDS) or LMCache |
| KV cache sharing/migration | [LMCache with Redis](https://docs.lmcache.ai/kv_cache/redis.html) |
| Model loading & inference | vLLM |
| Tensor parallelism | vLLM |
| Token generation | vLLM |

## When you need the router

**Multiple vLLM workers serving the same model.** If you have 2+ workers, you need something to distribute requests. The router gives you intelligent routing beyond what a generic load balancer (Nginx, HAProxy) provides:

- **Cache-aware routing** — routes to the worker most likely to have the prompt prefix cached
- **Session affinity with failover** — sticky sessions with automatic ring-walk on worker failure
- **Semantic routing** — routes by prompt content to specialized worker clusters
- **Response caching** — identical/similar prompts return cached responses without hitting any worker

## When you don't need the router

**Single worker.** No routing needed.

**LMCache with Redis handles your KV cache needs.** If you deploy [LMCache](https://docs.lmcache.ai) with a shared Redis backend, every worker can access any session's KV cache. In this scenario:

- `consistent_hash` becomes an **optimization** (local GPU cache is faster than Redis), not a requirement
- `cache_aware` routing adds little value since any worker can rebuild from shared cache
- You may still want the router for load balancing, response caching, and health checks

**vLLM's built-in PD disaggregation.** If vLLM handles prefill-decode coordination internally (via NIXL), you don't need the router's PD mode. The router's PD mode is useful when you want explicit control over prefill/decode worker selection and routing policies.

## Two levels of caching

There are two distinct caching layers that complement each other:

```
Client request
    │
    ▼
┌──────────────────────────────────────┐
│ Router Response Cache (Layer 1)      │
│ • Exact-match: FNV-1a hash → response│
│ • Semantic: cosine similarity → response│
│ • Returns cached HTTP response       │
│ • Avoids hitting any worker          │
└──────────────┬───────────────────────┘
               │ cache miss
               ▼
┌──────────────────────────────────────┐
│ vLLM KV Cache (Layer 2)             │
│ • Stored in GPU memory per worker   │
│ • Or shared via LMCache + Redis     │
│ • Reuses computed attention states   │
│ • Reduces time-to-first-token       │
└──────────────────────────────────────┘
```

| Cache | What it stores | Where | Scope | Benefit |
|-------|---------------|-------|-------|---------|
| **Router response cache** | Full HTTP responses | Router memory (or Redis) | Per-router instance (or shared with Redis) | Skip inference entirely for repeated requests |
| **vLLM KV cache** | Attention key-value states | Worker GPU memory (or Redis via LMCache) | Per-worker (or shared with LMCache) | Faster token generation for shared prefixes |

### When both layers help

A typical production deployment benefits from both:

1. **Router response cache** catches identical requests (e.g., health checks, repeated queries, A/B test duplicates) — zero GPU cost
2. **vLLM KV cache** accelerates unique requests that share prefixes (e.g., same system prompt, few-shot examples) — reduced TTFT

### Future: Redis-backed router response cache

The router's response cache is currently in-memory only. A Redis backend would allow:

- Multiple router instances sharing cached responses
- Cache persistence across router restarts
- Centralized cache management and eviction policies

This is a planned improvement. Note that this is separate from LMCache's Redis — the router would cache **HTTP responses**, while LMCache caches **KV states**.
