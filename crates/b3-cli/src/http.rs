//! Shared HTTP client builder for CLI subsystems.
//!
//! Centralizes client configuration (timeout, pool, user-agent) so every
//! subsystem uses consistent settings. Also provides a pre-computed Bearer
//! header to avoid allocating `format!("Bearer {key}")` on every request.

use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use std::time::Duration;

/// Default timeout for most HTTP requests.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Build an HTTP client with standard settings.
///
/// All CLI subsystems should use this instead of `Client::builder()` directly.
/// Subsystem-specific overrides (e.g. pusher's 10s timeout) pass a custom timeout.
pub fn build_client(timeout: Duration) -> reqwest::Result<Client> {
    Client::builder()
        .timeout(timeout)
        .pool_max_idle_per_host(2)
        .user_agent(concat!("b3/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Build an HTTP client with default timeout (30s).
pub fn default_client() -> reqwest::Result<Client> {
    build_client(DEFAULT_TIMEOUT)
}

/// Build an HTTP client for long-lived streaming responses (SSE).
///
/// Unlike `default_client()`, this has NO total response timeout.
/// reqwest's `timeout()` is a wall-clock limit on the entire response
/// (including streaming body), which kills SSE streams after 30s.
/// SSE idle detection is handled at the application level via `SSE_TIMEOUT`.
pub fn streaming_client() -> reqwest::Result<Client> {
    Client::builder()
        .pool_max_idle_per_host(2)
        .user_agent(concat!("b3/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Build an HTTP client with a pre-set Authorization header.
/// Every request made with this client includes the Bearer token automatically.
pub fn authed_client(api_key: &str, timeout: Duration) -> reqwest::Result<Client> {
    let mut headers = HeaderMap::new();
    let value = format!("Bearer {api_key}");
    // HeaderValue::from_str fails on non-visible-ASCII. API keys should be safe,
    // but we fall back to a client without default auth if parsing fails.
    if let Ok(hv) = HeaderValue::from_str(&value) {
        headers.insert(reqwest::header::AUTHORIZATION, hv);
    }
    Client::builder()
        .timeout(timeout)
        .pool_max_idle_per_host(2)
        .user_agent(concat!("b3/", env!("CARGO_PKG_VERSION")))
        .default_headers(headers)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_client_succeeds() {
        let client = build_client(Duration::from_secs(5));
        assert!(client.is_ok());
    }

    #[test]
    fn default_client_succeeds() {
        let client = default_client();
        assert!(client.is_ok());
    }

    #[test]
    fn authed_client_succeeds() {
        let client = authed_client("test-api-key-123", Duration::from_secs(10));
        assert!(client.is_ok());
    }

    #[test]
    fn streaming_client_succeeds() {
        let client = streaming_client();
        assert!(client.is_ok());
    }
}
