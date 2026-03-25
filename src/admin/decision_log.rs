use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Schema version for the decision record format.
///
/// Bump this when adding, removing, or renaming fields. Consumers
/// (replay, dashboards, scripts) can use this to detect incompatible
/// exports without silent data loss.
///
/// History:
///   1 — initial schema (v0.7.0)
///   2 — added `schema_version` and `hooks_ran` fields (v0.7.2)
///   3 — added `tenant` field for multi-tenant observability (v0.9.0)
pub const DECISION_SCHEMA_VERSION: u32 = 3;

/// A single routing decision record.
///
/// This is the stable serialisation format for `/admin/decisions`, JSONL
/// export, and the `replay` subcommand. All fields are considered part
/// of the public contract — see [`DECISION_SCHEMA_VERSION`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// Schema version that produced this record.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub route: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// How the worker was selected: `"policy"`, `"cluster"`, `"lmcache-prefix"`,
    /// `"cache-hit"`, or `"semantic-hit"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Load-balancing policy name (e.g. `"round_robin"`, `"consistent_hash"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// Semantic cluster that matched, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    /// Backend worker URL that handled the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// Cache status: `"exact-hit"`, `"semantic-hit"`, or `"miss"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<String>,
    /// HTTP status code returned to the client.
    pub status: u16,
    /// Total routing latency in milliseconds.
    pub duration_ms: u64,
    /// Pre-routing hooks that executed. Each entry is `"name"` on success or
    /// `"name:outcome"` (e.g. `"pii:timeout"`, `"safety:rejected-ignored"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks_ran: Vec<String>,
    /// Request text (prompt content). Only populated when `include_request_text` is enabled.
    /// WARNING: may contain PII — do not enable in production without data handling review.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_text: Option<String>,
    /// Tenant name that made the request (from multi-tenant API key auth).
    /// Only populated when `api_keys` is configured. Never contains the raw key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

fn default_schema_version() -> u32 {
    1 // Records from before schema_version was added are version 1.
}

/// Bounded ring buffer of recent routing decisions.
///
/// Thread-safe via `parking_lot::Mutex`. Oldest entries are evicted when
/// capacity is reached.
#[derive(Debug)]
pub struct DecisionLog {
    entries: Mutex<VecDeque<DecisionRecord>>,
    capacity: usize,
}

impl DecisionLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn push(&self, record: DecisionRecord) {
        let mut entries = self.entries.lock();
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(record);
    }

    /// Return the last `limit` decisions (most recent last).
    pub fn recent(&self, limit: usize) -> Vec<DecisionRecord> {
        let entries = self.entries.lock();
        let skip = entries.len().saturating_sub(limit);
        entries.iter().skip(skip).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }
}

/// Required (always-present) field names in a serialized DecisionRecord.
pub const REQUIRED_FIELDS: &[&str] = &["schema_version", "timestamp", "route", "status", "duration_ms"];

/// All known field names in a serialized DecisionRecord (required + optional).
pub const ALL_FIELDS: &[&str] = &[
    "schema_version", "timestamp", "request_id", "route", "model",
    "method", "policy", "cluster", "worker", "cache_status",
    "status", "duration_ms", "hooks_ran", "request_text", "tenant",
];

