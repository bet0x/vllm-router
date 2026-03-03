# Prefill-Decode Disaggregation

Splits inference into two phases across separate worker pools:

- **Prefill workers** — compute the KV cache from the input prompt (compute-bound)
- **Decode workers** — generate tokens using the transferred KV cache (memory-bandwidth-bound)

vLLM handles the KV cache transfer between pools via the NIXL connector (UCX/GDS). The router embeds both worker addresses in the vLLM request ID so vLLM knows where to send the cache.

```bash
vllm-router --config-file configs/pd-disagg.yaml
```

## Multi-turn chat with PD disaggregation

The KV cache accumulates on the **decode worker**, not the prefill worker. For session affinity across turns, the decode pool must use `consistent_hash`. The prefill pool is stateless between turns and can use any load-balancing policy.

```
Turn 1:  any prefill  +  Decode D2  →  KV cache built on D2
Turn 2:  any prefill  +  Decode D2  →  KV cache reused        ✓
Turn 2:  any prefill  +  Decode D1  →  full re-prefill needed  ✗
```

## Recommended configuration

From `configs/pd-disagg.yaml`:

```yaml
prefill_policy:
  type: power_of_two       # stateless — balance load freely

decode_policy:
  type: consistent_hash    # stateful — pin session to same decode worker
  virtual_nodes: 160
```

For a full Kubernetes example see `scripts/k8s/llama3/vllm-router/pd-disagg/`.
