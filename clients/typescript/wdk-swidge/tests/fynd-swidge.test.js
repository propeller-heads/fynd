// Copyright 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0

import { afterEach, describe, expect, it, vi } from 'vitest'
import { inspect } from 'node:util'
import { Interface, MaxUint256, ZeroAddress } from 'ethers'
import FyndSwidgeProtocol from '../src/fynd-swidge.js'
import { CHAINS } from '../src/router.js'

const WETH = '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2'
const USDC = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48'
const USDT = '0xdac17f958d2ee523a2206206994597c13d831ec7'
const SENDER = '0x1111111111111111111111111111111111111111'
const RECEIVER = '0x2222222222222222222222222222222222222222'
const ROUTER_NATIVE = '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'
const AMOUNT = 10000n
const options = { fromToken: WETH, toToken: USDC, fromTokenAmount: AMOUNT }
const hash = n => `0x${n.toString(16).padStart(64, '0')}`
const SWAP_HASH = hash(100)
// Independent outer ABI fixture, matching tycho-execution 0.423.0's deployed router.
const abi = new Interface([
  'function singleSwap(uint256 amountIn,address tokenIn,address tokenOut,uint256 expectedAmountOut,uint256 minAmountOut,address receiver,(uint32,address,uint256,uint256,bytes) clientFeeParams,bytes swaps)'
])

function harness ({ chainId = 1, allowance = AMOUNT, config = {}, mode = 'full' } = {}) {
  const chain = chainId === 1 ? CHAINS.ethereum : CHAINS.base
  const state = {
    allowance, encodedQuotes: 0, valuationQuotes: 0,
    grossOutput: 20000n, routerFee: 20n, minimum: 19881n, valuation: AMOUNT,
    quotePatch: {}, infoPatch: {}, networkFee: 100n,
    tokens: [{ address: WETH, symbol: 'WETH', decimals: 18, quality: 100, tax: 0 }]
  }
  const approvals = new Map()
  const account = {
    getAddress: vi.fn(async () => SENDER),
    getDelegation: vi.fn(async () => ({ isDelegated: false })),
    getAllowance: vi.fn(async () => state.allowance),
    quoteSendTransaction: vi.fn(async () => ({ fee: state.networkFee })),
    approve: vi.fn(async ({ amount }) => {
      const id = hash(approvals.size + 1)
      approvals.set(id, amount)
      return { hash: id, fee: 10n }
    }),
    waitForTransaction: vi.fn(async id => {
      state.allowance = approvals.get(id)
      return { hash: id, finality: 'confirmed', success: true, fee: 10n }
    }),
    sendTransaction: vi.fn(async () => ({ hash: SWAP_HASH, fee: 100n })),
    getTransaction: vi.fn(async () => ({ finality: 'confirmed', success: true, fee: 100n }))
  }
  const fetchMock = vi.fn(async (url, request) => {
    if (url.endsWith('/info')) return Response.json({ chain_id: chain.id, router_address: chain.router, version: '0.107.1', ...state.infoPatch })
    if (url.includes('/tokens?')) return Response.json({ tokens: state.tokens, total: state.tokens.length, block: 1 })
    if (!url.endsWith('/quote')) throw new Error('Unexpected test request')
    const { orders: [order], options: requestOptions } = JSON.parse(request.body)
    const encoded = requestOptions.encoding_options !== undefined
    if (encoded) state.encodedQuotes++
    else state.valuationQuotes++
    const quote = {
      status: 'success', amount_in: order.amount, amount_out: (encoded ? state.grossOutput : state.valuation).toString(),
      gas_estimate: '100', gas_price: '1', price_impact_bps: 5
    }
    if (encoded) {
      quote.fee_breakdown = {
        router_fee: state.routerFee.toString(), client_fee: '0', min_amount_received: state.minimum.toString(),
        max_slippage: (state.grossOutput - state.routerFee - state.minimum).toString()
      }
      quote.transaction = {
        to: chain.router, value: order.token_in === ZeroAddress ? order.amount : '0',
        data: abi.encodeFunctionData('singleSwap', [
          order.amount, order.token_in === ZeroAddress ? ROUTER_NATIVE : order.token_in,
          order.token_out === ZeroAddress ? ROUTER_NATIVE : order.token_out,
          state.grossOutput, state.minimum, order.receiver,
          [0n, ZeroAddress, 0n, MaxUint256, '0x'], '0x1234'
        ])
      }
      Object.assign(quote, state.quotePatch)
    }
    return Response.json({ orders: [quote] })
  })
  vi.stubGlobal('fetch', fetchMock)
  const connected = mode === 'none' ? undefined : mode === 'read-only'
    ? { getAddress: account.getAddress, getAllowance: account.getAllowance, quoteSendTransaction: account.quoteSendTransaction, getTransaction: account.getTransaction }
    : account
  return { api: new FyndSwidgeProtocol(connected, { chainId, ...config }), account, state, fetchMock }
}

