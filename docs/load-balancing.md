# Load Balancing Policies

The vLLM Router supports multiple load balancing policies for distributing requests across backend workers. Each policy has a ready-to-use config file in `configs/`.

## Available Policies

| Policy | Description | Session Affinity | Config file |
|--------|-------------|-----------------|-------------|
| `round_robin` | Strict sequential rotation | No | `configs/round-robin.yaml` |
| `random` | Random worker per request | No | `configs/random.yaml` |
| `consistent_hash` | Same session → same worker | Yes | `configs/consistent-hash.yaml` |
| `power_of_two` | Least loaded of 2 random workers | No | `configs/power-of-two.yaml` |
| `cache_aware` | Worker with most cached prompt prefix | Yes | `configs/cache-aware.yaml` |
| `lmcache_aware` | Worker with most real KV cache data (via LMCache controller) | Yes | `configs/lmcache-aware.yaml` |

---

## Policy by use case

| Use case | Recommended policy | Why |
|----------|--------------------|-----|
| Multi-turn chat (strict affinity) | `consistent_hash` | Pins each `session_id` / `user_id` to one worker for the lifetime of the session. vLLM's KV cache is preserved across turns. If the worker dies, the next healthy worker in the ring is used automatically. |
| Multi-turn chat (fault-tolerant) | `cache_aware` | Every turn sends the full conversation history in the request body. The router picks the worker that already has the most of that prefix cached, so vLLM can rebuild the KV cache even after a failure. |
| Multi-turn chat with PD disaggregation | `decode_policy: consistent_hash` + `prefill_policy: power_of_two` | KV cache accumulates on the decode worker, not the prefill worker. Pin the decode pool by session; let the prefill pool balance freely. See `configs/pd-disagg.yaml`. |
| Batch inference / one-shot completions | `power_of_two` | No session state needed; picks the least loaded of two random workers, avoiding hot-spots under variable request durations. |
| Repeated prompts / few-shot templates | `cache_aware` | Maximises KV cache reuse when many requests share a long common prefix (system prompt, few-shot examples). |
| Multi-turn with LMCache controller | `lmcache_aware` | Routes to the worker with the most real KV cache data, as reported by the LMCache controller. Eliminates heuristic guesswork. Requires LMCache controller deployment. See [LMCache Integration](lmcache-integration.md). |
| PD disaggregation with LMCache | `prefill: lmcache_aware` + `decode: consistent_hash` | Prefill workers are no longer stateless when LMCache tracks cache state. Route prefill to the worker that already has the prefix cached. See `configs/pd-lmcache-aware.yaml`. |
| Simple scaling, homogeneous workers | `round_robin` | Predictable, zero overhead, works well when all workers are equivalent and request durations are similar. |
| Routing by prompt content (topics / domains) | `consistent_hash` + semantic clusters | Requests are embedded and matched to the nearest cluster centroid; workers within that cluster are then chosen by consistent hash for KV cache affinity. See `configs/test-semantic-cluster.yaml`. |
| Multi-tenant API (per-customer isolation) | `consistent_hash` | `x-user-id` or `x-tenant-id` header pins each tenant to a dedicated worker, preventing cross-tenant cache pollution. |

---

## Multi-turn chat: `consistent_hash` vs `cache_aware`

```
Turn 1: session-123 → consistent_hash → Worker A  ✓ (KV cache built on A)
Turn 2: session-123 → sticky map     → Worker A  ✓ (KV cache reused)
Turn 3: Worker A fails ✗
Turn 4: session-123 → next in ring   → Worker B  ✓ (KV cache lost, rebuilt from history)

                 vs.

Turn 1: session-123 → cache_aware → Worker A  ✓ (A has no cache yet, routed by load)
Turn 2: session-123 → cache_aware → Worker A  ✓ (A has 100% prefix match)
Turn 3: Worker A fails ✗
Turn 4: session-123 → cache_aware → Worker B  ✓ (full history in body, B rebuilds cache)
```

