#!/usr/bin/env bash
# Levanta instancias de vLLM para testing del router
#
# Workers de chat (LLM):
#   Worker 1: puerto 8010 — RedHatAI/Llama-3.2-1B-Instruct-FP8
#   Worker 2: puerto 8020 — RedHatAI/Llama-3.2-1B-Instruct-FP8
#
# Worker de embeddings (opcional, requerido para semantic-cluster/semantic-cache):
#   Worker 3: puerto 8030 — BAAI/bge-small-en-v1.5  (pasa --embeddings)
#
# Uso:
#   ./scripts/start_test_workers.sh           # solo workers de chat
#   ./scripts/start_test_workers.sh --all     # chat + embeddings

set -e

VENV="/home/alberto/vllm-dist"
CHAT_MODEL="RedHatAI/Llama-3.2-1B-Instruct-FP8"
EMBED_MODEL="BAAI/bge-small-en-v1.5"
GPU_MEMORY_UTIL=0.3
MAX_MODEL_LEN=32768
LOG_DIR="/tmp/vllm-test-workers"

START_EMBED=false
[[ "${1:-}" == "--all" ]] && START_EMBED=true

source "$VENV/.venv/bin/activate"

mkdir -p "$LOG_DIR"

# Matar instancias previas si las hay
echo "Limpiando instancias previas en puertos 8010, 8020 y 8030..."
fuser -k 8010/tcp 2>/dev/null || true
fuser -k 8020/tcp 2>/dev/null || true
fuser -k 8030/tcp 2>/dev/null || true
sleep 1

echo "Iniciando Worker 1 (chat) en puerto 8010..."
vllm serve "$CHAT_MODEL" \
    --port 8010 \
    --host 0.0.0.0 \
    --gpu-memory-utilization $GPU_MEMORY_UTIL \
    --max-model-len $MAX_MODEL_LEN \
    --disable-log-requests \
    > "$LOG_DIR/worker1.log" 2>&1 &
WORKER1_PID=$!
echo "Worker 1 PID: $WORKER1_PID"

echo "Iniciando Worker 2 (chat) en puerto 8020..."
vllm serve "$CHAT_MODEL" \
    --port 8020 \
    --host 0.0.0.0 \
    --gpu-memory-utilization $GPU_MEMORY_UTIL \
    --max-model-len $MAX_MODEL_LEN \
    --disable-log-requests \
    > "$LOG_DIR/worker2.log" 2>&1 &
WORKER2_PID=$!
echo "Worker 2 PID: $WORKER2_PID"

if $START_EMBED; then
    echo "Iniciando Worker 3 (embeddings) en puerto 8030..."
    vllm serve "$EMBED_MODEL" \
        --port 8030 \
        --host 0.0.0.0 \
        --gpu-memory-utilization 0.1 \
        --disable-log-requests \
        > "$LOG_DIR/worker3-embed.log" 2>&1 &
    WORKER3_PID=$!
    echo "Worker 3 (embeddings) PID: $WORKER3_PID"
fi

echo ""
echo "Workers iniciados. Esperando que estén listos..."
echo "Logs en: $LOG_DIR/"

# Esperar hasta que ambos respondan en /health
wait_for_health() {
    local port=$1
    local name=$2
    local max_wait=120
    local elapsed=0

    while true; do
        if curl -sf "http://localhost:$port/health" > /dev/null 2>&1; then
            echo "$name (puerto $port) listo ✓"
            return 0
        fi
        if [ $elapsed -ge $max_wait ]; then
            echo "ERROR: $name no respondió en ${max_wait}s. Ver $LOG_DIR/$(echo $name | tr '[:upper:]' '[:lower:]' | tr ' ' '_').log"
            return 1
        fi
        sleep 2
        elapsed=$((elapsed + 2))
        printf "."
    done
}

wait_for_health 8010 "Worker 1" &
wait_for_health 8020 "Worker 2" &
if $START_EMBED; then
    wait_for_health 8030 "Worker 3 (embeddings)" &
fi
wait

echo ""
echo "Workers listos."
echo ""
echo "--- Para arrancar el router (round-robin simple):"
echo "  cargo run -- --worker-urls http://localhost:8010 http://localhost:8020 --policy round_robin"
echo ""
if $START_EMBED; then
    echo "--- Para arrancar con Semantic Cluster Routing:"
    echo "  cargo run -- --config-file configs/test-semantic-cluster.yaml"
    echo ""
fi
echo "Para parar los workers:"
if $START_EMBED; then
    echo "  kill $WORKER1_PID $WORKER2_PID $WORKER3_PID"
    echo "  # o: fuser -k 8010/tcp 8020/tcp 8030/tcp"
else
    echo "  kill $WORKER1_PID $WORKER2_PID"
    echo "  # o: fuser -k 8010/tcp 8020/tcp"
fi
