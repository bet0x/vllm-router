#!/usr/bin/env bash
# Benchmark the router with genai-perf.
# Usage: ./scripts/test_router.sh
#
# Override with env vars:
#   MODEL=... API_KEY=... MODEL_ADDR=... ./scripts/test_router.sh

export MODEL="${MODEL:-RedHatAI/Llama-3.2-1B-Instruct-FP8}"
export API_KEY="${API_KEY:-my-secret-admin-key}"
export MODEL_ADDR="${MODEL_ADDR:-http://localhost:3000}"

for C in 1 4 8 16 32 64; do
  echo "=== Concurrency: $C ==="
  genai-perf profile \
    -m "$MODEL" \
    --endpoint-type chat \
    --streaming \
    -u $MODEL_ADDR \
    --synthetic-input-tokens-mean 128 \
    --output-tokens-mean 32 \
    --extra-inputs max_tokens:32 \
    --concurrency $C \
    --request-count 100 \
    --warmup-request-count 2 \
    -H "Authorization: Bearer ${API_KEY}" \
    --artifact-dir "results_c${C}"
done
