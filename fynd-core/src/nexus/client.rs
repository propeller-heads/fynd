//! The pod's connection to a `fynd-nexus` service.

use reqwest::Url;

use super::price_levels::{PriceLevelsAssembler, PriceLevelsFeed};

/// Errors when creating a [`NexusClient`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NexusError {
    /// The service URL is not an HTTP URL.
    #[error(
        "nexus URL '{url}' must be an http:// or https:// URL, e.g. \
         http://fynd-nexus:3000"
    )]
    InvalidUrl {
        /// The service URL supplied by the caller.
        url: String,
    },
    /// The HTTP client could not be built.
    #[error("cannot build the nexus HTTP client: {0}")]
    HttpClient(String),
}

/// Connects a pod to a `fynd-nexus` service.
#[derive(Debug, Clone)]
pub struct NexusClient {
    connection_url: String,
    http: reqwest::Client,
}

impl NexusClient {
    /// A client for the service at `connection_url`, such as `http://fynd-nexus:3000`.
    ///
    /// The service serves one chain; the caller points `connection_url` at the one of the pod's
    /// chain.
    ///
    /// # Errors
    ///
    /// [`NexusError::InvalidUrl`] when `connection_url` is not an http:// or https:// URL, and
    /// [`NexusError::HttpClient`] when the HTTP client cannot be built.
    pub fn new(connection_url: &str) -> Result<Self, NexusError> {
        let valid = Url::parse(connection_url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host());
        if !valid {
            return Err(NexusError::InvalidUrl { url: connection_url.to_string() });
        }
        let connection_url = connection_url.trim_end_matches('/');
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| NexusError::HttpClient(e.to_string()))?;
        Ok(Self { connection_url: connection_url.to_string(), http })
    }

    /// A feed of one venue's price levels, built by `assembler` from the prices the service serves.
    pub fn price_levels_feed(&self, assembler: PriceLevelsAssembler) -> PriceLevelsFeed {
        PriceLevelsFeed::new(&self.connection_url, self.http.clone(), assembler)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::http("http://fynd-nexus:3000")]
    #[case::https("https://shared.example.com")]
    #[case::trailing_slash("http://fynd-nexus:3000/")]
    fn test_new_http_url(#[case] url: &str) {
        assert!(NexusClient::new(url).is_ok());
    }

    #[rstest]
    #[case::websocket("ws://shared:3000")]
    #[case::no_scheme("shared:3000")]
    #[case::empty("")]
    #[case::no_host("http://")]
    fn test_new_invalid_url(#[case] url: &str) {
        let error = NexusClient::new(url).unwrap_err();

        assert!(matches!(error, NexusError::InvalidUrl { .. }), "got {error}");
        assert!(
            error
                .to_string()
                .contains("http:// or https://"),
            "got {error}"
        );
    }
}
