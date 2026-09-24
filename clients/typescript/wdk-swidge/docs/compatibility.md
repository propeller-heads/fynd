<!-- Copyright 2026 PropellerHeads. SPDX-License-Identifier: Apache-2.0 -->
# Compatibility

`0.1.0-alpha.0` is private and unpublished. Usage and configuration: [README](../README.md).

## Dependencies and runtime

| Dependency | Pinned version |
| --- | --- |
| `@tetherto/wdk` | `1.0.0-beta.18` |
| `@tetherto/wdk-wallet` | `1.0.0-beta.19` |
| `@tetherto/wdk-wallet-evm` | `1.0.0-beta.19` |
| `ethers` | `6.17.0` |
| `bare-node-runtime` | `1.5.1` |

ESM JavaScript, JSDoc-generated declarations, Node.js 22+, and a conditional Bare entrypoint.

The `require-asset` dev pin prevents pnpm 10 from omitting Bare’s asset loader. Recheck it when upgrading pnpm/Bare; its version must satisfy the platform runtime’s dependency range.

EVM beta.19 pins wallet beta.17; core beta.18 uses a caret wallet dependency. Core and the adapter must resolve the same Swidge base class for `registerProtocol`. Account errors can come from another wallet-package instance, so cross-package `instanceof` checks are unreliable.

Strict TypeScript `WDK.registerWallet(..., WalletManagerEvm, ...)` fails on conflicting private `_seed` declarations, even without Fynd. JavaScript registration works. Type fixtures record this upstream error and check Fynd's constructor, `registerProtocol` and public interfaces.

WDK has no public `quoteApprove`. `approve` accepts token, spender and amount, without gas overrides. The account provider sets the transaction chain ID. Configure WDK and Fynd for the same chain; adapter configuration does not verify the provider's network.

## Errors and legacy helpers

Fynd uses WDK's `ValueError`, `InvalidTokenError`, `AccountRequiredError`, `ReadOnlyAccountRequiredError`, `MaximumFeeExceededError`, `ProviderError`, `SwidgeError` and `NotImplementedError`. Wallet/RPC failures can propagate their own errors. `FyndExecutionError` adds transaction progress; inherited legacy wrappers expose it through `cause`.

WDK's legacy scalar fee adds unlike token denominations. Use Swidge's itemized fees. See [fees, approvals and status](../README.md#fees-and-failures) for execution limits.

## Unresolved compatibility

The npm scope, publisher and [private security contact](../SECURITY.md) remain unset. Tether Generic Implementation Requirements were inaccessible. Tether confirmation remains open for exact-input scope, account binding, fee estimates/caps, legacy fees, original-hash status and discovery semantics. Full conformance is unverified.

## Upgrades

- **WDK:** review published account methods, chain population, approvals, receipts, errors and Swidge types. Check class identity, direct account construction and registration in a fresh packed consumer.
- **Fynd:** compare deployed responses with parser fixtures, including slippage, fee arithmetic, gas units, experimental token discovery and timeout/error handling. Gas-adjusted output is not paid gas.
- **Router:** follow the [deployment and ABI checks](router-verification.md). Update constants, validator and fixtures together.
- **Package:** rebuild declarations; test exports, cap boundaries, approvals, partial failures, status and discovery. Run Node and actual Bare checks, audit dependencies and inspect the pack for secrets. Record changed contracts in the changelog. Keep local-fixture results separate from hosted-chain settlement evidence.
