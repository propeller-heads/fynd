---
description: Route your users' swaps through Fynd InBlock. Keep your aggregators, quotes and fee.
---

# Fynd InBlock for Swap Frontends

## What Fynd InBlock gives you

Fynd InBlock executes your users' swaps with a better route when the block has one. You keep your aggregators and your quotes. You change only where the transaction goes and which contract the user approves.

When you route your users' swaps through Fynd InBlock, you get:

* **More swaps that go through.** If prices move and the quoted route can no longer give the minimum output, a better route can still give it.
* **Your fee on each swap.** You get your fee when the quoted route executes and, if Fynd InBlock knows your fee, when Fynd InBlock replaces the route.
* **No loss for your users.** Users always get at least the output of the quoted route.
* **One change for all supported routers.** The same change works for the Fynd router and for each supported aggregator router.

## How a swap executes

<figure><picture><source srcset="../.gitbook/assets/inblock-frontends-flow-dark.png" media="(prefers-color-scheme: dark)"><img src="../.gitbook/assets/inblock-frontends-flow-light.png" alt="Fynd InBlock executes a better route, or the quoted route as sent"></picture><figcaption><p>How a swap executes through Fynd InBlock</p></figcaption></figure>

Your app sends the swap to the Fynd InBlock router (`FyndJITRouter` contract) with the quoted calldata. If the builder stored a better route, the Fynd router (`TychoRouterV3` contract) executes it. If not, or if the better route fails, the aggregator router executes the swap as sent.

## Where the money comes from

<figure><picture><source srcset="../.gitbook/assets/inblock-frontends-split-dark.png" media="(prefers-color-scheme: dark)"><img src="../.gitbook/assets/inblock-frontends-split-light.png" alt="You get your fee on both routes"></picture><figcaption><p>Who gets what on each path (not to scale)</p></figcaption></figure>

Fynd InBlock pays all shares from the surplus, the extra output of the better route. The user pays no fee for a replacement. If there is no better route, there is no surplus, and nobody gets a share.

| Party      | When the quoted route executes | When Fynd InBlock replaces the route    |
| ---------- | ------------------------------ | --------------------------------------- |
| User       | The output of the quoted route | At least the output of the quoted route |
| You        | Your fee                       | Your fee, if Fynd InBlock knows it      |
| Aggregator | Its router fees                | A share of the surplus                  |
| Builder    | Nothing                        | A share of the surplus                  |
| Fynd       | Nothing                        | The rest of the surplus                 |

## Integration steps

Pick one of two paths.

### Path A: through your aggregator

Use this path when your aggregator supports Fynd InBlock. The aggregator API then prepares the transaction for you.

1. Check with your aggregator or the Fynd team that your aggregator supports Fynd InBlock.
2. Request Fynd InBlock quotes from the aggregator API. They differ from normal quotes in three ways:
   * The transaction goes to the Fynd InBlock router, not to the aggregator router.
   * The quote names the Fynd InBlock router as the caller (the taker) and the user as the receiver.
   * The address for the token approval is the Fynd InBlock router, not the aggregator router.

### Path B: through your own contract

Use this path when your swaps go through your own contract. You prepare the calldata yourself, so your aggregators do not need to support Fynd InBlock.

1. Check that Fynd InBlock has a decoder for each aggregator router you use. If one is missing, you or the Fynd team can write it.
2. Ask each aggregator API for a quote with the Fynd InBlock router as the caller (the taker). Set the receiver to the user or to your contract.
3. Change your contract to call the Fynd InBlock router, not the aggregator router. The calldata is `jitSwap(aggregatorRouter, quoteCalldata)`.
4. If the decoder cannot read all the swap data from the quote calldata, send the missing data as an explicit order. The calldata is then `jitSwapWithOrder(aggregatorRouter, quoteCalldata, explicitOrder)`.
5. Approve the Fynd InBlock router for the input token from your contract. The router takes the input tokens from your contract and sends unused input tokens back to it.

### Your fee on a replaced swap

Fynd InBlock preserves your fee only when it knows it. It learns the fee from the original calldata, or from an explicit order. In path A, the aggregator API sends the explicit order. In path B, you send it.

On a replaced swap, the Fynd router pays this fee in the output token, as a balance in the Fynd router. You can withdraw this balance at any time.

{% hint style="warning" %}
If Fynd InBlock does not know your fee, you get no fee on a replaced swap.
{% endhint %}

In path B, your contract can also take the fee from the output after the swap. Fynd InBlock then does not need to know the fee.

## Protections for your users

* **Minimum output.** The user always gets at least the minimum output of the quote. The contract checks this on each swap.
* **Output of the quoted route.** On a replaced swap, the user gets at least the output of the quoted route. The builder calculates this value, and Fynd doesn't suggest routes that give the user less.
* **Fallback.** If the better route fails, the contract executes the quoted route as sent.
* **No balances.** The Fynd InBlock router keeps no tokens after a swap. It gives the aggregator router an approval for the exact input amount, and removes the approval after the call.
* **Only known routers.** The Fynd InBlock router calls only routers that have a registered decoder.
* **Fast stop.** Fynd can stop all replacements for one router at once. The swaps then execute as sent.

### Known limits

These swaps work through the aggregator router directly, but fail through Fynd InBlock:

* A swap to a token that takes a fee on each transfer. The receiver's balance can then increase by less than the minimum output.
* A swap to a receiver contract that sends the ETH it gets to a different address.
* A swap with a permit for the aggregator router in the calldata.
* A quote with a market maker order that names a different caller. Ask for the quote with the Fynd InBlock router as the caller.

## Questions

**What do my users see?** In path A, they approve the Fynd InBlock router once per token. In path B, their approvals do not change - they still approve your router. In both paths, they get at least the output of the quoted route.

**What happens when Fynd InBlock finds no better route?** The quoted route executes as sent. You get your fee as usual.

**Does the user pay for Fynd InBlock?** The user pays no fee for a replacement. All shares come from the surplus. The Fynd InBlock router adds some gas to each swap: about 28k gas when the quoted route executes and about 68k gas when Fynd InBlock replaces it, on Ethereum Mainnet ([details](fynd-in-block-solver.md#better-price-guarantee)).

**What if the router of my quote is not supported?** Send the swap to that router directly, as you do now. Ask your aggregator or the Fynd team to add a decoder for that router.

**Which chains are supported?** Ethereum Mainnet only, for now.

## Next step

To start, contact the Fynd team on [Telegram](https://t.me/tanay_j). The Fynd InBlock router address is on the [Fynd InBlock page](fynd-in-block-solver.md#contract-addresses).
