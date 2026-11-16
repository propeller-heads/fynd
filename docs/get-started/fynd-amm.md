---
description: >-
  Concentrated liquidity pools that charge every trade just under the highest
  fee that still wins it.
icon: water-ladder
---

# Fynd AMM

## What is Fynd AMM?

<figure><img src="../.gitbook/assets/assetsfynd-amm-price-to-beat.png" alt=""><figcaption></figcaption></figure>

A Fynd AMM pool is a normal concentrated liquidity pool (like a Uniswap v4 or Ekubo pool) with one difference: **Fynd AMM adjusts its fee per trade to outcompete other pools**.

Other AMMs or hooks have fixed fees or adjust their fee per block or even per trade, but are blind to what competing pools bid.

Fynd AMM sets its fee just-in-time. Fynd compares the best public route with the route through the Fynd AMM pool, and the pool charges 90% of the difference. The trader keeps the other 10%, so whenever the pool can improve on the public route, it wins the trade.

* **Low competition:** the trade pays a high fee.
* **High competition:** the trade pays a low fee, and the pool still wins it.

Everything else works like any concentrated liquidity position: pick a range, deposit, and earn fees on the trades your liquidity fills.

The first Fynd AMM pools run on [Ekubo](https://ekubo.org/), the most gas-efficient concentrated liquidity AMM. Curve and Metric pools are in the works.

Deposit in Fynd AMM pools: [swap.fynd.xyz/pools](https://swap.fynd.xyz/pools)

## Why competition-aware pools earn more

Every trade is an auction between pools. A router asks every pool "how much USDC do you pay for 1 SPY?" and the best offer wins.

Say SPY trades at 760 USD. AMMs and propAMMs answer:

* Metric: 759.65 USDC
* Uniswap: 759.82 USDC
* Aerodrome: 759.85 USDC

Aerodrome wins. But Aerodrome bid more than it needed: 759.83 would have won too, and its LPs would have kept 2 cents more. Metric lost the trade, even though a bid of 759.86 would have won it at a 14 cent profit.

Fynd AMM fixes this. Fynd looks up the best competing offer and tells Fynd AMM, which beats it and keeps 90% of the difference as fee. See [How the fee is set](fynd-amm.md#how-the-fee-is-set).

<figure><img src="../.gitbook/assets/assetsfynd-amm-static-bids.png" alt=""><figcaption></figcaption></figure>

As a result traders get at least the best public price, plus 10% of what the pool adds, and the Fynd AMM LPs earn most of the pool's edge on every trade.

<figure><img src="../.gitbook/assets/assetsfynd-amm-price-curve-crossing.gif" alt="Fynd AMM adjusts its bid per trade."><figcaption><p>Fynd AMM adjusts its bid per trade.</p></figcaption></figure>

## How the fee is set

For every quote, Fynd solves the trade twice:

1. **Public route:** the best route using public liquidity only. This is the price to beat.
2. **Fynd AMM route:** the best route that can also use the Fynd AMM pool, priced at zero fee.

If the Fynd AMM route pays out more, the difference is the pool's **edge**. Fynd splits it:

* **10% to the trader,** on top of what the public route pays.
* **90% is the fee.** The pool takes it from its own leg of the trade, in the output token. LPs get 80% of the fee and Fynd keeps 20%.

<figure><picture><source srcset="../.gitbook/assets/fynd-amm-fee-waterfall-dark.png" media="(prefers-color-scheme: dark)"><img src="../.gitbook/assets/fynd-amm-fee-waterfall-light.png" alt="Waterfall: the best public route pays 759.85 USDC. The Fynd AMM route at zero fee pays 0.10 more. The fee takes 0.09, 90% of the edge: 0.072 to LPs and 0.018 to Fynd. The trader gets 759.86."></picture><figcaption><p>Illustrative: the SPY order from above. Say that at zero fee the Fynd AMM route would pay 759.95, an edge of 0.10 over Aerodrome. The pool charges 0.09 and the trader gets 759.86.</p></figcaption></figure>

The trader is never quoted less than the public route. If the Fynd AMM route has no edge, Fynd uses the public route and the pool doesn't trade.

The fee grows with the edge, not with the trade size. When public pools charge more, are shallower or price the trade worse, the edge is bigger and the pool earns more. When competition is tight, the fee is close to zero and the pool still wins the trade.

### Split and multi-hop routes

The pool doesn't have to fill a whole trade. Fynd can send part of a trade through the pool and the rest through public pools, or use the pool for one hop of a longer route (for example SPY → USDC → ETH).

* The edge is measured on the **whole route**. If the pool's slice lowers the price impact on the rest of a large trade, that saving counts as edge.
* The fee is taken only from the **pool's leg**. Public legs pay out in full.
* Gas counts: the edge is measured net of each route's gas cost.

### When there is no public route

If Fynd finds no public route for a trade, there is no price to beat. The pool then charges a flat 5 bps on its leg.

## How to provide liquidity

1. **Pick a pool** on [swap.fynd.xyz/pools](https://swap.fynd.xyz/pools). Each pool shows TVL, 24h volume, 24h fees, 24h APR, and the range of fees traders paid in the last 24 hours.
2. **Click Deposit.** This opens Ekubo's "new position" page with the Fynd AMM pool preselected.
3. **Choose your price range** and the amounts of both tokens, then confirm the transaction in your wallet. This creates a standard Ekubo position NFT in your wallet.
4. **Monitor** your position on the Fynd pools page (every trade, the fee it paid, and what LPs earned) and on Ekubo.
5. **Withdraw anytime** from Ekubo. There is no lockup.

{% hint style="warning" %}
Always deposit through the Deposit link on the Fynd pools page. Ekubo's generic "Add liquidity" button creates a position in a different pool (same tokens, no extension) that Fynd does not route to. You can identify a Fynd AMM pool by its token pair, a pool fee of `0`, tick spacing `1024`, and the Fynd `SignedExclusiveSwap` extension address listed in [Contracts](fynd-amm.md#contracts).
{% endhint %}

### Fees and earnings

* Every trade pays the fee Fynd sets for it. Fynd routes exact-input swaps, so the fee is taken in the output token.
* Fynd keeps a 20% owner fee. The rest goes to in-range LPs, pro rata to their liquidity, like on any Ekubo pool.
* Collect fees from your Ekubo position at any time.

### Risks

Fynd AMM pools carry these risks on top of the usual concentrated liquidity risks.

* **Fynd controls the fee, but not your funds.** Fynd signs the fee for each swap. It cannot withdraw, move or lock your liquidity; your position is a standard Ekubo position that only you can withdraw. Fynd can't set the pool's price; it only moves through swaps.
* **Smart contract risk.** Ekubo Core, the Ekubo positions contract, and the `SignedExclusiveSwap` extension. See [Audits](fynd-amm.md#audits).

***

## How it works

The rest of this page is for readers who want the mechanics: who can trade on the pool, what Fynd signs, and what the contracts enforce.

### Overview

<figure><img src="../.gitbook/assets/assetsfynd-amm-ekubo-extension-flow.png" alt=""><figcaption></figcaption></figure>

1. A trader (via a wallet, aggregator or Fynd InBlock) asks Fynd for a quote.
2. Fynd solves the order twice: once with public liquidity only, and once including the Fynd AMM pool at zero fee.
3. If the Fynd AMM route is better, Fynd sets the fee to 90% of the difference (see [How the fee is set](fynd-amm.md#how-the-fee-is-set)).
4. Fynd signs the fee, a deadline, a nonce, and the expected trade size, and encodes the signature into the swap calldata.
5. The trader sends the transaction to the Tycho Router.
6. The router swaps through Ekubo Core, which forwards the swap to the `SignedExclusiveSwap` extension.
7. The extension checks the signature, deadline and nonce, runs the swap at the pool's zero base fee, and takes the signed fee for the LPs.

Any swap that does not carry a valid Fynd signature reverts.

<figure><img src="../.gitbook/assets/assetsfynd-amm-sequence.png" alt=""><figcaption></figcaption></figure>

### Who can trade on the pool

Fynd AMM pools are exclusive to Fynd. Trades reach them through:

* the Fynd API, for integrators enabled for Fynd AMM routing;
* Fynd InBlock, which re-solves swaps inside the block and can route them through Fynd AMM pools when that improves the trade;
* the Fynd swap UI at [swap.fynd.xyz](https://swap.fynd.xyz/).

The more flow reaches Fynd, the more trades Fynd AMM pools can win. Deeper pools win larger trades, which brings more fees and more LPs.

### Pool price and the market price

Because only Fynd-signed swaps can trade on the pool, nobody else arbitrages it. Its price only moves when Fynd routes a trade through it, so it can drift away from the market.

On a normal pool a stale price is a loss for LPs: an arbitrageur trades against it at the old price and keeps the difference. On a Fynd AMM pool it isn't:

* The price to beat is the best public route, not an external market price. When a trade crosses a gap between the pool's price and the public pools' prices, that gap is part of the pool's edge, so 90% of it is charged as fee and goes to the LPs (after the owner fee). If the public pools have drifted too, the edge is only what the pool offers beyond them.
* Fynd also runs a rebalancing bot that keeps the pool aligned with the market. It trades through the pool and back through a public pool, and its swaps go through Fynd like any other trade. So Fynd charges them the same per-trade fee, and most of the price gap the bot closes goes to the LPs as fees. When the gap is small, that fee is close to zero. These swaps show up in the pool's trades and count toward volume and fees, but the pools page excludes them from the "Swap fee 24h" range.

The gap still matters for LPs in one way: the inventory you hold while the pool lags the market moves with the market. That is ordinary inventory risk, not a fee loss.

### What is signed

Fynd signs an EIP-712 message per swap, which the extension verifies on-chain. It covers:

* **Pool ID:** the signature is valid for one pool only.
* **Fee:** the fee rate for this swap (Q32 fixed point).
* **Deadline:** the signature expires after a short window (about two minutes).
* **Nonce:** each signature can be used once; the extension tracks used nonces in a bitmap.
* **Authorized locker:** only the Tycho Router can execute the swap, so a signature can't be lifted into another contract.
* **Minimum balance update:** bounds on the direction and size of the swap, so a signature quoted for a $1k trade can't be spent on a much larger one.

### Contract guarantees for LPs

The Ekubo extension enforces these properties, not Fynd's off-chain code:

* **No unsigned trades.** The pool is forward-only: direct swaps revert. Nobody can trade against the pool without a fresh Fynd signature.
* **The fee can't be negative.** The pool's base fee is zero and the signed fee is added on top. A trade can never get a better price than the pool's own curve.
* **Fynd has no custody.** LP funds sit in standard Ekubo positions owned by the LPs. The controller key can only sign fees for swaps.
* **Fees are paid out per timestamp.** The extension holds swap fees and pays them to the liquidity in range at the pool's next interaction (a swap, deposit or withdrawal) with a later block timestamp. On chains with sub-second blocks, several blocks share one timestamp. A withdrawal collects the fees you are owed, except fees from swaps with the same timestamp as your withdrawal. If no liquidity is in range when fees are paid out, they are lost.

### Contracts

Ekubo `SignedExclusiveSwap` extension (Fynd controller), at the same address on Ethereum, Robinhood Chain, Base and BSC: `0x550CFe1292d0D9c0d28f232b3F42Edb4439B1D09`

Live pools (Ekubo pool IDs), as listed on [swap.fynd.xyz/pools](https://swap.fynd.xyz/pools):

* Ethereum, ETH/USDC: `0x158035970f75e9e11b86d8f6ac43e95d8c3b4a88faa1e77b41570ea0b8430360`
* Robinhood Chain, AI/USDG: `0x46e76723bc9334ec559b6c1302a8ac15bef0edbdc42ce139a642df711eae9715`
* Robinhood Chain, ETH/USDG: `0xb67e0601a0363a1ad259dd848cad1c52d6cc4eb18bc526a0dd4f4bdf83773546`
* Robinhood Chain, NVDA/USDG: `0xa9a97762fae3ec95a9b7e51ac6844a1270eee0a223ae69ce598950990ba7becb`
* Robinhood Chain, PONS/USDG: `0xbc2615db1f10ca2f2cc9d491fe697ac8a11290f8c900fad313be53bb6763df79`
* Base, DRV/USDC: `0x628dc44c0a6a6e4a6300db8faa081ed4f391fcac359cf494bf3bbe26b49cee3a`
* Base, WETH/USDC: `0x34d55c244920f8e10638cab7086defe26974752142a894d52c9451e45c91d209`
* BSC, BNCB/USDT: `0x942e8f465b349913a0a115c8f346028f549129748184d8eac4fafc7b88c863a9`

All pools use pool fee `0` and tick spacing `1024`.

### Audits

The `SignedExclusiveSwap` extension is part of Ekubo's [evm-contracts](https://github.com/EkuboProtocol/evm-contracts) and runs on Ekubo Core, which has its own audits ([Code4rena, Riley Holterhus](https://github.com/EkuboProtocol/evm-contracts/tree/main/audits)). The extension itself has had automated security reviews, and is currently under manual audit:

* [**Cantina Apex audit scan**](https://github.com/EkuboProtocol/evm-contracts/blob/main/audits/EkuboProtocol%20Audit%20Scan%20-%20AI%20Scan.pdf) (July 2026, evm-contracts commit `7b8b0a0`): no critical, high or medium findings. One informational finding on the extension (EKUB3-64: zero-amount swaps consume a nonce).
* [**Extension review**](https://github.com/EkuboProtocol/evm-contracts/blob/main/audits/SignedExclusiveSwap-Extension-Audit.md) (June 2026): no critical or high findings; Ekubo fixed the low-severity items before deployment.
* [**V12 review of the owner fee change**](https://github.com/EkuboProtocol/evm-contracts/pull/373) (September 2026): one issue fixed (input overflow on exact-output swaps), one acknowledged by design (fees go to the liquidity in range when they are paid out, see [Contract guarantees for LPs](fynd-amm.md#contract-guarantees-for-lps)).

### Further reading

* [Fynd In-Block Solver](fynd-in-block-solver.md)
