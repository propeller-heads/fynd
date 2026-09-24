// Copyright 2026 PropellerHeads
// Licensed under the Apache License, Version 2.0.

import { ProviderError, ProviderErrorReason, ValueError } from '@tetherto/wdk-wallet'
import { SwidgeError } from '@tetherto/wdk-wallet/protocols'

/**
 * @typedef {object} FyndOrder
 * @property {string} tokenIn
 * @property {string} tokenOut
 * @property {bigint} amountIn
 * @property {string} sender
 * @property {string} receiver
 *
 * @typedef {object} ValuationQuote
 * @property {bigint} amountIn
 * @property {bigint} grossOutput
 * @property {bigint} gas
 * @property {bigint | undefined} gasPrice
 * @property {number | undefined} priceImpact - Decimal fraction (0.01 is 1%).
 *
 * @typedef {ValuationQuote & {
 *   routerFee: bigint, clientFee: bigint, minimum: bigint,
 *   transaction: {to: string, data: string, value: bigint}
 * }} EncodedQuote
 *
 * @typedef {object} FyndToken
 * @property {string} address
 * @property {string} symbol
 * @property {number} decimals
 * @property {number} quality
 * @property {number} tax
 */

const QUOTE_FAILURES = new Set([
  'no_route_found', 'insufficient_liquidity', 'timeout', 'not_ready',
  'price_check_failed', 'encoding_failed'
])
const MAX_UINT256 = (1n << 256n) - 1n

/** @param {string} field */
function malformed (field) {
  return new ProviderError(`Fynd returned invalid ${field}.`, {
    reason: ProviderErrorReason.INTERNAL_SERVER_ERROR
  })
}

/** @param {unknown} value @param {string} field @returns {Record<string, unknown>} */
function object (value, field) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw malformed(field)
  return /** @type {Record<string, unknown>} */ (value)
}

/** @param {unknown} value @param {string} field */
function uint (value, field) {
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]{0,77})$/.test(value)) throw malformed(field)
  const result = BigInt(value)
  if (result > MAX_UINT256) throw malformed(field)
  return result
}

/** @param {unknown} value @param {string} field */
function integer (value, field) {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) throw malformed(field)
  return value
}

/** @param {unknown} value @param {string} field */
function address (value, field) {
  if (typeof value !== 'string' || !/^0x[0-9a-fA-F]{40}$/.test(value)) throw malformed(field)
  return value.toLowerCase()
}

/** Hosted Fynd API client. WDK signs and submits transactions. */
export class FyndApi {
  #baseUrl
  #apiKey
  #timeoutMs

