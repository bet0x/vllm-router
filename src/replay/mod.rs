//! Decision replay engine.
//!
//! Reads a JSONL file of routing decisions exported by the router and
//! re-evaluates them against a different configuration to produce a
//! comparison report.

use crate::admin::DecisionRecord;
use crate::config::types::RouterConfig;
use crate::core::WorkerRegistry;
use crate::policies::PolicyRegistry;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

/// Summary of a replay comparison.
pub struct ReplayReport {
    pub total: usize,
    pub same_worker: usize,
    pub different_worker: usize,
    pub no_original_worker: usize,
    pub no_new_worker: usize,
    pub original_latency_ms: Vec<u64>,
    pub policy_counts: HashMap<String, usize>,
}

impl ReplayReport {
    pub fn print(&self) {
        println!("Replay: {} decisions", self.total);
        if self.total == 0 {
            println!("  (no decisions to replay)");
            return;
        }
        let same_pct = self.same_worker as f64 / self.total as f64 * 100.0;
        let diff_pct = self.different_worker as f64 / self.total as f64 * 100.0;
        println!(
            "  Same worker selected:  {:>6} ({:.1}%)",
            self.same_worker, same_pct
        );
        println!(
            "  Different worker:      {:>6} ({:.1}%)",
            self.different_worker, diff_pct
        );
        if self.no_original_worker > 0 {
            println!(
                "  No original worker:    {:>6} (cache hits in original)",
                self.no_original_worker
            );
        }
        if self.no_new_worker > 0 {
            println!(
                "  No new worker:         {:>6} (no healthy workers for model)",
                self.no_new_worker
            );
        }

        if !self.original_latency_ms.is_empty() {
            let mut sorted = self.original_latency_ms.clone();
            sorted.sort();
            let p50 = sorted[sorted.len() / 2];
            let p99 = sorted[(sorted.len() as f64 * 0.99) as usize];
            println!(
                "  Original latency:      P50={}ms  P99={}ms",
                p50, p99
            );
        }

        if !self.policy_counts.is_empty() {
            println!("  Policy distribution:");
            let mut entries: Vec<_> = self.policy_counts.iter().collect();
            entries.sort_by(|a, b| b.1.cmp(a.1));
            for (policy, count) in entries {
                let pct = *count as f64 / self.total as f64 * 100.0;
                println!("    {:<20} {:>6} ({:.1}%)", policy, count, pct);
            }
        }
    }
}

/// Load decisions from a JSONL file.
pub fn load_decisions(path: &Path) -> Result<Vec<DecisionRecord>, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("Failed to open {}: {}", path.display(), e))?;
    let reader = std::io::BufReader::new(file);

    let mut decisions = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("Line {}: {}", i + 1, e))?;
        if line.trim().is_empty() {
            continue;
        }
        let record: DecisionRecord =
            serde_json::from_str(&line).map_err(|e| format!("Line {}: {}", i + 1, e))?;
        decisions.push(record);
    }
    Ok(decisions)
}

/// Replay decisions against a new configuration.
///
/// For each decision, simulates worker selection using the new config's
/// policy. Policies that need request text (cache_aware, semantic_cluster)
/// can only work when `request_text` is present in the decision records.
pub fn replay(decisions: &[DecisionRecord], new_config: &RouterConfig) -> ReplayReport {
    // Build a fake worker registry from the new config's worker URLs
    let worker_urls = new_config.mode.all_worker_urls();
    let registry = WorkerRegistry::new();
    for url in &worker_urls {
        let worker = crate::core::BasicWorker::new(
            url.clone(),
            crate::core::WorkerType::Regular,
        );
        registry.register(std::sync::Arc::new(worker));
    }

    // Build the policy registry
    let policy_registry = PolicyRegistry::new(new_config.policy.clone());

    let mut report = ReplayReport {
        total: decisions.len(),
        same_worker: 0,
        different_worker: 0,
        no_original_worker: 0,
        no_new_worker: 0,
        original_latency_ms: Vec::new(),
        policy_counts: HashMap::new(),
    };

    for record in decisions {
        // Skip cache hits (no worker selection was made)
        if record.worker.is_none() {
            report.no_original_worker += 1;
            continue;
        }

        let model_id = record.model.as_deref();

        // Get workers for model
        let workers = match model_id {
            Some(m) => {
                let by_model = registry.get_by_model_fast(m);
                if by_model.is_empty() {
                    registry.get_all()
                } else {
                    by_model
                }
            }
            None => registry.get_all(),
        };

        let available: Vec<std::sync::Arc<dyn crate::core::Worker>> = workers
            .iter()
            .filter(|w| w.is_available())
            .cloned()
            .collect();

        if available.is_empty() {
            report.no_new_worker += 1;
            continue;
        }

        // Select worker using new policy
        let policy = match model_id {
            Some(m) => policy_registry.get_policy_or_default(m),
            None => policy_registry.get_default_policy(),
        };

        let text = record.request_text.as_deref();
        let idx = policy.select_worker_with_headers(&available, text, None);

        match idx {
            Some(i) => {
                let new_worker = available[i].url();
                let original_worker = record.worker.as_deref().unwrap_or("");

                *report.policy_counts.entry(policy.name().to_string()).or_default() += 1;

                if new_worker == original_worker {
                    report.same_worker += 1;
                } else {
                    report.different_worker += 1;
                }
            }
            None => {
                report.no_new_worker += 1;
            }
        }

        report.original_latency_ms.push(record.duration_ms);
    }

    report
}
