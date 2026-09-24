// Copyright 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0

import { createHash } from 'node:crypto'
import { once } from 'node:events'
import { readFileSync } from 'node:fs'
import { createServer } from 'node:http'
import { spawn, spawnSync } from 'node:child_process'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest'
import { AbiCoder, Contract, Interface, JsonRpcProvider, MaxUint256, ZeroAddress, toBeHex } from 'ethers'
import WDK from '@tetherto/wdk'
import WalletManagerEvm from '@tetherto/wdk-wallet-evm'
import FyndSwidgeProtocol from '../index.js'
import { CHAINS } from '../src/router.js'

const fixtures = JSON.parse(readFileSync(new URL('./fixtures/wallet-fixtures.json', import.meta.url)))
const fixtureSource = readFileSync(new URL('./fixtures/WalletFixtures.sol', import.meta.url))
const routerAbi = new Interface(fixtures.contracts.FixtureRouter.abi)
const tokenAbi = new Interface(fixtures.contracts.FixtureToken.abi)
const router = CHAINS.ethereum.router
const inputToken = '0x000000000000000000000000000000000000a100'
const outputToken = '0x000000000000000000000000000000000000a200'
const usdt = '0xdac17f958d2ee523a2206206994597c13d831ec7'
const recipient = '0x000000000000000000000000000000000000b100'
const native = '0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee'
// Anvil's public, disposable development mnemonic. Never use for real funds.
const mnemonic = 'test test test test test test test test test test test junk'
const inputAmount = 10n ** 16n
const grossOutput = 1_000_000n

let anvil, provider, mockApi, baseUrl, snapshot, wdk, account, protocol, sender
let quoteCalls, nextMinimum

async function listen (server) {
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  return server.address().port
}

async function handleApi (request, response) {
  response.setHeader('Content-Type', 'application/json')
  if (request.url === '/v1/ethereum/info') {
    response.end(JSON.stringify({ chain_id: 1, router_address: router, version: 'test-fixture' }))
    return
  }
  if (request.url !== '/v1/ethereum/quote') {
    response.writeHead(404).end('{}')
    return
  }
  let body = ''
  for await (const chunk of request) body += chunk
  const query = JSON.parse(body)
  const order = query.orders[0]
  const tokenIn = order.token_in === ZeroAddress ? native : order.token_in
  const tokenOut = order.token_out === ZeroAddress ? native : order.token_out
  const allowance = tokenIn === native ? 0n : await new Contract(tokenIn, tokenAbi, provider).allowance(sender, router)
  const encoded = query.options.encoding_options !== undefined
  quoteCalls.push({ order, allowance, encoded })
  if (!encoded) {
    // A full-trade input-to-native valuation quote, without transaction encoding.
    if (order.token_out !== ZeroAddress) throw new Error('Unexpected valuation token')
    response.end(JSON.stringify({ orders: [{
      status: 'success', amount_in: order.amount, amount_out: '1000000000000000000',
      gas_estimate: '180000', gas_price: '1000000000'
    }] }))
    return
  }
  // Independently mirror Fynd's documented integer slippage calculation.
  // Keep this separate from the production guard so the fixture can catch drift.
  const slippageUnits = BigInt(Math.floor(Number(query.options.encoding_options.slippage) * 1_000_000))
  const encodedMinimum = grossOutput - grossOutput * slippageUnits / 1_000_000n
  const minimum = nextMinimum(quoteCalls.length, encodedMinimum)
  const data = routerAbi.encodeFunctionData('singleSwap', [
    order.amount, tokenIn, tokenOut, grossOutput, minimum, order.receiver,
    [0, ZeroAddress, 0, MaxUint256, '0x'], AbiCoder.defaultAbiCoder().encode(['uint256'], [grossOutput])
  ])
  response.end(JSON.stringify({ orders: [{
    status: 'success', amount_in: order.amount, amount_out: grossOutput.toString(),
    gas_estimate: '180000', gas_price: '1000000000',
    fee_breakdown: { router_fee: '0', client_fee: '0', min_amount_received: minimum.toString(), max_slippage: (grossOutput - minimum).toString() },
    transaction: { to: router, data, value: tokenIn === native ? order.amount : '0' }
  }] }))
}

