<!-- Copyright 2026 PropellerHeads. SPDX-License-Identifier: Apache-2.0 -->
# Compatibility, limitations and release checks

This document records the implementation contract, not evidence that all checks have passed. The module is `0.1.0-alpha.0`, private and unpublished; package scope, publisher and reviewer ownership remain to be agreed.

## Dependency and runtime baseline

| Dependency | Pinned version |
| --- | --- |
| `@tetherto/wdk` | `1.0.0-beta.18` |
| `@tetherto/wdk-wallet` | `1.0.0-beta.19` |
| `@tetherto/wdk-wallet-evm` | `1.0.0-beta.19` |
| `ethers` | `6.17.0` |
| `bare-node-runtime` | `1.5.1` |

Production code is ESM JavaScript with JSDoc-generated declarations. Node.js 22+ is declared. A conditional `bare` entrypoint supplies the WDK runtime-mapping pattern; its presence is not a Bare test result.

EVM beta.19 pins base wallet beta.17 internally. Core beta.18 has a caret base-wallet dependency. The adapter and core must resolve the same Swidge base class for WDK's `registerProtocol` class-identity check; test the resolved/packed-consumer dependency tree. Upstream account errors can come from a different base-wallet instance, so do not assume they pass `instanceof` against the adapter's imported error constructors. Verify registration, declarations, public-account behavior and error handling together before changing versions.

Strict TypeScript `WDK.registerWallet(..., WalletManagerEvm, ...)` currently fails with
incompatible private `_seed` declarations, reproducible without importing Fynd. This is an
upstream EVM/core dependency-version conflict and remains a release gate; JavaScript runtime
registration passes the real-account and packed-consumer checks. Our `registerProtocol` and
public interface assignments are type-tested. The type fixture explicitly records the upstream
expected error; it does not hide it with a cast or change upstream dependency pins.

No public `quoteApprove` exists in this baseline. Execution calls public `approve`; the adapter does not claim that WDK can estimate a router swap before its allowance exists. That approval helper accepts token, spender and amount, not gas overrides. Account transaction population takes chain ID from its provider. A configured adapter chain or a transaction field does not override or authenticate the wallet's runtime network.

## Supported and intentionally limited behavior

| Area | Current contract / limitation |
| --- | --- |
| Execution | Same-chain exact-input swaps on configured Ethereum/Base, ordinary non-delegated EVM accounts, native and standard untaxed ERC-20s. No exact-output, cross-chain, paymaster, Permit2, local Fynd engine or integrator-fee customization. |
| Account binding | Application constructs WDK and the adapter with the same chain/provider choice. No runtime proof through private fields or invented public chain accessor. |
| Quote modes | Full, read-only or no account. No-account quotes require `quoteSender` and are indicative. Quote methods validate minimum/slippage but do not apply constructor fee caps. Status requires an account; execution requires a writable account. |
| Output minimum | Check the encoded executable minimum against `minAmountOut`; reject if it is too low. Do not patch calldata or retry slippage tuning. |
| Approvals | Exact required allowance; skip sufficient allowance; reset nonzero insufficient Ethereum USDT first. Wait for included success, refresh after approvals and preserve progress on later failure. |
| Network fees | Itemized estimates, with `networkFeeComplete` extension. Execution network caps require Ethereum, no required approval and a successful WDK swap estimate. A required approval or Base network cap rejects before any write. |
| Protocol cap | Execution-only ratio of quoted router fee to gross output under the quote-implied exchange-rate policy. Rechecked after approvals. Positive-slippage fee treatment prevents a claim that this caps every settled router charge. |
| Broadcast | Return expected output and estimated fees with hash and known transactions. Actual settled output/full fees are not promised at this point. No automatic retry/rebroadcast or replacement engine. |
| Status | Original-hash included success/revert maps to completed/failed. Pending/dropped maps to pending; replacement can leave the original unresolved. Unknown hashes/provider errors throw. Arbitrary supplied hashes are not authenticated as Fynd operations. |
| Chains | Static adapter list, not a provider chain-list endpoint or live readiness check. |
| Tokens | Feature-gated experimental `/tokens`; verify hosted availability. Complete block-consistent pagination, quality 100/tax 0 and native asset. Metadata is not proof of token behavior. `fromToken` route filtering throws `NotImplementedError`; mismatching chain filters throw `ValueError`. |
| Legacy delegates | Retained unchanged. WDK's legacy scalar fee sums unlike denominations; use Swidge itemized fees. Legacy wrappers may expose execution metadata through `cause`. |

