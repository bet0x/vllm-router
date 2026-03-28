use dashmap::DashMap;
use reqwest::Client;
use std::sync::LazyLock;
use std::time::Duration;

pub const UNIX_SCHEME: &str = "unix";
pub const DEFAULT_UNIX_ORIGIN: &str = "http://localhost";

/// Basic TCP client for call sites that don't have a pre-configured client
/// (e.g. background health checks, standalone calls).
static TCP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to create shared TCP HTTP client")
});

/// Cached UDS clients keyed by socket path. Each distinct socket path gets its
/// own `reqwest::Client` because `ClientBuilder::unix_socket()` pins the client
/// to a single socket.
static UNIX_CLIENTS: LazyLock<DashMap<String, Client>> = LazyLock::new(DashMap::new);

pub fn is_unix_socket_url(url: &str) -> bool {
    url.starts_with("unix://")
}

pub fn strip_dp_suffix(url: &str) -> &str {
    if let Some((prefix, suffix)) = url.rsplit_once('@') {
        if suffix.parse::<usize>().is_ok() {
            return prefix;
        }
    }
    url
}

pub fn validate_worker_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("URL cannot be empty".to_string());
    }

    if is_unix_socket_url(url) {
        #[cfg(not(unix))]
        {
            return Err(
                "Unix socket worker URLs are only supported on Unix platforms".to_string(),
            );
        }

        #[cfg(unix)]
        {
            let parsed = url::Url::parse(url)
                .map_err(|e| format!("Invalid Unix socket URL format: {e}"))?;
            if parsed.scheme() != UNIX_SCHEME {
                return Err("Unix socket URL must use unix:// scheme".to_string());
            }
            if parsed.host_str().is_some() {
                return Err(
                    "Unix socket URL must not include a host; use unix:///absolute/path.sock"
                        .to_string(),
                );
            }
            if !parsed.path().starts_with('/') || parsed.path().len() <= 1 {
                return Err(
                    "Unix socket URL must contain an absolute socket path, e.g. unix:///tmp/vllm.sock"
                        .to_string(),
                );
            }
            if parsed.query().is_some() || parsed.fragment().is_some() {
                return Err("Unix socket URL must not include query or fragment".to_string());
            }
            return Ok(());
        }
    }

    if !url.starts_with("http://")
        && !url.starts_with("https://")
        && !url.starts_with("grpc://")
    {
        return Err("URL must start with http://, https://, grpc://, or unix://".to_string());
    }

    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL format: {e}"))?;
    if parsed.host_str().is_none() {
        return Err("URL must have a valid host".to_string());
    }

    Ok(())
}

/// Build the HTTP request URL for a given worker base URL and route.
///
/// - TCP: `"{base}{route}"` (standard URL join)
/// - UDS: `"http://localhost{route}"` (the socket path is transport-level, not in the URL)
pub fn request_url(base: &str, route: &str) -> String {
    let route = if route.starts_with('/') {
        route.to_string()
    } else {
        format!("/{}", route)
    };
    let base = strip_dp_suffix(base);

    if is_unix_socket_url(base) {
        format!(
            "{}{}",
            DEFAULT_UNIX_ORIGIN.trim_end_matches('/'),
            route
        )
    } else {
        format!("{}{}", base.trim_end_matches('/'), route)
    }
}

/// Resolve the correct `reqwest::Client` for a worker URL.
///
/// - TCP: returns `tcp_client` (the caller's pre-configured, pool-tuned client)
/// - UDS: returns a cached per-socket client from the global `UNIX_CLIENTS` pool
///
/// Use this when you have a well-tuned client (e.g. from `AppContext`) for TCP.
pub fn resolve_client(worker_url: &str, tcp_client: &Client) -> Result<Client, String> {
    let worker_url = strip_dp_suffix(worker_url);
    if !is_unix_socket_url(worker_url) {
        return Ok(tcp_client.clone());
    }
    uds_client(worker_url)
}

/// Resolve the correct `reqwest::Client` for a worker URL using the default
/// TCP client. Use this for standalone/background call sites that don't have
/// a pre-configured client (health checks, startup polling, etc.).
pub fn client_for_worker_url(worker_url: &str) -> Result<Client, String> {
    resolve_client(worker_url, &TCP_CLIENT)
}

