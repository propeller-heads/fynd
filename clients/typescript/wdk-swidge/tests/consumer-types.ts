// Copyright 2026 PropellerHeads
// SPDX-License-Identifier: Apache-2.0
import WDK from '@tetherto/wdk'
import Wallet, { WalletAccountEvm, WalletAccountReadOnlyEvm } from '@tetherto/wdk-wallet-evm'
import Fynd, { FyndSwidgeProtocol, type FyndConfig, type ISwidgeProtocol } from '../index.js'

const config: FyndConfig = { chainId: 1, quoteSender: '0x1111111111111111111111111111111111111111' }
const protocol: ISwidgeProtocol = new Fynd(undefined, config)
const named: ISwidgeProtocol = new FyndSwidgeProtocol(undefined, config)
const seed = 'test test test test test test test test test test test junk'
const readOnly = new WalletAccountReadOnlyEvm('0x1111111111111111111111111111111111111111', { chainId: 1 })
const full = new WalletAccountEvm(seed, "0'/0/0", { chainId: 1 })
const withReadOnly: ISwidgeProtocol = new Fynd(readOnly, config)
const withFull: ISwidgeProtocol = new Fynd(full, config)
const wdk = new WDK(seed)
wdk.registerProtocol('ethereum', 'fynd', Fynd, config)

// Upstream EVM beta.19/core beta.18 resolve different nominal WalletManager._seed
// declarations even without importing Fynd. Runtime registration is tested separately.
// Remove this known-failure assertion when the compatibility gate is resolved.
// @ts-expect-error Upstream EVM wallet registration has incompatible private _seed types.
wdk.registerWallet('ethereum', Wallet, { chainId: 1 })
void protocol
void named
void withReadOnly
void withFull
