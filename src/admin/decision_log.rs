use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;

/// A single routing decision record.
#[derive(Debug, Clone, Serialize)]
pub struct DecisionRecord {
    pub timestamp: String,
    pub request_id: Option<String>,
    pub route: String,
    pub model: Option<String>,
    pub method: Option<String>,
    pub policy: Option<String>,
    pub cluster: Option<String>,
    pub worker: Option<String>,
    pub cache_status: Option<String>,
    pub status: u16,
    pub duration_ms: u64,
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
