// Copyright 2026 PropellerHeads
// Licensed under the Apache License, Version 2.0.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { ProviderError, ValueError } from '@tetherto/wdk-wallet'
import { FyndApi } from '../src/fynd-api.js'

const INPUT = '0x1111111111111111111111111111111111111111'
const OUTPUT = '0x2222222222222222222222222222222222222222'
const SENDER = '0x3333333333333333333333333333333333333333'
const RECEIVER = '0x4444444444444444444444444444444444444444'
const ROUTER = '0x5555555555555555555555555555555555555555'
const KEY = 'secret-test-key'
const amountIn = 1000000000000000001n
const order = { tokenIn: INPUT, tokenOut: OUTPUT, amountIn, sender: SENDER, receiver: RECEIVER }
const encoding = { slippage: '0.005' }

function encodedOrder (overrides = {}) {
  return {
    status: 'success', order_id: 'server-generated', amount_in: amountIn.toString(),
    amount_out: '10000', amount_out_net_gas: '9890', gas_estimate: '150000', gas_price: '1000000000',
    price_impact_bps: 5,
    fee_breakdown: { router_fee: '10', client_fee: '0', min_amount_received: '9941', max_slippage: '49' },
    transaction: { to: ROUTER, value: '0', data: '0x1234567890' }, ...overrides
  }
}

function token (n, overrides = {}) {
  return {
    address: `0x${n.toString(16).padStart(40, '0')}`, symbol: `T${n}`, decimals: 18,
    quality: 100, tax: 0, ...overrides
  }
}

function json (value, status = 200) {
  return new Response(JSON.stringify(value), { status })
}

const fetchMock = vi.fn()
let api
beforeEach(() => {
  vi.stubGlobal('fetch', fetchMock)
  api = new FyndApi({ chain: 'ethereum', apiKey: KEY })
})
afterEach(() => {
  vi.unstubAllGlobals()
  vi.useRealTimers()
  fetchMock.mockReset()
})

