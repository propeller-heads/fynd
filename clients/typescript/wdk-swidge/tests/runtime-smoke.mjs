// Copyright 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0

import FyndSwidgeProtocol, * as exports from 'wdk-protocol-swidge-fynd'

function check (condition, message) {
  if (!condition) throw new Error(message)
}

const baseUrl = process.env.FYND_SMOKE_URL
const apiKey = 'wdk-local-runtime-secret'
check(typeof baseUrl === 'string' && baseUrl.startsWith('http://127.0.0.1:'), 'Smoke URL must be a local server.')
check(FyndSwidgeProtocol === exports.FyndSwidgeProtocol, 'Default and named exports differ.')
check(typeof exports.FyndExecutionError === 'function', 'Missing execution error export.')
check(!('ISwidgeProtocol' in exports), 'ISwidgeProtocol must remain a type-only export.')
check(typeof fetch === 'function' && typeof URL === 'function' && typeof AbortController === 'function', 'Missing HTTP runtime globals.')

const adapter = new FyndSwidgeProtocol(undefined, { chainId: 1, baseUrl, apiKey })
const chains = await adapter.getSupportedChains()
check(chains.some(chain => chain.id === 1) && chains.some(chain => chain.id === 8453), 'Missing supported chains.')
const tokens = await adapter.getSupportedTokens()
check(tokens.length === 2 && tokens.some(token => token.symbol === 'WETH'), 'Token discovery failed.')

const slow = new FyndSwidgeProtocol(undefined, { chainId: 1, baseUrl: `${baseUrl}/timeout`, apiKey, timeoutMs: 100 })
const started = Date.now()
let timeoutError
try { await slow.getSupportedTokens() } catch (error) { timeoutError = error }
check(timeoutError?.reason === 'REQUEST_TIMEOUT', 'Response-body timeout did not produce REQUEST_TIMEOUT.')
check(Date.now() - started < 1000, 'Response-body timeout waited for the delayed body.')

const redirect = new FyndSwidgeProtocol(undefined, { chainId: 1, baseUrl: `${baseUrl}/redirect`, apiKey })
let redirectError
try { await redirect.getSupportedTokens() } catch (error) { redirectError = error }
check(redirectError?.reason === 'NETWORK_ERROR', 'Redirect was not rejected.')
check(!String(redirectError).includes(apiKey) && redirectError.cause === undefined, 'Redirect error leaked request credentials.')

const unauthorized = new FyndSwidgeProtocol(undefined, { chainId: 1, baseUrl: `${baseUrl}/unauthorized`, apiKey })
let authError
try { await unauthorized.getSupportedTokens() } catch (error) { authError = error }
check(authError?.reason === 'UNAUTHORIZED', 'Text HTTP error did not retain its safe reason.')
check(!String(authError).includes(apiKey) && authError.cause === undefined, 'HTTP error leaked echoed credentials.')

console.log(JSON.stringify({ runtime: typeof Bare === 'undefined' ? 'node' : 'bare', exports: true, tokens: true, timeout: true, redirect: true, redaction: true }))
