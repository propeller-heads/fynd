// Copyright 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0

import { getAddress, ZeroAddress } from 'ethers'
import { InvalidTokenError, MaximumFeeExceededError, ValueError } from '@tetherto/wdk-wallet'

export const NATIVE = ZeroAddress
const U256_MAX = (1n << 256n) - 1n

/** @param {unknown} value @param {string} name @param {boolean} [allowZero] */
export function amount (value, name, allowZero = false) {
  if (!(typeof value === 'bigint' || (typeof value === 'number' && Number.isSafeInteger(value)))) {
    throw new ValueError(`${name} must be a bigint or safe integer.`)
  }
  const result = BigInt(value)
  if (result < (allowZero ? 0n : 1n) || result > U256_MAX) throw new ValueError(`${name} is outside the supported range.`)
  return result
}

/** @param {unknown} value @param {string} name */
export function address (value, name) {
  try {
    if (typeof value !== 'string') throw new Error()
    return getAddress(value).toLowerCase()
  } catch {
    throw new ValueError(`${name} must be an EVM address.`)
  }
}

/** @param {unknown} value */
export function token (value) {
  try {
    const result = address(value, 'token')
    return result === '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee' ? NATIVE : result
  } catch {
    throw new InvalidTokenError('Use an EVM token address or the native zero address.', {})
  }
}

/** @param {unknown} value */
export function slippage (value = 0.005) {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0 || value >= 1) {
    throw new ValueError('slippage must be a decimal from 0 (inclusive) to 1 (exclusive).')
  }
  // Match the official Fynd TypeScript client; preserve the caller's precision.
  return value.toString()
}

/** @param {number | bigint | undefined} value @param {string} name */
export function cap (value, name) {
  return value === undefined ? undefined : amount(value, name, true)
}

/** @param {bigint} fee @param {bigint} denominator @param {bigint | undefined} bps @param {string} name */
export function enforceCap (fee, denominator, bps, name) {
  if (bps !== undefined && fee * 10000n > denominator * bps) {
    throw new MaximumFeeExceededError(`The quoted ${name} fee exceeds its cap.`, {})
  }
}

/** Match Fynd's post-fee, six-decimal slippage truncation (encoder.rs).
 * @param {bigint} netOutput @param {string} slippage
 */
export function minimumForSlippage (netOutput, slippage) {
  const units = BigInt(Math.floor(Number(slippage) * 1_000_000))
  return netOutput - netOutput * units / 1_000_000n
}
