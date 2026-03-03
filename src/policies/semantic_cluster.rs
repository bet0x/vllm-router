//! Semantic cluster routing policy.
//!
//! Routes requests to pre-defined clusters of workers based on cosine similarity
//! between the request embedding and per-cluster centroid vectors.
//!
//! Each cluster is defined by:
//! - A name (used in log messages and response headers).
//! - A centroid vector — the L2-normalised average of the cluster's example embeddings.
//! - A list of worker URLs that belong to this cluster.
//!
//! At routing time the policy picks the cluster whose centroid is closest to the
//! query embedding (by cosine similarity).  If the best score is below `threshold`
//! the policy returns `None` and the caller falls back to the regular load-balancing
//! policy.  Within the winning cluster workers are selected round-robin.

use crate::core::Worker;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Internal state for a single named cluster.
#[derive(Debug)]
pub struct ClusterState {
    /// Cluster name (used for logging / headers).
    pub name: String,
    /// L2-normalised centroid vector (average of example embeddings).
    centroid: Vec<f32>,
    /// Worker base URLs that belong to this cluster.
    worker_urls: Vec<String>,
    /// Round-robin counter for intra-cluster worker selection.
    next: AtomicUsize,
}

/// Routes requests to the best-matching cluster of workers using cosine similarity.
///
/// Returns `None` when no cluster reaches `threshold`, letting the caller fall back
/// to the regular [`LoadBalancingPolicy`](crate::policies::LoadBalancingPolicy).
#[derive(Debug)]
pub struct SemanticClusterPolicy {
    clusters: Vec<ClusterState>,
    /// Minimum cosine similarity required to commit to a cluster (default 0.75).
    pub threshold: f32,
}

impl SemanticClusterPolicy {
    /// Build from raw cluster data: `(name, centroid, worker_urls)`.
    pub fn from_parts(parts: Vec<(String, Vec<f32>, Vec<String>)>, threshold: f32) -> Self {
        let clusters = parts
            .into_iter()
            .map(|(name, centroid, worker_urls)| ClusterState {
                name,
                centroid,
                worker_urls,
                next: AtomicUsize::new(0),
            })
            .collect();
        Self { clusters, threshold }
    }

    /// Given a query embedding, select a worker from the best-matching cluster.
    ///
    /// Returns `(worker, cluster_name)` when a cluster exceeds `threshold` and
    /// has at least one available worker, otherwise returns `None`.
    pub fn route<'a>(
        &'a self,
        embedding: &[f32],
        available_workers: &[Arc<dyn Worker>],
    ) -> Option<(Arc<dyn Worker>, &'a str)> {
        // Find the cluster with the highest cosine similarity to the query.
        let (best_idx, best_score) = self
            .clusters
            .iter()
            .enumerate()
            .map(|(i, c)| (i, cosine_similarity(embedding, &c.centroid)))
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))?;

        if best_score < self.threshold {
            return None;
        }

        let cluster = &self.clusters[best_idx];

        // Filter available workers to those belonging to this cluster.
        let cluster_workers: Vec<&Arc<dyn Worker>> = available_workers
            .iter()
            .filter(|w| {
                cluster
                    .worker_urls
                    .iter()
                    .any(|url| w.url() == url.as_str() || w.url().starts_with(url.as_str()))
            })
            .collect();

        if cluster_workers.is_empty() {
            return None;
        }

        // Round-robin within the cluster.
        let idx = cluster.next.fetch_add(1, Ordering::Relaxed) % cluster_workers.len();
        Some((cluster_workers[idx].clone(), &cluster.name))
    }

    /// Number of configured clusters.
    pub fn cluster_count(&self) -> usize {
        self.clusters.len()
    }
}

/// Cosine similarity between two vectors.
///
/// Returns `0.0` for zero-length vectors or mismatched dimensions.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{BasicWorker, WorkerType};

    fn make_workers(urls: &[&str]) -> Vec<Arc<dyn Worker>> {
        urls.iter()
            .map(|u| {
                Arc::new(BasicWorker::new(u.to_string(), WorkerType::Regular)) as Arc<dyn Worker>
            })
            .collect()
    }

    fn math_code_policy(threshold: f32) -> SemanticClusterPolicy {
        SemanticClusterPolicy::from_parts(
            vec![
                (
                    "math".to_string(),
                    vec![1.0f32, 0.0, 0.0],
                    vec!["http://w-math:8000".to_string()],
                ),
                (
                    "code".to_string(),
                    vec![0.0f32, 1.0, 0.0],
                    vec!["http://w-code:8000".to_string()],
                ),
            ],
            threshold,
        )
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        assert!(cosine_similarity(&[1.0, 0.0, 0.0], &[0.0, 1.0, 0.0]).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        assert_eq!(cosine_similarity(&[0.0, 0.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn test_cosine_similarity_mismatched_dims() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn test_route_selects_math_cluster() {
        let policy = math_code_policy(0.5);
        let workers = make_workers(&["http://w-math:8000", "http://w-code:8000"]);
        let (worker, cluster) = policy.route(&[0.99, 0.01, 0.0], &workers).unwrap();
        assert_eq!(cluster, "math");
        assert_eq!(worker.url(), "http://w-math:8000");
    }

    #[test]
    fn test_route_selects_code_cluster() {
        let policy = math_code_policy(0.5);
        let workers = make_workers(&["http://w-math:8000", "http://w-code:8000"]);
        let (worker, cluster) = policy.route(&[0.01, 0.99, 0.0], &workers).unwrap();
        assert_eq!(cluster, "code");
        assert_eq!(worker.url(), "http://w-code:8000");
    }

    #[test]
    fn test_route_below_threshold_returns_none() {
        let policy = math_code_policy(0.99); // very high threshold
        let workers = make_workers(&["http://w-math:8000", "http://w-code:8000"]);
        // Embedding at 45° between math and code — similarity ~0.71, below 0.99
        let emb = vec![
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
            0.0,
        ];
        assert!(policy.route(&emb, &workers).is_none());
    }

    #[test]
    fn test_route_no_cluster_workers_available_returns_none() {
        let policy = math_code_policy(0.5);
        // Only "other" worker available — not in any cluster
        let workers = make_workers(&["http://w-other:8000"]);
        assert!(policy.route(&[0.99, 0.01, 0.0], &workers).is_none());
    }

    #[test]
    fn test_route_round_robin_within_cluster() {
        let policy = SemanticClusterPolicy::from_parts(
            vec![(
                "math".to_string(),
                vec![1.0f32, 0.0, 0.0],
                vec!["http://w1:8000".to_string(), "http://w2:8000".to_string()],
            )],
            0.5,
        );
        let workers = make_workers(&["http://w1:8000", "http://w2:8000"]);
        let emb = vec![0.99f32, 0.01, 0.0];
        let first = policy
            .route(&emb, &workers)
            .map(|(w, _)| w.url().to_string())
            .unwrap();
        let second = policy
            .route(&emb, &workers)
            .map(|(w, _)| w.url().to_string())
            .unwrap();
        assert_ne!(first, second, "should round-robin between cluster workers");
    }

    #[test]
    fn test_cluster_count() {
        let policy = math_code_policy(0.5);
        assert_eq!(policy.cluster_count(), 2);
    }
}
