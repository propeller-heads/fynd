//! Stratified sampler: caps trades per (token_in, token_out) pair and takes N total.

use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;

use crate::types::AuditTrade;

#[derive(Deserialize)]
struct FileRequest {
    orders: Vec<FileOrder>,
}

#[derive(Deserialize)]
struct FileOrder {
    token_in: String,
    token_out: String,
    amount: String,
    #[serde(default = "default_sender")]
    sender: String,
}

fn default_sender() -> String {
    // Matches `tools/benchmark/src/requests.rs::SENDER`. The sampler uses this
    // string only as an input to quoting; the simulator overrides the sender
    // balance regardless, so the value never needs funded-on-chain status.
    "0x0000000000000000000000000000000000000001".to_string()
}

/// Parse the aggregator JSON dataset into `AuditTrade`s (one per file entry).
pub fn parse_dataset(path: &Path) -> anyhow::Result<Vec<AuditTrade>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read dataset at {}: {e}", path.display()))?;
    parse_dataset_str(&content)
}

fn parse_dataset_str(json: &str) -> anyhow::Result<Vec<AuditTrade>> {
    let templates: Vec<FileRequest> = serde_json::from_str(json)?;
    let mut out = Vec::with_capacity(templates.len());
    for (i, tmpl) in templates.iter().enumerate() {
        let order = tmpl
            .orders
            .first()
            .ok_or_else(|| anyhow::anyhow!("dataset entry {i} has no orders"))?;
        out.push(AuditTrade {
            token_in: order.token_in.to_lowercase(),
            token_out: order.token_out.to_lowercase(),
            amount_in: order.amount.clone(),
            sender: order.sender.to_lowercase(),
        });
    }
    Ok(out)
}

/// Stratified sample: cap each (token_in, token_out) pair to `max_per_pair`
/// entries, then deterministically shuffle and take the first `n`.
///
/// Returns an error if fewer than `n` trades survive the cap.
pub fn stratified_sample(
    trades: &[AuditTrade],
    n: usize,
    max_per_pair: usize,
    seed: u64,
) -> anyhow::Result<Vec<AuditTrade>> {
    // BTreeMap for deterministic iteration across pairs.
    let mut by_pair: BTreeMap<(String, String), Vec<AuditTrade>> = BTreeMap::new();
    for t in trades {
        let key = (t.token_in.clone(), t.token_out.clone());
        by_pair
            .entry(key)
            .or_default()
            .push(t.clone());
    }

    let mut rng = fastrand::Rng::with_seed(seed);
    let mut capped: Vec<AuditTrade> = Vec::new();
    for (_, mut group) in by_pair {
        rng.shuffle(&mut group);
        group.truncate(max_per_pair);
        capped.extend(group);
    }

    if capped.len() < n {
        anyhow::bail!(
            "only {} trades survived the cap of {} per pair; need {}",
            capped.len(),
            max_per_pair,
            n
        );
    }

    rng.shuffle(&mut capped);
    capped.truncate(n);
    Ok(capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(token_in: &str, token_out: &str, amount: &str) -> AuditTrade {
        AuditTrade {
            token_in: token_in.to_lowercase(),
            token_out: token_out.to_lowercase(),
            amount_in: amount.to_string(),
            sender: "0x0000000000000000000000000000000000000001".to_string(),
        }
    }

    #[test]
    fn caps_per_pair_strictly() {
        let mut trades = Vec::new();
        // 30 WETH->USDC, 3 DAI->USDC
        for i in 0..30 {
            trades.push(trade("0xWETH", "0xUSDC", &i.to_string()));
        }
        for i in 0..3 {
            trades.push(trade("0xDAI", "0xUSDC", &i.to_string()));
        }

        let sampled = stratified_sample(&trades, 8, 5, 42).unwrap();

        let mut by_pair = std::collections::HashMap::<(String, String), usize>::new();
        for t in &sampled {
            *by_pair
                .entry((t.token_in.clone(), t.token_out.clone()))
                .or_default() += 1;
        }
        for count in by_pair.values() {
            assert!(*count <= 5, "pair exceeded cap");
        }
        assert_eq!(sampled.len(), 8);
    }

    #[test]
    fn deterministic_seed() {
        let mut trades = Vec::new();
        for i in 0..50 {
            trades.push(trade("0xWETH", "0xUSDC", &i.to_string()));
        }
        let a = stratified_sample(&trades, 4, 5, 42).unwrap();
        let b = stratified_sample(&trades, 4, 5, 42).unwrap();
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.amount_in, y.amount_in);
        }
    }

    #[test]
    fn errors_when_too_few() {
        let trades: Vec<AuditTrade> = (0..3)
            .map(|i| trade("0xWETH", "0xUSDC", &i.to_string()))
            .collect();
        let err = stratified_sample(&trades, 10, 5, 42).unwrap_err();
        assert!(err.to_string().contains("only 3"));
    }

    #[test]
    fn parse_dataset_str_lowercases_addresses() {
        let json = r#"[{"orders":[{"token_in":"0xAbC","token_out":"0xDEF","amount":"1"}]}]"#;
        let trades = parse_dataset_str(json).unwrap();
        assert_eq!(trades[0].token_in, "0xabc");
        assert_eq!(trades[0].token_out, "0xdef");
    }
}