describe('hosted Fynd wire boundary', () => {
  it('sends one exact-input order with raw authorization, decimal string slippage and redirects disabled', async () => {
    fetchMock.mockResolvedValue(json({ orders: [encodedOrder()] }))
    const quote = await api.quote(order, encoding)
    expect(fetchMock).toHaveBeenCalledTimes(1)
    const [url, options] = fetchMock.mock.calls[0]
    expect(url).toBe('https://fynd-api.propellerheads.xyz/v1/ethereum/quote')
    expect(options).toMatchObject({ method: 'POST', redirect: 'error', headers: { Authorization: KEY } })
    expect(JSON.parse(options.body)).toEqual({
      orders: [{ token_in: INPUT, token_out: OUTPUT, amount: amountIn.toString(), side: 'sell', sender: SENDER, receiver: RECEIVER }],
      options: { encoding_options: { slippage: '0.005', transfer_type: 'transfer_from' } }
    })
    expect(quote).toEqual({
      amountIn, grossOutput: 10000n, routerFee: 10n, clientFee: 0n, minimum: 9941n,
      gas: 150000n, gasPrice: 1000000000n, priceImpact: 0.0005,
      transaction: { to: ROUTER, data: '0x1234567890', value: 0n }
    })
  })

  it('allows a valuation quote without encoding, fees or gas price', async () => {
    fetchMock.mockResolvedValue(json({ orders: [encodedOrder({
      fee_breakdown: undefined, transaction: null, gas_price: undefined, price_impact_bps: undefined
    })] }))
    await expect(api.quote(order, { encode: false })).resolves.toEqual({
      amountIn, grossOutput: 10000n, gas: 150000n, gasPrice: undefined, priceImpact: undefined
    })
    expect(JSON.parse(fetchMock.mock.calls[0][1].body).options).toEqual({})
  })

  it('keeps unavailable gas prices unavailable on encoded quotes', async () => {
    fetchMock.mockResolvedValue(json({ orders: [encodedOrder({ gas_price: null })] }))
    expect((await api.quote(order, encoding)).gasPrice).toBeUndefined()
  })

  it('preserves a small slippage exponent string like the official TypeScript client', async () => {
    fetchMock.mockResolvedValue(json({ orders: [encodedOrder({
      fee_breakdown: { router_fee: '10', client_fee: '0', min_amount_received: '9990', max_slippage: '0' }
    })] }))
    await expect(api.quote(order, { slippage: '1e-7' })).resolves.toMatchObject({ minimum: 9990n })
    expect(JSON.parse(fetchMock.mock.calls[0][1].body).options.encoding_options.slippage).toBe('1e-7')
  })

  it.each([' 0.005', '0.005 ', '0x0', '-0.1', 'NaN', 'Infinity', '1e999', '1', '1.0', '1e0'])(
    'rejects invalid or out-of-range slippage text %s without an HTTP request', async slippage => {
      await expect(api.quote(order, { slippage })).rejects.toBeInstanceOf(ValueError)
      expect(fetchMock).not.toHaveBeenCalled()
    }
  )

  it.each(['no_route_found', 'insufficient_liquidity', 'timeout', 'not_ready', 'price_check_failed', 'encoding_failed'])(
    'rejects HTTP-success semantic status %s without retrying', async (status) => {
      fetchMock.mockResolvedValue(json({ orders: [encodedOrder({ status, transaction: null })] }))
      await expect(api.quote(order, encoding)).rejects.toMatchObject({
        name: 'SwidgeError', reason: `FYND_${status.toUpperCase()}`
      })
      expect(fetchMock).toHaveBeenCalledTimes(1)
    }
  )

  it.each([
    ['missing transaction', { transaction: null }],
    ['missing fees', { fee_breakdown: undefined }],
    ['mismatched input', { amount_in: '42' }],
    ['unsafe JSON number', { amount_out: 9007199254740992 }],
    ['negative amount', { amount_out: '-1' }],
    ['exponent amount', { amount_out: '1e20' }],
    ['hexadecimal amount', { amount_out: '0x100' }],
    ['uint256 overflow', { amount_out: (1n << 256n).toString() }],
    ['zero output', { amount_out: '0' }],
    ['invalid calldata', { transaction: { to: ROUTER, value: '0', data: '0x123' } }],
    ['invalid destination', { transaction: { to: '0x123', value: '0', data: '0x12345678' } }],
    ['unsafe transaction value', { transaction: { to: ROUTER, value: 1, data: '0x12345678' } }],
    ['bad price impact', { price_impact_bps: 0.5 }],
    ['inconsistent fees', { fee_breakdown: { router_fee: '10', client_fee: '0', min_amount_received: '10000', max_slippage: '50' } }],
    ['unknown status', { status: 'success-but-different' }]
  ])('rejects %s before returning an executable quote', async (_name, overrides) => {
    fetchMock.mockResolvedValue(json({ orders: [encodedOrder(overrides)] }))
    await expect(api.quote(order, encoding)).rejects.toBeInstanceOf(ProviderError)
  })

  it.each([[], [encodedOrder(), encodedOrder()], undefined])('requires exactly one response order', async (orders) => {
    fetchMock.mockResolvedValue(json({ orders }))
    await expect(api.quote(order, encoding)).rejects.toThrow('order count')
  })

  it('validates local requests without calling the provider', async () => {
    await expect(api.quote({ ...order, amountIn: 42 }, encoding)).rejects.toBeInstanceOf(ValueError)
    await expect(api.quote(order, { slippage: 0.005 })).rejects.toBeInstanceOf(ValueError)
    await expect(api.quote(order, { slippage: '1.001' })).rejects.toBeInstanceOf(ValueError)
    expect(fetchMock).not.toHaveBeenCalled()
  })
})

