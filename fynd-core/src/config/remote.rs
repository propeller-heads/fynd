//! Remote config layer: tuned values pulled from S3 at startup.
//!
//! # Layering
//!
//! The remote payload is a [`PartialConfig`] in the same schema as the local config file,
//! published per chain. It applies **right above the embedded default**:
//!
//! ```text
//! CLI flags  >  local config file (fynd.toml)  >  remote config (S3)  >  embedded default
//! ```
//!
//! Operators' local settings therefore always beat remotely tuned values.
//!
//! # Safety properties
//!
//! The remote layer must never take a solver down or degrade it silently:
//!
//! 1. **Never blocks or fails startup.** The caller bounds the whole fetch (including retries) with
//!    [`tokio::time::timeout`]; on any error or timeout it logs a warning and resolves without the
//!    remote layer — the embedded default covers everything.
//! 2. **Bounded, transient-only retries.** Connection-level failures and 5xx responses are retried
//!    up to a few times with linear backoff; 4xx responses and parse failures are deterministic and
//!    fail immediately.
//! 3. **Size-capped download.** The response body is size-capped before parsing, so a misconfigured
//!    URL cannot exhaust memory.
//! 4. **No panic paths.** Every failure — client construction, transport, decoding, parsing,
//!    payload checks — returns an error; nothing in this module unwraps, expects, or panics. The
//!    caller downgrades every error to a warning and resolves without the remote layer.
//! 5. **Forward-compatible parsing, backward-safe application.** Unknown fields in the payload are
//!    ignored (a payload written for a newer binary still applies the fields this binary knows),
//!    but a payload whose pools reference an algorithm this binary does not implement is rejected
//!    wholesale — an outdated binary falls back to its embedded defaults instead of failing at
//!    solver build time.
//! 6. **No validation bypass.** The remote layer is folded like any other; the final resolved
//!    config still passes [`Config::validate`](super::Config::validate) (and
//!    `FyndBuilder::apply_config` validates again at the engine boundary). A remote payload that
//!    would produce an invalid config fails resolution the same way a bad local file does —
//!    visibly, at startup.
//!
//! # Deliberate non-features (open for review)
//!
//! - **No on-disk cache / TTL.** The fetch happens once at startup and the embedded default covers
//!   the offline case, so a cache only adds staleness questions.
//! - **No schema version field.** Forward compatibility comes from permissive parsing; an
//!   incompatible future schema would fail parsing and fall back safely.
//! - **No payload signing.** Integrity currently rests on TLS + bucket ACLs. If the bucket becomes
//!   a broader attack surface, a detached signature (e.g. sidecar `latest.toml.sig` verified
//!   against an embedded public key) can be added without changing this API.

use std::time::Duration;

use tycho_simulation::tycho_common::models::Chain;

use super::{Config, ConfigError, PartialConfig};
use crate::worker_pool::registry::AVAILABLE_ALGORITHMS;

/// URL template behind [`default_remote_config_url`]; `{chain}` is substituted with the
/// lowercase chain name.
const DEFAULT_REMOTE_URL_TEMPLATE: &str =
    "https://s3.eu-central-1.amazonaws.com/repo.propellerheads-propellerheads/fynd/presets/{chain}/latest.toml";

/// Number of attempts for transient network failures before giving up.
const FETCH_ATTEMPTS: u32 = 3;

/// Base delay between retries; grows linearly with the attempt number.
const RETRY_BACKOFF: Duration = Duration::from_millis(150);

/// Maximum accepted response body size. A config payload is a few KiB; anything near this
/// limit is misconfiguration or abuse.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

/// Returns the default remote config URL for `chain` (the PropellerHeads-maintained S3
/// object with the latest tuned values).
pub fn default_remote_config_url(chain: Chain) -> String {
    DEFAULT_REMOTE_URL_TEMPLATE.replace("{chain}", &chain.to_string())
}

