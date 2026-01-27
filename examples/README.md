# Tycho Solver Server

Runs the Tycho Solver as an HTTP server.

## Usage

```bash
export TYCHO_API_KEY=<your-key>
cargo run --example solver --release -- \
  --rpc-url <RPC_URL> \
  --tycho-url <TYCHO_URL> \
  [OPTIONS]
```

The solver will:

1. Load market data from Tycho
2. Start the HTTP server
3. Wait for market data to be ready
4. Keep running until you press Ctrl+C

## Options

```
--rpc-url <RPC_URL>              RPC endpoint URL (required) [env: RPC_URL]
--tycho-url <TYCHO_URL>          Tycho indexer URL (required) [env: TYCHO_URL]
--chain <CHAIN>                  Blockchain network [env: CHAIN] [default: Ethereum]
--protocols <PROTOCOLS>          Comma-separated protocol list [env: PROTOCOLS] [default: uniswap_v2,uniswap_v3]
--http-port <PORT>               HTTP server port [env: HTTP_PORT] [default: 3000]
--worker-pools-config <FILE>     Worker pool configuration file [env: WORKER_POOLS_CONFIG] [default: worker_pools.toml]
-h, --help                       Print help
```

CLI flags take precedence over environment variables.

## Example

```bash
export TYCHO_API_KEY=<your-key>
cargo run --example solver --release -- \
  --rpc-url https://node-provider.com/v2/YOUR_KEY \
  --tycho-url tycho-dev.propellerheads.xyz \
  --protocols uniswap_v2,uniswap_v3
```

Once running, the solver accepts HTTP requests at `http://localhost:3000/v1/solve`.

## Testing the Solver

You can test the solver with a curl request:

```bash
curl -X POST http://localhost:3000/v1/solve \
  -H "Content-Type: application/json" \
  -d '{
    "orders": [
      {
        "id": "",
        "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        "amount": "1000000000000000000",
        "side": "sell",
        "sender": "0x0000000000000000000000000000000000000001",
        "receiver": null
      }
    ],
    "options": {
      "timeout_ms": 10000,
      "min_responses": null,
      "max_gas": null
    }
  }'
```
