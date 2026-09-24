# Supported router deployments

Checked 24 September 2026. This record covers router identity and outer-call checks; nested DEX routes need separate settlement tests.

## Source and deployments

Fynd `9acb371d` pins `tycho-execution 0.423.0`. The [crate](https://crates.io/api/v1/crates/tycho-execution/0.423.0/download) SHA-256 matches Cargo.lock: `48d8746474d9bc872e3a9d5ed28da8f2835ddc0ac659fa04ce5e44939929c680`. It records Tycho commit `8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39`.

| Chain | ID | Router | Runtime Keccak-256 |
|---|---:|---|---|
| Ethereum | 1 | `0x1644D2477f809cc2C71bCCFd6Dc9497E3F83210d` | `0x4c632d15966cd3da920d6608e1aea264228933b3d3879dffd745c33fa90cc933` |
| Base | 8453 | `0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67` | `0xf8c8c80c5f75fe8a3d91d994309a921248c43031c229ef3220bbb0aed9f1ea35` |

The [deployment registry](https://github.com/propeller-heads/tycho/blob/8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39/crates/tycho-execution/config/router_addresses.json) supplies these addresses independently of `/info`. `eth_getCode` at `https://ethereum-rpc.publicnode.com` and `https://mainnet.base.org` returned 22,390-byte runtimes.

Base's [Sourcify exact match](https://sourcify.dev/server/v2/contract/8453/0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67?fields=all), dated 7 September 2026, uses Solidity 0.8.33, viaIR, optimizer 1000 and Cancun. TychoRouterV3, Dispatcher, TransferManager and Vault match the crate. Sourcify reports no proxy.

Ethereum had no Sourcify record. Its runtime differs from Base by 54 bytes, all in compiler-declared EIP-712 immutables: chain ID, router address and domain separator. Addresses and IDs decode correctly; ethers reproduces each `TychoRouter`, version `1` domain separator. All other instructions and compiler metadata match. This is bytecode-equivalence evidence, not separate Ethereum explorer verification.

## ABI and checks

[TychoRouterV3](https://github.com/propeller-heads/tycho/blob/8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39/crates/tycho-execution/contracts/src/TychoRouterV3.sol) defines:

```
0x0c1a0ee7 singleSwap(uint256,address,address,uint256,uint256,address,(uint32,address,uint256,uint256,bytes),bytes)
0x3c226834 sequentialSwap(uint256,address,address,uint256,uint256,address,(uint32,address,uint256,uint256,bytes),bytes)
0xfe745a0f splitSwap(uint256,address,address,uint256,uint256,uint256,address,(uint32,address,uint256,uint256,bytes),bytes)
```

Arguments are input amount, input token, output token, gross expected output and post-fee minimum; split adds token count. Recipient, client-fee tuple and route bytes follow. The tuple contains fee units, receiver, maximum contribution, deadline and signature.

The adapter checks destination, selector, tokens, amounts, recipient, minimum, native value and zero client-fee/signing fields. It preserves trailing watermarks and sends only `to`, `value` and `data` to WDK.

- Native tokens map from Fynd's zero address to `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE`. Native value equals input amount; ERC-20 value is zero.
- Approvals target the router. [TransferManager](https://github.com/propeller-heads/tycho/blob/8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39/crates/tycho-execution/contracts/src/TransferManager.sol) binds caller/token and caps cumulative withdrawals at the declared amount.
- The router checks minimum output after fees and final transfer. Router recipients receive vault credit, so the adapter rejects them.
- Unsigned fee parameters are `(0, zeroAddress, 0, uint256.max, emptyBytes)`. A zero fee receiver disables deadline enforcement: these quotes have no on-chain expiry.
- The validator does not inspect nested route bytes, price the trade or verify the account network. Fynd, router/executor behavior and wallet configuration remain trust dependencies.

## Fees

Read-only observations on 24 September:

| Chain | Active calculator | Fee-unit denominator | Default output fee | Positive-slippage capture |
|---|---|---:|---:|---|
| Ethereum | `0x2930cb02f8293a684981ef7fb63c160329c410f1` | 100,000,000 | 0 | Enabled |
| Base | `0x4c95d7678be1668fca8205f0d076b5e872b715e1` | 100,000,000 | 0 | Enabled |

Rates are mutable; per-client overrides were not queried. Zero defaults do not establish zero fees for every wallet. The adapter reads fees from the quote.

[FeeCalculator](https://github.com/propeller-heads/tycho/blob/8be328bfc02c6f46cf39f60ef8b31cbb5d1b0d39/crates/tycho-execution/contracts/src/FeeCalculator.sol) adds captured surplus to router fees, so quoted caps do not bound settled surplus fees. The active calculators lacked Sourcify records; their source-to-runtime identity remains unverified. Executors and calculators can change through authorized upgrades despite the non-proxied router.

## Tests and upgrades

Router tests cover all selectors, an upstream Rust-generated fixture, watermarks, native assets, minimums and mismatches. WDK account tests use simplified contracts on Anvil; see [fixtures](../tests/fixtures/README.md). They do not test live Tycho settlement.

For upgrades, verify the Fynd pin and served schema; check changed addresses against registry, source and deployed bytecode. Compare ABI, funding, fees, recipients and watermark behavior, then update constants and fixtures. Unknown selectors must fail. Rerun API, account, Node/Bare and packed-consumer tests; verify hosted execution before advertising new chain support.
