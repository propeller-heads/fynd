// Copyright (c) 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0

import { describe, expect, it } from 'vitest'
import { Interface, MaxUint256, ZeroAddress } from 'ethers'
import { CHAINS, normalizeToken, validateTransaction } from '../src/router.js'

// Declare the deployed ABI independently so selector changes cannot alter fixtures.
const abi = new Interface([
  'function singleSwap(uint256 amountIn,address tokenIn,address tokenOut,uint256 expectedAmountOut,uint256 minAmountOut,address receiver,(uint32,address,uint256,uint256,bytes) clientFeeParams,bytes swaps)',
  'function sequentialSwap(uint256 amountIn,address tokenIn,address tokenOut,uint256 expectedAmountOut,uint256 minAmountOut,address receiver,(uint32,address,uint256,uint256,bytes) clientFeeParams,bytes swaps)',
  'function splitSwap(uint256 amountIn,address tokenIn,address tokenOut,uint256 expectedAmountOut,uint256 minAmountOut,uint256 nTokens,address receiver,(uint32,address,uint256,uint256,bytes) clientFeeParams,bytes swaps)'
])
const sender = '0xcd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2'
const weth = '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2'
const usdc = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48'
const routerNative = '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'

function fixture (method = 'singleSwap', overrides = {}, chain = CHAINS.ethereum) {
  const intent = {
    amountIn: 10n ** 18n, tokenIn: weth, tokenOut: usdc,
    expectedAmountOut: 2_000_000_000n, minAmountOut: 1_990_000_000n,
    receiver: sender, nTokens: 3n,
    clientFeeParams: [0n, ZeroAddress, 0n, MaxUint256, '0x'], swaps: '0x1234',
    ...overrides
  }
  const args = [intent.amountIn, intent.tokenIn, intent.tokenOut, intent.expectedAmountOut, intent.minAmountOut]
  if (method === 'splitSwap') args.push(intent.nTokens)
  args.push(intent.receiver, intent.clientFeeParams, intent.swaps)
  const quote = {
    amountIn: 10n ** 18n, grossOutput: 2_000_000_000n, minimum: 1_990_000_000n, clientFee: 0n,
    transaction: { to: chain.router, value: 0n, data: abi.encodeFunctionData(method, args) }
  }
  const request = { tokenIn: weth, tokenOut: usdc, amountIn: 10n ** 18n, recipient: sender }
  return { quote, request, chain }
}
function validate ({ quote, request, chain }) {
  return validateTransaction(quote, request, chain)
}

