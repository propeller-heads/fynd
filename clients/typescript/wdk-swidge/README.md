<!-- Copyright 2026 PropellerHeads. SPDX-License-Identifier: Apache-2.0 -->
# Fynd Swidge for WDK

[![Powered by WDK](https://img.shields.io/badge/Powered_by-WDK-26A17B)](https://docs.wdk.tether.io/)

Connect an ordinary WDK EVM account to the hosted Fynd API for same-chain, exact-input swaps on Ethereum (`1`) or Base (`8453`). Fynd supplies routes and encoded transactions; WDK owns approval, signing, broadcast and transaction lookup.

**Unpublished alpha.** The unscoped package name `wdk-protocol-swidge-fynd` and `private: true` are intentional until package ownership and release approval are agreed. This is not a Tether-approved release or evidence of funded settlement on either chain. See [compatibility and release gates](docs/compatibility.md).

## Install from this checkout

From the Fynd repository root, with Node.js 22+ and pnpm 10.30.3:

```sh
pnpm --dir clients/typescript install --frozen-lockfile
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd build
cd clients/typescript/wdk-swidge
cp .env.example .env
```

Keep `.env` local and supply credentials through your application's secret handling. Examples below are `.mjs` files saved in this package directory; the self-reference import resolves through its package exports. No public npm installation is implied.

## Configure

```js
import FyndSwidgeProtocol from 'wdk-protocol-swidge-fynd'
/** @typedef {import('wdk-protocol-swidge-fynd').ISwidgeProtocol} ISwidgeProtocol */

const config = { chainId: 1, apiKey: process.env.FYND_API_KEY }
// new FyndSwidgeProtocol(account, config)
```

`FyndSwidgeProtocol` is both the default and a named export. `ISwidgeProtocol` is available as a declaration type, not a runtime named export. Generated declarations describe the public methods and `FyndConfig`.

| Configuration | Default / meaning |
| --- | --- |
| `chainId` | Required numeric `1` or `8453`. Configure the WDK wallet for the same chain. |
| `apiKey` | Optional raw `Authorization` value, without `Bearer`. Required for direct hosted access. |
| `baseUrl` | `https://fynd-api.propellerheads.xyz`; adapter appends `/v1/ethereum` or `/v1/base`. A trusted proxy must expose the same API. |
| `quoteSender` | Required nonzero address when no account is supplied; account address takes precedence otherwise. |
| `timeoutMs` | `10000`; request and response-body deadline, also the total token-pagination budget. |
| `approvalTimeoutMs` | `180000`; wait budget for each approval to confirm. |
| `maxNetworkFeeBps` | Optional execution-only `number` or `bigint` cap. Requires an Ethereum account, no required approval and a successful WDK swap estimate. |
| `maxProtocolFeeBps` | Optional execution-only `number` or `bigint` cap on the quoted router-fee ratio. |

Use a server-held API key or an integrator-controlled proxy. Do not embed a shared key in a browser/mobile bundle. The proxy is supplied by the integrator; this package does not provide one. HTTPS is required except for a loopback development proxy; URLs containing credentials, query strings or fragments are rejected. Requests are not retried automatically.

The same configured chain ID is an **application construction contract**, not proof of the account's runtime RPC network. Verify and maintain that binding in your wallet setup. The adapter uses public WDK methods and does not inspect private provider fields. Execution requires ordinary, non-delegated EVM accounts; ERC-4337/paymasters, EIP-7702 delegation and Permit2 are outside this version.

## Quote without a wallet

Set `FYND_API_KEY`, `QUOTE_SENDER`, `FROM_TOKEN`, `TO_TOKEN` and `AMOUNT_IN` in `.env`. Save this as `quote.mjs` and run `node --env-file=.env quote.mjs`:

```js
import FyndSwidgeProtocol from 'wdk-protocol-swidge-fynd'

const required = name => {
  if (!process.env[name]) throw new Error(`Set ${name}`)
  return process.env[name]
}
const fynd = new FyndSwidgeProtocol(undefined, {
  chainId: Number(process.env.CHAIN_ID ?? '1'),
  apiKey: required('FYND_API_KEY'),
  baseUrl: process.env.FYND_BASE_URL || undefined,
  quoteSender: required('QUOTE_SENDER')
})
const quote = await fynd.quoteSwidge({
  fromToken: required('FROM_TOKEN'),
  toToken: required('TO_TOKEN'),
  fromTokenAmount: BigInt(required('AMOUNT_IN')),
  slippage: 0.005
})
console.dir(quote, { depth: null })
```

This quote is indicative. The real sender's allowance, balance and fee estimate can differ. `networkFeeComplete` is false without an account. A sender address never supplies execution authority.

For read-only quotes and status, use `WalletAccountReadOnlyEvm` from `@tetherto/wdk-wallet-evm`:

```js
import { WalletAccountReadOnlyEvm } from '@tetherto/wdk-wallet-evm'

// With the same config and environment as the quote example:
const chainId = Number(process.env.CHAIN_ID ?? '1')
const account = new WalletAccountReadOnlyEvm(required('QUOTE_SENDER'), {
  provider: required('RPC_URL'), chainId
})
const readOnlyFynd = new FyndSwidgeProtocol(account, {
  chainId, apiKey: required('FYND_API_KEY'),
  baseUrl: process.env.FYND_BASE_URL || undefined
})
```

A read-only account cannot execute `swidge`.

## Register, execute and check status

The following complete `wallet.mjs` supports `quote`, `execute` and `status`. It defaults to quoting. `execute` broadcasts transactions and spends funds; use it only in the wallet environment you intend to trade from. The adapter refreshes its own execution quote and checks the caller's minimum; it does not submit an earlier displayed quote.

```js
import WDK from '@tetherto/wdk'
import WalletManagerEvm from '@tetherto/wdk-wallet-evm'
import FyndSwidgeProtocol, { FyndExecutionError } from 'wdk-protocol-swidge-fynd'

const required = name => {
  if (!process.env[name]) throw new Error(`Set ${name}`)
  return process.env[name]
}
const chainId = Number(process.env.CHAIN_ID ?? '1')
const walletName = chainId === 1 ? 'ethereum' : 'base'
const wdk = new WDK(required('WDK_SEED_PHRASE'))
  .registerWallet(walletName, WalletManagerEvm, {
    provider: required('RPC_URL'), chainId
  })
  .registerProtocol(walletName, 'fynd', FyndSwidgeProtocol, {
    chainId, apiKey: required('FYND_API_KEY'),
    baseUrl: process.env.FYND_BASE_URL || undefined
  })

try {
  const account = await wdk.getAccount(walletName, 0)
  const fynd = account.getSwidgeProtocol('fynd')
  const action = process.env.ACTION ?? 'quote'
  if (action === 'status') {
    console.dir(await fynd.getSwidgeStatus(required('SWAP_ID')), { depth: null })
  } else {
    if (action !== 'quote' && action !== 'execute') throw new Error('Unknown ACTION')
    const options = {
      fromToken: required('FROM_TOKEN'), toToken: required('TO_TOKEN'),
      fromTokenAmount: BigInt(required('AMOUNT_IN')), slippage: 0.005,
      ...(process.env.RECIPIENT ? { recipient: process.env.RECIPIENT } : {}),
      ...(process.env.MIN_AMOUNT_OUT ? { minAmountOut: BigInt(process.env.MIN_AMOUNT_OUT) } : {})
    }
    if (action === 'quote') {
      console.dir(await fynd.quoteSwidge(options), { depth: null })
    } else {
      required('MIN_AMOUNT_OUT') // Choose your own output floor in token base units.
      const caps = {
        ...(process.env.MAX_NETWORK_FEE_BPS ? { maxNetworkFeeBps: BigInt(process.env.MAX_NETWORK_FEE_BPS) } : {}),
        ...(process.env.MAX_PROTOCOL_FEE_BPS ? { maxProtocolFeeBps: BigInt(process.env.MAX_PROTOCOL_FEE_BPS) } : {})
      }
      const result = await fynd.swidge(options, caps)
      console.dir(result, { depth: null })
      // Persist result.id and result.transactions for later reconciliation.
    }
  }
} catch (error) {
  if (error instanceof FyndExecutionError) {
    // Inspect only these public fields; causes may contain wallet/RPC details.
    console.error({ stage: error.stage, transactions: error.transactions,
      submissionUnknown: error.submissionUnknown })
  } else {
    console.error(error instanceof Error ? error.message : 'Operation failed')
  }
  process.exitCode = 1
} finally {
  wdk.dispose()
}
```

```sh
ACTION=quote node --env-file=.env wallet.mjs
ACTION=execute node --env-file=.env wallet.mjs
ACTION=status node --env-file=.env wallet.mjs
```

Before `execute`, supply your chosen `MIN_AMOUNT_OUT` and review the selected account/chain. Before `status`, set `SWAP_ID` to the returned swap hash. Environment variables are illustrative; production wallets should supply WDK's secret/signer material through their established custody flow.

## Public methods and amounts

| Method | Behavior |
| --- | --- |
| `quoteSwidge(options)` | Non-binding quote with itemized estimated fees and `networkFeeComplete`. Checks requested minimum/slippage, but does not apply constructor fee caps or send transactions. |
| `swidge(options, config?)` | Validates, approves exact spending if needed, confirms approvals, refreshes after approval and broadcasts one swap. Optional second argument supplies fee-cap overrides. Returns at broadcast. |
| `getSwidgeStatus(id, options?)` | Looks up the original transaction through a bound full/read-only account. Optional `fromChain`/`toChain` must match the configured chain. |
| `getSupportedChains()` | Static adapter list of Ethereum and Base; not a live service-health report. |
| `getSupportedTokens(options?)` | Complete, consistent paginated token metadata for the configured chain, filtered to quality 100/tax 0, plus native ETH. Chain filters must match; `fromToken` throws `NotImplementedError`. |

Swap options require `fromToken`, `toToken` and `fromTokenAmount`. Optional fields: `recipient` (defaults to the sender), `slippage` (decimal, default `0.005` = 0.5%), `minAmountOut`, and matching `toChain`. Chain hints accept the numeric ID, its decimal string or `ethereum`/`base`. Amounts are token base-unit `bigint` values or safe integer numbers; use bigint for wallet amounts. Native ETH is `0x0000000000000000000000000000000000000000`; the all-`e` sentinel is also normalized to native. Wrapped ETH remains an ERC-20.

Exact-output `toTokenAmount`, cross-chain destinations, same-token swaps and `refundAddress` are rejected. Fee-on-transfer/rebasing tokens are unsupported. Metadata screening cannot prove every token's behavior, and token presence does not guarantee a route for every pair/size.

Token discovery depends on Fynd's feature-gated, experimental `/tokens` endpoint, whose response contract can change between releases. Availability must be verified for each hosted chain. If pagination observes a changed block or cannot finish within its deadline, the call fails instead of returning a partial list or restarting internally. A subsequent caller request starts at offset zero; this can require another attempt on fast-block chains.

Quotes include net `toTokenAmount`, executable `toTokenAmountMin`, `fees` and optional decimal `priceImpact`. `minAmountOut` is checked against the encoded minimum; an insufficient floor rejects the quote or execution instead of rewriting calldata. Slippage is checked relative to Fynd's reported net output, using its rounding policy. These checks do not independently establish market price or fee rates; use your own absolute `minAmountOut` when that is the constraint you need. Execution adds `id`, `hash` and `transactions`. A returned hash is not proof of settlement.

## Fees, failures and compatibility

Network fees use native-token base units; the quoted router fee uses output-token base units and is already included in the returned net output. Never sum different fee tokens. No integrator fee is requested. Positive-slippage fee rules may add to the settled router fee.

For a quote, `networkFeeComplete: true` requires an Ethereum account, no required approval and a successful WDK swap estimate. The value remains an estimate. An unapproved ERC-20 swap cannot be estimated against its future approved state through the published WDK API, so its quote is incomplete. When no cost could be estimated, the network fee entry is omitted; **an omitted entry does not mean a free transaction**. Base estimates exclude unverified L1/other rollup components and are incomplete. Fynd gas × gas price is only an indicative fallback if WDK estimation fails.

Fee caps apply only to `swidge`, using constructor defaults or its second argument; `quoteSwidge` remains available regardless of those caps. Caps compare estimates in basis points and are not signed settlement ceilings. The protocol cap checks quoted router fee / gross output. A network cap is supported only on Ethereum when the input is native or already sufficiently approved and WDK can estimate the swap. It compares that estimate with native input or a separate trade-sized ERC-20→native valuation quote. If any approval is required, the cap rejects before the first approval; Base network caps also reject. Zero is an active cap. Per-call values override constructor caps; omitted or undefined per-call values inherit them. Omit a cap at construction when no cap is requested.

Execution without a network cap can perform approvals and refresh the quote. Known approval receipt fees are then added to the returned network estimate. They are not part of a post-approval network-cap check. A refreshed quote can still fail its protocol cap, requested output minimum or slippage limit after approval gas has been spent.

Execution skips sufficient allowance, otherwise approves the exact input. Nonzero insufficient Ethereum USDT allowance is reset to zero first. Failed allowance reads stop the operation. Approval confirmation requires a successful included receipt. Earlier approvals persist after later failures. `FyndExecutionError` exposes `stage`, known `transactions`, `submissionUnknown` and `cause`. Unknown submission means the RPC may have accepted a transaction even though no hash was returned: reconcile wallet history before deciding to submit again. The adapter performs no automatic resend.

Status maps included success to `completed`, included revert to `failed`, and pending/dropped original hashes to `pending`. Dropped hashes can remain unresolved after replacement or cancellation; the wallet owns replacement tracking. Unknown/invalid hashes and provider failures remain errors. Status lookup does not authenticate an arbitrary hash as a Fynd swap, and `completed` does not assert irreversible finality.

Inherited `swap`, `quoteSwap`, `bridge` and `quoteBridge` remain WDK delegates. **The pinned WDK legacy swap helpers sum mixed-token fees into one scalar. Do not use that scalar for fee accounting.** Use modern Swidge itemized fees. Cross-chain bridge requests fail under this adapter's scope. Legacy error wrapping can place progress metadata under `cause`.

Other errors use WDK `ValueError`, `InvalidTokenError`, `AccountRequiredError`/`ReadOnlyAccountRequiredError`, `MaximumFeeExceededError`, `ProviderError`, `SwidgeError` and `NotImplementedError`; account operations can also propagate their own WDK/RPC errors. Treat error text and causes as diagnostics, not stable machine contracts. See [compatibility](docs/compatibility.md) for dependency identity, runtime checks, detailed limitations and upgrade steps.

## Development and support

```sh
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd typecheck
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd lint
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd test
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd build
```

Run these from the Fynd repository root. Tests require Anvil 1.7.1 on `PATH` (or `ANVIL_BIN`) and permission to bind ephemeral loopback ports; the pinned Bare runtime is a dev dependency. The account tests use fixture contracts on an isolated chain, not live settlement. Node checks alone do not establish Bare support; execute the packed-artifact Node/Bare checks before release. The library has no direct logging or telemetry. Examples deliberately log only their requested results/progress.

Source: [propeller-heads/fynd](https://github.com/propeller-heads/fynd/tree/main/clients/typescript/wdk-swidge). For ordinary hosted API support, use the [Fynd support group](https://t.me/+B4CNQwv7dgIyYTJl) linked by Fynd's hosted API guide. For sensitive reports, see [SECURITY.md](SECURITY.md). No support SLA or Tether review approval is implied.

Copyright 2026 PropellerHeads. Licensed under [Apache 2.0](LICENSE).
