# fynd-rpc

HTTP RPC server for the [Fynd](https://fynd.xyz) DEX router.

Wraps [`fynd-core`](https://crates.io/crates/fynd-core) with Actix Web and exposes swap routing
as a REST service on `http://0.0.0.0:3000` by default.

For documentation, configuration guides, and API reference see **<https://docs.fynd.xyz/>**.

## Endpoints

| Endpoint | Description |
|---|---|
| `POST /v1/quote` | Request an optimal swap route |
| `GET /v1/health` | Data freshness and solver readiness |
| `GET /v1/info` | Static instance metadata (chain ID, contract addresses) |

## Quick start

```rust,no_run
use std::collections::HashMap;
use fynd_rpc::{builder::FyndRPCBuilder, config::WorkerPoolsConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // See https://docs.fynd.xyz/get-started/quickstart for prerequisites.
    let server = FyndRPCBuilder::new(
        "ethereum".parse()?,
        HashMap::new(),                          // pool configs
        "tycho-fynd-ethereum.propellerheads.xyz".into(),
        "https://reth-ethereum.ithaca.xyz/rpc".into(),
        vec!["uniswap_v3".into(), "uniswap_v2".into()],
    )
    .build()?;

    server.run().await?;
    Ok(())
}
```

See the [server configuration guide](https://docs.fynd.xyz/guides/server-configuration) for all
available options.