describe('HTTP failures and configuration', () => {
  it.each([
    [401, 'UNAUTHORIZED', 'not allowed'], [403, 'FORBIDDEN', 'denied'],
    [429, 'INTERNAL_SERVER_ERROR', 'rate limited'], [503, 'INTERNAL_SERVER_ERROR', { code: 'NOT_READY' }]
  ])('reports HTTP %s without reflecting credentials or arbitrary response content', async (status, reason, body) => {
    fetchMock.mockResolvedValue(new Response(JSON.stringify({ body, echoed_key: KEY }), { status }))
    const error = await api.info().catch(error => error)
    expect(error).toMatchObject({ name: 'ProviderError', reason })
    expect(error.message).toContain(String(status))
    expect(String(error)).not.toContain(KEY)
    expect(error.cause).toBeUndefined()
    expect(fetchMock).toHaveBeenCalledTimes(1)
  })

  it('redacts network exceptions, including redirect errors, without retrying', async () => {
    fetchMock.mockRejectedValue(new Error(`Request with Authorization: ${KEY} failed`))
    await expect(api.info()).rejects.toMatchObject({ message: 'Fynd network request failed.', reason: 'NETWORK_ERROR' })
    expect(fetchMock.mock.calls[0][1].redirect).toBe('error')
    expect(fetchMock).toHaveBeenCalledTimes(1)
  })

  it('rejects a malformed success body without including its text', async () => {
    fetchMock.mockResolvedValue(new Response(`not JSON: ${KEY}`))
    await expect(api.info()).rejects.toMatchObject({ message: 'Fynd returned invalid JSON response.' })
  })

  it.each([
    { baseUrl: 'http://api.example.com' }, { baseUrl: 'https://user:password@example.com' },
    { baseUrl: 'https://example.com?key=secret' }, { baseUrl: 'https://example.com#fragment' },
    { baseUrl: 'file:///tmp/example' }, { baseUrl: 'invalid' }, { chain: '../base' },
    { timeoutMs: 0 }, { timeoutMs: Infinity }, { apiKey: 'key\r\nX-Evil: value' }
  ])('rejects unsafe configuration %j', config => {
    expect(() => new FyndApi({ chain: 'ethereum', ...config })).toThrow(ValueError)
  })

  it('allows a local proxy without an API key and preserves its base path', async () => {
    fetchMock.mockResolvedValue(json({ chain_id: 8453, router_address: ROUTER, version: '0.107.1' }))
    const proxy = new FyndApi({ baseUrl: 'http://localhost:3000/fynd/', chain: 'base' })
    await expect(proxy.info()).resolves.toEqual({ chainId: 8453, routerAddress: ROUTER, version: '0.107.1' })
    expect(fetchMock.mock.calls[0][0]).toBe('http://localhost:3000/fynd/v1/base/info')
    expect(fetchMock.mock.calls[0][1].headers).not.toHaveProperty('Authorization')
  })

  it('preserves a quote-only chain without claiming an executable router', async () => {
    fetchMock.mockResolvedValue(json({ chain_id: 1, router_address: null, version: '0.107.1' }))
    await expect(api.info()).resolves.toMatchObject({ routerAddress: null })
  })
})

describe('complete token discovery', () => {
  it('paginates past 1000 using raw page offsets and filters taxed or nonstandard tokens', async () => {
    const first = Array.from({ length: 1000 }, (_unused, index) => token(index + 1))
    first[0].quality = 99
    first[1].tax = 20
    fetchMock.mockResolvedValueOnce(json({ tokens: first, total: 1002, block: 123 }))
      .mockResolvedValueOnce(json({ tokens: [token(1001), token(1002)], total: 1002, block: 123 }))
    const tokens = await api.tokens()
    expect(tokens).toHaveLength(1000)
    expect(tokens[0].address).toBe(token(3).address)
    expect(tokens.at(-1).address).toBe(token(1002).address)
    expect(fetchMock.mock.calls.map(([url]) => url)).toEqual([
      'https://fynd-api.propellerheads.xyz/v1/ethereum/tokens?limit=1000&offset=0',
      'https://fynd-api.propellerheads.xyz/v1/ethereum/tokens?limit=1000&offset=1000'
    ])
  })

  it.each([
    ['block changes', { tokens: [token(2)], total: 2, block: 124 }],
    ['total changes', { tokens: [token(2)], total: 3, block: 123 }],
    ['duplicate token', { tokens: [token(1)], total: 2, block: 123 }],
    ['empty incomplete page', { tokens: [], total: 2, block: 123 }],
    ['too many entries', { tokens: [token(2), token(3)], total: 2, block: 123 }]
  ])('never returns a partial list when %s', async (_name, secondPage) => {
    fetchMock.mockResolvedValueOnce(json({ tokens: [token(1)], total: 2, block: 123 }))
      .mockResolvedValueOnce(json(secondPage))
    await expect(api.tokens()).rejects.toBeInstanceOf(ProviderError)
    expect(fetchMock).toHaveBeenCalledTimes(2)
  })

  it('accepts a complete empty graph', async () => {
    fetchMock.mockResolvedValue(json({ tokens: [], total: 0, block: 123 }))
    await expect(api.tokens()).resolves.toEqual([])
  })

  it('uses one deadline for the full pagination operation', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(0)
    fetchMock.mockImplementationOnce(async () => {
      vi.setSystemTime(40)
      return json({ tokens: [token(1)], total: 2, block: 123 })
    }).mockImplementationOnce(async () => {
      vi.setSystemTime(101)
      return json({ tokens: [token(2)], total: 2, block: 123 })
    })
    const client = new FyndApi({ chain: 'ethereum', timeoutMs: 100 })
    await expect(client.tokens()).rejects.toMatchObject({ reason: 'REQUEST_TIMEOUT' })
    expect(fetchMock).toHaveBeenCalledTimes(2)
  })
})
