// Copyright (c) 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0

import { SwidgeError } from '@tetherto/wdk-wallet/protocols'
import { getAddress, Interface, ZeroAddress } from 'ethers'

/** @typedef {{ id: number, router: string, nativeSymbol: string }} Chain */

// tycho-execution 0.423.0, config/router_addresses.json. Deployment provenance
// and the release procedure are documented in docs/router-verification.md.
export const CHAINS = Object.freeze({
  ethereum: Object.freeze({ id: 1, router: '0x1644d2477f809cc2c71bccfd6dc9497e3f83210d', nativeSymbol: 'ETH' }),
  base: Object.freeze({ id: 8453, router: '0xaba5b53b03eafad1c5fc8bd5fc765fc85bb3de67', nativeSymbol: 'ETH' })
})

const ROUTER_NATIVE = '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'
const FEE_TUPLE = '(uint32 clientFeeBps,address clientFeeReceiver,uint256 maxClientContribution,uint256 deadline,bytes clientSignature)'
const PARAMETERS = 'uint256 amountIn,address tokenIn,address tokenOut,uint256 expectedAmountOut,uint256 minAmountOut'
const routerInterface = new Interface([
  `function singleSwap(${PARAMETERS},address receiver,${FEE_TUPLE} clientFeeParams,bytes swaps) payable returns (uint256)`,
  `function sequentialSwap(${PARAMETERS},address receiver,${FEE_TUPLE} clientFeeParams,bytes swaps) payable returns (uint256)`,
  `function splitSwap(${PARAMETERS},uint256 nTokens,address receiver,${FEE_TUPLE} clientFeeParams,bytes swaps) payable returns (uint256)`
])

/** Normalize WDK/Fynd native-token aliases to Fynd's zero address.
 * @param {string} token
 * @returns {string}
 */
export function normalizeToken (token) {
  const address = getAddress(token).toLowerCase()
  return address === ROUTER_NATIVE ? ZeroAddress : address
}

/** @param {string} message */
function invalid (message) {
  return new SwidgeError(`Invalid Fynd transaction: ${message}`, { reason: 'INVALID_TRANSACTION' })
}

/**
 * Check the supported router call's outer arguments before approvals or signing.
 * This does not inspect the opaque pool/executor route or independently price it.
 *
 * @param {{ amountIn: bigint, grossOutput: bigint, minimum: bigint, clientFee: bigint,
 *   transaction: { to: string, value: bigint, data: string } }} quote
 * @param {{ tokenIn: string, tokenOut: string, amountIn: bigint, recipient: string,
 *   minAmountOut?: bigint }} request
 * @param {Chain} chain
 * @returns {{ to: string, value: bigint, data: string }}
 */
export function validateTransaction (quote, request, chain) {
  const { transaction } = quote
  let tokenIn, tokenOut, recipient, destination
  try {
    tokenIn = normalizeToken(request.tokenIn)
    tokenOut = normalizeToken(request.tokenOut)
    recipient = getAddress(request.recipient).toLowerCase()
    destination = getAddress(transaction.to).toLowerCase()
  } catch {
    throw invalid('invalid token, recipient or destination address')
  }
  if (destination !== chain.router || recipient === chain.router || recipient === ZeroAddress) {
    throw invalid('router or recipient does not match the supported wallet swap')
  }
  if (quote.amountIn !== request.amountIn || request.amountIn <= 0n || quote.grossOutput <= 0n ||
      quote.minimum <= 0n || quote.minimum > quote.grossOutput || quote.clientFee !== 0n) {
    throw invalid('invalid quote amounts or unsupported client fee')
  }
  if (request.minAmountOut !== undefined && quote.minimum < request.minAmountOut) {
    throw invalid('encoded minimum is below the requested output floor')
  }
  const expectedValue = tokenIn === ZeroAddress ? request.amountIn : 0n
  if (transaction.value !== expectedValue) {
    throw invalid('native value does not match the input amount')
  }
  if (typeof transaction.data !== 'string' || !/^0x(?:[0-9a-fA-F]{2}){4,}$/.test(transaction.data)) {
    throw invalid('malformed calldata')
  }

  let call
  try {
    // Solidity ignores trailing attribution bytes. Ethers decodes the ABI payload
    // without requiring the Fynd watermark to be removed or re-encoded.
    call = routerInterface.parseTransaction({ data: transaction.data })
    if (!call) throw invalid('unsupported router method')
    const { args } = call
    const routerTokenIn = tokenIn === ZeroAddress ? ROUTER_NATIVE : tokenIn
    const routerTokenOut = tokenOut === ZeroAddress ? ROUTER_NATIVE : tokenOut
    if (args.amountIn !== request.amountIn || args.tokenIn.toLowerCase() !== routerTokenIn ||
        args.tokenOut.toLowerCase() !== routerTokenOut || args.receiver.toLowerCase() !== recipient ||
        args.expectedAmountOut !== quote.grossOutput || args.minAmountOut !== quote.minimum) {
      throw invalid('calldata does not match the requested swap and quote')
    }
    const fee = args.clientFeeParams
    if (fee.clientFeeBps !== 0n || fee.clientFeeReceiver !== ZeroAddress ||
        fee.maxClientContribution !== 0n || fee.clientSignature !== '0x') {
      throw invalid('client fee or signing parameters are unsupported')
    }
    // With a zero client receiver the router ignores the deadline entirely.
    // Do not describe this tuple as providing an on-chain quote expiry.
    if (args.swaps === '0x' || (call.name === 'splitSwap' && args.nTokens < 2n)) {
      throw invalid('empty or invalid swap route')
    }
  } catch (error) {
    if (error instanceof SwidgeError) throw error
    throw invalid('malformed calldata or unsupported router method')
  }
  return { to: destination, value: transaction.value, data: transaction.data }
}
