//! Integration tests for LocalFyndInstance.
//!
//! These tests require Docker and a local fynd image (`fynd:latest`).
//! They are `#[ignore]`-gated so they don't run in PR CI.
//! Run explicitly with:
//!   cargo test -p fynd-client --features local-testing -- --ignored

#[cfg(feature = "local-testing")]
mod tests {
    use fynd_client::local::{LocalFyndConfig, LocalFyndInstance};

    #[tokio::test]
    #[ignore = "requires Docker and fynd:latest image"]
    async fn test_local_fynd_starts_and_is_healthy() {
        let config = LocalFyndConfig { host_port: 13001, ..Default::default() };

        let instance = LocalFyndInstance::start(config)
            .await
            .expect("should start fynd instance");

        // Verify health endpoint is reachable
        let response = reqwest::get(format!("{}/v1/health", instance.url()))
            .await
            .expect("health request should succeed");

        assert!(response.status().is_success(), "health endpoint should return 200");

        // Drop causes teardown
        drop(instance);
    }

    #[tokio::test]
    #[ignore = "requires Docker and fynd:latest image"]
    async fn test_local_fynd_url() {
        let config = LocalFyndConfig { host_port: 13002, ..Default::default() };

        let instance = LocalFyndInstance::start(config)
            .await
            .expect("should start fynd instance");

        assert_eq!(instance.url(), "http://localhost:13002");
    }
}