describe('router call validation', () => {
  it.each([
    ['singleSwap', '0x0c1a0ee7'], ['sequentialSwap', '0x3c226834'], ['splitSwap', '0xfe745a0f']
  ])('accepts the verified %s selector and a trailing Fynd watermark', (method, selector) => {
    const test = fixture(method)
    expect(test.quote.transaction.data.slice(0, 10)).toBe(selector)
    test.quote.transaction.data += '66796e642f302e3130372e31'
    expect(validate(test)).toEqual(test.quote.transaction)
  })

  it('uses the Base router only for Base', () => {
    const test = fixture('singleSwap', {}, CHAINS.base)
    expect(validate(test).to).toBe(CHAINS.base.router)
    expect(() => validateTransaction(test.quote, test.request, CHAINS.ethereum)).toThrow(/router/)
  })

  it('returns only the transaction fields that WDK should sign', () => {
    const test = fixture()
    test.quote.transaction.nonce = 999n
    test.quote.transaction.from = usdc
    expect(Object.keys(validate(test))).toEqual(['to', 'value', 'data'])
  })

  it('requires the native input value and maps the Fynd native alias', () => {
    const test = fixture('singleSwap', { tokenIn: routerNative })
    test.request.tokenIn = ZeroAddress
    test.quote.transaction.value = test.request.amountIn
    expect(validate(test).value).toBe(test.request.amountIn)
    test.quote.transaction.value--
    expect(() => validate(test)).toThrow(/native value/)
  })

  it('accepts native output and never sends native value for ERC20 input', () => {
    const test = fixture('sequentialSwap', { tokenOut: routerNative })
    test.request.tokenOut = routerNative
    expect(validate(test).value).toBe(0n)
    test.quote.transaction.value = 1n
    expect(() => validate(test)).toThrow(/native value/)
  })

  it.each([
    ['amountIn', 1n], ['tokenIn', usdc], ['tokenOut', weth],
    ['expectedAmountOut', 1_999_999_999n], ['minAmountOut', 1_989_999_999n], ['receiver', usdc]
  ])('rejects mismatching %s before signing', (field, value) => {
    expect(() => validate(fixture('singleSwap', { [field]: value }))).toThrow(/calldata does not match/)
  })

  it('does not accept the zero-address token in router calldata', () => {
    const test = fixture('singleSwap', { tokenIn: ZeroAddress })
    test.request.tokenIn = ZeroAddress
    test.quote.transaction.value = test.request.amountIn
    expect(() => validate(test)).toThrow(/calldata does not match/)
  })

  it('rejects the router as recipient because that credits a vault balance', () => {
    const test = fixture('singleSwap', { receiver: CHAINS.ethereum.router })
    test.request.recipient = CHAINS.ethereum.router
    expect(() => validate(test)).toThrow(/recipient/)
  })

  it('rejects the zero recipient', () => {
    const test = fixture('singleSwap', { receiver: ZeroAddress })
    test.request.recipient = ZeroAddress
    expect(() => validate(test)).toThrow(/recipient/)
  })

  it.each([
    [1n, ZeroAddress, 0n, MaxUint256, '0x'],
    [0n, sender, 0n, MaxUint256, '0x'],
    [0n, ZeroAddress, 1n, MaxUint256, '0x'],
    [0n, ZeroAddress, 0n, MaxUint256, '0x12']
  ])('rejects unsupported client-fee parameters %#', (...clientFeeParams) => {
    expect(() => validate(fixture('singleSwap', { clientFeeParams }))).toThrow(/client fee or signing/)
  })

  it('does not mistake the unsigned fee deadline for an enforced expiry', () => {
    expect(() => validate(fixture('singleSwap', {
      clientFeeParams: [0n, ZeroAddress, 0n, 0n, '0x']
    }))).not.toThrow()
  })


  it.each([
    ['amountIn', 2n], ['grossOutput', 0n], ['minimum', 0n], ['minimum', 2_000_000_001n], ['clientFee', 1n]
  ])('rejects inconsistent quote %s', (field, value) => {
    const test = fixture()
    test.quote[field] = value
    expect(() => validate(test)).toThrow(/quote amounts or unsupported client fee/)
  })

  it.each(['0x', '0x123', '0x00000000', '0x631eecea', '0x0c1a0ee7', '0xgg123456'])('rejects unusable calldata %s', data => {
    const test = fixture()
    test.quote.transaction.data = data
    expect(() => validate(test)).toThrow(/calldata|unsupported router method/)
  })

  it('reports invalid calldata with a safe WDK reason', () => {
    const test = fixture()
    test.quote.transaction.data = '0x0c1a0ee7' + 'ff'.repeat(100)
    try {
      validate(test)
      throw new Error('validation unexpectedly passed')
    } catch (error) {
      expect(error.reason).toBe('INVALID_TRANSACTION')
      expect(error.message).not.toContain(test.quote.transaction.data)
    }
  })

  it('rejects an invalid destination address', () => {
    const test = fixture()
    test.quote.transaction.to = 'broken'
    expect(() => validate(test)).toThrow(/address/)
  })

  it('rejects empty routes and split routes with no token graph', () => {
    expect(() => validate(fixture('singleSwap', { swaps: '0x' }))).toThrow(/route/)
    expect(() => validate(fixture('splitSwap', { nTokens: 0n }))).toThrow(/route/)
  })
})

it('normalizes the two supported native aliases without accepting malformed addresses', () => {
  expect(normalizeToken(ZeroAddress)).toBe(ZeroAddress)
  expect(normalizeToken('0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE')).toBe(ZeroAddress)
  expect(normalizeToken(weth)).toBe(weth)
  expect(() => normalizeToken('ETH')).toThrow()
})

// Unmodified Rust-generated fixture from tycho-execution 0.423.0,
// contracts/test/assets/calldata.txt, test_sequential_swap_strategy_encoder_transfer_from.
it('accepts the upstream Rust encoder fixture without local re-encoding', () => {
  const transaction = { to: CHAINS.ethereum.router, value: 0n, data: '0x3c2268340000000000000000000000000000000000000000000000000de0b6b3a7640000000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000018f61ec0000000000000000000000000000000000000000000000000000000001876515000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a400505615deb798bb3e4dfa0139dfa1b3d433cc23b72fbb2b8038a1640196fbe3e38816f3e67cba72d940c02aaa39b223fe8d0a0e5c4f27ead9083c756cc22260fac5e5542a773aa44fbcfedf7c193bc2c59900505615deb798bb3e4dfa0139dfa1b3d433cc23b72f004375dff511095cc5a197a54140a24efef3a4162260fac5e5542a773aa44fbcfedf7c193bc2c599a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000000000000000000000000000000000000' }
  expect(validateTransaction({ amountIn: 10n ** 18n, grossOutput: 26173932n, minimum: 25650453n, clientFee: 0n, transaction },
    { tokenIn: weth, tokenOut: usdc, amountIn: 10n ** 18n, recipient: sender }, CHAINS.ethereum)).toEqual(transaction)
})
