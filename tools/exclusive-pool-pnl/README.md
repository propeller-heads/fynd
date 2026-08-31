# exclusive-pool-pnl

Reports LP fee revenue and markout for the Fynd exclusive Ekubo V3 pool on Ethereum mainnet.

**PnL** is profit and loss: what the pool's liquidity providers gained or lost. This tool states it
in USDC, per swap and in total. A negative PnL means the LPs ended a swap worse off than if they
had held their tokens and traded at the reference price instead.

The pool is hardcoded in `src/pool.rs`: ETH/USDC behind the `SignedExclusiveSwap` extension at
`0x55b703eed01b35641963da2fb2e14885993605a3`. Its `PoolKey` sets `fee = 0`, so every unit of LP
revenue comes from the extension's per-swap fee.

## Usage

```bash
export RPC_URL=<archive node>
cargo run --release -p exclusive-pool-pnl
```

Useful flags:

| Flag | Default | Purpose |
|---|---|---|
| `--from-block` | extension deploy block | First block to scan |
| `--to-block` | chain head | Last block to scan |
| `--chunk` | `1000` | Block span per `eth_getLogs`; most nodes cap here |
| `--concurrency` | `8` | Maximum in-flight RPC requests |
| `--retries` | `5` | Attempts per request; managed nodes throttle under concurrency |
| `--markout-secs` | `0,300,3600` | Markout horizons, comma separated |
| `--no-prices` | off | Skip the price download and report fees only |
| `--json <path>` | none | Write every swap and total as JSON |

A full scan is around 160 `eth_getLogs` calls and finishes in well under a minute.

## What it measures

For one swap, with `p` the reference price in USDC per ETH:

```
adverse_selection = delta0·p + delta1        the raw curve trade, valued at p
fee_revenue       = lp_fee0·p + lp_fee1      what the pool kept for its LPs
lp_pnl            = adverse_selection + fee_revenue   the LPs' profit and loss
```

`delta0` and `delta1` are the pool-side deltas from Ekubo's swap event, so `adverse_selection` is
zero when the curve trades exactly at the reference price and negative when the pool traded worse
than the market.

### This is markout, not LVR

LVR is the loss an LP takes to traders who swap *because* the pool is mispriced. Nobody can
arbitrage this pool — every swap carries a controller signature — so classical LVR is close to
zero here by construction. What the report measures instead is markout on the flow Fynd routes in,
which answers the question that does apply: is that flow benign, or is Fynd picking off its own
LPs?

### Fees are measured, not modelled

Fee amounts come from Ekubo's `FeesAccumulated` events rather than from recomputing the signed
rate. Ekubo credits an accrued fee on the *next* interaction with the pool, so `pnl::attribute_fees`
shifts each credit back onto the swap that earned it. Two consequences show up in the output:

- The newest swap's fee is marked pending until something else touches the pool.
- A swap whose fee never reached the pool reports zero LP revenue even though the taker paid it.
  Early swaps on this pool behave that way.

The `fee bps` column is separate: it is the rate the controller signed into `user_data`, decoded
from calldata, and shows what the taker was charged whether or not LPs received it.

### Reference prices

`fynd-core`'s `price_guard` providers are not used. They stream or poll the current price to
validate a live quote and keep no history, whereas a markout needs the price at a past timestamp.
This tool pulls one-minute klines from Binance's public REST endpoint instead. No API key needed.

A horizon that has not elapsed yet for every swap prices less than the full volume; those swaps
drop out of that row rather than reusing a stale price, and the totals block says so.

## Caveats

- Volume on this pool is small. Treat single-horizon numbers as anecdote, not as a measured edge.
- Binance ETHUSDC carries its own basis against on-chain ETH/USDC. Differences below roughly 30 bps
  are inside that noise.
- Long horizons on a one-directional day mostly measure inventory drift, not adverse selection.
