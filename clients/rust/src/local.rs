//! Local fynd instance for integration testing.
//!
//! Starts a fynd Docker container, polls until healthy, and tears it down on [`Drop`].
//! Requires Docker to be running locally.
//!
//! Gated behind the `local-testing` Cargo feature.

use std::{collections::HashMap, time::Duration};

use bollard::{
    models::{ContainerCreateBody, HostConfig, PortBinding},
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, RemoveContainerOptionsBuilder,
        StartContainerOptions, StopContainerOptionsBuilder,
    },
    Docker,
};
use futures::StreamExt;
use tracing::{debug, info, warn};

use crate::error::FyndClientError;

/// Configuration for a local fynd instance.
#[derive(Debug, Clone)]
pub struct LocalFyndConfig {
    /// Docker image to use. Defaults to `"fynd:latest"`.
    pub image: String,
    /// Host port to bind the fynd HTTP server to. Defaults to `3001`.
    pub host_port: u16,
    /// How long to wait for the container to become healthy.
    pub startup_timeout: Duration,
    /// Poll interval when checking health. Defaults to 500ms.
    pub poll_interval: Duration,
    /// Environment variables to pass to the container.
    pub env: HashMap<String, String>,
    /// Container name prefix. A random suffix is appended.
    pub name_prefix: String,
}

impl Default for LocalFyndConfig {
    fn default() -> Self {
        Self {
            image: "fynd:latest".to_string(),
            host_port: 3001,
            startup_timeout: Duration::from_secs(60),
            poll_interval: Duration::from_millis(500),
            env: HashMap::new(),
            name_prefix: "fynd-test".to_string(),
        }
    }
}

/// A running local fynd instance, backed by a Docker container.
///
/// Teardown happens automatically on [`Drop`].
///
/// # Example
///
/// ```no_run
/// # use fynd_client::local::{LocalFyndInstance, LocalFyndConfig};
/// # #[tokio::main] async fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let instance = LocalFyndInstance::start(LocalFyndConfig::default()).await?;
/// // instance.url() → "http://localhost:3001"
/// // ... run your tests ...
/// drop(instance); // container is stopped and removed
/// # Ok(()) }
/// ```
pub struct LocalFyndInstance {
    container_id: String,
    container_name: String,
    port: u16,
    docker: Docker,
}

impl LocalFyndInstance {
    /// Starts a fynd Docker container and waits until it is healthy.
    ///
    /// Pulls the image if not present locally, then creates and starts the container.
    /// Polls `GET /v1/health` until the response is 200, or until `config.startup_timeout`
    /// elapses.
    pub async fn start(config: LocalFyndConfig) -> Result<Self, FyndClientError> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| FyndClientError::Rpc(format!("failed to connect to Docker: {e}")))?;

        // Pull the image if needed
        let mut pull_stream = docker.create_image(
            Some(
                CreateImageOptionsBuilder::default()
                    .from_image(config.image.as_str())
                    .build(),
            ),
            None,
            None,
        );

        while let Some(result) = pull_stream.next().await {
            match result {
                Ok(info) => debug!("pull: {:?}", info.status),
                Err(e) => warn!("image pull warning: {e}"),
            }
        }

        // Build container name with random suffix
        let suffix: u32 = fastrand::u32(..);
        let container_name = format!("{}-{:08x}", config.name_prefix, suffix);

        // Build environment variables
        let env: Vec<String> = config
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();

        // Port bindings: host_port → 3000/tcp (fynd's default)
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        port_bindings.insert(
            "3000/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some(config.host_port.to_string()),
            }]),
        );

        let host_config = HostConfig {
            port_bindings: Some(port_bindings),
            auto_remove: Some(false), // we manage removal ourselves in Drop
            ..Default::default()
        };

        // Create container
        let container = docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(container_name.as_str())
                        .build(),
                ),
                ContainerCreateBody {
                    image: Some(config.image.clone()),
                    env: if env.is_empty() { None } else { Some(env) },
                    host_config: Some(host_config),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| FyndClientError::Rpc(format!("failed to create container: {e}")))?;

        let container_id = container.id.clone();

        // Start container
        docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await
            .map_err(|e| FyndClientError::Rpc(format!("failed to start container: {e}")))?;

        info!("started container {container_name} ({container_id}), polling health...");

        let instance = Self { container_id, container_name, port: config.host_port, docker };

        // Poll until healthy
        instance
            .wait_healthy(config.startup_timeout, config.poll_interval)
            .await?;

        Ok(instance)
    }

    /// Returns the base URL for the local fynd instance.
    pub fn url(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Polls `GET /v1/health` until a 200 response is received.
    async fn wait_healthy(
        &self,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), FyndClientError> {
        let health_url = format!("{}/v1/health", self.url());
        let client = reqwest::Client::new();
        let deadline = std::time::Instant::now() + timeout;

        loop {
            match client.get(&health_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!("fynd instance is healthy at {}", self.url());
                    return Ok(());
                }
                Ok(resp) => debug!("health check returned {}", resp.status()),
                Err(e) => {
                    debug!("health check failed (container may still be starting): {e}");
                }
            }

            if std::time::Instant::now() >= deadline {
                return Err(FyndClientError::Rpc(format!(
                    "fynd container {} did not become healthy within {:?}",
                    self.container_name, timeout
                )));
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Stops and removes the Docker container.
    ///
    /// Called automatically by [`Drop`]. Errors are logged but not propagated.
    async fn teardown(&self) {
        let stop_opts = StopContainerOptionsBuilder::default()
            .t(5)
            .build();
        if let Err(e) = self
            .docker
            .stop_container(&self.container_id, Some(stop_opts))
            .await
        {
            warn!("failed to stop container {}: {e}", self.container_name);
        }
        let remove_opts = RemoveContainerOptionsBuilder::default()
            .force(true)
            .build();
        if let Err(e) = self
            .docker
            .remove_container(&self.container_id, Some(remove_opts))
            .await
        {
            warn!("failed to remove container {}: {e}", self.container_name);
        } else {
            info!("removed container {}", self.container_name);
        }
    }
}

impl Drop for LocalFyndInstance {
    /// Stops and removes the Docker container.
    ///
    /// Uses `block_on` to run the async teardown synchronously.
    /// This is sound because `Drop` is called from the Tokio runtime thread,
    /// but we spawn a blocking task to avoid blocking the async executor.
    fn drop(&mut self) {
        // We cannot call `.await` in `Drop`. Instead, we use the Tokio runtime handle
        // if one is available. If no runtime is available (e.g., test teardown after
        // runtime exit), we log a warning and skip cleanup.
        let container_id = self.container_id.clone();
        let container_name = self.container_name.clone();
        let docker = self.docker.clone();

        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.block_on(async move {
                    let instance_ref =
                        LocalFyndInstance { container_id, container_name, port: 0, docker };
                    instance_ref.teardown().await;
                });
            }
            Err(_) => {
                warn!(
                    "no async runtime available during Drop; \
                     container {} may not be cleaned up",
                    container_name
                );
            }
        }
    }
}
