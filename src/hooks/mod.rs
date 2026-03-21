//! Pre-routing hook system.
//!
//! Hooks are HTTP callouts to external services that run before routing.
//! They can allow, reject, or transform request bodies. Configured via
//! `pre_routing_hooks` in the YAML config.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, field::Empty, warn};

/// Configuration for a single pre-routing hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreRoutingHook {
    /// Human-readable name for logging and the `x-vllm-router-hooks` header.
    pub name: String,
    /// URL to POST the request body to.
    pub url: String,
    /// Timeout in milliseconds for the HTTP call.
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    /// What to do on error or timeout: `"pass"` (skip) or `"block"` (reject request).
    #[serde(default = "default_on_error")]
    pub on_error: HookErrorPolicy,
    /// What to do when the hook rejects: `"block_403"`, `"block_400"`, or `"pass"`.
    #[serde(default = "default_on_reject")]
    pub on_reject: HookRejectPolicy,
    /// When true, replace the request body with the hook's response body.
    #[serde(default)]
    pub transform: bool,
}

/// Policy when a hook errors or times out.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum HookErrorPolicy {
    /// Skip the hook and continue routing (default).
    #[default]
    Pass,
    /// Block the request with 502 Bad Gateway.
    Block,
}

/// Policy when a hook rejects the request.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum HookRejectPolicy {
    /// Return 403 Forbidden (default).
    #[default]
    Block403,
    /// Return 400 Bad Request.
    Block400,
    /// Ignore the rejection and continue routing.
    Pass,
}

fn default_hook_timeout_ms() -> u64 {
    200
}
fn default_on_error() -> HookErrorPolicy {
    HookErrorPolicy::Pass
}
fn default_on_reject() -> HookRejectPolicy {
    HookRejectPolicy::Block403
}

/// Response from a hook service.
#[derive(Debug, Deserialize)]
struct HookResponse {
    action: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    body: Option<serde_json::Value>,
}

/// Result of running all hooks on a request body.
pub enum HookOutcome {
    /// All hooks passed. Contains the (possibly transformed) body and list of hook names that ran.
    Allow {
        body: serde_json::Value,
        hooks_ran: Vec<String>,
    },
    /// A hook rejected the request.
    Reject(Response),
}

