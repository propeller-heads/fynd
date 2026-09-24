<!-- Copyright 2026 PropellerHeads. SPDX-License-Identifier: Apache-2.0 -->
# Fynd Swidge for WDK

[![Powered by WDK](https://img.shields.io/badge/Powered_by-WDK-26A17B)](https://docs.wdk.tether.io/)

Same-chain, exact-input swaps on Ethereum (`1`) and Base (`8453`) through the hosted Fynd API. Fynd supplies routes and calldata; WDK handles approvals, signing, broadcast and transaction lookup.

## Install

This alpha package is private and unpublished. From the Fynd repository root, using Node.js 22+ and pnpm 10.30.3:

```sh
pnpm --dir clients/typescript install --frozen-lockfile
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd build
cd clients/typescript/wdk-swidge
cp .env.example .env
```

## Configure

| Option | Meaning |
| --- | --- |
| `chainId` | Required numeric `1` or `8453`. |
| `apiKey` | Raw `Authorization` value, without `Bearer`; required for direct hosted access. |
| `baseUrl` | Defaults to `https://fynd-api.propellerheads.xyz`; appends `/v1/ethereum` or `/v1/base`. |
| `quoteSender` | Nonzero address required for quotes without an account. Account address takes precedence. |
| `timeoutMs` | Default `10000`; includes response bodies and the entire token-pagination request. |
| `approvalTimeoutMs` | Default `180000`; confirmation deadline per approval. |
| `maxNetworkFeeBps`, `maxProtocolFeeBps` | Optional execution caps, as `number` or `bigint`. See fees below. |

Keep API keys server-side or behind your own compatible proxy. HTTPS is required except on loopback; URLs cannot contain credentials, queries or fragments. Requests are not retried.

Configure WDK and the adapter for the same chain. The adapter cannot verify the account's runtime RPC network. Execution requires an ordinary, non-delegated EVM account; ERC-4337/paymasters, EIP-7702 and Permit2 are unsupported.

`FyndSwidgeProtocol` has default and named exports. `FyndConfig` and `ISwidgeProtocol` are declaration types. For account-free quotes, construct with `undefined` and `quoteSender`; for read-only quotes/status, pass `WalletAccountReadOnlyEvm` from `@tetherto/wdk-wallet-evm`.

## Use

Fill `.env`, save this as `wallet.mjs` in the package directory, then run the commands below. The default action quotes. Execution spends funds and requires your chosen `MIN_AMOUNT_OUT`, in output-token base units. For status, set `SWAP_ID` to the returned hash.

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
      required('MIN_AMOUNT_OUT')
      const caps = {
        ...(process.env.MAX_NETWORK_FEE_BPS ? { maxNetworkFeeBps: BigInt(process.env.MAX_NETWORK_FEE_BPS) } : {}),
        ...(process.env.MAX_PROTOCOL_FEE_BPS ? { maxProtocolFeeBps: BigInt(process.env.MAX_PROTOCOL_FEE_BPS) } : {})
      }
      console.dir(await fynd.swidge(options, caps), { depth: null })
    }
  }
} catch (error) {
  if (error instanceof FyndExecutionError) {
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

Keep `.env` local. Production wallets should use their existing custody flow for signer material.

## API

| Method | Result |
| --- | --- |
| `quoteSwidge(options)` | Non-binding quote; validates minimum/slippage without applying fee caps. |
| `swidge(options, config?)` | Approves if needed, refreshes after approval, broadcasts one swap and returns `id`, `hash`, `transactions` and quoted amounts/fees. |
| `getSwidgeStatus(id, options?)` | Original-hash lookup through a full or read-only account. |
| `getSupportedChains()` | Static Ethereum/Base list. |
| `getSupportedTokens(options?)` | Complete, block-consistent metadata: quality 100/tax 0 tokens plus native ETH. `fromToken` filtering is unsupported. |

Swap options require `fromToken`, `toToken` and `fromTokenAmount`. Optional: `recipient` (sender by default), `slippage` (default `0.005`, or 0.5%), `minAmountOut` and matching `toChain`. Chain hints accept the numeric ID, decimal string or `ethereum`/`base`; status/token filters must also match.

Amounts use token base-unit `bigint` values or safe integers. Native ETH uses `0x0000000000000000000000000000000000000000` or the all-`e` sentinel; wrapped ETH is an ERC-20. Exact-output, cross-chain, same-token swaps, `refundAddress`, fee-on-transfer and rebasing tokens are unsupported.

Quotes return net `toTokenAmount`, executable `toTokenAmountMin`, itemized `fees`, `networkFeeComplete` and optional decimal `priceImpact`. Minimum/slippage checks reject unsuitable calldata; they do not change it. Slippage uses Fynd's reported net output and rounding, not an independent market price. Execution fetches a fresh quote, so set an absolute `minAmountOut` when needed.

Token discovery requires Fynd's experimental `/tokens` endpoint. Changed pagination blocks/totals or an expired deadline fail the call without partial results. Token metadata neither guarantees behavior nor a route.

## Fees and failures

Network fees use native-token units. Router fees use output-token units and already reduce net output. Never sum unlike tokens. No integrator fee is requested; positive slippage can increase the settled router fee.

`networkFeeComplete` is true only with an Ethereum account, no required approval and a successful WDK swap estimate. Base excludes unverified rollup costs. Fynd gas × gas price is an indicative fallback; an omitted network fee means unknown cost, not zero.

Caps apply only to execution and constrain estimates. The protocol cap compares quoted router fee/gross output. A network cap requires Ethereum, no approval and a successful WDK swap estimate; it compares cost with native input or an ERC-20→native valuation quote. Otherwise it rejects before any write. Per-call caps override constructor values; `undefined` inherits, and zero remains active.

Execution skips sufficient allowance, otherwise approves exact input, resetting nonzero insufficient Ethereum USDT allowance first. It confirms approvals before refreshing. **A later quote/cap/minimum/slippage failure can leave approvals active and gas spent.** Known approval receipt fees enter the result's network estimate.

Persist returned transaction hashes. `FyndExecutionError` exposes `stage`, `transactions`, `submissionUnknown` and `cause`. If submission is unknown, reconcile wallet history before resubmitting. Other failures use WDK errors; diagnostic text and causes are not stable contracts.

Status maps included success/revert to `completed`/`failed`, and pending/dropped to `pending`. Replacement tracking belongs to the wallet. Unknown hashes/provider failures throw; a supplied hash is not authenticated as a Fynd swap, and completion does not imply irreversible finality.

Use Swidge's itemized fees: inherited WDK legacy swap helpers sum unlike fees into one scalar. See [known limitations](docs/compatibility.md), including the upstream TypeScript wallet-registration conflict.

## Development

From the repository root:

```sh
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd typecheck
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd lint
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd test
pnpm --dir clients/typescript --filter wdk-protocol-swidge-fynd build
```

Tests require Anvil 1.7.1 on `PATH` (or `ANVIL_BIN`) and loopback ports. They run fixture contracts, Node and Bare locally.

[Source](https://github.com/propeller-heads/fynd/tree/main/clients/typescript/wdk-swidge) · [Fynd support](https://t.me/+B4CNQwv7dgIyYTJl) · [Security reports](SECURITY.md)

Copyright 2026 PropellerHeads. Licensed under [Apache 2.0](LICENSE).