**Rule of thumb:** use `consistent_hash` when minimising latency on the first token is critical and your workers are stable. Use `cache_aware` when you need automatic recovery without manual session management.

---

## Per-policy details

### Round Robin

Distributes requests evenly across all healthy workers in sequential order.

```bash
vllm-router --config-file configs/round-robin.yaml
```

```yaml
# configs/round-robin.yaml
policy:
  type: round_robin
```

- Cycles through workers: worker1 → worker2 → worker3 → worker1 → ...
- Skips unhealthy workers automatically
- Simple and predictable distribution
- No session affinity (each request may go to a different worker)

Best for stateless workloads, single-turn requests, and when even distribution is more important than cache locality.

### Random

Selects a random healthy worker for each request.

```bash
vllm-router --config-file configs/random.yaml
```

```yaml
# configs/random.yaml
policy:
  type: random
```

- Uniform random selection among healthy workers
- Statistically even distribution over many requests
- No session affinity

Best for simple deployments, testing, and development.

### Consistent Hash

Routes requests with the same session/user identifier to the same backend worker. Essential for multi-turn conversations, KV cache reuse, and session affinity.

```bash
vllm-router --config-file configs/consistent-hash.yaml
```

```yaml
# configs/consistent-hash.yaml
policy:
  type: consistent_hash
  virtual_nodes: 160   # vnodes per worker (default: 160, can be omitted)
```

> **Note:** All policy fields have sensible defaults and can be omitted. For example, `policy: { type: consistent_hash }` is valid — `virtual_nodes` defaults to 160.

#### Session key extraction order

The consistent hash policy extracts a routing key in the following priority order:

| Priority | Source | Key |
|----------|--------|-----|
| 1 | HTTP Header | `x-session-id` |
| 2 | HTTP Header | `x-user-id` |
| 3 | Request Body | `session_params.session_id` |
| 4 | Request Body | `user` |
| 5 | Fallback | Hash of full body (not sticky) |

Only `session:` and `user:` keys are pinned in the sticky map. Requests without a session ID use the body hash fallback and are not sticky.

#### Usage example

```bash
# Same x-session-id always routes to the same worker
curl http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-session-id: user-abc" \
  -d '{"model":"my-model",
       "messages":[{"role":"user","content":"Hello"}]}'
```

#### Behavior

- **Consistency**: Same session ID always routes to the same worker
- **Unhealthy fallback**: If the target worker is unhealthy, walks the ring to the next healthy worker
- **Virtual nodes**: Uses 160 virtual nodes per worker for even distribution (configurable)
- **DP-aware routing**: Supports data-parallel worker URLs (e.g., `http://worker:8000@0`)

### Power of Two Choices

Randomly selects two workers and routes to the one with lower load.

```bash
vllm-router --config-file configs/power-of-two.yaml
```

```yaml
# configs/power-of-two.yaml
policy:
  type: power_of_two
  load_check_interval_secs: 10   # how often to poll worker load
```

1. Randomly pick two healthy workers
2. Query their current load (pending requests)
3. Route to the worker with lower load

Best for load-sensitive workloads and variable request durations. Requires at least 2 workers.

### Cache Aware

Optimizes for prefix caching by maintaining an approximate radix tree of request prefixes per worker.

```bash
vllm-router --config-file configs/cache-aware.yaml
```

