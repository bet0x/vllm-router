# LMCache Integration

The `lmcache_aware` routing policy queries the [LMCache](https://github.com/LMCache/LMCache) controller for **real KV cache state** instead of maintaining an approximate radix tree. This replaces heuristic-based cache-aware routing with data-driven routing based on actual cache occupancy reported by LMCache.

## When to use this

- Two or more vLLM instances running the same model behind the router
- LMCache enabled as KVConnector (`kv_role: kv_both`) on each worker
- An LMCache controller deployed separately, tracking which workers hold which KV cache chunks
- Multi-turn conversations where routing follow-up turns to the worker that already holds the KV cache saves significant recomputation

The existing `cache_aware` policy maintains an approximate prefix tree based on request history — it guesses which worker has the cache. The `lmcache_aware` policy asks the controller what the actual state is.

## Architecture

```
                    ┌─────────────────────┐
                    │   vllm-router       │
                    │   (Rust, port 8080) │
                    └─────────┬───────────┘
                              │ HTTP polling
                              │ GET /controller/workers
                              ▼
                    ┌─────────────────────┐
                    │  LMCache Controller │
                    │  (FastAPI, port 9000)│
                    └─────────┬───────────┘
                              │ ZMQ (ports 8300/8400)
                    ┌─────────┴───────────┐
                    │                     │
              ┌─────┴──────┐       ┌──────┴─────┐
              │ vLLM       │       │ vLLM       │
              │ Worker 001 │       │ Worker 002 │
              │ + LMCache  │       │ + LMCache  │
              └────────────┘       └────────────┘
```

### Without the controller (current behavior)

```
Turn 1 → Router (round robin) → Worker 001 → computes KV cache, responds
Turn 2 → Router (round robin) → Worker 002 → recomputes EVERYTHING from scratch
```

### With the controller + lmcache_aware policy

```
Turn 1 → Router → Worker 001 → computes KV, LMCache reports chunks to controller
Turn 2 → Router polls controller → "001 has 100 cached chunks, 002 has 10"
         Router → Worker 001 → cache hit, only computes new tokens
```

## Prerequisites

### 1. LMCache controller pod

The controller is a separate pod (no GPU needed) that runs alongside the vLLM workers:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: lmcache-controller
spec:
  replicas: 1
  selector:
    matchLabels:
      app.kubernetes.io/name: lmcache-controller
  template:
    metadata:
      labels:
        app.kubernetes.io/name: lmcache-controller
    spec:
      containers:
        - name: controller
          image: barrahome/lmcache-controller:v0.3.15
          ports:
            - containerPort: 9000  # HTTP API
            - containerPort: 8300  # ZMQ PULL
            - containerPort: 8400  # ZMQ REPLY
          resources:
            requests:
              cpu: "500m"
              memory: "512Mi"
            limits:
              cpu: "2"
              memory: "2Gi"
```

Expose it as a Service:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: lmcache-controller
spec:
  selector:
    app.kubernetes.io/name: lmcache-controller
  ports:
    - name: api
      port: 9000
    - name: zmq-pull
      port: 8300
    - name: zmq-reply
      port: 8400
```

### 2. Worker LMCache configuration

Each vLLM worker needs `enable_controller: true` in its LMCache config so it reports cache state to the controller via ZMQ.

**Worker 001 — `lmcache-config.yaml`:**

```yaml
chunk_size: 2048
local_cpu: true
max_local_cpu_size: 5

enable_controller: true
lmcache_instance_id: "vllm-001"
controller_pull_url: "lmcache-controller:8300"
controller_reply_url: "lmcache-controller:8400"
lmcache_worker_ports: 8500

# Optional: P2P cache sharing between workers
enable_p2p: true
p2p_host: "0.0.0.0"
p2p_init_ports: 8200
p2p_lookup_ports: 8201
transfer_channel: "nixl"
```

Set the env var on each worker pod:

```yaml
env:
  - name: LMCACHE_CONFIG_FILE
    value: /etc/lmcache/config.yaml
```

### 3. Verify registration

```bash
# Check workers registered with the controller
curl http://lmcache-controller:9000/controller/workers
# Should return: {"workers": [...], "total_count": 2}

# Check cache stats
curl http://lmcache-controller:9000/controller/key-stats
```

## Router configuration

### YAML config (recommended)

```yaml
policy:
  type: lmcache_aware

  # URL of the LMCache controller API server
  controller_url: "http://lmcache-controller:9000"

  # How often to poll the controller (seconds)
  poll_interval_secs: 10

  # Balance between cache affinity and load distribution
  # 0.0 = pure load balancing, 1.0 = pure cache affinity
  cache_weight: 0.7

  # Policy when controller is unreachable
  fallback_policy: "power_of_two"

  # HTTP timeout for controller calls (milliseconds)
  controller_timeout_ms: 2000

  # Lookup mode (see Phases below)
  lookup_mode: occupancy

  # Mapping: LMCache instance_id → router worker URL
  lmcache_worker_map:
    "vllm-001": "http://vllm-worker-001:8000"
    "vllm-002": "http://vllm-worker-002:8000"
```

See `configs/lmcache-aware.yaml` and `configs/pd-lmcache-aware.yaml` for complete examples.

### CLI flags

```bash
vllm-router \
  --policy lmcache_aware \
  --lmcache-controller-url http://lmcache-controller:9000 \
  --lmcache-poll-interval 10 \
  --lmcache-cache-weight 0.7 \
  --lmcache-lookup-mode occupancy \
  --lmcache-controller-timeout-ms 2000 \
  --worker-urls http://vllm-worker-001:8000 http://vllm-worker-002:8000
```

Note: the `lmcache_worker_map` (instance_id to worker URL mapping) can only be set via YAML config, not CLI flags.

## Configuration reference

| Parameter | Default | Description |
|-----------|---------|-------------|
| `controller_url` | `http://localhost:9000` | LMCache controller API server URL |
| `poll_interval_secs` | `10` | How often to poll the controller for worker cache state |
| `cache_weight` | `0.7` | Weight for cache vs load (0.0 = pure load, 1.0 = pure cache) |
| `fallback_policy` | `power_of_two` | Policy to use when controller is unreachable |
| `controller_timeout_ms` | `2000` | HTTP timeout for controller API calls |
| `lookup_mode` | `occupancy` | `occupancy` (Phase 1) or `prefix_lookup` (Phase 2) |
| `controller_api_key` | `null` | Optional Bearer token for controller authentication |
| `lmcache_worker_map` | `null` | Explicit instance_id → worker URL mapping |

## How routing works

### Scoring formula

For each healthy worker, the policy computes:

```
score = cache_weight * normalized_key_count + (1 - cache_weight) * normalized_inverse_load
```

Where:
- `normalized_key_count` = worker's `key_count` / max `key_count` across all workers
- `normalized_inverse_load` = (max_load - worker_load) / max_load
- The worker with the highest score is selected

### Fallback behavior

When the LMCache controller is unreachable (timeout, error, not deployed yet):
- The policy delegates to the configured `fallback_policy` (default: `power_of_two`)
- No error is returned to the client — routing continues transparently
- When the controller becomes available again, the policy automatically starts using cache state

## Worker mapping

The router needs to know which LMCache `instance_id` corresponds to which router worker URL. There are two approaches:

### Explicit mapping (recommended for Kubernetes)

```yaml
lmcache_worker_map:
  "vllm-001": "http://vllm-worker-001:8000"
  "vllm-002": "http://vllm-worker-002:8000"
```

The `instance_id` values must match the `lmcache_instance_id` configured on each worker. This is the most reliable approach because in Kubernetes the router sees service DNS names while LMCache sees pod IPs.

### Without explicit mapping

If `lmcache_worker_map` is not configured, workers without a mapping will have zero cache score. The policy will still work but will effectively fall back to load-based routing for unmapped workers.

## Phases

### Phase 1: Occupancy routing (implemented)

`lookup_mode: occupancy`

Polls `GET /controller/workers` every `poll_interval_secs` seconds. Each worker's `key_count` (number of cached KV chunks) is used to compute the routing score. Workers with more cached data are preferred.

**Limitation:** No prefix-level matching. Routing is based on total cache occupancy, not whether a specific prefix is cached on a worker. Still better than heuristic because `key_count` reflects real state including evictions.

### Phase 2: Prefix lookup (planned)

`lookup_mode: prefix_lookup`

Per-request `POST /lookup` to the controller with tokenized prompt. The controller returns which worker has the longest cached prefix for that specific request. This endpoint already exists in LMCache (`lmcache.v1.api_server`):

```
POST /lookup
{"tokens": [1, 2, 3, 4, 5, ...]}

Response:
{"event_id": "...", "layout_info": {"vllm-001": ["LocalCPUBackend", 768]}}
```

The `layout_info` maps `instance_id → (location, matched_token_count)`. This gives exact prefix-match routing.

**Requirements for Phase 2:**
- Router tokenizer must produce the same token IDs as vLLM/LMCache (same HuggingFace model)
- Configure `tokenizer_model_map` to point to the exact same model
- Adds per-request latency for the lookup call (mitigated by `controller_timeout_ms`)

## PD disaggregation

With LMCache + controller, prefill workers are no longer stateless. The `lmcache_aware` policy is especially valuable as a prefill policy in PD setups:

```yaml
prefill_policy:
  type: lmcache_aware
  controller_url: "http://lmcache-controller:9000"
  lookup_mode: prefix_lookup
  cache_weight: 0.8
  lmcache_worker_map:
    "prefill-1": "http://prefill-1:8081"
    "prefill-2": "http://prefill-2:8081"

decode_policy:
  type: consistent_hash
  virtual_nodes: 160
```

See `configs/pd-lmcache-aware.yaml` for a complete example.

## Troubleshooting

### Controller unreachable

The router logs `WARN` messages when the controller is unreachable and falls back to the configured fallback policy. Check:

```bash
# Is the controller pod running?
kubectl get pods -l app.kubernetes.io/name=lmcache-controller

# Can the router reach the controller?
kubectl exec deploy/vllm-router -- curl -s http://lmcache-controller:9000/controller/workers
```

### Workers not showing cache data

If `key_count` is always 0, workers may not be reporting to the controller:

```bash
# Check worker LMCache config
kubectl exec deploy/vllm-worker-001 -- cat $LMCACHE_CONFIG_FILE
# Verify: enable_controller: true, controller_pull_url, controller_reply_url

# Check controller sees workers
curl http://lmcache-controller:9000/controller/workers
```

### Worker map mismatch

If the `lmcache_instance_id` on a worker doesn't match any key in `lmcache_worker_map`, that worker will have zero cache score. Check the router logs for debug messages showing instance IDs and mapped URLs.