  /**
   * @param {{baseUrl?: string, apiKey?: string, chain: string, timeoutMs?: number}} config
   */
  constructor ({ baseUrl = 'https://fynd-api.propellerheads.xyz', apiKey, chain, timeoutMs = 10000 }) {
    let url
    try { url = new URL(baseUrl) } catch { throw new ValueError('baseUrl must be a valid HTTPS URL.') }
    const local = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)
    if ((url.protocol !== 'https:' && !(local && url.protocol === 'http:')) ||
        url.username || url.password || url.search || url.hash) {
      throw new ValueError('baseUrl must use HTTPS without credentials, query or fragment (HTTP is allowed on localhost).')
    }
    if (typeof chain !== 'string' || !/^[a-z]+$/.test(chain)) throw new ValueError('chain must be a Fynd chain slug.')
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 2147483647) {
      throw new ValueError('timeoutMs must be a positive 32-bit integer.')
    }
    if (apiKey !== undefined && (typeof apiKey !== 'string' || !/^[!-~]+$/.test(apiKey))) {
      throw new ValueError('apiKey must be a nonempty printable ASCII token without whitespace.')
    }
    this.#baseUrl = `${url.href.replace(/\/$/, '')}/v1/${chain}`
    this.#apiKey = apiKey
    this.#timeoutMs = timeoutMs
  }

  /** @returns {Promise<{chainId: number, routerAddress: string | null, version: string}>} */
  async info () {
    const info = object(await this.#request('/info'), 'instance metadata')
    if (typeof info.version !== 'string') throw malformed('instance version')
    return {
      chainId: integer(info.chain_id, 'chain ID'),
      routerAddress: info.router_address === null ? null : address(info.router_address, 'router address'),
      version: info.version
    }
  }

  /**
   * @overload
   * @param {FyndOrder} order
   * @param {{encode: false, slippage?: string}} options
   * @returns {Promise<ValuationQuote>}
   */
  /**
   * @overload
   * @param {FyndOrder} order
   * @param {{slippage: string, encode?: true}} options
   * @returns {Promise<EncodedQuote>}
   */
  /**
   * @param {FyndOrder} order
   * @param {{slippage?: string, encode?: boolean}} options
   * @returns {Promise<EncodedQuote | ValuationQuote>}
   */
  async quote (order, { slippage, encode = true }) {
    if (typeof order.amountIn !== 'bigint' || order.amountIn <= 0n || order.amountIn > MAX_UINT256) {
      throw new ValueError('Fynd quote amountIn must be a positive uint256 bigint.')
    }
    if (encode && (typeof slippage !== 'string' ||
        !/^(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?$/.test(slippage) ||
        !Number.isFinite(Number(slippage)) || Number(slippage) >= 1)) {
      throw new ValueError('Fynd quote slippage must be a numeric string from 0 (inclusive) to 1 (exclusive).')
    }
    const request = {
      orders: [{
        token_in: order.tokenIn, token_out: order.tokenOut, amount: order.amountIn.toString(),
        side: 'sell', sender: order.sender, receiver: order.receiver
      }],
      options: encode ? { encoding_options: { slippage, transfer_type: 'transfer_from' } } : {}
    }
    const response = object(await this.#request('/quote', request), 'quote response')
    if (!Array.isArray(response.orders) || response.orders.length !== 1) throw malformed('quote order count')
    const quote = object(response.orders[0], 'quote order')
    if (typeof quote.status === 'string' && QUOTE_FAILURES.has(quote.status)) {
      throw new SwidgeError(`Fynd quote failed: ${quote.status}.`, { reason: `FYND_${quote.status.toUpperCase()}` })
    }
    if (quote.status !== 'success') throw malformed('quote status')
    const amountIn = uint(quote.amount_in, 'quote input amount')
    if (amountIn !== order.amountIn) throw malformed('quote input amount (does not match request)')
    const grossOutput = uint(quote.amount_out, 'quote output amount')
    if (grossOutput === 0n) throw malformed('quote output amount')
    let priceImpact
    if (quote.price_impact_bps !== undefined && quote.price_impact_bps !== null) {
      if (typeof quote.price_impact_bps !== 'number' || !Number.isInteger(quote.price_impact_bps) ||
          quote.price_impact_bps < -2147483648 || quote.price_impact_bps > 2147483647) throw malformed('price impact')
      priceImpact = quote.price_impact_bps / 10000
    }
    const result = {
      amountIn, grossOutput, gas: uint(quote.gas_estimate, 'gas estimate'),
      gasPrice: quote.gas_price == null ? undefined : uint(quote.gas_price, 'gas price'), priceImpact
    }
    if (!encode) return result
    const fees = object(quote.fee_breakdown, 'fee breakdown')
    const routerFee = uint(fees.router_fee, 'router fee')
    const clientFee = uint(fees.client_fee, 'client fee')
    const minimum = uint(fees.min_amount_received, 'minimum received')
    const maxSlippage = uint(fees.max_slippage, 'maximum slippage')
    if (routerFee + clientFee + minimum + maxSlippage !== grossOutput) throw malformed('fee arithmetic')
    const transaction = object(quote.transaction, 'encoded transaction')
    if (typeof transaction.data !== 'string' || !/^0x(?:[0-9a-fA-F]{2}){4,}$/.test(transaction.data)) {
      throw malformed('transaction calldata')
    }
    return {
      ...result, routerFee, clientFee, minimum,
      transaction: {
        to: address(transaction.to, 'transaction destination'), data: transaction.data,
        value: uint(transaction.value, 'transaction value')
      }
    }
  }

  /** Returns a complete single-block list filtered to ordinary, untaxed tokens. @returns {Promise<FyndToken[]>} */
  async tokens () {
    const deadline = Date.now() + this.#timeoutMs
    /** @type {FyndToken[]} */
    const tokens = []
    const seen = new Set()
    let offset = 0
    let expectedBlock
    let expectedTotal
    do {
      const page = object(await this.#request(`/tokens?limit=1000&offset=${offset}`, undefined, deadline), 'token page')
      const total = integer(page.total, 'token total')
      const block = integer(page.block, 'token block')
      if (expectedBlock !== undefined && (block !== expectedBlock || total !== expectedTotal)) {
        throw new ProviderError('Fynd token list changed during pagination; request the list again.', {
          reason: ProviderErrorReason.INTERNAL_SERVER_ERROR
        })
      }
      expectedBlock = block
      expectedTotal = total
      if (!Array.isArray(page.tokens) || page.tokens.length > 1000 || offset + page.tokens.length > total ||
          (offset < total && page.tokens.length === 0)) throw malformed('token pagination')
      for (const entry of page.tokens) {
        const token = object(entry, 'token metadata')
        const tokenAddress = address(token.address, 'token address')
        if (seen.has(tokenAddress)) throw malformed('duplicate token across pages')
        seen.add(tokenAddress)
        const quality = integer(token.quality, 'token quality')
        const tax = integer(token.tax, 'token tax')
        const decimals = integer(token.decimals, 'token decimals')
        if (decimals > 255 || typeof token.symbol !== 'string') throw malformed('token metadata')
        if (quality === 100 && tax === 0) tokens.push({ address: tokenAddress, symbol: token.symbol, decimals, quality, tax })
      }
      offset += page.tokens.length
    } while (offset < expectedTotal)
    return tokens
  }

  /** @param {string} path @param {unknown} [body] @param {number} [deadline] @returns {Promise<unknown>} */
  async #request (path, body, deadline = Date.now() + this.#timeoutMs) {
    const remaining = deadline - Date.now()
    if (remaining <= 0) throw new ProviderError('Fynd request timed out.', { reason: ProviderErrorReason.REQUEST_TIMEOUT })
    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), remaining)
    try {
      const response = await fetch(`${this.#baseUrl}${path}`, {
        method: body === undefined ? 'GET' : 'POST',
        headers: {
          Accept: 'application/json',
          ...(body === undefined ? {} : { 'Content-Type': 'application/json' }),
          ...(this.#apiKey === undefined ? {} : { Authorization: this.#apiKey })
        },
        body: body === undefined ? undefined : JSON.stringify(body),
        redirect: 'error', signal: controller.signal
      })
      // Body reads share the deadline. Omit provider text and fetch causes, which
      // can expose API keys, proxy credentials or request details.
      const text = await response.text()
      if (Date.now() >= deadline) {
        throw new ProviderError('Fynd request timed out.', { reason: ProviderErrorReason.REQUEST_TIMEOUT })
      }
      if (!response.ok) {
        const reason = response.status === 401 ? ProviderErrorReason.UNAUTHORIZED
          : response.status === 403 ? ProviderErrorReason.FORBIDDEN
            : response.status === 408 || response.status === 504 ? ProviderErrorReason.REQUEST_TIMEOUT
              : ProviderErrorReason.INTERNAL_SERVER_ERROR
        throw new ProviderError(`Fynd HTTP request failed (${response.status}).`, { reason })
      }
      try { return JSON.parse(text) } catch { throw malformed('JSON response') }
    } catch (error) {
      if (error instanceof ProviderError) throw error
      throw new ProviderError(controller.signal.aborted ? 'Fynd request timed out.' : 'Fynd network request failed.', {
        reason: controller.signal.aborted ? ProviderErrorReason.REQUEST_TIMEOUT : ProviderErrorReason.NETWORK_ERROR
      })
    } finally {
      clearTimeout(timer)
      controller.abort()
    }
  }
}
