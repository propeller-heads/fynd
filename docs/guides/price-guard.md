---
icon: shield-check
---

# Price Guard

Fynd quotes can carry a concrete `min_amount_out` that commits the user to a trade. A mispriced
quote — caused by stale pool data, a bug, or manipulated liquidity — can lock the user into an
unfavorable execution. The price guard catches these before they reach the caller.

It sits between solving and encoding in the order pipeline, querying multiple independent price
oracles (Hyperliquid and Binance by default) concurrently and rejecting any solution whose
`amount_out` falls outside the tolerance interval. When validation fails, the quote's status is set
to `price_check_failed`; other orders in the same batch are unaffected.

## Configuration

The server controls only whether the guard is on or off. All other parameters — tolerance
thresholds and fallback behavior — are set per-request by the client. Omitted fields fall back to
struct defaults.

### Server-side

The guard is disabled by default. Enable it with `--enable-price-guard`:

```bash
fynd --enable-price-guard
```

When enabled, price providers (Hyperliquid, Binance) are started in the background so their
caches stay warm. Validation only runs for requests where the client sets `enabled: true` in
`encoding_options.price_guard`. When disabled, no providers are started and requests that set
`enabled: true` return an error.

### Per-request

Clients configure tolerance and fallback behavior through `encoding_options.price_guard`
(see [encoding options](encoding-options.md)). Omitted fields use the defaults shown below.

| Field                            | Type      | Default | Description                                                                                                                   |
|----------------------------------|-----------|---------|-------------------------------------------------------------------------------------------------------------------------------|
| `enabled`                        | `boolean` | `false` | Set to `true` to run price guard validation for this request. Requires the server to have `--enable-price-guard`.             |
| `lower_tolerance_bps`            | `integer` | `300`   | Max allowed deviation in basis points when the quote's `amount_out` is below the provider's expected amount out.              |
| `upper_tolerance_bps`            | `integer` | `10000` | Max allowed deviation in basis points when the quote's `amount_out` is above the provider's expected amount out.              |
| `fail_on_provider_error`         | `boolean` | `false` | See [fallback behavior](#fallback-behavior).                                                                                  |
| `fail_on_token_price_not_found`  | `boolean` | `false` | See [fallback behavior](#fallback-behavior).                                                                                  |

```bash
fynd --enable-price-guard
```

**Development** — leave the guard disabled on the server. No providers are started and no resources
are used.

```bash
fynd  # no --enable-price-guard
```

## Tolerance

The quote's `amount_out` is compared to each provider's expected amount out and the check
short-circuits as soon as one provider validates within tolerance — the remaining providers are
not consulted.

For both directions, deviation is computed the same way:

```
deviation_bps = abs(expected - actual) * 10000 / expected
```

- `amount_out < expected` → reject if deviation exceeds `lower_tolerance_bps`
- `amount_out >= expected` → reject if deviation exceeds `upper_tolerance_bps`

The lower bound is stricter by default (`300` bps = 3%) to catch under-delivery — the user getting
less than expected. The upper bound is looser (`10000` bps = 100%, allowing `amount_out` up to
twice the expected) to catch suspicious over-delivery that may indicate a pricing bug; lower it for
stricter checks.

## Fallback behavior

The fallback flags apply only when no provider returned a price — every response was either an
infrastructure error or `price_not_found`. If any provider returned a price, the quote is judged
purely on tolerance regardless of these flags: in-tolerance passes, out-of-tolerance rejects.

- **`fail_on_provider_error`** — applies when all providers failed with an infrastructure error
  (network issue, API down, rate-limited). `false` (default) lets the quote pass; `true` rejects it.
- **`fail_on_token_price_not_found`** — applies when every provider was reached but none list the
  token. `false` (default) lets the quote pass; `true` rejects it.

When responses mix `price_not_found` with infrastructure errors, the token might be listed on one
of the unreachable providers, so the guard applies `fail_on_provider_error` rather than
`fail_on_token_price_not_found`.

## Symbol collisions and long-tail tokens

Price providers (Binance, Hyperliquid) identify tokens by their trading symbol — e.g. "ETH",
"LINK", "PEPE". On-chain, symbols are not unique: any token can declare itself "PEPE", and
multiple unrelated tokens on the same chain may share a symbol. The guard resolves tokens by
matching the on-chain symbol from `MarketState` to a provider's symbol, so a long-tail token
whose symbol collides with a well-known token will be priced as if it were that token.

In practice this means the guard works reliably for major tokens listed on CEXs, but may produce
false rejections (or false passes) for obscure tokens that happen to share a symbol with a listed
asset. Clients trading long-tail tokens should consider leaving `enabled: false` for those
requests.

## Custom providers

The `PriceProvider` trait, `ExternalPrice`, and `PriceProviderError` are public. Implement the
trait to add your own price provider and register it via
`FyndBuilder::register_price_provider()`:

```rust
let solver = FyndBuilder::new(chain, tycho_url, rpc_url, protocols, min_tvl)
    .register_price_provider(Box::new(MyCustomProvider::new()))
    .price_guard_enabled(true)
    .build()
    .await?;
```

Providers follow a worker+cache pattern: `start()` spawns a background task that populates an
in-memory cache, and `get_expected_out()` reads from that cache without blocking or making network
calls.

If no providers are registered before `build()`, the built-in providers (Hyperliquid, Binance) are
added automatically. Calling `register_price_provider()` skips the defaults — register only what
you need.

To keep the defaults **and** add a custom provider, call `add_default_price_providers()` first:

```rust
let solver = FyndBuilder::new(chain, tycho_url, rpc_url, protocols, min_tvl)
    .add_default_price_providers()
    .register_price_provider(Box::new(MyCustomProvider::new()))
    .price_guard_enabled(true)
    .build()
    .await?;
```

## Example Quote with Price Guard protection

Enable the price guard server-side, then tighten the lower bound and fail-closed on unknown tokens
for a specific request:

```json
{
  "orders": [
    {
      "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
      "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
      "amount": "1000000000000000000",
      "side": "sell",
      "sender": "0x0000000000000000000000000000000000000001"
    }
  ],
  "options": {
    "encoding_options": {
      "price_guard": {
        "enabled": true,
        "lower_tolerance_bps": 100,
        "upper_tolerance_bps": 5000,
        "fail_on_token_price_not_found": true
      }
    }
  }
}
```

### Disabling per-request

To disable the price guard on a server that has it enabled, send `enabled: false` in
`encoding_options.price_guard`:

```json
"encoding_options": {
  "price_guard": { "enabled": false }
}
```

