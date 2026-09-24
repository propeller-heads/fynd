# Supported router deployments

Verified 24 September 2026. This record covers router identity and the adapter's outer-call checks. It does not certify every nested DEX route or replace hosted-chain settlement testing.

## Source and deployment evidence

Fynd commit `9acb371d` pins `tycho-execution 0.423.0`. Its [published crate](https://crates.io/api/v1/crates/tycho-execution/0.423.0/download) SHA-256 matches Cargo.lock: `48d8746474d9bc872e3a9d5ed28da8f2835ddc0ac659fa04ce5e44939929c680`. The crate records Tycho commit `8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39`.

| Chain | ID | Router | Runtime Keccak-256 |
|---|---:|---|---|
| Ethereum | 1 | `0x1644D2477f809cc2C71bCCFd6Dc9497E3F83210d` | `0x4c632d15966cd3da920d6608e1aea264228933b3d3879dffd745c33fa90cc933` |
| Base | 8453 | `0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67` | `0xf8c8c80c5f75fe8a3d91d994309a921248c43031c229ef3220bbb0aed9f1ea35` |

Addresses come from the [published deployment registry](https://github.com/propeller-heads/tycho/blob/8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39/crates/tycho-execution/config/router_addresses.json), independently of Fynd `/info`. Read-only `eth_getCode` at `https://ethereum-rpc.publicnode.com` and `https://mainnet.base.org` returned 22,390-byte runtime programs.

Base has a [Sourcify exact-match record](https://sourcify.dev/server/v2/contract/8453/0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67?fields=all), verified 7 September 2026, compiled with Solidity 0.8.33, viaIR, optimizer 1000, Cancun. Its TychoRouterV3, Dispatcher, TransferManager and Vault files exactly match the crate; Sourcify reports no proxy.

Ethereum has no separate Sourcify record. Comparing its runtime with Base found only 54 different bytes, all within compiler-declared EIP-712 immutables: chain ID, router address and domain separator. Both addresses and IDs decode correctly; recomputing each `TychoRouter`, version `1` EIP-712 domain with ethers matches its separator. All other instructions and compiler metadata match. This is bytecode-equivalence evidence, not a claim of a separate Ethereum explorer verification.

## Outer ABI and enforcement

[Router Solidity](https://github.com/propeller-heads/tycho/blob/8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39/crates/tycho-execution/contracts/src/TychoRouterV3.sol) supplies these supported selectors:

```
0x0c1a0ee7 singleSwap(uint256,address,address,uint256,uint256,address,(uint32,address,uint256,uint256,bytes),bytes)
0x3c226834 sequentialSwap(uint256,address,address,uint256,uint256,address,(uint32,address,uint256,uint256,bytes),bytes)
0xfe745a0f splitSwap(uint256,address,address,uint256,uint256,uint256,address,(uint32,address,uint256,uint256,bytes),bytes)
```

Common arguments: input amount, input token, output token, gross expected output, post-fee minimum; split adds token count; all then have recipient, client-fee tuple and opaque route bytes. Client tuple fields are fee units, receiver, maximum contribution, deadline and signature.

The adapter checks the supported destination and selector, tokens, amounts, recipient, minimum, native value and zero client-fee/signing fields. It preserves trailing Fynd watermark bytes. Other response transaction fields do not pass through to WDK signing.

- Fynd zero-address native tokens map to router sentinel `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE`. Native input requires value equal to the input amount; ERC-20 input requires zero value.
- ERC-20 approval goes to the router. [TransferManager](https://github.com/propeller-heads/tycho/blob/8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39/crates/tycho-execution/contracts/src/TransferManager.sol) binds the original caller and input token and caps cumulative withdrawals at the declared amount.
- Minimum output is checked after fees and the final transfer. Router-as-recipient instead credits a vault balance, so this wallet-focused adapter rejects it.
- Normal unsigned fee parameters are `(0, zeroAddress, 0, uint256.max, emptyBytes)`. Solidity ignores the deadline when the fee receiver is zero: these quotes have **no enforced on-chain expiry**. Fresh execution quotes do not create an expiry guarantee.
- The outer validator does not decode opaque pool/executor bytes, independently price the quote or verify the WDK account's network. Trusted Fynd routing, supported router/executor behavior and correct wallet configuration remain required.

## Fee observations

Read-only calls on 24 September returned:

| Chain | Active fee calculator | Fee-unit denominator | Default output fee | Positive-slippage capture |
|---|---|---:|---:|---|
| Ethereum | `0x2930cb02f8293a684981ef7fb63c160329c410f1` | 100,000,000 | 0 | Enabled |
| Base | `0x4c95d7678be1668fca8205f0d076b5e872b715e1` | 100,000,000 | 0 | Enabled |

These are mutable observations. Per-client overrides were not queried; zero default does not imply zero fees for every wallet. The adapter uses the API breakdown rather than hardcoding the documented/fallback 0.1bps.

The [published FeeCalculator](https://github.com/propeller-heads/tycho/blob/8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39/crates/tycho-execution/contracts/src/FeeCalculator.sol) adds captured positive surplus to router fees. A cap on the quoted fee is therefore not a ceiling on final fees including surplus. The active calculators were not Sourcify-listed; their source-to-runtime identity was not established, so reads establish state only. Executors and fee calculators remain authorized upgrade boundaries even though router instructions are not proxied.

## Validation and upgrade procedure

The router unit suite covers all three selectors, an untouched upstream Rust-generated calldata fixture, watermarks, native input/output, minimum boundaries and mismatches. The WDK-account suite exercises published WDK registration, real signing, approvals, receipt waits and status against an ephemeral Anvil chain with simplified fixture contracts. It is **not live Tycho or Fynd settlement validation**; see [fixture details](../tests/fixtures/README.md).

For any API/router/dependency upgrade:

1. Verify the Fynd dependency pin and served wire contract.
2. Authenticate changed deployment addresses using the independent registry plus verified source/deployed bytecode; `/info` alone is insufficient.
3. Compare ABI, funding, fee and recipient semantics. Update the bounded constants/ABI module and fixtures deliberately; unknown selectors must fail.
4. Rerun public API, real-account, Node/Bare and packed-package checks. Perform separately authorized hosted-chain checks before changing advertised execution support.
