use crate::config::types::ApiKeyEntry;
use crate::middleware::TokenBucket;
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Information about the authenticated tenant, propagated through the request pipeline.
#[derive(Debug, Clone)]
pub struct TenantInfo {
    pub name: String,
    pub allowed_models: Vec<String>,
    pub metadata: HashMap<String, String>,
}

/// Per-tenant runtime state: config + rate limiter + counters.
pub struct TenantState {
    pub name: String,
    pub enabled: bool,
    pub allowed_models: Vec<String>,
    pub metadata: HashMap<String, String>,
    pub rate_limiter: Arc<TokenBucket>,
    pub total_requests: AtomicU64,
    pub total_rate_limited: AtomicU64,
    pub rate_limit_rps: usize,
    pub max_concurrent: usize,
}

/// Registry mapping hashed API keys to tenant state.
///
/// Keys are stored as hex-encoded SHA-256 hashes — the plaintext key is never kept in memory
/// after initialization.
pub struct TenantRegistry {
    tenants: DashMap<String, Arc<TenantState>>,
}

impl TenantRegistry {
    /// Build a registry from config entries. Hashes each key with SHA-256.
    pub fn from_config(entries: &[ApiKeyEntry]) -> Self {
        let tenants = DashMap::new();
        for entry in entries {
            let hash = Self::hash_key(&entry.key);
            let state = Arc::new(TenantState {
                name: entry.name.clone(),
                enabled: entry.enabled,
                allowed_models: entry.allowed_models.clone(),
                metadata: entry.metadata.clone(),
                rate_limiter: Arc::new(TokenBucket::new(
                    entry.max_concurrent,
                    entry.rate_limit_rps,
                )),
                total_requests: AtomicU64::new(0),
                total_rate_limited: AtomicU64::new(0),
                rate_limit_rps: entry.rate_limit_rps,
                max_concurrent: entry.max_concurrent,
            });
            tenants.insert(hash, state);
        }
        Self { tenants }
    }

    /// Look up a tenant by raw (unhashed) API key.
    pub fn lookup(&self, raw_key: &str) -> Option<Arc<TenantState>> {
        let hash = Self::hash_key(raw_key);
        self.tenants.get(&hash).map(|r| r.value().clone())
    }

    /// Check if a model name is allowed for the given tenant.
    pub fn is_model_allowed(tenant: &TenantState, model: Option<&str>) -> bool {
        let model = match model {
            Some(m) => m,
            None => return true, // No model specified — allow (model check happens later)
        };
        for pattern in &tenant.allowed_models {
            if pattern == "*" {
                return true;
            }
            // Simple glob: only support trailing wildcard for now (e.g. "Llama-3*")
            if let Some(prefix) = pattern.strip_suffix('*') {
                if model.starts_with(prefix) {
                    return true;
                }
            } else if pattern == model {
                return true;
            }
        }
        false
    }

    /// List all tenants with their current stats (for /admin/tenants).
    pub fn list_tenants(&self) -> Vec<serde_json::Value> {
        self.tenants
            .iter()
            .map(|entry| {
                let t = entry.value();
                serde_json::json!({
                    "name": t.name,
                    "enabled": t.enabled,
                    "rate_limit_rps": t.rate_limit_rps,
                    "max_concurrent": t.max_concurrent,
                    "allowed_models": t.allowed_models,
                    "total_requests": t.total_requests.load(Ordering::Relaxed),
                    "total_rate_limited": t.total_rate_limited.load(Ordering::Relaxed),
                    "metadata": t.metadata,
                })
            })
            .collect()
    }

    /// Rebuild the registry from new config (for hot reload).
    /// Returns a new registry — the caller swaps the Arc.
    pub fn reload(entries: &[ApiKeyEntry]) -> Self {
        Self::from_config(entries)
    }

    /// Number of registered tenants.
    pub fn len(&self) -> usize {
        self.tenants.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tenants.is_empty()
    }

