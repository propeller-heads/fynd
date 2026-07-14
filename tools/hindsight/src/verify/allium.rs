//! Allium API client for the `verify` subcommand.
//!
//! A saved query (`query_id`) parameterized by `block_number` is run per block: kick off an async
//! run, poll until it succeeds, then fetch results. The API rate-limits aggressively, so requests
//! retry on HTTP 429 with a linear backoff.

use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::{json, Value};
use tokio::time::sleep;

const BASE_URL: &str = "https://api.allium.so/api/v1/explorer";
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const MAX_POLLS: u32 = 60;
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(10);
const MAX_RETRIES: u32 = 5;
const ROW_LIMIT: u64 = 10_000;

/// One row of Allium's `aggregator_trades` table — the ground truth for a single settled swap leg.
///
/// Every field is optional: Allium rows can carry nulls (e.g. an unresolved token address), and
/// one patchy row must not abort a whole verify run.
#[derive(serde::Deserialize, Debug, Clone)]
pub(crate) struct AlliumRow {
    pub(crate) project: Option<String>,
    pub(crate) token_sold_address: Option<String>,
    pub(crate) token_sold_amount: Option<f64>,
    pub(crate) token_bought_address: Option<String>,
    pub(crate) token_bought_amount: Option<f64>,
    pub(crate) transaction_hash: Option<String>,
}

/// Client for Allium's async Explorer API.
pub(crate) struct AlliumClient {
    http: reqwest::Client,
    api_key: String,
    query_id: String,
}

impl AlliumClient {
    pub(crate) fn new(api_key: String, query_id: String) -> Self {
        Self { http: reqwest::Client::new(), api_key, query_id }
    }

    /// Fetch every `aggregator_trades` row for a block.
    pub(crate) async fn fetch_block(&self, block_number: u64) -> anyhow::Result<Vec<AlliumRow>> {
        let run_id = self.start_run(block_number).await?;
        self.await_success(&run_id).await?;
        self.results(&run_id).await
    }

    async fn start_run(&self, block_number: u64) -> anyhow::Result<String> {
        let url = format!("{BASE_URL}/queries/{}/run-async", self.query_id);
        // The saved query takes block_number as a string parameter.
        let body = json!({
            "parameters": {"block_number": block_number.to_string()},
            "run_config": {"limit": ROW_LIMIT},
        });
        let value = self.post(&url, &body).await?;
        value
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("Allium run-async returned no run_id: {value}"))
    }

    async fn await_success(&self, run_id: &str) -> anyhow::Result<()> {
        let url = format!("{BASE_URL}/query-runs/{run_id}/status");
        for _ in 0..MAX_POLLS {
            match self.status(&url).await?.as_str() {
                "success" => return Ok(()),
                "failed" => bail!("Allium run {run_id} failed"),
                _ => sleep(POLL_INTERVAL).await,
            }
        }
        bail!("Allium run {run_id} did not finish within {MAX_POLLS} polls")
    }

    async fn status(&self, url: &str) -> anyhow::Result<String> {
        let value = self.get(url).await?;
        value
            .as_str()
            .or_else(|| {
                value
                    .get("status")
                    .and_then(Value::as_str)
            })
            .map(str::to_string)
            .with_context(|| format!("unexpected Allium status response: {value}"))
    }

    async fn results(&self, run_id: &str) -> anyhow::Result<Vec<AlliumRow>> {
        let url = format!("{BASE_URL}/query-runs/{run_id}/results");
        let value = self.post(&url, &json!({})).await?;
        let data = value
            .get("data")
            .with_context(|| format!("Allium results missing 'data': {value}"))?;
        serde_json::from_value(data.clone()).context("failed to parse Allium rows")
    }

    async fn get(&self, url: &str) -> anyhow::Result<Value> {
        self.send(|| self.http.get(url)).await
    }

    async fn post(&self, url: &str, body: &Value) -> anyhow::Result<Value> {
        self.send(|| self.http.post(url).json(body))
            .await
    }

    async fn send<F>(&self, build: F) -> anyhow::Result<Value>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        for attempt in 0..MAX_RETRIES {
            let response = build()
                .header("X-API-KEY", &self.api_key)
                .send()
                .await
                .context("Allium request failed")?;

            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                sleep(RATE_LIMIT_BACKOFF * (attempt + 1)).await;
                continue;
            }
            if !status.is_success() {
                let body = response
                    .text()
                    .await
                    .unwrap_or_default();
                bail!("Allium returned {status}: {body}");
            }

            return response
                .json::<Value>()
                .await
                .context("failed to decode Allium response");
        }
        bail!("Allium rate limit: exhausted {MAX_RETRIES} retries")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parses_bare_string() {
        // The API sometimes returns a bare JSON string "success" instead of {"status": "success"}.
        let value: Value = serde_json::from_str("\"success\"").unwrap();
        let result = value
            .as_str()
            .or_else(|| {
                value
                    .get("status")
                    .and_then(Value::as_str)
            })
            .map(str::to_string);
        assert_eq!(result, Some("success".to_string()));
    }

    #[test]
    fn status_parses_object_form() {
        // The API can also return {"status": "running"}.
        let value: Value = serde_json::from_str(r#"{"status": "running"}"#).unwrap();
        let result = value
            .as_str()
            .or_else(|| {
                value
                    .get("status")
                    .and_then(Value::as_str)
            })
            .map(str::to_string);
        assert_eq!(result, Some("running".to_string()));
    }

    #[test]
    fn status_returns_none_for_unexpected_shape() {
        let value: Value = serde_json::from_str(r#"{"other_key": "val"}"#).unwrap();
        let result = value
            .as_str()
            .or_else(|| {
                value
                    .get("status")
                    .and_then(Value::as_str)
            })
            .map(str::to_string);
        assert_eq!(result, None);
    }
}
