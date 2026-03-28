# Unix Domain Socket Support

The router can connect to local vLLM workers over Unix domain sockets (UDS) instead of TCP. This is useful for same-host deployments where you want to avoid local TCP overhead and port exposure.

## Quick start

```bash
# Start vLLM on a Unix socket
vllm serve meta-llama/Llama-3.1-8B-Instruct --uds /tmp/vllm.sock

# Start the router pointing at it
vllm-router --config-file configs/unix-socket.yaml
```

## Configuration

Worker URLs use the `unix:///absolute/path.sock` format in `worker_urls`:

```yaml
mode:
  type: regular
  worker_urls:
    - "unix:///tmp/vllm.sock"

policy:
  type: round_robin
```

You can mix TCP and UDS workers in the same pool:

```yaml
mode:
  type: regular
  worker_urls:
    - "unix:///tmp/vllm-local.sock"
    - "http://gpu-node-2:8000"
```

### Per-worker API keys

UDS workers use the same `unix://` URL as the `worker_api_keys` key:

```yaml
worker_api_keys:
  "unix:///tmp/vllm.sock": "sk-local-vllm"
  "http://gpu-node-2:8000": "sk-remote"
```

### CLI

```bash
vllm-router --worker-url "unix:///tmp/vllm.sock" --policy round_robin
```

## How it works

Under the hood, the router keeps a pool of `reqwest::Client` instances keyed by transport type:

- **TCP workers** use the shared, connection-pool-tuned client from `AppContext`.
- **UDS workers** each get a dedicated client created with `reqwest::ClientBuilder::unix_socket()`.

HTTP semantics are unchanged. The request URL uses `http://localhost` as the authority (Host header), but the actual bytes flow through the Unix socket. This is transparent to vLLM.

## What works

All regular HTTP routing features work over UDS:

| Feature | UDS support |
|---------|-------------|
| Health checks (background + startup) | Yes |
| Model discovery (`/get_server_info`, `/v1/models`) | Yes |
| Chat completions (`/v1/chat/completions`) | Yes |
| Completions (`/v1/completions`) | Yes |
| Embeddings (`/v1/embeddings`) | Yes |
| Rerank (`/v1/rerank`) | Yes |
| Responses API (`/v1/responses`) | Yes |
| Streaming (SSE) | Yes |
| Response cache (exact + semantic) | Yes |
| All load balancing policies | Yes |
| Session affinity (`consistent_hash`) | Yes |
| Anthropic Messages API translation | Yes |
| OpenAI backend routing | Yes |
| Per-worker API keys | Yes |
| Admin APIs (`add_worker`, `drain`, `reload`) | Yes |
| Worker metrics proxy | Yes |
| Routing explainability headers | Yes |

## What does NOT work

| Feature | Reason |
|---------|--------|
| PD disaggregation (`pd_disaggregation`) | Protocol-level incompatibility (see below) |
| vLLM PD (`vllm_pd`) | Same as above |
| gRPC over UDS | gRPC uses `tonic::transport::Channel`, not `reqwest` |
| Service discovery (Kubernetes) | Pod IPs are TCP; UDS requires same-host access |

The router **rejects** `unix://` URLs in PD modes at config validation time with a clear error message.

### Why PD mode cannot use Unix sockets

PD disaggregation requires a **P2P TCP channel** between prefill and decode workers for KV cache transfer (via ZMQ or NIXL). The router embeds the prefill worker's TCP `host:port` into:

1. **`bootstrap_host` / `bootstrap_port`** in the JSON request body — the decode worker uses this to open the KV cache transfer channel back to the prefill worker.
2. **`X-Request-Id`** header — vLLM parses `___prefill_addr_<host:port>___decode_addr_<host:port>_<uuid>` to coordinate the transfer.

A Unix socket path (`/tmp/vllm.sock`) is local to a single machine and cannot serve as a P2P rendezvous address between two separate worker processes. Even if both workers ran on the same host, vLLM's disaggregation protocol expects a TCP address in these fields.

This is not a router limitation — it is a vLLM protocol constraint.

## Platform support

Unix sockets are only available on Unix platforms (Linux, macOS). On non-Unix platforms, the router rejects `unix://` worker URLs at config validation time.

## Validation rules

- Path must be absolute: `unix:///tmp/vllm.sock` (three slashes)
- No host component: `unix://localhost/...` is rejected
- No query or fragment: `unix:///tmp/vllm.sock?foo=bar` is rejected
- Socket file does not need to exist at config parse time (vLLM may start later)

## Deployment patterns

### Single-host: router + vLLM as sidecar

```
┌──────────────────────────────┐
│ Host / Pod                   │
│                              │
│  Router ─── UDS ──── vLLM   │
│  :3000        /tmp/vllm.sock │
└──────────────────────────────┘
```

No TCP ports exposed for the worker. The router still listens on TCP for client traffic.

### Hybrid: local UDS + remote TCP workers

```
┌──────────────────────────────┐
│ Host A                       │
│  Router ─── UDS ──── vLLM-1 │
│  :3000                       │
│         ─── TCP ──── vLLM-2  │──── Host B (http://gpu-node:8000)
└──────────────────────────────┘
```

Round-robin (or any policy) distributes across both transports. The policy treats them identically.

## Troubleshooting

**Worker stays unhealthy:**
- Check that the socket file exists: `ls -la /tmp/vllm.sock`
- Check permissions: the router process needs read/write access to the socket file
- Check vLLM is actually listening: `curl --unix-socket /tmp/vllm.sock http://localhost/health`

**Stale socket file:**
- If vLLM crashed without cleanup, the socket file may still exist. Remove it before restarting vLLM: `rm /tmp/vllm.sock`

**Connection refused:**
- vLLM may not have finished starting. The router retries health checks on a configurable interval (`health_check.check_interval_secs`). The worker will be marked healthy once it responds.