/// Execute all pre-routing hooks in order against the request body.
///
/// Returns `HookOutcome::Allow` with the (possibly transformed) body if all
/// hooks pass, or `HookOutcome::Reject` with an error response if any hook
/// rejects.
pub async fn execute(
    hooks: &[PreRoutingHook],
    client: &Client,
    mut body: serde_json::Value,
) -> HookOutcome {
    if hooks.is_empty() {
        return HookOutcome::Allow {
            body,
            hooks_ran: Vec::new(),
        };
    }

    let mut hooks_ran = Vec::new();

    for hook in hooks {
        let timeout = Duration::from_millis(hook.timeout_ms);

        let _hook_span = if crate::otel_trace::is_otel_enabled() {
            Some(tracing::info_span!(
                target: "otel_trace",
                "hook.execute",
                otel.name = %format_args!("hook {}", hook.name),
                hook.name = %hook.name,
                hook.url = %hook.url,
                hook.timeout_ms = hook.timeout_ms,
                hook.outcome = Empty,
            ))
        } else {
            None
        };

        let result = tokio::time::timeout(timeout, async {
            client
                .post(&hook.url)
                .json(&body)
                .send()
                .await
        })
        .await;

        let response = match result {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                warn!(hook = hook.name.as_str(), error = %e, "hook request failed");
                if let Some(ref s) = _hook_span { s.record("hook.outcome", "error"); }
                if hook.on_error == HookErrorPolicy::Block {
                    return HookOutcome::Reject(
                        (StatusCode::BAD_GATEWAY, format!("Hook '{}' failed: {}", hook.name, e))
                            .into_response(),
                    );
                }
                hooks_ran.push(format!("{}:error", hook.name));
                continue;
            }
            Err(_) => {
                warn!(hook = hook.name.as_str(), timeout_ms = hook.timeout_ms, "hook timed out");
                if let Some(ref s) = _hook_span { s.record("hook.outcome", "timeout"); }
                if hook.on_error == HookErrorPolicy::Block {
                    return HookOutcome::Reject(
                        (StatusCode::BAD_GATEWAY, format!("Hook '{}' timed out", hook.name))
                            .into_response(),
                    );
                }
                hooks_ran.push(format!("{}:timeout", hook.name));
                continue;
            }
        };

        if !response.status().is_success() {
            warn!(hook = hook.name.as_str(), status = response.status().as_u16(), "hook returned non-200");
            if let Some(ref s) = _hook_span { s.record("hook.outcome", "error"); }
            if hook.on_error == HookErrorPolicy::Block {
                return HookOutcome::Reject(
                    (StatusCode::BAD_GATEWAY, format!("Hook '{}' returned {}", hook.name, response.status()))
                        .into_response(),
                );
            }
            hooks_ran.push(format!("{}:error", hook.name));
            continue;
        }

        // Parse hook response
        let hook_resp: HookResponse = match response.json().await {
            Ok(r) => r,
            Err(e) => {
                warn!(hook = hook.name.as_str(), error = %e, "hook response parse failed");
                if let Some(ref s) = _hook_span { s.record("hook.outcome", "parse_error"); }
                if hook.on_error == HookErrorPolicy::Block {
                    return HookOutcome::Reject(
                        (StatusCode::BAD_GATEWAY, format!("Hook '{}' returned invalid JSON", hook.name))
                            .into_response(),
                    );
                }
                hooks_ran.push(format!("{}:error", hook.name));
                continue;
            }
        };

        match hook_resp.action.as_str() {
            "allow" => {
                debug!(hook = hook.name.as_str(), "hook allowed");
                if let Some(ref s) = _hook_span { s.record("hook.outcome", "allow"); }
                hooks_ran.push(hook.name.clone());
            }
            "reject" => {
                let reason = hook_resp.reason.unwrap_or_else(|| "rejected by hook".to_string());
                debug!(hook = hook.name.as_str(), reason = reason.as_str(), "hook rejected");
                if let Some(ref s) = _hook_span { s.record("hook.outcome", "reject"); }
                match hook.on_reject {
                    HookRejectPolicy::Block403 => {
                        return HookOutcome::Reject(
                            (StatusCode::FORBIDDEN, reason).into_response(),
                        );
                    }
                    HookRejectPolicy::Block400 => {
                        return HookOutcome::Reject(
                            (StatusCode::BAD_REQUEST, reason).into_response(),
                        );
                    }
                    HookRejectPolicy::Pass => {
                        hooks_ran.push(format!("{}:rejected-ignored", hook.name));
                    }
                }
            }
            "transform" => {
                if hook.transform {
                    if let Some(new_body) = hook_resp.body {
                        debug!(hook = hook.name.as_str(), "hook transformed body");
                        if let Some(ref s) = _hook_span { s.record("hook.outcome", "transform"); }
                        body = new_body;
                        hooks_ran.push(format!("{}:transformed", hook.name));
                    } else {
                        warn!(hook = hook.name.as_str(), "hook returned transform without body");
                        if let Some(ref s) = _hook_span { s.record("hook.outcome", "transform-no-body"); }
                        hooks_ran.push(format!("{}:transform-no-body", hook.name));
                    }
                } else {
                    debug!(hook = hook.name.as_str(), "hook returned transform but transform=false in config, treating as allow");
                    if let Some(ref s) = _hook_span { s.record("hook.outcome", "allow"); }
                    hooks_ran.push(hook.name.clone());
                }
            }
            other => {
                warn!(hook = hook.name.as_str(), action = other, "hook returned unknown action");
                if let Some(ref s) = _hook_span { s.record("hook.outcome", "unknown"); }
                hooks_ran.push(format!("{}:unknown", hook.name));
            }
        }
    }

    HookOutcome::Allow {
        body,
        hooks_ran,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use serde_json::json;

    /// Start a tiny Axum server that returns a fixed JSON response.
    async fn start_hook_server(
        response_json: serde_json::Value,
        status: StatusCode,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move || {
                let body = response_json.clone();
                let st = status;
                async move { (st, axum::Json(body)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://127.0.0.1:{}/hook", addr.port()), handle)
    }

    /// Start a server that sleeps longer than the hook timeout.
    async fn start_slow_hook_server(delay_ms: u64) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move || async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                axum::Json(json!({"action": "allow"}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://127.0.0.1:{}/hook", addr.port()), handle)
    }

    /// Start a server that returns invalid (non-JSON) body.
    async fn start_invalid_json_server() -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(|| async { "not json" }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://127.0.0.1:{}/hook", addr.port()), handle)
    }

    fn make_hook(url: &str) -> PreRoutingHook {
        PreRoutingHook {
            name: "test-hook".to_string(),
            url: url.to_string(),
            timeout_ms: 2000,
            on_error: HookErrorPolicy::Pass,
            on_reject: HookRejectPolicy::Block403,
            transform: false,
        }
    }

    fn client() -> Client {
        Client::new()
    }

    // ── Contract: empty hooks → immediate Allow ──

    #[tokio::test]
    async fn empty_hooks_returns_allow() {
        let body = json!({"prompt": "hi"});
        match execute(&[], &client(), body.clone()).await {
            HookOutcome::Allow { body: b, hooks_ran } => {
                assert_eq!(b, body);
                assert!(hooks_ran.is_empty());
            }
            HookOutcome::Reject(_) => panic!("expected Allow"),
        }
    }

    // ── Contract: action=allow → Allow with hook name ──

    #[tokio::test]
    async fn hook_allow_returns_allow() {
        let (url, _h) = start_hook_server(json!({"action": "allow"}), StatusCode::OK).await;
        let hook = make_hook(&url);
        let body = json!({"prompt": "hi"});
        match execute(&[hook], &client(), body).await {
            HookOutcome::Allow { hooks_ran, .. } => {
                assert_eq!(hooks_ran, vec!["test-hook"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow"),
        }
    }

    // ── Contract: action=reject + on_reject=Block403 → 403 ──

    #[tokio::test]
    async fn hook_reject_block403() {
        let (url, _h) = start_hook_server(
            json!({"action": "reject", "reason": "bad content"}),
            StatusCode::OK,
        ).await;
        let mut hook = make_hook(&url);
        hook.on_reject = HookRejectPolicy::Block403;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Reject(resp) => {
                assert_eq!(resp.status(), StatusCode::FORBIDDEN);
                let body = to_bytes(resp.into_body(), 1024).await.unwrap();
                assert!(String::from_utf8_lossy(&body).contains("bad content"));
            }
            HookOutcome::Allow { .. } => panic!("expected Reject"),
        }
    }

    // ── Contract: action=reject + on_reject=Block400 → 400 ──

    #[tokio::test]
    async fn hook_reject_block400() {
        let (url, _h) = start_hook_server(
            json!({"action": "reject", "reason": "invalid"}),
            StatusCode::OK,
        ).await;
        let mut hook = make_hook(&url);
        hook.on_reject = HookRejectPolicy::Block400;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Reject(resp) => assert_eq!(resp.status(), StatusCode::BAD_REQUEST),
            HookOutcome::Allow { .. } => panic!("expected Reject"),
        }
    }

    // ── Contract: action=reject + on_reject=Pass → Allow (rejection ignored) ──

    #[tokio::test]
    async fn hook_reject_pass_ignores_rejection() {
        let (url, _h) = start_hook_server(
            json!({"action": "reject", "reason": "nope"}),
            StatusCode::OK,
        ).await;
        let mut hook = make_hook(&url);
        hook.on_reject = HookRejectPolicy::Pass;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Allow { hooks_ran, .. } => {
                assert_eq!(hooks_ran, vec!["test-hook:rejected-ignored"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow (on_reject=pass)"),
        }
    }

    // ── Contract: action=transform + transform=true + body → body replaced ──

    #[tokio::test]
    async fn hook_transform_replaces_body() {
        let new_body = json!({"prompt": "sanitized"});
        let (url, _h) = start_hook_server(
            json!({"action": "transform", "body": new_body}),
            StatusCode::OK,
        ).await;
        let mut hook = make_hook(&url);
        hook.transform = true;
        match execute(&[hook], &client(), json!({"prompt": "original"})).await {
            HookOutcome::Allow { body, hooks_ran } => {
                assert_eq!(body, new_body);
                assert_eq!(hooks_ran, vec!["test-hook:transformed"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow"),
        }
    }

    // ── Contract: action=transform without body → no-op ──

    #[tokio::test]
    async fn hook_transform_without_body_is_noop() {
        let (url, _h) = start_hook_server(
            json!({"action": "transform"}),
            StatusCode::OK,
        ).await;
        let mut hook = make_hook(&url);
        hook.transform = true;
        let original = json!({"prompt": "hi"});
        match execute(&[hook], &client(), original.clone()).await {
            HookOutcome::Allow { body, hooks_ran } => {
                assert_eq!(body, original);
                assert_eq!(hooks_ran, vec!["test-hook:transform-no-body"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow"),
        }
    }

    // ── Contract: action=transform but transform=false in config → treated as allow ──

    #[tokio::test]
    async fn hook_transform_disabled_treated_as_allow() {
        let (url, _h) = start_hook_server(
            json!({"action": "transform", "body": {"x": 1}}),
            StatusCode::OK,
        ).await;
        let hook = make_hook(&url); // transform=false by default
        let original = json!({"prompt": "hi"});
        match execute(&[hook], &client(), original.clone()).await {
            HookOutcome::Allow { body, hooks_ran } => {
                assert_eq!(body, original, "body must not change when transform=false");
                assert_eq!(hooks_ran, vec!["test-hook"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow"),
        }
    }

    // ── Contract: hook timeout + on_error=pass → skip ──

    #[tokio::test]
    async fn hook_timeout_on_error_pass_skips() {
        let (url, _h) = start_slow_hook_server(5000).await;
        let mut hook = make_hook(&url);
        hook.timeout_ms = 50; // very short timeout
        hook.on_error = HookErrorPolicy::Pass;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Allow { hooks_ran, .. } => {
                assert_eq!(hooks_ran, vec!["test-hook:timeout"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow (on_error=pass)"),
        }
    }

    // ── Contract: hook timeout + on_error=block → 502 ──

    #[tokio::test]
    async fn hook_timeout_on_error_block_rejects() {
        let (url, _h) = start_slow_hook_server(5000).await;
        let mut hook = make_hook(&url);
        hook.timeout_ms = 50;
        hook.on_error = HookErrorPolicy::Block;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Reject(resp) => assert_eq!(resp.status(), StatusCode::BAD_GATEWAY),
            HookOutcome::Allow { .. } => panic!("expected Reject (on_error=block)"),
        }
    }

    // ── Contract: hook returns non-200 + on_error=pass → skip ──

    #[tokio::test]
    async fn hook_non200_on_error_pass_skips() {
        let (url, _h) = start_hook_server(json!({}), StatusCode::INTERNAL_SERVER_ERROR).await;
        let mut hook = make_hook(&url);
        hook.on_error = HookErrorPolicy::Pass;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Allow { hooks_ran, .. } => {
                assert_eq!(hooks_ran, vec!["test-hook:error"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow (on_error=pass)"),
        }
    }

    // ── Contract: hook returns non-200 + on_error=block → 502 ──

    #[tokio::test]
    async fn hook_non200_on_error_block_rejects() {
        let (url, _h) = start_hook_server(json!({}), StatusCode::INTERNAL_SERVER_ERROR).await;
        let mut hook = make_hook(&url);
        hook.on_error = HookErrorPolicy::Block;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Reject(resp) => assert_eq!(resp.status(), StatusCode::BAD_GATEWAY),
            HookOutcome::Allow { .. } => panic!("expected Reject (on_error=block)"),
        }
    }

    // ── Contract: hook returns invalid JSON + on_error=pass → skip ──

    #[tokio::test]
    async fn hook_invalid_json_on_error_pass_skips() {
        let (url, _h) = start_invalid_json_server().await;
        let mut hook = make_hook(&url);
        hook.on_error = HookErrorPolicy::Pass;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Allow { hooks_ran, .. } => {
                assert_eq!(hooks_ran, vec!["test-hook:error"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow (on_error=pass)"),
        }
    }

    // ── Contract: hook returns invalid JSON + on_error=block → 502 ──

    #[tokio::test]
    async fn hook_invalid_json_on_error_block_rejects() {
        let (url, _h) = start_invalid_json_server().await;
        let mut hook = make_hook(&url);
        hook.on_error = HookErrorPolicy::Block;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Reject(resp) => assert_eq!(resp.status(), StatusCode::BAD_GATEWAY),
            HookOutcome::Allow { .. } => panic!("expected Reject (on_error=block)"),
        }
    }

    // ── Contract: unknown action → skip (treated as allow with :unknown suffix) ──

    #[tokio::test]
    async fn hook_unknown_action_skips() {
        let (url, _h) = start_hook_server(
            json!({"action": "foobar"}),
            StatusCode::OK,
        ).await;
        let hook = make_hook(&url);
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Allow { hooks_ran, .. } => {
                assert_eq!(hooks_ran, vec!["test-hook:unknown"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow"),
        }
    }

    // ── Contract: connection refused + on_error=pass → skip ──

    #[tokio::test]
    async fn hook_connection_refused_on_error_pass_skips() {
        let mut hook = make_hook("http://127.0.0.1:1/hook"); // port 1 — nobody listens
        hook.on_error = HookErrorPolicy::Pass;
        match execute(&[hook], &client(), json!({})).await {
            HookOutcome::Allow { hooks_ran, .. } => {
                assert_eq!(hooks_ran, vec!["test-hook:error"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow (on_error=pass)"),
        }
    }

    // ── Contract: multiple hooks execute in order, first reject stops chain ──

    #[tokio::test]
    async fn multiple_hooks_first_reject_stops_chain() {
        let (url_allow, _h1) = start_hook_server(json!({"action": "allow"}), StatusCode::OK).await;
        let (url_reject, _h2) = start_hook_server(
            json!({"action": "reject", "reason": "blocked"}),
            StatusCode::OK,
        ).await;
        let (url_allow2, _h3) = start_hook_server(json!({"action": "allow"}), StatusCode::OK).await;

        let hooks = vec![
            PreRoutingHook { name: "h1".into(), url: url_allow, ..make_hook("") },
            PreRoutingHook { name: "h2".into(), url: url_reject, on_reject: HookRejectPolicy::Block403, ..make_hook("") },
            PreRoutingHook { name: "h3".into(), url: url_allow2, ..make_hook("") },
        ];
        match execute(&hooks, &client(), json!({})).await {
            HookOutcome::Reject(resp) => assert_eq!(resp.status(), StatusCode::FORBIDDEN),
            HookOutcome::Allow { hooks_ran, .. } => panic!("expected Reject, got Allow with {:?}", hooks_ran),
        }
    }

    // ── Contract: multiple hooks all allow → all names recorded ──

    #[tokio::test]
    async fn multiple_hooks_all_allow() {
        let (url1, _h1) = start_hook_server(json!({"action": "allow"}), StatusCode::OK).await;
        let (url2, _h2) = start_hook_server(json!({"action": "allow"}), StatusCode::OK).await;

        let hooks = vec![
            PreRoutingHook { name: "first".into(), url: url1, ..make_hook("") },
            PreRoutingHook { name: "second".into(), url: url2, ..make_hook("") },
        ];
        match execute(&hooks, &client(), json!({})).await {
            HookOutcome::Allow { hooks_ran, .. } => {
                assert_eq!(hooks_ran, vec!["first", "second"]);
            }
            HookOutcome::Reject(_) => panic!("expected Allow"),
        }
    }
}
