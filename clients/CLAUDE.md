# Clients

HTTP clients for the Fynd RPC API. Both clients wrap the same OpenAPI spec (`clients/openapi.json`)
and provide typed quote/health interfaces.

| Client | Location | Package | Language ecosystem |
|---|---|---|---|
| [Rust](#rust-client) | `clients/rust/` | `fynd-client` (Cargo workspace member) | reqwest + alloy |
| [TypeScript](#typescript-client) | `clients/typescript/` | `@kayibal/fynd-client` (pnpm workspace) | fetch + viem |

## OpenAPI Spec

`clients/openapi.json` is the single source of truth for the wire format. Generated from Rust
types via `cargo run -- openapi`. CI checks for drift.

When adding/changing RPC endpoints: update Rust types → run `./scripts/update-openapi.sh` →
both clients pick up changes (Rust via `fynd-rpc-types`, TypeScript via `openapi-typescript`
codegen).

---

## Rust Client

Crate name: `fynd-client` (workspace member at `clients/rust/`).

### Module Map

| File | Purpose |
|---|---|
| `client.rs` | `FyndClient` + `FyndClientBuilder`. Quote, sign, execute flow. Retry with exponential backoff |
| `types.rs` | Client-side types (`Order`, `Quote`, `QuoteParams`, `HealthStatus`, etc.) — independent from wire DTOs |
| `mapping.rs` | Converts between client types and `fynd-rpc-types` wire DTOs |
| `signing.rs` | EIP-712 signing: `SwapPayload`, `FyndPayload`, `SignedSwap`, `TxReceipt`, `SignedApproval` |
| `error.rs` | `FyndError` with `ErrorCode` enum and `is_retryable()` classification |

### Key Types

**`FyndClientBuilder`**
```rust
FyndClientBuilder::new(fynd_url, rpc_url)
    .retry_config(RetryConfig::default())
    .build()
    .await?
```

**`FyndClient`**
- `quote(params: QuoteParams) -> Result<Quote, FyndError>` — request a swap quote
- `health() -> Result<HealthStatus, FyndError>` — check solver health
- `info() -> Result<InstanceInfo, FyndError>` — fetch static instance metadata (cached)
- `swap_payload(quote, signer, hints) -> Result<SwapPayload, FyndError>` — EIP-712 sign a quote
- `execute_swap(signed_swap, options) -> Result<TxReceipt, FyndError>` — submit swap on-chain
- `approval(params: ApprovalParams) -> Result<Option<SignedApproval>, FyndError>` — build and sign an ERC-20 approval if needed
- `execute_approval(approval: SignedApproval) -> Result<TxReceipt, FyndError>` — broadcast a signed approval

**`RetryConfig`** — Exponential backoff for transient failures. Default: 3 attempts, 100ms initial, 2s max.

**`StorageOverrides`** — Dry-run execution with simulated ERC-20 balances/approvals (storage slot
overrides for `eth_call`).

### Backend Detection

`FyndClientBuilder` auto-detects the `BackendKind` (Fynd vs Turbine) by checking the health
endpoint response shape. This determines which signing and execution paths to use.

---

## TypeScript Client

For full details, see [`.claude/knowledge/typescript.md`](../.claude/knowledge/typescript.md).

pnpm workspace at `clients/typescript/` with two packages:

- **`@fynd/autogen`** (`autogen/`) — Generated types from `openapi-typescript`. `schema.d.ts` is
  auto-generated; do not edit manually.
- **`@kayibal/fynd-client`** (`client/`) — Typed HTTP client: `FyndClient`, signing, Permit2, error types.

### Build & Test

```bash
pnpm --dir clients/typescript install --frozen-lockfile
pnpm --dir clients/typescript --filter @fynd/autogen run build
pnpm --dir clients/typescript --filter @fynd/client run typecheck
pnpm --dir clients/typescript --filter @fynd/client run lint
pnpm --dir clients/typescript --filter @fynd/client run test
```
