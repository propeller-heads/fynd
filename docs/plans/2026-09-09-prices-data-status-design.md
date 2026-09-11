# `/v1/prices` Data Status Design

**Status:** Approved

**Date:** 2026-09-09

## Context

`GET /v1/prices` currently returns computation block numbers in a `blocks` object:

```json
{
  "blocks": {
    "token_prices": 21000000,
    "spot_prices": 21000000,
    "component_depths": 21000000
  }
}
```

This does not tell consumers how recently Fynd recomputed the data or how far the computation is behind Fynd's current Tycho market head. `/v1/health.last_update_ms` exposes the age of the latest Tycho market block, but that value is not the age of the stored token-price computation.

The endpoint is experimental, so a breaking response reshape is acceptable. The new contract should make the distinction between upstream market state and Fynd computations explicit and leave room for more status metadata without another reshape.

## Goals

- Expose Fynd's current Tycho market head as full block identity: number, hash, and timestamp.
- Expose the age of that head using the same semantics as `/v1/health.last_update_ms`.
- Expose the block and operational refresh age of token prices, spot prices, and component depths.
- Give status metadata a coherent, extensible namespace.
- Keep the response internally consistent enough to reveal, rather than hide, computation lag behind the Tycho head.
- Update OpenAPI and generated TypeScript types with the server contract.

## Non-goals

- Do not query a chain RPC for an independent canonical-chain head.
- Do not change `/v1/health` or its health thresholds.
- Do not make derived computations run more often.
- Do not guarantee a separate age for every item retained after a partial incremental computation.
- Do not add an API-version negotiation flag or a second prices endpoint.
- Do not add status for unrelated subsystems such as gas-price fetching.

## Decision

Replace `PricesResponse.blocks` with `PricesResponse.data_status`.

```json
{
  "prices": [],
  "gas_token": "0x0000000000000000000000000000000000000000",
  "data_status": {
    "tycho": {
      "head": {
        "number": 21000001,
        "hash": "0xabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd",
        "timestamp": 1788900000
      },
      "last_update_ms": 6200
    },
    "computations": {
      "token_prices": {
        "block": 21000000,
        "last_update_ms": 85
      },
      "spot_prices": {
        "block": 21000000,
        "last_update_ms": 72
      },
      "component_depths": {
        "block": 21000000,
        "last_update_ms": 79
      }
    }
  },
  "spot_prices": [],
  "component_depths": []
}
```

The status types are conceptually:

```text
DataStatus
├── tycho: TychoDataStatus
│   ├── head: BlockInfo { number, hash, timestamp }
│   └── last_update_ms: u64
└── computations: ComputationDataStatuses
    ├── token_prices: ComputationDataStatus
    │   ├── block: u64
    │   └── last_update_ms: u64
    ├── spot_prices: Option<ComputationDataStatus>
    └── component_depths: Option<ComputationDataStatus>
```

## Field semantics

### `data_status.tycho.head`

This is the `MarketState::last_updated()` block. The Tycho feed updates it when at least one synchronizer in an accepted update reports `SynchronizerState::Ready`; among the ready headers in that update, Fynd selects the highest block number. If an update has no ready header, the previous head remains.

The value describes Fynd's latest accepted Tycho market state. It is not an independently queried canonical-chain head. In partial-block mode, it still follows the ready synchronizer header selected by the feed rather than the update's `block_number_or_timestamp` label.

### `data_status.tycho.last_update_ms`

This is the age of the head's chain timestamp:

```text
current Unix time - data_status.tycho.head.timestamp
```

It must use the same helper and semantics as `/v1/health.last_update_ms`. Since block timestamps are stored in seconds, this age has one-second source resolution. Subtraction saturates at zero for a future timestamp and at `u64::MAX` on numeric overflow.

### Computation `block`

This is the market block for which the currently stored computation output was persisted. It retains the semantics of the old `blocks.<computation>` scalar.

### Computation `last_update_ms`

This is elapsed monotonic time since Fynd successfully persisted the currently stored computation output. It is measured from computation completion/persistence, not from the source block timestamp and not from computation start.

The age describes the stored computation output as a whole. Incremental computations may retain individual entries and failure records from earlier blocks; the field does not claim every item was individually recomputed at the same instant.

## Presence and error behavior

- `data_status`, `data_status.tycho`, and `data_status.computations.token_prices` are required in every `200` response.
- `spot_prices` and `component_depths` computation statuses are serialized whenever those stored computations exist, even when their full datasets were not requested. This preserves the current status behavior of the old `blocks` fields.
- The full `spot_prices` and `component_depths` datasets remain controlled by the existing `include` query parameter.
- Missing token prices returns the existing `503 Data not yet available` response.
- Missing Tycho head also returns `503`; successful token-price computation should imply a head, but the handler must reject an inconsistent state rather than emit incomplete status.
- Requesting an optional computation that is unavailable keeps the existing `503` behavior.
- Unknown or oversized query parameters keep the existing `400` behavior.

## Internal architecture

### Derived computation metadata

Extend the generic `ComputedSlot` in `fynd-core/src/derived/store.rs` to retain a monotonic update instant alongside data and block number. Capture the instant in the common persistence path so token prices, spot prices, and component depths cannot drift into separate timestamp implementations.