/// Errors from fetching the remote config.
///
/// All variants are recoverable: callers log a warning and resolve without the remote
/// layer.
#[derive(Debug, thiserror::Error)]
pub enum RemoteConfigError {
    /// Network-level failure: DNS, connect, or non-2xx status (after retries).
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    /// The response body exceeded the size limit.
    #[error("response exceeds the {limit_bytes} byte limit")]
    TooLarge {
        /// The enforced limit.
        limit_bytes: u64,
    },
    /// The server answered with a non-success status outside plain 4xx/5xx — typically a
    /// redirect reqwest cannot follow (S3 omits the Location header when the URL targets
    /// the wrong region/endpoint for the bucket).
    #[error(
        "unexpected status {status} (redirect? check that the URL matches the bucket's region)"
    )]
    UnexpectedStatus {
        /// The status the server answered with.
        status: reqwest::StatusCode,
    },
    /// The response body is not valid UTF-8.
    #[error("response body is not valid UTF-8")]
    InvalidUtf8,
    /// The payload failed to parse as a [`PartialConfig`].
    #[error(transparent)]
    Parse(#[from] ConfigError),
    /// A pool in the payload references an algorithm this binary does not know — typically
    /// a payload written for a newer version. Falling back to the embedded defaults keeps
    /// outdated binaries running instead of failing startup at solver build time.
    #[error(
        "remote config pool '{pool}' uses unknown algorithm '{algorithm}' \
         (this binary supports: {available})"
    )]
    UnknownAlgorithm {
        /// The pool naming the unknown algorithm.
        pool: String,
        /// The unknown algorithm name.
        algorithm: String,
        /// Comma-separated algorithm names this binary supports.
        available: String,
    },
}

impl Config {
    /// Fetches the remote config layer from `url` and applies it on top of this config.
    ///
    /// The remote layer for the `apply` chain:
    ///
    /// ```ignore
    /// let config = embedded_default()
    ///     .clone()
    ///     .apply_remote(&default_remote_config_url(chain), timeout)
    ///     .await
    ///     .apply(&local_file)
    ///     .apply(&overrides);
    /// ```
    ///
    /// For embedded + remote with the default URL in one call, use
    /// [`get_default`](super::get_default).
    ///
    /// `timeout` bounds the whole fetch including retries. This method can never fail or
    /// panic (safety properties 1 and 4): on any fetch error or timeout it logs a warning
    /// and returns `self` unchanged.
    pub async fn apply_remote(self, url: &str, timeout: Duration) -> Self {
        match tokio::time::timeout(timeout, fetch_remote_config(url)).await {
            Ok(Ok(partial)) => {
                tracing::info!(url, "fetched remote config");
                self.apply(&partial)
            }
            Ok(Err(e)) => {
                tracing::warn!(url, error = %e, "remote config fetch failed; continuing without it");
                self
            }
            Err(_elapsed) => {
                tracing::warn!(
                    url,
                    timeout_ms = timeout.as_millis() as u64,
                    "remote config fetch timed out; continuing without it"
                );
                self
            }
        }
    }
}