```yaml
# configs/cache-aware.yaml
policy:
  type: cache_aware
  cache_threshold: 0.5           # min prefix match ratio [0.0–1.0]
  balance_abs_threshold: 32      # absolute load gap to force rebalancing
  balance_rel_threshold: 1.1     # relative load ratio to force rebalancing
  eviction_interval_secs: 30     # prefix tree pruning interval
  max_tree_size: 10000           # max nodes per radix tree
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `cache_threshold` | 0.5 | Minimum prefix match ratio to use cache-based routing |
| `balance_abs_threshold` | 32 | Absolute load difference threshold for load balancing |
| `balance_rel_threshold` | 1.1 | Relative load ratio threshold for load balancing |
| `eviction_interval_secs` | 30 | Interval for cache eviction |
| `max_tree_size` | 10000 | Maximum nodes per radix tree |

**Balanced mode** (when load is even): finds the worker with the highest prefix match. If the match rate exceeds `cache_threshold`, routes there (cache hit); otherwise routes to the worker with the smallest tree (most cache capacity).

**Imbalanced mode** (when load is skewed): routes to the worker with the lowest load.

Best for workloads with repeated prompt prefixes (system prompts, few-shot examples) and multi-tenant deployments with distinct prompt patterns.

### LMCache Aware

Routes requests based on **real KV cache state** reported by the LMCache controller, rather than heuristic prefix trees. Requires an LMCache controller deployment.

```bash
vllm-router --config-file configs/lmcache-aware.yaml
```

```yaml
# configs/lmcache-aware.yaml
policy:
  type: lmcache_aware
  controller_url: "http://lmcache-controller:9000"
  poll_interval_secs: 10
  cache_weight: 0.7
  fallback_policy: "power_of_two"
  controller_timeout_ms: 2000
  lookup_mode: occupancy
  lmcache_worker_map:
    "vllm-001": "http://vllm-worker-001:8000"
    "vllm-002": "http://vllm-worker-002:8000"
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `controller_url` | `http://localhost:9000` | LMCache controller API server URL |
| `poll_interval_secs` | `10` | Polling interval for worker cache state |
| `cache_weight` | `0.7` | Cache vs load weight (0.0 = pure load, 1.0 = pure cache) |
| `fallback_policy` | `power_of_two` | Policy when controller is unreachable |
| `controller_timeout_ms` | `2000` | HTTP timeout for controller calls |
| `lookup_mode` | `occupancy` | `occupancy` or `prefix_lookup` |
| `lmcache_worker_map` | — | Maps LMCache `instance_id` to router worker URL |

**Scoring:** `score = cache_weight * normalized_key_count + (1 - cache_weight) * normalized_inverse_load`. The worker with the highest score is selected.

**Fallback:** When the controller is unreachable, the policy transparently degrades to the configured `fallback_policy`. No errors are returned to clients.

Best for multi-turn workloads with LMCache enabled, where real cache state data produces better routing decisions than heuristic prefix trees. See [LMCache Integration](lmcache-integration.md) for full setup instructions.

---

## Choosing a Policy

```
                                    ┌─────────────────────┐
                                    │  Need session       │
                                    │  affinity?          │
                                    └─────────┬───────────┘
                                              │
                              ┌───────────────┴───────────────┐
                              │                               │
                             Yes                              No
                              │                               │
                              ▼                               ▼
                    ┌─────────────────┐             ┌─────────────────┐
                    │  Prefix caching │             │  Load-sensitive │
                    │  important?     │             │  workload?      │
                    └────────┬────────┘             └────────┬────────┘
                             │                               │
                 ┌───────────┴───────────┐       ┌───────────┴───────────┐
                 │                       │       │                       │
                Yes                      No     Yes                      No
                 │                       │       │                       │
                 ▼                       ▼       ▼                       ▼
        ┌────────────────┐     ┌────────────────┐ ┌────────────────┐ ┌────────────────┐
        │  cache_aware   │     │ consistent_hash│ │  power_of_two  │ │  round_robin   │
        │                │     │                │ │                │ │  or random     │
        └────────────────┘     └────────────────┘ └────────────────┘ └────────────────┘
```

### Quick Reference

| Scenario | Recommended Policy |
|----------|-------------------|
| Chat applications with conversation history | `consistent_hash` |
| Batch inference with no state | `round_robin` |
| Variable request complexity | `power_of_two` |
| Repeated system prompts / few-shot | `cache_aware` |
| LMCache controller deployed, multi-turn | `lmcache_aware` |
| PD disaggregation with LMCache | prefill: `lmcache_aware`, decode: `consistent_hash` |
| Simple testing / development | `random` |
