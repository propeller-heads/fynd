// Copyright 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0

import { afterAll, beforeAll, describe, expect, it } from 'vitest'
import { createServer } from 'node:http'
import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import { fileURLToPath } from 'node:url'

const run = promisify(execFile)
const packageRoot = fileURLToPath(new URL('../', import.meta.url))
const script = fileURLToPath(new URL('./runtime-smoke.mjs', import.meta.url))
const apiKey = 'wdk-local-runtime-secret'
const records = []
let baseUrl
const tokenPage = {
  total: 1, block: 1,
  tokens: [{ address: '0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2', symbol: 'WETH', decimals: 18, quality: 100, tax: 0 }]
}
const server = createServer((request, response) => {
  records.push({ path: request.url, authorization: request.headers.authorization })
  if (request.url.startsWith('/redirect/')) {
    response.writeHead(302, { Location: '/sink' })
    response.end()
  } else if (request.url.startsWith('/unauthorized/')) {
    response.writeHead(401, { 'Content-Type': 'text/plain' })
    response.end(`Unauthorized: ${request.headers.authorization}`)
  } else if (request.url.startsWith('/timeout/')) {
    response.writeHead(200, { 'Content-Type': 'application/json' })
    response.write('{"tokens":')
    const timer = setTimeout(() => response.end('[],"total":0,"block":1}'), 2000)
    response.on('close', () => clearTimeout(timer))
  } else {
    response.writeHead(200, { 'Content-Type': 'application/json' })
    response.end(JSON.stringify(tokenPage))
  }
})

beforeAll(async () => {
  await new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolve)
  })
  baseUrl = `http://127.0.0.1:${server.address().port}`
})
afterAll(async () => {
  server.closeAllConnections()
  await new Promise(resolve => server.close(resolve))
})

describe('public package with real HTTP', () => {
  it.each([
    ['node', process.execPath],
    ['bare', fileURLToPath(new URL('../node_modules/.bin/bare', import.meta.url))]
  ])('imports and enforces HTTP behavior under %s', async (runtime, executable) => {
    const firstRecord = records.length
    const { stdout, stderr } = await run(executable, [script], {
      cwd: packageRoot, env: { ...process.env, FYND_SMOKE_URL: baseUrl }, timeout: 10000
    })
    expect(stderr).toBe('')
    expect(JSON.parse(stdout.trim())).toEqual({ runtime, exports: true, tokens: true, timeout: true, redirect: true, redaction: true })
    const requests = records.slice(firstRecord)
    expect(requests.map(record => record.path)).toEqual([
      '/v1/ethereum/tokens?limit=1000&offset=0',
      '/timeout/v1/ethereum/tokens?limit=1000&offset=0',
      '/redirect/v1/ethereum/tokens?limit=1000&offset=0',
      '/unauthorized/v1/ethereum/tokens?limit=1000&offset=0'
    ])
    expect(requests.every(record => record.authorization === apiKey)).toBe(true)
    expect(requests.some(record => record.path === '/sink')).toBe(false)
  }, 15000)
})