    fn hash_key(raw: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entries() -> Vec<ApiKeyEntry> {
        vec![
            ApiKeyEntry {
                key: "sk-alpha-secret".to_string(),
                name: "alpha-team".to_string(),
                rate_limit_rps: 100,
                max_concurrent: 50,
                allowed_models: vec!["*".to_string()],
                enabled: true,
                metadata: HashMap::from([("org".to_string(), "acme".to_string())]),
            },
            ApiKeyEntry {
                key: "sk-beta-secret".to_string(),
                name: "beta-team".to_string(),
                rate_limit_rps: 20,
                max_concurrent: 10,
                allowed_models: vec!["Llama-3*".to_string(), "gpt-4".to_string()],
                enabled: true,
                metadata: HashMap::new(),
            },
            ApiKeyEntry {
                key: "sk-disabled".to_string(),
                name: "disabled-team".to_string(),
                rate_limit_rps: 10,
                max_concurrent: 5,
                allowed_models: vec!["*".to_string()],
                enabled: false,
                metadata: HashMap::new(),
            },
        ]
    }

    #[test]
    fn test_registry_lookup_valid_key() {
        let reg = TenantRegistry::from_config(&sample_entries());
        let tenant = reg.lookup("sk-alpha-secret");
        assert!(tenant.is_some());
        assert_eq!(tenant.unwrap().name, "alpha-team");
    }

    #[test]
    fn test_registry_lookup_invalid_key() {
        let reg = TenantRegistry::from_config(&sample_entries());
        assert!(reg.lookup("sk-nonexistent").is_none());
    }

    #[test]
    fn test_registry_key_not_stored_plaintext() {
        let reg = TenantRegistry::from_config(&sample_entries());
        // The DashMap keys should be hex hashes, not plaintext
        for entry in reg.tenants.iter() {
            assert!(!entry.key().starts_with("sk-"), "Key stored as plaintext!");
            assert_eq!(entry.key().len(), 64, "Expected SHA-256 hex (64 chars)");
        }
    }

    #[test]
    fn test_model_allowed_wildcard() {
        let reg = TenantRegistry::from_config(&sample_entries());
        let alpha = reg.lookup("sk-alpha-secret").unwrap();
        assert!(TenantRegistry::is_model_allowed(&alpha, Some("anything")));
        assert!(TenantRegistry::is_model_allowed(&alpha, None));
    }

    #[test]
    fn test_model_allowed_glob_prefix() {
        let reg = TenantRegistry::from_config(&sample_entries());
        let beta = reg.lookup("sk-beta-secret").unwrap();
        assert!(TenantRegistry::is_model_allowed(&beta, Some("Llama-3-70B")));
        assert!(TenantRegistry::is_model_allowed(&beta, Some("Llama-3.1-8B")));
        assert!(TenantRegistry::is_model_allowed(&beta, Some("gpt-4")));
        assert!(!TenantRegistry::is_model_allowed(&beta, Some("Mistral-7B")));
        assert!(!TenantRegistry::is_model_allowed(&beta, Some("gpt-4o")));
    }

    #[test]
    fn test_disabled_tenant_lookup_still_works() {
        let reg = TenantRegistry::from_config(&sample_entries());
        let tenant = reg.lookup("sk-disabled").unwrap();
        assert!(!tenant.enabled);
        assert_eq!(tenant.name, "disabled-team");
    }

    #[test]
    fn test_list_tenants() {
        let reg = TenantRegistry::from_config(&sample_entries());
        let list = reg.list_tenants();
        assert_eq!(list.len(), 3);
        let names: Vec<&str> = list.iter().filter_map(|v| v["name"].as_str()).collect();
        assert!(names.contains(&"alpha-team"));
        assert!(names.contains(&"beta-team"));
        assert!(names.contains(&"disabled-team"));
    }

    #[test]
    fn test_registry_len() {
        let reg = TenantRegistry::from_config(&sample_entries());
        assert_eq!(reg.len(), 3);
        assert!(!reg.is_empty());
    }

    #[test]
    fn test_empty_registry() {
        let reg = TenantRegistry::from_config(&[]);
        assert_eq!(reg.len(), 0);
        assert!(reg.is_empty());
        assert!(reg.lookup("anything").is_none());
    }

    #[tokio::test]
    async fn test_tenant_rate_limiter() {
        let entries = vec![ApiKeyEntry {
            key: "sk-test".to_string(),
            name: "test".to_string(),
            rate_limit_rps: 2,
            max_concurrent: 2,
            allowed_models: vec!["*".to_string()],
            enabled: true,
            metadata: HashMap::new(),
        }];
        let reg = TenantRegistry::from_config(&entries);
        let tenant = reg.lookup("sk-test").unwrap();

        // Should succeed twice (capacity=2)
        assert!(tenant.rate_limiter.try_acquire(1.0).await.is_ok());
        assert!(tenant.rate_limiter.try_acquire(1.0).await.is_ok());
        // Third should fail
        assert!(tenant.rate_limiter.try_acquire(1.0).await.is_err());
    }
}
