# Fynd

A high-performance DeFi route-finding engine built on [Tycho](https://www.propellerheads.xyz/tycho). Finds optimal swap
routes across multiple DeFi protocols in real-time.

> [!CAUTION]
  > **Alpha Software — Unaudited Contracts**
  >
  > Fynd's smart contracts ([TychoRouter V3](https://docs.propellerheads.xyz/tycho/for-solvers/execution#security-and-audits), Vault, Executors) are still undergoing a security audit. Funds stored in the router (including vault deposits) may be lost. Use at your own
   discretion.

## Features

- **Multi-protocol routing** - Routes through your favorite on-chain liquidity protocol, like Uniswap, Balancer, Curve,
  RFQ protocols, or any other protocol supported
  by [Tycho](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols).
- **Real-time market data** - Tycho Stream keeps all liquidity states synchronized every block
- **Multi-algorithm competition** - Multiple solver pools run different algorithm configurations in parallel; the best
  result wins
- **Gas-aware ranking** - Solutions are ranked by net output after gas costs, not just raw output
- **Sub-100ms solves** - Dedicated OS threads for CPU-bound route finding, separate from the async I/O runtime
- **Production-ready** - Prometheus metrics, structured logging, health endpoints, graceful shutdown
- **Extensible** - Implement the `Algorithm` trait to add new routing strategies with zero framework changes
- **Modular** - Use just the core solving logic, or build a custom HTTP server with your own middleware

## Prerequisites

- Rust 1.92+
- A Tycho API key ([get one here](https://t.me/fynd_portal_bot))

## Quickstart

```bash
cargo install fynd
export TYCHO_API_KEY=your-api-key
export RUST_LOG=fynd=info
fynd serve
```

The solver starts on `http://localhost:3000`. Request a quote:

```bash
curl -X POST http://localhost:3000/v1/quote \
  -H "Content-Type: application/json" \
  -d '{
    "orders": [
      {
        "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        "amount": "1000000000000000000",
        "side": "sell",
        "sender": "0x0000000000000000000000000000000000000001"
      }
    ],
    "options": { "timeout_ms": 5000 }
  }'
```

### Run on a specific chain

You can run on any chain supported by Tycho (see [Tycho Hosted endpoints](https://docs.propellerheads.xyz/tycho/for-solvers/hosted-endpoints)):

```bash
export RPC_URL=<RPC_FOR_TARGET_CHAIN>
cargo run --release serve --chain base
```

See the [full quickstart](https://docs.fynd.xyz/get-started/quickstart) for Docker, build-from-source, and client SDK examples (Rust & TypeScript).

## Documentation

For API reference, configuration options, encoding, client fees, custom algorithms, architecture, and more, visit the full documentation at **[docs.fynd.xyz](https://docs.fynd.xyz/)**.

## Packages

Fynd is organized into three crates:

- **[`fynd`](https://crates.io/crates/fynd)** — Complete CLI application that runs an HTTP RPC server. Use this to run Fynd as a standalone service.
- **[`fynd-core`](https://crates.io/crates/fynd-core)** — Pure solving logic with no HTTP dependencies. Use this if you want to integrate Fynd's routing algorithms into your own application.
- **[`fynd-rpc`](https://crates.io/crates/fynd-rpc)** — HTTP RPC server builder with customizable middleware. Use this to build a custom HTTP server with your own configuration.

Client SDKs that handle quoting, token approvals, and swap execution end-to-end:

- **[`fynd-client`](https://crates.io/crates/fynd-client)** — Rust
- **[`@kayibal/fynd-client`](https://www.npmjs.com/package/@kayibal/fynd-client)** — TypeScript