beforeAll(async () => {
  expect(createHash('sha256').update(fixtureSource).digest('hex')).toBe(fixtures.sourceSha256)
  const binary = process.env.ANVIL_BIN ?? 'anvil'
  if (spawnSync(binary, ['--version']).status !== 0) throw new Error('Install Anvil or set ANVIL_BIN to run the WDK account integration tests.')
  const reservation = createServer()
  const port = await listen(reservation)
  await new Promise(resolve => reservation.close(resolve))
  const rpcUrl = `http://127.0.0.1:${port}`
  anvil = spawn(binary, ['--host', '127.0.0.1', '--port', String(port), '--chain-id', '1', '--hardfork', 'cancun', '--accounts', '3'], { stdio: 'ignore' })
  let startError
  anvil.once('error', error => { startError = error })
  let ready = false
  for (let attempt = 0; attempt < 100; attempt++) {
    if (startError) throw startError
    if (anvil.exitCode !== null) throw new Error('Anvil exited before its local RPC became ready.')
    try {
      const response = await fetch(rpcUrl, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'eth_chainId', params: [] }) })
      if ((await response.json()).result === '0x1') { ready = true; break }
    } catch { /* The owned local process is still starting. */ }
    await new Promise(resolve => setTimeout(resolve, 25))
  }
  if (!ready) throw new Error('Anvil local RPC did not become ready.')
  provider = new JsonRpcProvider(rpcUrl, 1, { staticNetwork: true, cacheTimeout: -1 })
  for (const token of [inputToken, outputToken, usdt]) {
    await provider.send('anvil_setCode', [token, fixtures.contracts.FixtureToken.runtime])
  }
  await provider.send('anvil_setCode', [router, fixtures.contracts.FixtureRouter.runtime])
  await provider.send('anvil_setBalance', [router, toBeHex(10n ** 20n)])
  snapshot = await provider.send('evm_snapshot', [])
  mockApi = createServer((request, response) => {
    handleApi(request, response).catch(() => response.writeHead(500).end('{}'))
  })
  baseUrl = `http://127.0.0.1:${await listen(mockApi)}`
}, 15000)

beforeEach(async () => {
  quoteCalls = []
  nextMinimum = (_count, minimum) => minimum
  wdk = new WDK(mnemonic)
    .registerWallet('ethereum', WalletManagerEvm, { provider, chainId: 1 })
    .registerProtocol('ethereum', 'fynd', FyndSwidgeProtocol, { chainId: 1, baseUrl, approvalTimeoutMs: 10000 })
  account = await wdk.getAccount('ethereum', 0)
  sender = await account.getAddress()
  protocol = account.getSwidgeProtocol('fynd')
  const setupSigner = await provider.getSigner(1)
  for (const token of [inputToken, usdt]) {
    await (await new Contract(token, tokenAbi, setupSigner).mint(sender, inputAmount * 10n)).wait()
  }
})

afterEach(async () => {
  wdk?.dispose()
  if (provider && snapshot) {
    await provider.send('anvil_setAutomine', [true])
    await provider.send('evm_revert', [snapshot])
    snapshot = await provider.send('evm_snapshot', [])
  }
})

afterAll(async () => {
  provider?.destroy()
  if (mockApi) {
    mockApi.closeAllConnections()
    await new Promise(resolve => mockApi.close(resolve))
  }
  if (anvil && anvil.exitCode === null) {
    const exited = once(anvil, 'exit')
    anvil.kill('SIGTERM')
    await exited
  }
})

function options (overrides = {}) {
  return { fromToken: inputToken, toToken: outputToken, fromTokenAmount: inputAmount, recipient, ...overrides }
}

async function wait (hash) {
  const result = await account.waitForTransaction(hash, { target: 'confirmed', timeout: 10000, pollingInterval: 25 })
  expect(result.success).toBe(true)
  return result
}

