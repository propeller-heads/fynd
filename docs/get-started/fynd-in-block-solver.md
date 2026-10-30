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

Gas overhead, compared with a direct `TychoRouterV3` call of the same route, is \~28k gas (if the original route ran) and \~68k gas (if a new route ran) on Ethereum Mainnet. At October 2026 prices, it's \~$0.004 and \~$0.010, respectively.

A share of the extra output from improved routes goes to the builder and the protocol. If there is no surplus, nobody is paid and you swap as usual.

## Contract addresses

<table><thead><tr><th width="230">Chain</th><th>FyndJITRouter address</th></tr></thead><tbody><tr><td>Ethereum Mainnet</td><td>0x5BEFE2A5d993dcB63486ff04deDB1BE9DfB55e56</td></tr></tbody></table>
