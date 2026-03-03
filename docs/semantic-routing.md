# Semantic Cluster Routing

Routes requests to the worker cluster whose centroid is most similar to the request embedding. Cluster centroids are computed at startup from example prompts.

## Configuration

```yaml
# configs/test-semantic-cluster.yaml (excerpt)
policy:
  type: consistent_hash

semantic_cluster:
  embeddings_url: "http://localhost:8030"
  embeddings_model: "BAAI/bge-small-en-v1.5"
  threshold: 0.70
  clusters:
    - name: coding
      examples:
        - "Write a Python function to sort a list"
        - "Debug this Rust borrow checker error"
      workers:
        - "http://localhost:8010"
    - name: general
      examples:
        - "What is the capital of France?"
        - "Explain quantum entanglement"
      workers:
        - "http://localhost:8020"
```

## How it works

1. Each cluster defines example prompts and a set of workers.
2. At startup, the router computes embeddings for all example prompts and averages them into a cluster centroid.
3. For each incoming request, the router computes the prompt embedding and finds the cluster whose centroid has the highest cosine similarity.
4. If the similarity exceeds `threshold`, the request is routed to a worker in that cluster (using the configured load-balancing policy).
5. If no cluster exceeds the threshold, the request falls back to the default policy across all workers.

## Headers

When a request matches a semantic cluster, the router sets `x-semantic-cluster-id` and `x-semantic-*` headers on the forwarded request. These can be used by vLLM or downstream services for additional routing logic.