/// Helper: current UTC timestamp as ISO-8601 string.
pub fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Spawn a background task that periodically flushes new decisions to a JSONL file.
///
/// Writes only entries that haven't been exported yet (tracks a cursor).
pub fn spawn_export_task(
    log: std::sync::Arc<DecisionLog>,
    path: String,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    use std::io::Write;
    use tracing::{info, warn};

    info!(path = path.as_str(), interval_secs, "decision export started");

    tokio::spawn(async move {
        let mut exported_count: usize = 0;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        interval.tick().await; // first tick is immediate

        loop {
            interval.tick().await;

            let entries = log.entries.lock();
            let total = entries.len();
            if total <= exported_count {
                continue;
            }
            // Collect new entries
            let new_entries: Vec<DecisionRecord> =
                entries.iter().skip(exported_count).cloned().collect();
            drop(entries);

            // Append to file
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                Ok(mut file) => {
                    for entry in &new_entries {
                        if let Ok(line) = serde_json::to_string(entry) {
                            let _ = writeln!(file, "{}", line);
                        }
                    }
                    exported_count += new_entries.len();
                }
                Err(e) => {
                    warn!(error = %e, path = path.as_str(), "decision export: failed to open file");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_record() -> DecisionRecord {
        DecisionRecord {
            schema_version: DECISION_SCHEMA_VERSION,
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
            request_id: Some("req-abc".to_string()),
            route: "/v1/chat/completions".to_string(),
            model: Some("llama-3".to_string()),
            method: Some("policy".to_string()),
            policy: Some("round_robin".to_string()),
            cluster: None,
            worker: Some("http://w1:8000".to_string()),
            cache_status: Some("miss".to_string()),
            status: 200,
            duration_ms: 42,
            hooks_ran: vec!["safety".to_string(), "pii:timeout".to_string()],
            request_text: None,
            tenant: Some("ml-team".to_string()),
        }
    }

    fn minimal_record() -> DecisionRecord {
        DecisionRecord {
            schema_version: DECISION_SCHEMA_VERSION,
            timestamp: now_iso8601(),
            request_id: None,
            route: "/v1/completions".to_string(),
            model: None,
            method: None,
            policy: None,
            cluster: None,
            worker: None,
            cache_status: None,
            status: 503,
            duration_ms: 0,
            hooks_ran: vec![],
            request_text: None,
            tenant: None,
        }
    }

    // ── Contract: serialization round-trip ──

    #[test]
    fn roundtrip_full_record() {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
        let deser: DecisionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.schema_version, DECISION_SCHEMA_VERSION);
        assert_eq!(deser.route, record.route);
        assert_eq!(deser.model, record.model);
        assert_eq!(deser.method, record.method);
        assert_eq!(deser.policy, record.policy);
        assert_eq!(deser.worker, record.worker);
        assert_eq!(deser.cache_status, record.cache_status);
        assert_eq!(deser.status, record.status);
        assert_eq!(deser.duration_ms, record.duration_ms);
        assert_eq!(deser.hooks_ran, record.hooks_ran);
        assert_eq!(deser.request_text, record.request_text);
    }

    #[test]
    fn roundtrip_minimal_record() {
        let record = minimal_record();
        let json = serde_json::to_string(&record).unwrap();
        let deser: DecisionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.status, 503);
        assert!(deser.worker.is_none());
        assert!(deser.hooks_ran.is_empty());
    }

    // ── Contract: required fields are always present in output ──

    #[test]
    fn required_fields_always_present() {
        let json_val: serde_json::Value =
            serde_json::to_value(&minimal_record()).unwrap();
        let obj = json_val.as_object().unwrap();
        for field in REQUIRED_FIELDS {
            assert!(obj.contains_key(*field), "missing required field: {field}");
        }
    }

    // ── Contract: optional fields are omitted when None/empty ──

    #[test]
    fn optional_fields_omitted_when_none() {
        let json_val: serde_json::Value =
            serde_json::to_value(&minimal_record()).unwrap();
        let obj = json_val.as_object().unwrap();
        // These should NOT appear in the output:
        for field in &["request_id", "model", "method", "policy", "cluster", "worker", "cache_status", "request_text", "hooks_ran", "tenant"] {
            assert!(!obj.contains_key(*field), "field {field} should be omitted when None/empty");
        }
    }

    // ── Contract: no unknown fields leak into serialized output ──

    #[test]
    fn no_unknown_fields_in_output() {
        let json_val: serde_json::Value =
            serde_json::to_value(&sample_record()).unwrap();
        let obj = json_val.as_object().unwrap();
        for key in obj.keys() {
            assert!(
                ALL_FIELDS.contains(&key.as_str()),
                "unexpected field in serialized output: {key}"
            );
        }
    }

    // ── Contract: backward compatibility — v1 records without schema_version parse correctly ──

    #[test]
    fn v1_record_without_schema_version_parses() {
        let v1_json = json!({
            "timestamp": "2025-12-01T00:00:00.000Z",
            "route": "/v1/chat/completions",
            "status": 200,
            "duration_ms": 10
        });
        let record: DecisionRecord = serde_json::from_value(v1_json).unwrap();
        assert_eq!(record.schema_version, 1, "missing schema_version should default to 1");
        assert!(record.hooks_ran.is_empty());
    }

    // ── Contract: schema_version matches current constant ──

    #[test]
    fn schema_version_is_current() {
        let record = sample_record();
        let json_val: serde_json::Value = serde_json::to_value(&record).unwrap();
        assert_eq!(
            json_val["schema_version"].as_u64().unwrap(),
            DECISION_SCHEMA_VERSION as u64
        );
    }

    // ── Contract: ALL_FIELDS matches actual struct fields ──

    #[test]
    fn all_fields_covers_full_record() {
        // Serialize a record with every optional field populated
        let json_val: serde_json::Value = serde_json::to_value(&sample_record()).unwrap();
        let obj = json_val.as_object().unwrap();
        for key in obj.keys() {
            assert!(
                ALL_FIELDS.contains(&key.as_str()),
                "ALL_FIELDS is missing: {key}"
            );
        }
    }

    // ── Contract: DecisionLog ring buffer ──

    #[test]
    fn decision_log_capacity() {
        let log = DecisionLog::new(3);
        for i in 0..5 {
            let mut r = minimal_record();
            r.status = 200 + i;
            log.push(r);
        }
        assert_eq!(log.len(), 3);
        let recent = log.recent(10);
        assert_eq!(recent.len(), 3);
        // Should have the last 3 entries (202, 203, 204)
        assert_eq!(recent[0].status, 202);
        assert_eq!(recent[2].status, 204);
    }

    #[test]
    fn decision_log_recent_limit() {
        let log = DecisionLog::new(10);
        for i in 0..5 {
            let mut r = minimal_record();
            r.status = 200 + i;
            log.push(r);
        }
        let recent = log.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].status, 203);
        assert_eq!(recent[1].status, 204);
    }
}