/// Fetches the remote config from `url` and returns it as a [`PartialConfig`] layer.
///
/// Prefer [`Config::apply_remote`] unless you need the raw layer or custom error handling.
///
/// Transient network failures are retried a few times with a short backoff. There is no
/// internal deadline: bound the call with [`tokio::time::timeout`] — the timeout is caller
/// policy. Build the default `url` with [`default_remote_config_url`].
///
/// # Errors
///
/// Returns [`RemoteConfigError`] on any transport, size, parse, or payload-safety problem.
/// Treat every error as non-fatal: warn and resolve without the remote layer.
pub async fn fetch_remote_config(url: &str) -> Result<PartialConfig, RemoteConfigError> {
    let mut attempt = 1;
    let body = loop {
        match http_get_capped(url).await {
            Ok(body) => break body,
            Err(e) if attempt < FETCH_ATTEMPTS && is_retryable(&e) => {
                tracing::debug!(url, attempt, error = %e, "remote config fetch failed; retrying");
                tokio::time::sleep(RETRY_BACKOFF * attempt).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    };

    let partial = PartialConfig::from_toml_str(&body, url)?;
    check_payload(&partial)?;
    Ok(partial)
}

/// One GET attempt with the response body capped at [`MAX_RESPONSE_BYTES`]
/// (safety property 3).
async fn http_get_capped(url: &str) -> Result<String, RemoteConfigError> {
    // The `Client::new()` shortcut panics if the TLS backend fails to initialize; the
    // builder reports it as an error instead (safety property 4).
    let client = reqwest::Client::builder().build()?;
    // `error_for_status` only covers 4xx/5xx; catch everything else non-2xx (e.g. an
    // unfollowable 301 from S3, which omits the Location header on region mismatches)
    // explicitly instead of handing an error document to the parser.
    let mut response = client
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    if !response.status().is_success() {
        return Err(RemoteConfigError::UnexpectedStatus { status: response.status() });
    }

    if let Some(length) = response.content_length() {
        if length > MAX_RESPONSE_BYTES {
            return Err(RemoteConfigError::TooLarge { limit_bytes: MAX_RESPONSE_BYTES });
        }
    }
    // Content-Length can be absent or lie; enforce the cap on the actual stream too.
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if (body.len() + chunk.len()) as u64 > MAX_RESPONSE_BYTES {
            return Err(RemoteConfigError::TooLarge { limit_bytes: MAX_RESPONSE_BYTES });
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| RemoteConfigError::InvalidUtf8)
}

/// True for failures worth retrying: connection-level errors and 5xx responses
/// (safety property 2). 4xx responses and oversized bodies are deterministic and fail
/// immediately.
fn is_retryable(error: &RemoteConfigError) -> bool {
    match error {
        RemoteConfigError::Request(e) => match e.status() {
            Some(status) => status.is_server_error(),
            None => true,
        },
        RemoteConfigError::TooLarge { .. } |
        RemoteConfigError::UnexpectedStatus { .. } |
        RemoteConfigError::InvalidUtf8 |
        RemoteConfigError::Parse(_) |
        RemoteConfigError::UnknownAlgorithm { .. } => false,
    }
}

/// Rejects payloads whose pools reference algorithms this binary does not implement, so
/// they fail here (recoverable, falls back to embedded) rather than at solver build time
/// (fatal) — safety property 5.
fn check_payload(partial: &PartialConfig) -> Result<(), RemoteConfigError> {
    let Some(pools) = &partial.pools else {
        return Ok(());
    };
    for (pool_name, pool) in pools {
        if !AVAILABLE_ALGORITHMS.contains(&pool.algorithm()) {
            return Err(RemoteConfigError::UnknownAlgorithm {
                pool: pool_name.clone(),
                algorithm: pool.algorithm().to_string(),
                available: AVAILABLE_ALGORITHMS.join(", "),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal HTTP/1.1 server answering one canned response per expected request, in
    /// order. Returns the URL to fetch.
    async fn spawn_server(responses: Vec<(u16, Vec<u8>)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind failed");
        let addr = listener
            .local_addr()
            .expect("no local addr");
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .expect("accept failed");
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await;
                let head = format!(
                    "HTTP/1.1 {status} TEST\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });
        format!("http://{addr}/latest.toml")
    }

    #[tokio::test]
    async fn test_fetch_applies_valid_payload() {
        let url = spawn_server(vec![(200, b"worker_router_timeout_ms = 123".to_vec())]).await;
        let partial = fetch_remote_config(&url)
            .await
            .expect("fetch errored");
        assert_eq!(partial.worker_router_timeout_ms, Some(123));
        assert_eq!(partial.min_token_quality, None);
    }

    #[tokio::test]
    async fn test_fetch_retries_transient_5xx() {
        let url = spawn_server(vec![
            (500, b"".to_vec()),
            (503, b"".to_vec()),
            (200, b"min_token_quality = 90".to_vec()),
        ])
        .await;
        let partial = fetch_remote_config(&url)
            .await
            .expect("fetch errored");
        assert_eq!(partial.min_token_quality, Some(90));
    }

    #[tokio::test]
    async fn test_fetch_does_not_retry_4xx() {
        // A single 404 response; a retry would hang on the closed listener, so completing
        // with an error proves no retry happened.
        let url = spawn_server(vec![(404, b"".to_vec())]).await;
        let error = fetch_remote_config(&url)
            .await
            .unwrap_err();
        assert!(matches!(error, RemoteConfigError::Request(_)));
    }

    #[tokio::test]
    async fn test_fetch_rejects_unfollowable_redirect() {
        // S3 answers 301 without a Location header when the URL targets the wrong
        // region; the XML error body must not reach the parser.
        let url = spawn_server(vec![(301, b"<?xml version=\"1.0\"?><Error/>".to_vec())]).await;
        let error = fetch_remote_config(&url)
            .await
            .unwrap_err();
        assert!(matches!(error, RemoteConfigError::UnexpectedStatus { .. }));
        assert!(error.to_string().contains("301"));
    }

    #[tokio::test]
    async fn test_fetch_rejects_oversized_body() {
        let huge = vec![b'#'; (MAX_RESPONSE_BYTES + 1) as usize];
        let url = spawn_server(vec![(200, huge)]).await;
        let error = fetch_remote_config(&url)
            .await
            .unwrap_err();
        assert!(matches!(error, RemoteConfigError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn test_fetch_rejects_garbage_payload() {
        let url = spawn_server(vec![(200, b"not [ valid { toml".to_vec())]).await;
        let error = fetch_remote_config(&url)
            .await
            .unwrap_err();
        assert!(matches!(error, RemoteConfigError::Parse(_)));
    }

    #[tokio::test]
    async fn test_fetch_rejects_unknown_algorithm_payload() {
        let url =
            spawn_server(vec![(200, b"[pools.p]\nalgorithm = \"quantum_router\"".to_vec())]).await;
        let error = fetch_remote_config(&url)
            .await
            .unwrap_err();
        assert!(matches!(error, RemoteConfigError::UnknownAlgorithm { .. }));
    }

    #[tokio::test]
    async fn test_apply_remote_falls_back_on_fetch_failure() {
        // Port 9 (discard) refuses connections; the config must come through unchanged,
        // never a panic or an error.
        let base = super::super::embedded_default().clone();
        let config = base
            .clone()
            .apply_remote("http://127.0.0.1:9/latest.toml", Duration::from_millis(200))
            .await;
        assert_eq!(config, base);
    }

    #[test]
    fn test_default_remote_config_url() {
        assert_eq!(
            default_remote_config_url(Chain::Ethereum),
            "https://s3.eu-central-1.amazonaws.com/repo.propellerheads-propellerheads/fynd/presets/ethereum/latest.toml"
        );
        assert_eq!(
            default_remote_config_url(Chain::Base),
            "https://s3.eu-central-1.amazonaws.com/repo.propellerheads-propellerheads/fynd/presets/base/latest.toml"
        );
    }

    #[test]
    fn test_check_payload_rejects_unknown_algorithm() {
        let known = PartialConfig::from_toml_str("[pools.p]\nalgorithm = \"bellman_ford\"", "test")
            .expect("parse errored");
        assert!(check_payload(&known).is_ok());

        // A payload written for a newer binary must be recoverable, not fail startup.
        let unknown =
            PartialConfig::from_toml_str("[pools.p]\nalgorithm = \"quantum_router\"", "test")
                .expect("parse errored");
        assert!(matches!(check_payload(&unknown), Err(RemoteConfigError::UnknownAlgorithm { .. })));

        assert!(check_payload(&PartialConfig::default()).is_ok());
    }
}
