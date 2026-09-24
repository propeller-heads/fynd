# Local WDK account fixtures

`wdk-account.test.js` launches an ephemeral Anvil chain on loopback with chain ID 1 and a local mock Fynd HTTP server. It uses the actual published WDK core and EVM wallet packages for account registration, signing, approval, broadcast, transaction waits and receipt/status reads. No private WDK fields or wallet-method mocks are used.

`WalletFixtures.sol` is deliberately smaller than Tycho's contracts: its token has permissionless minting and an optional USDT-style approval reset; its router accepts the verified `singleSwap` outer ABI but decodes route bytes as a fixed output quantity. Anvil installs this fixture runtime at the adapter's pinned router address **on the disposable local chain only**. The public Anvil mnemonic and all balances are development fixtures, never production credentials or funds. The suite checks custom recipients, native input/output, fresh quotes after approvals, partial progress, reset approvals and real pending/reverted receipts. It does not claim real DEX settlement, chain liquidity, production gas accuracy or live-router end-to-end coverage.

Run from the package directory with Anvil installed on PATH, or set `ANVIL_BIN`:

```sh
pnpm exec vitest run tests/wdk-account.test.js
```

The compiled fixture was generated with npm `solc@0.8.33`, installed with scripts disabled in a temporary directory. `wallet-fixtures.json` records the exact compiler version, npm integrity, SHA-256 of the Solidity source, settings, ABI and deployed runtime. Tests verify the source checksum. No compiler dependency is needed to run the suite.

To regenerate, install `solc@0.8.33` with `npm install --prefix /tmp/fynd-wdk-fixture-build --ignore-scripts --save-exact solc@0.8.33`, then run the following from this package directory. Review any compiler diagnostics and the JSON diff:

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
