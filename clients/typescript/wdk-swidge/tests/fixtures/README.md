# Local WDK account fixtures

`wdk-account.test.js` starts an ephemeral Anvil chain and mock Fynd HTTP server on loopback. A native-input swap matrix checks WDK registration, signing, broadcast and status with all seven supported chain IDs. Ethereum cases cover approvals and failures. Tests use no wallet-method mocks or private WDK fields.

`WalletFixtures.sol` provides a permissionlessly mintable token with optional USDT-style resets. Its router accepts `singleSwap` but interprets route bytes as a fixed output amount. Anvil installs that runtime at the pinned router address on the local chain only. The mnemonic and balances are public development fixtures.

Tests cover recipients, native input/output, approval refresh/reset, partial progress and pending/reverted receipts. The chain matrix uses local fixtures, not each network's execution or fee rules; it does not establish live DEX settlement, RPC compatibility or production gas costs.

Run from the package directory with Anvil on `PATH`, or set `ANVIL_BIN`:

```sh
pnpm exec vitest run tests/wdk-account.test.js
```

`wallet-fixtures.json` records solc 0.8.33, npm integrity, source SHA-256, compiler settings, ABI and runtime. Tests check the source hash; running them needs no compiler.

To regenerate, install the compiler with scripts disabled:

```sh
npm install --prefix /tmp/fynd-wdk-fixture-build --ignore-scripts --save-exact solc@0.8.33
```

Then run this from the package directory and review diagnostics and the JSON diff:

```sh
node --input-type=module <<'JS'
import { readFileSync, writeFileSync } from 'node:fs'
import { createHash } from 'node:crypto'
import solc from '/tmp/fynd-wdk-fixture-build/node_modules/solc/index.js'
const source = readFileSync('tests/fixtures/WalletFixtures.sol', 'utf8')
const settings = { optimizer: { enabled: true, runs: 200 }, viaIR: true, evmVersion: 'cancun', outputSelection: { '*': { '*': ['abi', 'evm.deployedBytecode.object'] } } }
const output = JSON.parse(solc.compile(JSON.stringify({ language: 'Solidity', sources: { 'WalletFixtures.sol': { content: source } }, settings })))
for (const diagnostic of output.errors ?? []) {
  console.error(diagnostic.formattedMessage)
  if (diagnostic.severity === 'error') process.exit(1)
}
const lock = JSON.parse(readFileSync('/tmp/fynd-wdk-fixture-build/package-lock.json'))
const compilerIntegrity = Object.entries(lock.packages).find(([path]) => path.endsWith('node_modules/solc'))[1].integrity
const contracts = Object.fromEntries(Object.entries(output.contracts['WalletFixtures.sol']).map(([name, item]) => [name, { abi: item.abi, runtime: '0x' + item.evm.deployedBytecode.object }]))
writeFileSync('tests/fixtures/wallet-fixtures.json', JSON.stringify({ compiler: solc.version(), compilerIntegrity, sourceSha256: createHash('sha256').update(source).digest('hex'), settings, contracts }, null, 2) + '\n')
JS
```
