# Anthropic Messages API

The router natively accepts the Anthropic Messages API format and translates it internally to OpenAI Chat Completions.

## Usage

```bash
curl http://localhost:3000/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model": "my-model",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

## Streaming

Streaming is fully supported. The response follows the Anthropic SSE event format (`message_start`, `content_block_delta`, `message_stop`).

```bash
curl http://localhost:3000/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model": "my-model",
    "max_tokens": 1024,
    "stream": true,
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

## How it works

1. The router receives a request at `POST /v1/messages` in the Anthropic format.
2. It translates the request to the OpenAI Chat Completions format (`POST /v1/chat/completions`).
3. The request is forwarded to a backend vLLM worker.
4. The response is translated back to the Anthropic format before returning to the client.

This allows clients using the Anthropic SDK to talk to vLLM workers without any changes.
