---
description: Real-time in-block solver for Ethereum L1 and L2s. Powered by Fynd.
icon: foot-wing
---

# Fynd In-Block Solver

## Introduction

Today, trades execute **\~0.5-30s** after you commit to a route. Stale routes lead to reverts and bad prices.

The solution is to re-route your trade real-time: Fynd in-block helps builders and sequencers re-solve your trade at the exact position it executes: _real-time solving._

## How to re-solve real-time

{% tabs %}
{% tab title="Aggregator / Wallet" %}
**Benefit:** Up to 50% fewer reverts and less slippage for traders.

**How:** Send your tx to the `FyndJITRouter` contract instead of `TychoRouterV3`. Your swap calldata stays the same.\
\
If you use Fynd API, the transaction it returns can already target JIT router - just [talk to us](https://t.me/tanay_j). \
\
(_Over the next weeks Fynd In-Block re-solving will become the default on Fynd API_).

Users approve `FyndJITRouter` instead of `TychoRouterV3`, both with standard ERC20 approvals and with Permit2 flow.

<figure><picture><source srcset="../.gitbook/assets/inblock-docs-smart-contract-dark.png" media="(prefers-color-scheme: dark)"><img src="../.gitbook/assets/inblock-docs-smart-contract-light.png" alt=""></picture><figcaption></figcaption></figure>
{% endtab %}

{% tab title="Builder / Sequencer" %}
**Benefit:** Improve users' price and earn more tips from the same txs.\
\
**How:** Fynd In-Block is a side-car that you run alongside your builder. Fynd returns you a route-update transaction that you place right before the swap. You get paid via coinbase transfer, proportional to how much you improve the swap.

_(Alpha Rollout) Titan, BuilderNet, Quasar and Eureka (together \~85% of Ethereum blockspace) are currently already integrating Fynd In-Block Solver._\
\
Write Tanay on Telegram ([https://t.me/tanay\_j](https://t.me/tanay_j)) to integrate the Fynd In-Block Solving side-car in your builder/sequencer.

<figure><img src="../.gitbook/assets/inblock-logos-builders.png" alt=""><figcaption></figcaption></figure>
{% endtab %}

{% tab title="Trader" %}
**Benefit:** Get the best price available in the block.&#x20;

**How:** Use any of the swap UIs that will support Fynd In-Block Solving: Definitive, Relay, Bungee, Jumper, Fynd, Flashnet and Rhinestone. \
\
(_currently in Alpha, rolling out over the next weeks)._

<figure><img src="../.gitbook/assets/inblock-logos-routers.png" alt=""><figcaption></figcaption></figure>

_If you trade with Fynd locally or via the Fynd API, see the instructions for aggregators._
{% endtab %}
{% endtabs %}

## How does it work?

Builders run Fynd while they build a block.&#x20;

Before the builder adds a swap transaction to the block, it asks Fynd for a better route, based on the state of the block built so far.

If Fynd finds a better route, the builder stores it on-chain, in a transaction placed right before the swap. When the swap executes, FyndJITRouter picks up the stored route and runs it instead of the original route.

If no route is stored, FyndJITRouter runs the original route, exactly as if you had sent the swap to TychoRouterV3.

<figure><img src="../.gitbook/assets/inblock-how-it-works-flow-v2-jit-router.jpg" alt=""><figcaption><p>If Fynd finds a better route, the builder stores it and FyndJITRouter runs it. Otherwise, your swap runs as sent.</p></figcaption></figure>

## Better price guarantee

The builder only stores a route. It never trades against you.

Fynd InBlock never proposes a route that gives the trader a worse price than their original route. The FyndJITRouter contract also enforces the original `minAmountOut`, no matter which route runs.

On the Fynd InBlock dashboard (coming soon), you can compare each original route with the re-routed result.

Re-routing is free for traders, builders and aggregators.

A share of the extra output from improved routes goes to the builder and the protocol. If there is no surplus, nobody is paid and you swap as usual.

## FAQ

### Does the builder sandwich or front-run my swap?

No. The builder adds one transaction right before your swap. That transaction only stores a route for your order. It does not trade and moves no tokens.

The stored route is bound to your order in that block. It cannot be used for any other swap, and a route left over from an earlier block has no effect.

### Do users sign anything different?

No. There is no intent and no new signature type. The swap calldata is the same as for `TychoRouterV3`. Only the destination address changes.

Users approve `FyndJITRouter` once, with a standard ERC20 approval or with Permit2. `FyndJITRouter` sits behind a proxy, so approvals stay valid when the contract is upgraded.

### How does the builder swap in a better route?

1. The builder sees a swap sent to `FyndJITRouter`.
2. It asks Fynd to re-solve the trade on the state of the block at the position where the swap will run.
3. Fynd compresses the better route, usually into a single 32-byte storage word.
4. The builder stores that route in a transaction placed right before the swap.
5. When the swap runs, `FyndJITRouter` finds the route stored for this exact order in this exact block and runs it.

If no route is stored, the original route runs.

### What happens if re-routing fails?

Your original route runs, exactly as if you had sent the swap to `TychoRouterV3`. Every failure on the re-routed path (no stored route, a route for another order, a failed decompression, a revert inside the new route) falls back to the original route.

`minAmountOut` is enforced the same way on both paths.

### How much extra gas does it cost?

**For the swapper:** sending the swap to `FyndJITRouter` instead of `TychoRouterV3` costs about **27.7k more gas when no route is stored** and about **67.8k more gas when a better route runs**. This was measured on a mainnet fork in October 2026, for WETH to USDC through one Uniswap V3 pool, with cold accounts and slots.

At the median Ethereum gas price over the 1,024 blocks before 29 October 2026 (0.066 gwei, ETH at $2,323), that is about $0.004 without a re-route and $0.010 with one. At the 90th-percentile gas price (0.145 gwei), it is about $0.009 and $0.023.

**For the builder:** the builder pays the gas of the route-store transaction, about 39k gas for a one-word route. Fynd pays the builder back from the improvement. If a re-route would not cover its own cost, the builder leaves it out of the block.

### What are the trust assumptions?

- **Only allowlisted builders can store routes.** There is no permissionless store.
- **The worst case is your own slippage limit.** A compromised builder key can at most deliver your `minAmountOut` on a swap that would have paid more. It cannot exceed the slippage you accepted, and it cannot send the output anywhere else.
- **Upgrades are timelocked.** Only a multisig, through a timelock with a 24-hour delay, can upgrade `FyndJITRouter`. You can watch the timelock's `CallScheduled` events and read the new implementation a day before it goes live.
- **Swaps can be paused.** In an emergency, Fynd can pause swaps on `FyndJITRouter`. While paused, you can still send swaps to `TychoRouterV3` directly.
- **Client fees are kept.** If you charge a client fee, it applies on the re-routed path too.

### When does in-block re-solving help compared to running Fynd locally?

Running Fynd locally or calling the Fynd API gives you the best route **at the moment you quote**. Fynd In-Block re-solves the trade **at the moment it executes**.

The longer the gap between quote and execution, and the more prices move in that gap, the more in-block re-solving adds. The two work together: quote with Fynd, then send the swap through `FyndJITRouter`.

## Contract addresses

<table><thead><tr><th width="230">Chain</th><th>FyndJITRouter address</th></tr></thead><tbody><tr><td>Ethereum Mainnet</td><td>0x5BEFE2A5d993dcB63486ff04deDB1BE9DfB55e56</td></tr></tbody></table>
