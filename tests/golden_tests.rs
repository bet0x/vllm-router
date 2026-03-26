//! Phase 0 golden tests — capture current behavior of public API routes.
//!
//! These tests use mock workers + the actual Axum app (via tower::ServiceExt::oneshot).
//! They validate response structure and key fields, not exact byte content,
//! so that UUIDs and timestamps don't cause false failures.
//!
//! Covered routes:
//!   - POST /v1/chat/completions  (non-stream + stream)
//!   - POST /v1/messages          (non-stream + stream)
//!   - POST /v1/responses         (create)
//!   - GET  /v1/responses/{id}    (retrieve)
//!   - POST /v1/responses/{id}/cancel

mod common;

use axum::{
    body::Body,
    extract::Request,
    http::{header::CONTENT_TYPE, StatusCode},
};
use common::mock_worker::{MockWorker, MockWorkerConfig};
use reqwest::Client;
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;
use vllm_router_rs::{
    config::{PolicyConfig, RouterConfig, RoutingMode},
    routers::{RouterFactory, RouterTrait},
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/golden/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&content).unwrap_or_else(|e| panic!("{path}: {e}"))
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("parse JSON body")
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("body is utf-8")
}

// ---------------------------------------------------------------------------
// Shared test context
// ---------------------------------------------------------------------------

struct GoldenCtx {
    workers: Vec<MockWorker>,
    router: Arc<dyn RouterTrait>,
    client: Client,
    config: RouterConfig,
}

impl GoldenCtx {
    /// Spin up one healthy mock worker and build the Axum app around it.
    async fn new() -> Self {
        Self::with_workers(vec![MockWorkerConfig::default()]).await
    }

    async fn with_workers(worker_configs: Vec<MockWorkerConfig>) -> Self {
        let mut config = RouterConfig {
            mode: RoutingMode::Regular {
                worker_urls: vec![],
            },
            policy: PolicyConfig::Random,
            worker_startup_timeout_secs: 1,
            worker_startup_check_interval_secs: 1,
            ..Default::default()
        };

        let mut workers = Vec::new();
        let mut worker_urls = Vec::new();
        for wc in worker_configs {
            let mut w = MockWorker::new(wc);
            let url = w.start().await.expect("start mock worker");
            worker_urls.push(url);
            workers.push(w);
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        config.mode = RoutingMode::Regular { worker_urls };

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();

        let app_context = common::create_test_context(config.clone());
        let router = Arc::from(
            RouterFactory::create_router(&app_context)
                .await
                .expect("create router"),
        );

        // Let the router discover workers.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Self {
            workers,
            router,
            client,
            config,
        }
    }

    fn app(&self) -> axum::Router {
        common::test_app::create_test_app(
            Arc::clone(&self.router),
            self.client.clone(),
            &self.config,
        )
    }

    async fn shutdown(mut self) {
        for w in &mut self.workers {
            w.stop().await;
        }
    }
}

// ===========================================================================
// POST /v1/chat/completions
// ===========================================================================

#[cfg(test)]
mod chat_completions {
    use super::*;

    #[tokio::test]
    async fn non_streaming_returns_valid_chat_completion() {
        let ctx = GoldenCtx::new().await;
        let fixture = load_fixture("chat_completion_request.json");

        let resp = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&fixture).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp).await;

        // -- id --
        let id = json["id"].as_str().expect("id should be a string");
        assert!(id.starts_with("chatcmpl-"), "id = {id}");

        // -- top-level fields --
        assert_eq!(json["object"].as_str().unwrap(), "chat.completion");
        assert!(json["created"].as_u64().is_some());
        assert!(json["model"].as_str().is_some());

        // -- choices --
        let choices = json["choices"].as_array().expect("choices array");
        assert!(!choices.is_empty());
        let c0 = &choices[0];
        assert_eq!(c0["index"].as_u64().unwrap(), 0);
        assert_eq!(c0["message"]["role"].as_str().unwrap(), "assistant");
        assert!(
            c0["message"]["content"].as_str().is_some(),
            "content present"
        );
        assert_eq!(c0["finish_reason"].as_str().unwrap(), "stop");

        // -- usage --
        let u = &json["usage"];
        assert!(u["prompt_tokens"].as_u64().is_some());
        assert!(u["completion_tokens"].as_u64().is_some());
        assert!(u["total_tokens"].as_u64().is_some());

        ctx.shutdown().await;
    }

    #[tokio::test]
    async fn streaming_returns_sse_chunks() {
        let ctx = GoldenCtx::new().await;
        let fixture = load_fixture("chat_completion_stream_request.json");

        let resp = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&fixture).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let ct = resp
            .headers()
            .get(CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "Content-Type = {ct}"
        );

        let body = body_string(resp).await;

        // Must contain at least one data line and [DONE].
        assert!(body.contains("data:"), "has data lines");
        assert!(body.contains("[DONE]"), "stream ends with [DONE]");

        // Parse first non-DONE data chunk.
        let chunk_str = body
            .lines()
            .filter_map(|l| l.strip_prefix("data:").or_else(|| l.strip_prefix("data: ")))
            .map(str::trim)
            .find(|l| !l.is_empty() && *l != "[DONE]")
            .expect("at least one data chunk");
        let chunk: Value = serde_json::from_str(chunk_str).expect("valid JSON chunk");

        assert!(chunk["id"].as_str().is_some());
        assert_eq!(
            chunk["object"].as_str().unwrap(),
            "chat.completion.chunk"
        );
        assert!(chunk["choices"].as_array().is_some());

        ctx.shutdown().await;
    }
}

