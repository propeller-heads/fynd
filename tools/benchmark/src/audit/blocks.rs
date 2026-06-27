//! Block-detection helpers: poll the Fynd health endpoint to sync the audit loop to fresh blocks.

use std::time::Duration;

use anyhow::{Context, Result};
use fynd_client::FyndClient;
use tokio::time::sleep;

/// Poll until `last_update_ms < 3 s`, indicating a fresh block just processed.
pub(crate) async fn wait_for_fresh_block(fynd: &FyndClient) -> Result<()> {
    loop {
        sleep(Duration::from_millis(500)).await;
        let health = fynd
            .health()
            .await
            .context("health check failed while waiting for block")?;
        if health.last_update_ms() < 3_000 {
            return Ok(());
        }
    }
}

/// Poll until `last_update_ms > 8 s` so the next `wait_for_fresh_block` catches a new block.
async fn wait_until_stale(fynd: &FyndClient) -> Result<()> {
    loop {
        sleep(Duration::from_secs(2)).await;
        let health = fynd
            .health()
            .await
            .context("health check failed while waiting for stale")?;
        if health.last_update_ms() > 8_000 {
            return Ok(());
        }
    }
}

/// Wait until stale, then skip `stride - 1` additional Ethereum block periods (~12 s each),
/// then sync to the next fresh block. With stride=1 this is identical to the original behaviour.
pub(crate) async fn wait_for_next_sample(fynd: &FyndClient, stride: usize) -> Result<()> {
    wait_until_stale(fynd).await?;
    if stride > 1 {
        sleep(Duration::from_secs(12 * (stride - 1) as u64)).await;
    }
    wait_for_fresh_block(fynd).await
}
