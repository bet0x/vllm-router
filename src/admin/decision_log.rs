use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// A single routing decision record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub route: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<String>,
    pub status: u16,
    pub duration_ms: u64,
    /// Request text (prompt content). Only populated when `include_request_text` is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_text: Option<String>,
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
