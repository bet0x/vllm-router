# Kubernetes Service Discovery

The router can automatically discover vLLM worker pods in a Kubernetes cluster using label selectors.

## Configuration

```yaml
service_discovery: true
selector:
  - "app=vllm-worker"
  - "role=inference"
service_discovery_namespace: "default"
```

Or via CLI flags:

```bash
vllm-router --config-file your-config.yaml \
    --service-discovery \
    --selector app=vllm-worker role=inference \
    --service-discovery-namespace default
```

## How it works

1. The router watches the Kubernetes API for pods matching the given label selectors in the specified namespace.
2. When pods are added or removed, the router updates its worker pool automatically.
3. Health checks continue to run against discovered workers, so unhealthy pods are skipped during routing.

For a full Kubernetes deployment example with PD disaggregation, see `scripts/k8s/llama3/vllm-router/pd-disagg/`.