afterEach(() => vi.unstubAllGlobals())

describe('public quote and execution modes', () => {
  it('keeps the API key out of inspectable protocol fields', () => {
    const secret = 'private-fynd-test-key'
    const { api } = harness({ mode: 'none', config: { apiKey: secret, quoteSender: SENDER, maxProtocolFeeBps: 10 } })
    expect(api._config).toEqual({ maxNetworkFeeBps: undefined, maxProtocolFeeBps: 10 })
    expect(api).not.toHaveProperty('settings')
    expect(api).not.toHaveProperty('api')
    expect(inspect(api, { showHidden: true, depth: null })).not.toContain(secret)
    expect(JSON.stringify(api)).not.toContain(secret)
  })

  it('copies constructor caps and quote sender before the caller mutates its configuration', async () => {
    const { account, fetchMock } = harness()
    const config = { chainId: 1, quoteSender: SENDER, maxProtocolFeeBps: 0 }
    const api = new FyndSwidgeProtocol(undefined, config)
    const executable = new FyndSwidgeProtocol(account, config)
    config.quoteSender = RECEIVER
    config.maxProtocolFeeBps = 10000
    await expect(api.quoteSwidge(options)).resolves.toMatchObject({ toTokenAmount: 19980n })
    const request = JSON.parse(fetchMock.mock.calls.find(([url]) => url.endsWith('/quote'))[1].body)
    expect(request.orders[0]).toMatchObject({ sender: SENDER, receiver: SENDER })
    await expect(executable.swidge(options)).rejects.toMatchObject({ name: 'MaximumFeeExceededError' })
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('exposes only the five adapter methods, keeping helpers outside the WDK policy surface', () => {
    const { api } = harness()
    expect(Object.getOwnPropertyNames(Object.getPrototypeOf(api)).sort()).toEqual([
      'constructor', 'getSupportedChains', 'getSupportedTokens', 'getSwidgeStatus', 'quoteSwidge', 'swidge'
    ])
    for (const helper of ['checkChain', 'request', 'prepare', 'approvalAmounts', 'networkEstimate', 'resultQuote', 'enforceLimits']) {
      expect(helper in api).toBe(false)
    }
  })

  it.each(['none', 'read-only', 'full'])('keeps %s quotes available when configured execution caps are exceeded', async mode => {
    const { api, account, state } = harness({ mode, config: { quoteSender: SENDER, maxNetworkFeeBps: 0, maxProtocolFeeBps: 0 } })
    await expect(api.quoteSwidge(options)).resolves.toMatchObject({ toTokenAmount: 19980n, networkFeeComplete: mode !== 'none' })
    if (mode !== 'full') await expect(api.swidge(options)).rejects.toMatchObject({ name: 'AccountRequiredError' })
    expect(state.valuationQuotes).toBe(0)
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('maps net receipt, minimum, itemized fees and decimal price impact through the real API parser', async () => {
    const { api, account, state } = harness()
    const result = await api.quoteSwidge(options)
    expect(result).toMatchObject({
      fromTokenAmount: AMOUNT, toTokenAmount: 19980n, toTokenAmountMin: 19881n,
      priceImpact: 0.0005, networkFeeComplete: true
    })
    expect(result.fees).toMatchObject([
      { type: 'network', amount: 100n, token: ZeroAddress, included: false },
      { type: 'protocol', amount: 20n, token: USDC, included: true }
    ])
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
    expect(state.valuationQuotes).toBe(0)
  })

  it('requires an explicit quote sender when no account is available', async () => {
    const { api, fetchMock } = harness({ mode: 'none' })
    await expect(api.quoteSwidge(options)).rejects.toMatchObject({ name: 'ValueError' })
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it.each([
    { toTokenAmount: 1n }, { toChain: 8453 }, { fromTokenAmount: 0n }, { fromTokenAmount: 9007199254740992 },
    { fromToken: 'ETH' }, { toToken: WETH }, { recipient: ZeroAddress }, { slippage: 1 }, { refundAddress: SENDER }
  ])('rejects unsupported or invalid request %o before any network request or write', async patch => {
    const { api, account, fetchMock } = harness()
    await expect(api.swidge({ ...options, ...patch })).rejects.toThrow()
    expect(fetchMock).not.toHaveBeenCalled()
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('refuses delegated accounts before requesting executable quotes', async () => {
    const { api, account, fetchMock } = harness()
    account.getDelegation.mockResolvedValue({ isDelegated: true })
    await expect(api.swidge(options)).rejects.toThrow('ordinary')
    expect(fetchMock).not.toHaveBeenCalled()
  })

  it('checks the caller minimum before allowances can be changed', async () => {
    const { api, account } = harness({ allowance: 0n })
    await expect(api.swidge({ ...options, minAmountOut: 19882n })).rejects.toMatchObject({
      name: 'SwidgeError', reason: 'COULD_NOT_MET_THRESHOLD'
    })
    await expect(api.quoteSwidge({ ...options, minAmountOut: 19882n })).rejects.toMatchObject({
      name: 'SwidgeError', reason: 'COULD_NOT_MET_THRESHOLD'
    })
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
    await expect(api.quoteSwidge({ ...options, minAmountOut: 19881n })).resolves.toMatchObject({ toTokenAmountMin: 19881n })
  })

  it('preserves the standard minimum reason through both inherited swap delegates', async () => {
    const { api, account } = harness({ allowance: 0n })
    const legacy = { tokenIn: WETH, tokenOut: USDC, tokenInAmount: AMOUNT, minAmountOut: 19882n }
    await expect(api.quoteSwap(legacy)).rejects.toMatchObject({ name: 'SwapError', reason: 'COULD_NOT_MET_THRESHOLD' })
    await expect(api.swap(legacy)).rejects.toMatchObject({ name: 'SwapError', reason: 'COULD_NOT_MET_THRESHOLD' })
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('rejects a self-consistent quote and calldata that weaken the caller slippage before approving', async () => {
    const { api, account, state } = harness({ allowance: 0n })
    state.minimum = 1n
    await expect(api.swidge(options)).rejects.toMatchObject({ reason: 'SLIPPAGE_TOO_HIGH' })
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('preserves tiny slippage text and enforces the encoder rounding to zero permitted loss', async () => {
    const { api, fetchMock, state } = harness()
    state.minimum = 19980n
    await expect(api.quoteSwidge({ ...options, slippage: 1e-7 })).resolves.toMatchObject({ toTokenAmountMin: 19980n })
    const request = JSON.parse(fetchMock.mock.calls.find(([url]) => url.endsWith('/quote'))[1].body)
    expect(request.options.encoding_options.slippage).toBe('1e-7')
  })

  it('rejects Fynd metadata for a different chain or router', async () => {
    const { api, account, state } = harness({ allowance: 0n })
    state.infoPatch = { chain_id: 8453 }
    await expect(api.swidge(options)).rejects.toMatchObject({ reason: 'UNSUPPORTED_ROUTER' })
    state.infoPatch = { router_address: SENDER }
    await expect(api.swidge(options)).rejects.toMatchObject({ reason: 'UNSUPPORTED_ROUTER' })
    expect(account.approve).not.toHaveBeenCalled()
  })
})

describe('allowances, fresh execution and partial progress', () => {
  it('waits for exact approval, requests a fresh checked quote and includes the spent approval fee', async () => {
    const { api, account, state } = harness({ allowance: 0n })
    const result = await api.swidge(options)
    expect(account.approve).toHaveBeenCalledExactlyOnceWith({ token: WETH, spender: CHAINS.ethereum.router, amount: AMOUNT })
    expect(account.waitForTransaction).toHaveBeenCalledExactlyOnceWith(hash(1), { target: 'confirmed', timeout: 180000 })
    expect(state.encodedQuotes).toBe(2)
    expect(account.getAllowance).toHaveBeenCalledTimes(2)
    expect(result.transactions).toEqual([
      { hash: hash(1), chain: 1, type: 'approval' }, { hash: SWAP_HASH, chain: 1, type: 'source' }
    ])
    expect(result.fees[0].amount).toBe(110n)
    expect(Object.keys(account.sendTransaction.mock.calls[0][0])).toEqual(['to', 'value', 'data'])
  })

  it('resets nonzero insufficient Ethereum USDT allowance before exact approval', async () => {
    const { api, account } = harness({ allowance: 1n })
    await api.swidge({ ...options, fromToken: USDT })
    expect(account.approve.mock.calls.map(([request]) => request.amount)).toEqual([0n, AMOUNT])
    expect(account.waitForTransaction.mock.calls.map(([id]) => id)).toEqual([hash(1), hash(2)])
    expect(account.waitForTransaction.mock.invocationCallOrder[0]).toBeLessThan(account.approve.mock.invocationCallOrder[1])
  })

  it('does not reset an ordinary token with insufficient nonzero allowance', async () => {
    const { api, account } = harness({ allowance: 1n })
    await api.swidge(options)
    expect(account.approve.mock.calls.map(([request]) => request.amount)).toEqual([AMOUNT])
  })

  it('stops when allowance cannot be read instead of trying an approval', async () => {
    const { api, account } = harness()
    const failure = new Error('allowance RPC unavailable')
    account.getAllowance.mockRejectedValue(failure)
    await expect(api.swidge(options)).rejects.toBe(failure)
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it.each([
    { finality: 'confirmed', success: false, fee: 10n },
    { finality: 'dropped', success: false, fee: 10n },
    { finality: 'pending', success: true, fee: 10n }
  ])('retains a sent approval and stops on an unsuccessful receipt %o', async receipt => {
    const { api, account } = harness({ allowance: 0n })
    account.waitForTransaction.mockResolvedValue(receipt)
    await expect(api.swidge(options)).rejects.toMatchObject({
      name: 'FyndExecutionError', submissionUnknown: false,
      transactions: [{ hash: hash(1), chain: 1, type: 'approval' }]
    })
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('retains an approval hash when its receipt fee is malformed', async () => {
    const { api, account } = harness({ allowance: 0n })
    account.waitForTransaction.mockResolvedValue({ finality: 'confirmed', success: true, fee: undefined })
    await expect(api.swidge(options)).rejects.toMatchObject({
      name: 'FyndExecutionError', transactions: [{ hash: hash(1) }], submissionUnknown: false
    })
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('preserves approval progress if the fresh post-approval quote fails', async () => {
    const { api, account, state } = harness({ allowance: 0n })
    account.waitForTransaction.mockImplementation(async () => {
      state.allowance = AMOUNT
      state.quotePatch = { status: 'encoding_failed', transaction: null }
      return { finality: 'confirmed', success: true, fee: 10n }
    })
    await expect(api.swidge(options)).rejects.toMatchObject({
      stage: 'quote refresh', transactions: [{ hash: hash(1) }], submissionUnknown: false
    })
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('preserves the approval hash if a refreshed quote weakens the caller slippage', async () => {
    const { api, account, state } = harness({ allowance: 0n })
    account.waitForTransaction.mockImplementation(async () => {
      state.allowance = AMOUNT
      state.minimum = 1n
      return { finality: 'confirmed', success: true, fee: 10n }
    })
    await expect(api.swidge(options)).rejects.toMatchObject({
      stage: 'quote refresh', transactions: [{ hash: hash(1) }], submissionUnknown: false,
      cause: { reason: 'SLIPPAGE_TOO_HIGH' }
    })
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('preserves earlier hashes and marks an ambiguous swap submission without a retry', async () => {
    const { api, account } = harness({ allowance: 0n })
    account.sendTransaction.mockRejectedValue(new Error('RPC connection closed after submission'))
    await expect(api.swidge(options)).rejects.toMatchObject({
      stage: 'swap submission', transactions: [{ hash: hash(1) }], submissionUnknown: true
    })
    expect(account.sendTransaction).toHaveBeenCalledTimes(1)
  })

  it('skips excess allowance and preserves the swap hash without reading optional submission fees', async () => {
    const { api, account, state } = harness({ allowance: AMOUNT + 1n })
    account.sendTransaction.mockResolvedValue({ hash: SWAP_HASH, get fee () { throw new Error('invalid optional field') } })
    const result = await api.swidge(options)
    expect(result).toMatchObject({ id: SWAP_HASH, hash: SWAP_HASH })
    expect(result.transactions).toEqual([{ hash: SWAP_HASH, chain: 1, type: 'source' }])
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).toHaveBeenCalledTimes(1)
    expect(state.encodedQuotes).toBe(1)
  })

  it.each(['approval', 'swap'])('treats a missing %s hash as an unknown submission', async stage => {
    const { api, account } = harness({ allowance: stage === 'approval' ? 0n : AMOUNT })
    account[stage === 'approval' ? 'approve' : 'sendTransaction'].mockResolvedValue({ fee: 1n })
    await expect(api.swidge(options)).rejects.toMatchObject({
      name: 'FyndExecutionError', transactions: [], submissionUnknown: true
    })
  })
})

describe('requested fee caps', () => {
  it.each(['maxNetworkFeeBps', 'maxProtocolFeeBps'])('snapshots %s before awaiting account inspection', async field => {
    const { api, account } = harness()
    let resolveDelegation
    account.getDelegation.mockImplementation(() => new Promise(resolve => { resolveDelegation = resolve }))
    const config = { [field]: 0 }
    const pending = api.swidge(options, config)
    config[field] = 10000
    resolveDelegation({ isDelegated: false })
    await expect(pending).rejects.toMatchObject({ name: 'MaximumFeeExceededError' })
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('accepts protocol-cap equality and blocks an exceeded cap before approval', async () => {
    const { api, account } = harness({ allowance: 0n })
    await expect(api.swidge(options, { maxProtocolFeeBps: 9 })).rejects.toMatchObject({ name: 'MaximumFeeExceededError' })
    expect(account.approve).not.toHaveBeenCalled()
    await expect(api.swidge(options, { maxProtocolFeeBps: 10 })).resolves.toMatchObject({ id: SWAP_HASH })
  })

  it('uses native input directly for the network-cap boundary', async () => {
    const { api, account, state } = harness()
    await expect(api.swidge({ ...options, fromToken: ZeroAddress }, { maxNetworkFeeBps: 99 })).rejects.toMatchObject({ name: 'MaximumFeeExceededError' })
    expect(account.sendTransaction).not.toHaveBeenCalled()
    await expect(api.swidge({ ...options, fromToken: ZeroAddress }, { maxNetworkFeeBps: 100 })).resolves.toMatchObject({ id: SWAP_HASH })
    expect(state.valuationQuotes).toBe(0)
    expect(account.sendTransaction).toHaveBeenCalledWith(expect.objectContaining({ value: AMOUNT }))
    expect(account.getAllowance).not.toHaveBeenCalled()
    expect(account.approve).not.toHaveBeenCalled()
  })

  it.each([
    [WETH, 0n], [USDT, 1n]
  ])('rejects network-capped %s execution requiring approvals even when mock gas estimates succeed', async (fromToken, allowance) => {
    const { api, account } = harness({ allowance })
    await expect(api.swidge({ ...options, fromToken }, { maxNetworkFeeBps: 10000 })).rejects.toMatchObject({ reason: 'FEE_ESTIMATE_UNAVAILABLE' })
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.waitForTransaction).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('rechecks the protocol cap after a confirmed approval', async () => {
    const { api, account, state } = harness({ allowance: 0n })
    account.waitForTransaction.mockImplementation(async () => {
      state.allowance = AMOUNT
      state.routerFee = 40n
      return { finality: 'confirmed', success: true, fee: 10n }
    })
    await expect(api.swidge(options, { maxProtocolFeeBps: 10 })).rejects.toMatchObject({
      transactions: [{ hash: hash(1) }], cause: { name: 'MaximumFeeExceededError' }
    })
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('refuses a requested cap when a preapproved swap cannot be estimated', async () => {
    const { api, account } = harness()
    account.quoteSendTransaction.mockRejectedValueOnce(new Error('swap gas estimate unavailable'))
    await expect(api.swidge(options, { maxNetworkFeeBps: 10000 })).rejects.toMatchObject({ reason: 'FEE_ESTIMATE_UNAVAILABLE' })
    expect(account.approve).not.toHaveBeenCalled()
  })

  it('does not claim a zero estimate proves a free network transaction', async () => {
    const { api, account, state } = harness()
    state.networkFee = 0n
    await expect(api.swidge(options, { maxNetworkFeeBps: 0 })).rejects.toMatchObject({ reason: 'FEE_ESTIMATE_UNAVAILABLE' })
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })

  it('omits an unavailable network fee rather than reporting it as zero', async () => {
    const { api, account, state } = harness()
    state.quotePatch = { gas_price: undefined }
    account.quoteSendTransaction.mockRejectedValue(new Error('network estimate unavailable'))
    const result = await api.quoteSwidge(options)
    expect(result.networkFeeComplete).toBe(false)
    expect(result.fees.map(fee => fee.type)).toEqual(['protocol'])
    expect(result.fees[0]).toMatchObject({ token: USDC, amount: 20n })
  })

  it('fails closed for Base network caps when L1 data cost is not covered', async () => {
    const { api, account } = harness({ chainId: 8453 })
    await expect(api.swidge(options, { maxNetworkFeeBps: 10000 })).rejects.toMatchObject({ reason: 'FEE_ESTIMATE_UNAVAILABLE' })
    expect(account.sendTransaction).not.toHaveBeenCalled()
    await expect(api.quoteSwidge(options)).resolves.toMatchObject({ networkFeeComplete: false })
  })
})

describe('status, discovery and inherited WDK delegates', () => {
  it.each([
    [{ finality: 'dropped' }, 'pending'], [{ finality: 'final', success: false }, 'failed']
  ])('maps receipt %j without guessing replacements', async (receipt, status) => {
    const { api, account } = harness()
    account.getTransaction.mockResolvedValue(receipt)
    await expect(api.getSwidgeStatus(SWAP_HASH)).resolves.toEqual({
      status, transactions: [{ hash: SWAP_HASH, chain: 1, type: 'source' }]
    })
    expect(account.getTransaction).toHaveBeenCalledExactlyOnceWith(SWAP_HASH)
  })

  it('keeps missing transactions and provider errors distinct from pending', async () => {
    const { api, account } = harness()
    account.getTransaction.mockResolvedValueOnce(null)
    await expect(api.getSwidgeStatus(SWAP_HASH)).rejects.toMatchObject({ name: 'NoSuchElementError' })
    const failure = new Error('provider unavailable')
    account.getTransaction.mockRejectedValueOnce(failure)
    await expect(api.getSwidgeStatus(SWAP_HASH)).rejects.toBe(failure)
    await expect(api.getSwidgeStatus('0x123')).rejects.toMatchObject({ name: 'ValueError' })
  })

  it('requires an account for status lookup', async () => {
    const { api } = harness({ mode: 'none' })
    await expect(api.getSwidgeStatus(SWAP_HASH)).rejects.toMatchObject({ name: 'ReadOnlyAccountRequiredError' })
  })

  it('filters token metadata, deduplicates native currency and validates discovery filters', async () => {
    const { api, state } = harness()
    state.tokens.push({ address: ZeroAddress, symbol: 'ETH', decimals: 18, quality: 100, tax: 0 })
    state.tokens.push({ address: USDT, symbol: 'TAX', decimals: 6, quality: 100, tax: 1 })
    const tokens = await api.getSupportedTokens({ fromChain: 'ethereum', toChain: 1 })
    expect(tokens.map(token => token.token)).toEqual([ZeroAddress, WETH])
    await expect(api.getSupportedTokens({ fromToken: WETH })).rejects.toMatchObject({ name: 'NotImplementedError' })
    await expect(api.getSupportedTokens({ fromChain: 8453 })).rejects.toMatchObject({ name: 'ValueError' })
  })

  it('uses inherited swap helpers and exposes their documented mixed-unit fee limitation', async () => {
    const { api } = harness()
    const legacy = { tokenIn: WETH, tokenOut: USDC, tokenInAmount: AMOUNT }
    // beta.19 incorrectly sums 100 native units and 20 output-token units.
    await expect(api.quoteSwap(legacy)).resolves.toMatchObject({ fee: 120n, tokenInAmount: AMOUNT, tokenOutAmount: 19980n })
    await expect(api.swap(legacy)).resolves.toMatchObject({ hash: SWAP_HASH, tokenInAmount: AMOUNT, tokenOutAmount: 19980n })
  })

  it('rejects both inherited bridge paths without writing transactions', async () => {
    const { api, account } = harness()
    const bridge = { token: WETH, amount: AMOUNT, targetChain: 8453, recipient: RECEIVER }
    await expect(api.quoteBridge(bridge)).rejects.toThrow()
    await expect(api.bridge(bridge)).rejects.toThrow()
    expect(account.approve).not.toHaveBeenCalled()
    expect(account.sendTransaction).not.toHaveBeenCalled()
  })
})