Preserve existing public `*_block()` accessors for downstream `fynd-core` compatibility. Add status accessors that expose block and age calculation without exposing mutable storage internals. Use a pure `age_at(now)` form internally so tests can supply a comparison instant without sleeps.

Clearing a slot must clear its status at the same time because status is part of the slot.

### Tycho head snapshot

Refactor `HealthTracker` so one method snapshots both the cloned `BlockInfo` and its age under a single market-data read lock. Make the existing `age_ms()` delegate to the same logic. `/v1/prices` consumes that snapshot, which prevents health and prices from implementing two subtly different definitions of Tycho age.

### Prices handler

Under one derived-data read lock:

1. Validate required and requested computation availability.
2. Capture one monotonic comparison instant after acquiring the lock.
3. Read all computation statuses against that instant.
4. Clone the requested output data.
5. Release the lock before formatting and sorting response entries.

Then read the Tycho head snapshot. Do not try to atomically lock market and derived state together. A head newer than a computation block is valid and is the lag consumers need to observe. Avoid nested locks and preserve the existing short lock-hold behavior.

### Wire types and OpenAPI

Define the experimental response types in `fynd-rpc/src/api/prices.rs`:

- `DataStatus`
- `TychoDataStatus`
- `ComputationDataStatuses`
- `ComputationDataStatus`

Reuse the existing `fynd_rpc_types::BlockInfo` wire type for the Tycho head so block identity has one schema. Replace `PricesResponse.blocks` with `PricesResponse.data_status`. Register the new schemas in the experimental OpenAPI bundle.

Regenerate:

- `clients/openapi.json`
- `clients/typescript/client/src/schema.d.ts`

No hand-maintained Rust or TypeScript client currently wraps `/v1/prices`, so no client mapping method is needed unless code inspection during implementation finds a new consumer.

## Testing strategy

### Core store tests

- A persisted slot reports its block and an exact age against a supplied later instant.
- Re-persisting one computation replaces its block/update instant without changing other computation statuses.
- Clearing one computation clears that status.
- `clear_all` clears every status.
- Existing `*_block()` tests continue to pass.

### Health tracker tests

- A stored block produces full head identity and the expected timestamp-based age.
- A future block timestamp saturates age at zero.
- Missing head preserves `age_ms() == u64::MAX`.
- `age_ms()` and the head snapshot use the same age helper.

### Handler tests

- A successful response contains the exact approved `data_status` shape.
- The old `blocks` key is absent.
- Full Tycho head identity is returned.
- Computation statuses carry the correct blocks and numeric ages.
- Optional computation statuses are omitted when unavailable.
- Computation status remains present when its full data was not requested.
- Missing token prices, missing Tycho head, and missing requested optional data return `503`.
- Existing price serialization, sorting, include, and limit behavior remains unchanged.

### Contract and generated-client tests

- OpenAPI contains the four new schemas and references `data_status` from `PricesResponse`.
- OpenAPI no longer requires or documents `blocks`/`ComputationBlocks` for prices.
- `/v1/prices` remains marked `x-experimental`.
- Regeneration is idempotent.
- Generated TypeScript schema typechecks, lints, and passes tests.

## Validation and rollout

Before presenting a PR-ready diff:

1. Run targeted `fynd-core` and experimental `fynd-rpc` tests.
2. Regenerate OpenAPI and TypeScript schemas twice and compare checksums for idempotency.
3. Run TypeScript typecheck, lint, and tests.
4. Run nightly format checks and the repository `./check.sh` gate.
5. Inspect the full diff and obtain independent spec and code-quality review.

After merge and staging deployment, verify two or more `/v1/prices` responses:

- The head includes number, hash, and timestamp.
- Tycho age is compatible with both `now - head.timestamp` and `/v1/health.last_update_ms` within request timing and one-second timestamp resolution.
- Computation ages increase between reads and reset after each corresponding computation is persisted again.
- Computation block numbers visibly show any lag behind Tycho head.

## Compatibility and migration

This is an intentional breaking change to an experimental endpoint:

```text
blocks.token_prices                  -> data_status.computations.token_prices.block
blocks.spot_prices                   -> data_status.computations.spot_prices.block
blocks.component_depths              -> data_status.computations.component_depths.block
(new)                                -> data_status.tycho.head
(new)                                -> data_status.tycho.last_update_ms
data_status.computations.*.last_update_ms is new
```

The `data_status` namespace is the extension point for future source/computation status. New metadata can be added to the relevant nested object without another top-level response reshape.

## Risks and mitigations

- **Two clock bases share the field name `last_update_ms`.** Scope and OpenAPI descriptions make the distinction explicit: Tycho uses block timestamp age; computations use elapsed time since persistence.
- **Partial incremental results can contain older individual entries.** Document that computation age applies to the stored result as a whole; do not overstate per-item freshness.
- **Wall-clock changes could corrupt operational age.** Use a monotonic instant for computation age.
- **Missing head could produce an incomplete success response.** Return `503` instead.
- **Generated clients can drift from server types.** Regenerate from Rust and verify idempotency/typecheck in the same change.
- **Experimental consumers must migrate.** Mark the change as breaking in commit/PR metadata and include the old-to-new field map above in release-facing documentation.
