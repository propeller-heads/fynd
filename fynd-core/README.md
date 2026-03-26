# fynd-core

Pure solving logic for the [Fynd](https://fynd.xyz) DEX router.

Contains the route-finding algorithms, market-data pipeline, and on-chain encoder that power
Fynd. **No HTTP dependencies** — embed directly in any application or use the
[`fynd-rpc`](https://crates.io/crates/fynd-rpc) crate to expose it as an HTTP service.

For documentation, configuration guides, and API reference see **<https://docs.fynd.xyz/>**.

## Use cases

- **Standalone routing** — call `Solver::quote()` directly from your application.
- **Custom algorithms** — implement the `Algorithm` trait and plug in via `FyndBuilder::with_algorithm`.
- **HTTP server** — use `fynd-rpc`, which wraps this crate with Actix Web.

## Quick start

```rust,no_run
use fynd_core::{FyndBuilder, types::Order};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // See https://docs.fynd.xyz/get-started/quickstart for prerequisites.
    let solver = FyndBuilder::new(
        "ethereum".parse()?,
        "tycho-fynd-ethereum.propellerheads.xyz".into(),
        "https://reth-ethereum.ithaca.xyz/rpc".into(),
        vec!["uniswap_v3".into(), "uniswap_v2".into()],
        10.0, // minimum pool TVL (chain native token)
    )
    .build()?
    .wait_until_ready()
    .await?;

    // solver.quote(request).await?;
    Ok(())
}
```

See the [custom algorithm guide](https://docs.fynd.xyz/guides/custom-algorithm) to implement
your own routing strategy.