describe('published WDK account against local fixture contracts', () => {
  it('registers, signs approval and swap, refreshes after approval, and reads completion', async () => {
    expect(protocol).toBeInstanceOf(FyndSwidgeProtocol)
    const quoted = await protocol.quoteSwidge(options())
    expect(quoted.toTokenAmountMin).toBe(995_000n)
    expect(quoted.networkFeeComplete).toBe(false) // Swap cannot estimate before allowance.
    const result = await protocol.swidge(options())
    expect(result.transactions.map(tx => tx.type)).toEqual(['approval', 'source'])
    const approval = await account.getTransaction(result.transactions[0].hash)
    expect(approval.success).toBe(true)
    expect(quoteCalls.map(call => call.allowance)).toEqual([0n, 0n, inputAmount])
    await wait(result.hash)
    expect((await protocol.getSwidgeStatus(result.hash)).status).toBe('completed')
    expect(await new Contract(outputToken, tokenAbi, provider).balanceOf(recipient)).toBe(grossOutput)
    expect(await account.getAllowance(inputToken, router)).toBe(0n)
    const submitted = await provider.getTransaction(result.hash)
    expect(submitted.from.toLowerCase()).toBe(sender.toLowerCase())
    expect(submitted.to.toLowerCase()).toBe(router)
    expect(submitted.chainId).toBe(1n)
    expect(result.networkFeeComplete).toBe(true)
  }, 15000)

  it('executes native input without approval and sends the exact input value', async () => {
    const result = await protocol.swidge(options({ fromToken: ZeroAddress }))
    expect(result.transactions.map(tx => tx.type)).toEqual(['source'])
    await wait(result.hash)
    expect((await provider.getTransaction(result.hash)).value).toBe(inputAmount)
    expect(await new Contract(outputToken, tokenAbi, provider).balanceOf(recipient)).toBe(grossOutput)
  }, 15000)

  it('delivers native output to a custom recipient', async () => {
    const before = await provider.getBalance(recipient)
    const result = await protocol.swidge(options({ toToken: ZeroAddress }))
    await wait(result.hash)
    expect(await provider.getBalance(recipient)).toBe(before + grossOutput)
  }, 15000)

  it('preserves the confirmed approval when the fresh quote weakens the slippage floor', async () => {
    nextMinimum = (count, minimum) => count === 1 ? minimum : minimum - 10_000n
    const nonce = await provider.getTransactionCount(sender)
    let failure
    try { await protocol.swidge(options({ minAmountOut: 995_000n })) } catch (error) { failure = error }
    expect(failure).toBeDefined()
    expect(failure.transactions.map(tx => tx.type)).toEqual(['approval'])
    expect(failure.submissionUnknown).toBe(false)
    expect((await account.getTransaction(failure.transactions[0].hash)).success).toBe(true)
    expect(await provider.getTransactionCount(sender)).toBe(nonce + 1)
    expect(await account.getAllowance(inputToken, router)).toBe(inputAmount)
    expect(await new Contract(outputToken, tokenAbi, provider).balanceOf(recipient)).toBe(0n)
  }, 15000)

  it('confirms both USDT-style reset and exact approval before swapping', async () => {
    const contract = new Contract(usdt, tokenAbi, await provider.getSigner(1))
    await (await contract.setRequireReset(true)).wait()
    await wait((await account.approve({ token: usdt, spender: router, amount: 1n })).hash)
    const result = await protocol.swidge(options({ fromToken: usdt }))
    expect(result.transactions.map(tx => tx.type)).toEqual(['approval', 'approval', 'source'])
    const approvalValues = []
    for (const tx of result.transactions.slice(0, 2)) {
      const submitted = await provider.getTransaction(tx.hash)
      approvalValues.push(tokenAbi.decodeFunctionData('approve', submitted.data)[1])
      expect((await account.getTransaction(tx.hash)).success).toBe(true)
    }
    expect(approvalValues).toEqual([0n, inputAmount])
    expect(quoteCalls.map(call => call.allowance)).toEqual([1n, inputAmount])
    await wait(result.hash)
    expect(await new Contract(outputToken, tokenAbi, provider).balanceOf(recipient)).toBe(grossOutput)
  }, 15000)

  it('rejects an inadequate minimum before any signed transaction', async () => {
    const nonce = await provider.getTransactionCount(sender)
    await expect(protocol.swidge(options({ minAmountOut: grossOutput + 1n }))).rejects.toMatchObject({ reason: 'COULD_NOT_MET_THRESHOLD' })
    expect(await provider.getTransactionCount(sender)).toBe(nonce)
    expect(await account.getAllowance(inputToken, router)).toBe(0n)
  }, 15000)

  it('rejects a network-capped unapproved ERC20 swap before any approval', async () => {
    const nonce = await provider.getTransactionCount(sender)
    await expect(protocol.swidge(options(), { maxNetworkFeeBps: 100 })).rejects.toMatchObject({
      reason: 'FEE_ESTIMATE_UNAVAILABLE'
    })
    expect(await provider.getTransactionCount(sender)).toBe(nonce)
    expect(await account.getAllowance(inputToken, router)).toBe(0n)
    expect(await new Contract(outputToken, tokenAbi, provider).balanceOf(recipient)).toBe(0n)
    expect(quoteCalls).toHaveLength(1)
    expect(quoteCalls[0].encoded).toBe(true)
  }, 15000)

  it('executes a network-capped preapproved ERC20 swap after valuing the full input trade', async () => {
    await wait((await account.approve({ token: inputToken, spender: router, amount: inputAmount })).hash)
    const nonce = await provider.getTransactionCount(sender)
    const result = await protocol.swidge(options(), { maxNetworkFeeBps: 100 })
    expect(result.networkFeeComplete).toBe(true)
    expect(result.transactions.map(tx => tx.type)).toEqual(['source'])
    expect(quoteCalls.map(call => call.encoded)).toEqual([true, false])
    expect(quoteCalls[1].order).toMatchObject({ token_in: inputToken, token_out: ZeroAddress, amount: inputAmount.toString() })
    await wait(result.hash)
    expect(await provider.getTransactionCount(sender)).toBe(nonce + 1)
    expect(await new Contract(outputToken, tokenAbi, provider).balanceOf(recipient)).toBe(grossOutput)
  }, 15000)

  it('reports pending and then failed when a broadcast transaction reverts on inclusion', async () => {
    await provider.send('anvil_setAutomine', [false])
    const result = await protocol.swidge(options({ fromToken: ZeroAddress }))
    expect((await protocol.getSwidgeStatus(result.hash)).status).toBe('pending')
    // A local state change after broadcast forces a real EVM revert on inclusion.
    // This is a receipt-handling test, not a simulation of a Tycho router upgrade.
    await provider.send('anvil_setCode', [router, '0x5f5ffd'])
    await provider.send('anvil_mine', [1])
    expect((await account.getTransaction(result.hash)).success).toBe(false)
    expect((await protocol.getSwidgeStatus(result.hash)).status).toBe('failed')
  }, 15000)
})
