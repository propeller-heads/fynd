---
icon: coins
---

# Fynd Fees

Fynd charges a fee when you execute a swap. Quotes are free.

The default Fynd fee is 0.1 bps (0.001%) of swap output. Contact us for volume discounts.

Negotiated rates are keyed to your address. When your users submit swaps from their own wallets,
[identify yourself with zero-fee client fee params](client-fees.md#identify-as-a-client-without-charging-a-fee)
so the rates apply.

If you charge your own swap fees, Fynd also takes 20% of those fees. See [Charge Fees on your Swaps](client-fees.md).

## Fee breakdown

Quotes with encoding include a `fee_breakdown` with the exact amounts.

`amount_out` is the raw pre-fee swap output: what the route produces before router or client fees. It is **not** what the user receives. The user receives at least `fee_breakdown.min_amount_received` on-chain.

Fynd mirrors the on-chain `FeeCalculator` with identical integer arithmetic, then uses the result for `minAmountOut` in the encoded transaction.

Given `amount_out`, `router_fee_bps`, and `slippage`:

```
1. router_fee        = amount_out * router_fee_bps / 10,000
2. amount_after_fees = amount_out - router_fee
3. max_slippage      = amount_after_fees * slippage
4. min_amount_received = amount_after_fees - max_slippage
```

All response fields use output token units:

| Field                 | Description                                                   |
| --------------------- | ------------------------------------------------------------- |
| `router_fee`          | Fynd fee                                                      |
| `client_fee`          | `0` unless you [charge fees on your swaps](client-fees.md)    |
| `max_slippage`        | Slippage allowance on the post-fee amount                     |
| `min_amount_received` | On-chain minimum the user receives (`minAmountOut` in the tx) |

Invariant without client fees: `amount_out = router_fee + max_slippage + min_amount_received`

### Example

Example: 1,000,000 USDC output, 0.1 bps Fynd fee, 1% slippage:

```
router_fee           = 1,000,000 * 0.1 / 10,000         = 10
amount_after_fees    = 1,000,000 - 10                   = 999,990
max_slippage         = 999,990 * 0.01                   = 9,999
min_amount_received  = 999,990 - 9,999                  = 989,991
```

## Charge Fees on your Swaps

Fynd fees are separate from integrator fees. If you want to monetize your swap flow, see [Charge Fees on your Swaps](client-fees.md).

When you add a client fee, `router_fee` also includes Fynd's 20% share of that client fee.
