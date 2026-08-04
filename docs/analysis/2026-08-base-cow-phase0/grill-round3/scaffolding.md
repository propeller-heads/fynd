# Scaffolding (round 3 — verdict: proceed to build)

The crate scaffold already exists (`tools/apex-batch/src/`); this maps the build against it.

## File tree

| Path | Action | Why |
|---|---|---|
| `tools/apex-batch/src/scaling.rs` | de-`#[ignore]` tests | round-trip / decline behavior is step-0 verification |
| `tools/apex-batch/src/adapter.rs` | extend + property tests | direct-vs-adapter ProtocolSim agreement; v4 keccak address derivation (v3 item 10) |
| `tools/apex-batch/src/prices.rs` | new | fynd rational → apex U256 transform (v3.1 item B) + overflow-bound scale selection (item A); property test vs fynd-core's ETH=2000 fixture |
| `tools/apex-batch/src/orders.rs` | new (split from adapter) | trade → LimitOrder build: 18-dec Fraction lift, `{tx_hash}:{ordinal}` ids, uniqueness assert, component partitioning (items C, v3 7–9) |
| `tools/apex-batch/src/runner.rs` | rewrite doc + solve path | per-component calls, solve-start deadlines, precondition-first panics doc (item J) |
| `tools/apex-batch/src/snapshot.rs` | extend | price-map + freshness stamps (v2 item 13) |
| `tools/hindsight/src/resolve/mod.rs` | refactor | tops/advance/backs phase split; mock tests move to phases (item H) |
| `tools/hindsight/src/resolve/monitor.rs` | extend | APEX stage: owned 2-thread pool, bounded channel, brackets + singles dispatch, async join |
| `tools/hindsight/src/resolve/apex_stage.rs` | new | the live stage itself (worker pool, watchdog, reconciliation, metrics) |

## Key stubs

```rust
// prices.rs
pub struct ApexPriceMap { scale: U256, prices: FxHashMap<ApexAddress, U256> }
pub fn build_price_map(derived: &TokenGasPrices, tokens: &[TokenMeta], batch_notional_cap_usd: u64)
    -> Result<ApexPriceMap, PriceMapError>; // inversion + 10^(dec−18) + S from 2^126 bound

// orders.rs
pub fn build_components(trades: &[DecodedTrade], subset: &PoolSubset)
    -> Result<Vec<Component>, BuildDecline>; // partition, uniqueness assert, closure precheck

// apex_stage.rs
pub struct ApexStage { workers: OwnedPool, queue: SyncSender<SolveJob> }
impl ApexStage { pub fn dispatch(&self, job: SolveJob) -> DispatchOutcome /* Queued | Skipped */ }
```

## Test outline (step 0, before shadow run)

- scaling: round-trip identity 0..=18 dec; floor≤ceil straddle; >18 dec + overflow declined (de-ignore).
- adapter: direct `ProtocolSim::get_amount_out` == through-`TychoApexPool`, both directions,
  USDC/WETH + 18/18 + 0-dec pairs, live-fetched states.
- prices: ETH=2000-USDC fixture → apex ratio exact incl. 10^12 decimals factor; $1e-9 token
  triggers the overflow-bound exclusion, counted.
- limits: mixed-decimal Fraction equality both directions; at-limit order (limit == clearing)
  solves without error on a synthetic 2-order cross.
- orders: duplicate (pair, price, id) declined + counted; multi-hop single-order batch →
  internalization ≈ 0.
- components: hub-linked orders merge, disjoint long-tail pairs split (mirrors component_count.py).