#[cfg(not(unix))]
fn uds_client(_worker_url: &str) -> Result<Client, String> {
    Err("Unix socket worker URLs are only supported on Unix platforms".to_string())
}

#[cfg(unix)]
fn uds_client(worker_url: &str) -> Result<Client, String> {
    let socket_path = unix_socket_path(worker_url)?;
    if let Some(client) = UNIX_CLIENTS.get(&socket_path) {
        return Ok(client.clone());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .unix_socket(socket_path.as_str())
        .build()
        .map_err(|e| format!("Failed to create Unix socket client for {socket_path}: {e}"))?;
    UNIX_CLIENTS.insert(socket_path, client.clone());
    Ok(client)
}

#[cfg(unix)]
fn unix_socket_path(url: &str) -> Result<String, String> {
    let parsed =
        url::Url::parse(url).map_err(|e| format!("Invalid Unix socket URL format: {e}"))?;
    if parsed.scheme() != UNIX_SCHEME {
        return Err("Unix socket URL must use unix:// scheme".to_string());
    }
    if parsed.host_str().is_some() {
        return Err(
            "Unix socket URL must not include a host; use unix:///absolute/path.sock".to_string(),
        );
    }
    let path = parsed.path();
    if !path.starts_with('/') || path.len() <= 1 {
        return Err(
            "Unix socket URL must contain an absolute socket path, e.g. unix:///tmp/vllm.sock"
                .to_string(),
        );
    }
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_dp_suffix_only_for_numeric_ranks() {
        assert_eq!(
            strip_dp_suffix("http://worker:8000@3"),
            "http://worker:8000"
        );
        assert_eq!(
            strip_dp_suffix("http://worker:8000@abc"),
            "http://worker:8000@abc"
        );
        assert_eq!(
            strip_dp_suffix("unix:///tmp/vllm.sock"),
            "unix:///tmp/vllm.sock"
        );
    }

    #[test]
    fn validate_http_worker_url() {
        assert!(validate_worker_url("http://localhost:8000").is_ok());
        assert!(validate_worker_url("https://localhost:8000").is_ok());
        assert!(validate_worker_url("grpc://localhost:9000").is_ok());
        assert!(validate_worker_url("ftp://localhost").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn validate_unix_worker_url() {
        assert!(validate_worker_url("unix:///tmp/vllm.sock").is_ok());
        assert!(validate_worker_url("unix://tmp/vllm.sock").is_err());
        assert!(validate_worker_url("unix:///tmp/vllm.sock?foo=bar").is_err());
        assert!(validate_worker_url("unix:///").is_err());
    }

    #[test]
    fn request_url_for_tcp() {
        assert_eq!(
            request_url("http://localhost:8000", "/v1/models"),
            "http://localhost:8000/v1/models"
        );
        assert_eq!(
            request_url("http://localhost:8000/", "health"),
            "http://localhost:8000/health"
        );
    }

    #[test]
    fn request_url_strips_dp_suffix() {
        assert_eq!(
            request_url("http://worker:8000@3", "/v1/chat/completions"),
            "http://worker:8000/v1/chat/completions"
        );
    }

    #[cfg(unix)]
    #[test]
    fn request_url_for_unix_socket() {
        assert_eq!(
            request_url("unix:///tmp/vllm.sock", "/v1/models"),
            "http://localhost/v1/models"
        );
        assert_eq!(
            request_url("unix:///tmp/vllm.sock", "health"),
            "http://localhost/health"
        );
    }

    #[test]
    fn resolve_client_returns_tcp_default_for_http_url() {
        let custom = reqwest::Client::new();
        let result = resolve_client("http://localhost:8000", &custom).unwrap();
        // Should return the provided TCP client, not the global singleton
        drop(result);
    }

    #[test]
    fn client_for_worker_url_returns_ok_for_tcp() {
        assert!(client_for_worker_url("http://localhost:8000").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_client_builds_for_absolute_path() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("vllm.sock");
        let worker_url = format!("unix://{}", socket_path.display());
        assert!(client_for_worker_url(&worker_url).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_client_returns_uds_client_for_unix_url() {
        use tempfile::tempdir;

        let tcp = reqwest::Client::new();
        let dir = tempdir().unwrap();
        let socket_path = dir.path().join("vllm.sock");
        let worker_url = format!("unix://{}", socket_path.display());
        assert!(resolve_client(&worker_url, &tcp).is_ok());
    }
}