// ===========================================================================
// POST /v1/messages  (Anthropic → OpenAI translation)
// ===========================================================================

#[cfg(test)]
mod messages {
    use super::*;

    #[tokio::test]
    async fn non_streaming_returns_anthropic_format() {
        let ctx = GoldenCtx::new().await;
        let fixture = load_fixture("anthropic_messages_request.json");

        let resp = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&fixture).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp).await;

        // -- id: prefixed with msg_ --
        let id = json["id"].as_str().expect("id string");
        assert!(id.starts_with("msg_"), "id = {id}");

        // -- envelope --
        assert_eq!(json["type"].as_str().unwrap(), "message");
        assert_eq!(json["role"].as_str().unwrap(), "assistant");

        // -- content blocks --
        let content = json["content"].as_array().expect("content array");
        assert!(!content.is_empty());
        assert_eq!(content[0]["type"].as_str().unwrap(), "text");
        assert!(content[0]["text"].as_str().is_some());

        // -- stop_reason: OpenAI "stop" maps to Anthropic "end_turn" --
        assert_eq!(json["stop_reason"].as_str().unwrap(), "end_turn");

        // -- usage (Anthropic shape) --
        assert!(json["usage"]["input_tokens"].as_u64().is_some());
        assert!(json["usage"]["output_tokens"].as_u64().is_some());

        // -- model: comes from the backend, not the original request --
        assert_eq!(json["model"].as_str().unwrap(), "mock-model");

        ctx.shutdown().await;
    }

    #[tokio::test]
    async fn streaming_returns_anthropic_sse_events() {
        let ctx = GoldenCtx::new().await;
        let fixture = load_fixture("anthropic_messages_stream_request.json");

        let resp = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&fixture).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let ct = resp
            .headers()
            .get(CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("text/event-stream"),
            "Content-Type = {ct}"
        );

        let body = body_string(resp).await;

        // The Anthropic SSE translator must emit these events (in order):
        //   message_start → content_block_start → content_block_delta(s)
        //   → content_block_stop → message_delta → message_stop
        let required = [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ];
        for event in required {
            assert!(
                body.contains(&format!("event: {event}")),
                "missing event: {event}\nbody:\n{body}"
            );
        }

        // The delta should carry the text.
        assert!(body.contains("text_delta"), "text_delta present");

        ctx.shutdown().await;
    }
}

// ===========================================================================
// POST /v1/responses  +  GET  +  cancel
// ===========================================================================

#[cfg(test)]
mod responses {
    use super::*;

    #[tokio::test]
    async fn create_returns_response_object() {
        let ctx = GoldenCtx::new().await;
        let fixture = load_fixture("responses_create_request.json");

        let resp = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&fixture).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let json = body_json(resp).await;

        // Proxied through route_transparent — response is the mock worker's JSON.
        let id = json["id"].as_str().expect("id string");
        assert!(id.starts_with("resp-"), "id = {id}");
        assert_eq!(json["object"].as_str().unwrap(), "response");
        assert!(json["created_at"].as_i64().is_some());
        assert!(json["model"].as_str().is_some());
        assert_eq!(json["status"].as_str().unwrap(), "completed");

        let output = json["output"].as_array().expect("output array");
        assert!(!output.is_empty());

        let usage = &json["usage"];
        assert!(usage["input_tokens"].as_u64().is_some());
        assert!(usage["output_tokens"].as_u64().is_some());

        ctx.shutdown().await;
    }

    #[tokio::test]
    async fn get_returns_stored_response() {
        let ctx = GoldenCtx::new().await;

        // Create a background response so the mock worker stores the ID.
        let create_body = serde_json::json!({
            "model": "mock-model",
            "input": "Tell me a joke",
            "background": true,
            "request_id": "resp-golden-get-1"
        });

        let create_resp = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&create_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_resp.status(), StatusCode::OK);

        let cj = body_json(create_resp).await;
        assert_eq!(cj["status"].as_str().unwrap(), "queued");

        // Retrieve it.
        let get_resp = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/responses/resp-golden-get-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(get_resp.status(), StatusCode::OK);

        let json = body_json(get_resp).await;
        assert_eq!(json["id"].as_str().unwrap(), "resp-golden-get-1");
        assert_eq!(json["object"].as_str().unwrap(), "response");
        assert_eq!(json["status"].as_str().unwrap(), "completed");

        ctx.shutdown().await;
    }

    #[tokio::test]
    async fn cancel_returns_cancelled_status() {
        let ctx = GoldenCtx::new().await;

        // Create a background response first.
        let create_body = serde_json::json!({
            "model": "mock-model",
            "input": "Tell me a joke",
            "background": true,
            "request_id": "resp-golden-cancel-1"
        });

        let _ = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_string(&create_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Cancel it.
        let cancel_resp = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses/resp-golden-cancel-1/cancel")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(cancel_resp.status(), StatusCode::OK);

        let json = body_json(cancel_resp).await;
        assert_eq!(json["id"].as_str().unwrap(), "resp-golden-cancel-1");
        assert_eq!(json["status"].as_str().unwrap(), "cancelled");

        ctx.shutdown().await;
    }

    #[tokio::test]
    async fn get_nonexistent_returns_not_found() {
        let ctx = GoldenCtx::new().await;

        let resp = ctx
            .app()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/responses/resp-does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Mock worker returns 404 for unknown IDs; router forwards it.
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        ctx.shutdown().await;
    }
}
