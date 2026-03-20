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
use tracing::{debug, warn};

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
                hooks_ran.push(hook.name.clone());
            }
            "reject" => {
                let reason = hook_resp.reason.unwrap_or_else(|| "rejected by hook".to_string());
                debug!(hook = hook.name.as_str(), reason = reason.as_str(), "hook rejected");
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
                        body = new_body;
                        hooks_ran.push(format!("{}:transformed", hook.name));
                    } else {
                        warn!(hook = hook.name.as_str(), "hook returned transform without body");
                        hooks_ran.push(format!("{}:transform-no-body", hook.name));
                    }
                } else {
                    debug!(hook = hook.name.as_str(), "hook returned transform but transform=false in config, treating as allow");
                    hooks_ran.push(hook.name.clone());
                }
            }
            other => {
                warn!(hook = hook.name.as_str(), action = other, "hook returned unknown action");
                hooks_ran.push(format!("{}:unknown", hook.name));
            }
        }
    }

    HookOutcome::Allow {
        body,
        hooks_ran,
    }
}