For a quote, `networkFeeComplete` is true only with an Ethereum account, no required approval and a successful WDK swap estimate. It is not a guaranteed gas cost. With `transfer_from`, a swap requiring approval cannot be estimated before that approval changes chain state; the adapter therefore rejects a requested execution network cap before any approval. Quote methods do not apply fee caps, so capped registration still permits indicative quotes.

When execution has no network cap, it can approve and refresh. Known approval receipt fees are added only to the returned network estimate after refresh, not to a post-approval network-cap check. The refreshed protocol cap, output minimum or slippage check can still fail after approval costs were paid. Slippage validation is relative to API-reported net output; it does not independently price the trade or establish router fee rates.

The experimental token endpoint is an explicit upgrade dependency. Pagination deliberately fails on a changed block/total or an exhausted deadline; it does not restart internally or present a partial list as complete. A caller may request a new snapshot from offset zero. Verify this behavior and hosted availability on each chain, especially for multi-page lists on Base.

Adapter-specific helper methods and state use JavaScript `#` privacy. Only the intended Swidge methods and inherited delegates are exposed to consumers and WDK policy handling; transaction-planning helpers are not public extension points.

Public Fynd API failures are exposed as typed WDK errors without echoing raw HTTP bodies. Execution failures preserve stage, known transactions and uncertain-submission state. Inspect those public fields before considering retry. Do not use diagnostic reason strings as a substitute for the declared public error types; wallet/RPC errors and causes can differ across resolved WDK versions.

## Acceptance and release gates

The supplied Tether guides are partly addressed by implementation and partly subject to external agreement. **Full conformance is not established.** The linked Generic Implementation Requirements document was inaccessible during research. Obtain and check it; confirm exact-input-only scope, account-network binding, estimated execution fees/cap policies, legacy fee behavior, original-hash status, static chain discovery and unavailable route-scoped token filtering with the assigned Tether reviewer.

Before calling this a release:

1. Agree the npm scope/publisher, maintainer and private security-disclosure contact. Replace the intentional private/unscoped placeholder only with release authorization. Confirm Apache-2.0 ownership and support details.
2. Run scoped type, lint and behavior tests. Test cap zero/equality/overrides, cap-independent quoting, unapproved network-cap rejection with unchanged nonce, successful capped pre-approved execution, reset/approval failure, post-approval protocol/minimum/slippage rejection, ambiguous submission, status and discovery boundaries.
3. Build declarations and inspect the packed artifact. In a fresh consumer, test default/named class exports and the `ISwidgeProtocol` declaration type, WDK registration and declaration compatibility with the actual dependency tree. Execute Node and actual Bare import/HTTP/account smoke checks; Node `--conditions=bare` alone is insufficient. Audit dependencies and exclude secrets/test credentials from the pack.
4. Verify authenticated hosted metadata, encoded responses and experimental token discovery for each advertised chain; independently check the router deployment/ABI. Record controlled chain/fork execution evidence and, only when separately authorized, funded settlement evidence. Deterministic fixtures and returned hashes are different evidence.
5. Pass the required Claude Opus 5.5 review at Max effort on the completed implementation before opening even a draft implementation PR. Record the exact reviewed revision/diff, test evidence, verdict and resolved findings; materially changed code needs affected checks/review again. Obtain the engineering/Tether review required by the agreed process. Model review is not Tether acceptance.
6. After separate release authorization, create the tagged package release and verify the installable artifact. WDK docs/listing contribution and wallet adoption are separate subsequent steps; neither follows automatically from package publication.

## Upgrade procedure

- **WDK:** review published package changes, not only main. Recheck public account signatures, chain population, approval behavior, receipts, errors, Swidge types/legacy delegates and core class identity. Update pins deliberately and rerun fresh-consumer Node/Bare/type tests.
- **Fynd API:** compare the deployed authenticated schema and representative responses with parser fixtures. Verify decimal slippage, per-order success, fee arithmetic, gas units/price, experimental token-endpoint availability/schema, pagination and timeout/error behavior. Do not infer paid gas from gas-adjusted routing output.
- **Router:** independently verify deployment addresses, bytecode/ABI, supported selectors, native sentinel, outer fields, watermark handling and fee/minimum semantics. Update the bounded validator and fixtures together; do not auto-accept an unknown router from `/info`.
- **Release:** record changed contracts and limitations in the changelog, repeat relevant execution/runtime evidence, rerun required review and obtain release authorization. Keep static chain capability separate from current service health.

Source and support links are in the [README](../README.md); sensitive-report policy is in [SECURITY.md](../SECURITY.md). This file makes no unverified security-owner, service SLA, live-settlement or Tether-approval claim.
