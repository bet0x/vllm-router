//! Model name rewriting rules.
//!
//! Evaluated before any routing decision. Rewrites the client-provided model
//! name to a canonical model name that the worker registry knows about.

use crate::core::WorkerRegistry;
use serde::{Deserialize, Serialize};
use tracing::info;

/// A single model rewrite rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRule {
    /// Pattern to match against the incoming model name.
    /// Exact string match, or glob with trailing `*` (e.g. `"openai/*"`).
    #[serde(rename = "match")]
    pub match_pattern: String,

    /// Simple rewrite: replace the model name with this value.
    #[serde(default)]
    pub rewrite: Option<String>,

    /// Fallback chain: try each model in order, use the first one that has
    /// healthy workers in the registry.
    #[serde(default)]
    pub fallback: Option<Vec<String>>,
}

/// Result of evaluating model rules.
#[derive(Debug)]
pub struct RewriteResult {
    /// The resolved model name (may be the same as input if no rule matched).
    pub model: String,
    /// The original model name before rewriting (None if unchanged).
    pub original: Option<String>,
}

/// Evaluate model rules against an incoming model name.
///
/// Returns `None` if `model_id` is `None` (no model in request).
/// Returns the resolved model name, potentially rewritten by the first
/// matching rule.
pub fn resolve(
    model_id: Option<&str>,
    rules: &[ModelRule],
    registry: &WorkerRegistry,
) -> Option<RewriteResult> {
    let model = model_id?;

    for rule in rules {
        if !matches_pattern(&rule.match_pattern, model) {
            continue;
        }

        // Simple rewrite
        if let Some(ref target) = rule.rewrite {
            info!(from = model, to = target.as_str(), "model rewrite (alias)");
            return Some(RewriteResult {
                model: target.clone(),
                original: Some(model.to_string()),
            });
        }

        // Fallback chain: try each model, pick first with healthy workers
        if let Some(ref chain) = rule.fallback {
            for candidate in chain {
                let workers = registry.get_by_model_fast(candidate);
                let has_healthy = workers.iter().any(|w| w.is_available());
                if has_healthy {
                    if candidate != model {
                        info!(from = model, to = candidate.as_str(), "model rewrite (fallback)");
                        return Some(RewriteResult {
                            model: candidate.clone(),
                            original: Some(model.to_string()),
                        });
                    } else {
                        // First candidate is the same as input, no rewrite needed
                        return Some(RewriteResult {
                            model: candidate.clone(),
                            original: None,
                        });
                    }
                }
            }
            // No candidate had healthy workers — pass through unchanged
            info!(from = model, "model fallback: no healthy candidates, passing through");
        }
    }

    // No rule matched — pass through unchanged
    Some(RewriteResult {
        model: model.to_string(),
        original: None,
    })
}

/// Check if `pattern` matches `model`.
/// Supports exact match and trailing wildcard (`"openai/*"` matches `"openai/gpt-4"`).
fn matches_pattern(pattern: &str, model: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        model.starts_with(prefix)
    } else {
        pattern == model
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        assert!(matches_pattern("gpt-4", "gpt-4"));
        assert!(!matches_pattern("gpt-4", "gpt-4o"));
    }

    #[test]
    fn test_wildcard_match() {
        assert!(matches_pattern("openai/*", "openai/gpt-4"));
        assert!(matches_pattern("openai/*", "openai/gpt-3.5-turbo"));
        assert!(!matches_pattern("openai/*", "anthropic/claude"));
    }

    #[test]
    fn test_wildcard_all() {
        assert!(matches_pattern("*", "anything"));
    }
}
