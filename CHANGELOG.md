## [0.21.0](https://github.com/propeller-heads/fynd/compare/0.20.1...0.21.0) (2026-03-19)


### Features

* **tools:** add fynd-swap-cli binary ([7a39785](https://github.com/propeller-heads/fynd/commit/7a39785f5c78f37d90ceea40b64f8cd07813d26d))


### Bug Fixes

* **protocols:** replace debug! with info! in fetch_protocol_systems ([e22aa82](https://github.com/propeller-heads/fynd/commit/e22aa82526f3ab9f5fd77f054b68ca41bff6a11d))

## [0.20.1](https://github.com/propeller-heads/fynd/compare/0.20.0...0.20.1) (2026-03-19)

## [0.20.0](https://github.com/propeller-heads/fynd/compare/0.19.1...0.20.0) (2026-03-19)


### Features

* **benchmark:** add `scale` subcommand for CPU scaling benchmarks ([7bcfe99](https://github.com/propeller-heads/fynd/commit/7bcfe994e5670ce6f0f92cfae931dad07fde175f))

## [0.19.1](https://github.com/propeller-heads/fynd/compare/0.19.0...0.19.1) (2026-03-18)

## [0.19.0](https://github.com/propeller-heads/fynd/compare/0.18.0...0.19.0) (2026-03-17)


### Features

* add gas_price_age_ms to health response ([7832b9a](https://github.com/propeller-heads/fynd/commit/7832b9a0f93d0f7a60f65527254e0208e4edadbe))
* optionally return 503 on stale gas price ([85103dd](https://github.com/propeller-heads/fynd/commit/85103ddd77643f74015ddeadaa43710364d6141a))


### Bug Fixes

* handle gas price RPC errors instead of panicking ([e544b97](https://github.com/propeller-heads/fynd/commit/e544b97e3f3a614517c847d8e8ccc1e0ffb550d7))

## [0.18.0](https://github.com/propeller-heads/fynd/compare/0.17.2...0.18.0) (2026-03-17)


### Features

* add chain-specific defaults for tycho_url ([92cd81d](https://github.com/propeller-heads/fynd/commit/92cd81dee3ac92781aa4555e969d06a38f41750c)), closes [#88](https://github.com/propeller-heads/fynd/issues/88)
* add traded_n_days_ago parameter ([e92946c](https://github.com/propeller-heads/fynd/commit/e92946c37104761b2c0d27657f90788a539b0037))


### Bug Fixes

* apply TVL buffer as lower bound for hysteresis ([d0cdf09](https://github.com/propeller-heads/fynd/commit/d0cdf099a321811b2cad80b9c7ac846b152a8f2e))
* connect min_token_quality CLI arg to builder ([df7785b](https://github.com/propeller-heads/fynd/commit/df7785b47193917335a51829af563a8c2aab04af))
* error on unknown chain in default_tycho_url ([23a5411](https://github.com/propeller-heads/fynd/commit/23a5411bcb13c89de890c26403b52a80d654aca3))

## [0.17.2](https://github.com/propeller-heads/fynd/compare/0.17.1...0.17.2) (2026-03-17)


### Bug Fixes

* rename all refs of /v1/solve to /v1/quote ([a8cedac](https://github.com/propeller-heads/fynd/commit/a8cedac01928a201dc1ce5a8ae85ec271cf9c347))

## [0.17.1](https://github.com/propeller-heads/fynd/compare/0.17.0...0.17.1) (2026-03-17)


### Bug Fixes

* **ci:** pre-build current rustdoc to avoid experimental feature false positives ([4d693b9](https://github.com/propeller-heads/fynd/commit/4d693b984e67ae444d68420f69361489a61b0c1c))

## [0.17.0](https://github.com/propeller-heads/fynd/compare/0.16.2...0.17.0) (2026-03-17)


### Features

* add all_onchain keyword for combining on-chain and RFQ protocols ([5d440a6](https://github.com/propeller-heads/fynd/commit/5d440a6ab88e48fcefe988b463a730617d16651f))
* default to all protocol systems when --protocols is omitted ([c0c8abd](https://github.com/propeller-heads/fynd/commit/c0c8abd9cc3986b2aea227abd7ac6a61d00701f7))

## [0.16.2](https://github.com/propeller-heads/fynd/compare/0.16.1...0.16.2) (2026-03-17)


### Bug Fixes

* **benchmark:** add missing reqwest dep and apply formatting ([296d5e9](https://github.com/propeller-heads/fynd/commit/296d5e9c74de55a2f97ee930559c3c086fbefd0d))
* **benchmark:** clean up unused deps, naming, and API surface ([071b6bf](https://github.com/propeller-heads/fynd/commit/071b6bfcc1a3cbc42319fdfd032ff26c0f33a070))
* restore fynd-client dev-dep removed by benchmark cleanup ([72fc0fe](https://github.com/propeller-heads/fynd/commit/72fc0fedb13e272c53576af2831fd539783c80a0))
* update Cargo.lock after restoring fynd-client dev-dep ([e558e31](https://github.com/propeller-heads/fynd/commit/e558e31a50a9fd949b1fd3c1ede1f5f1d8c3dc25))
* update Cargo.lock to reflect workspace version 0.16.0 ([e8a967f](https://github.com/propeller-heads/fynd/commit/e8a967f8e33c5674f2abf0e3b0dcabd3a1d88ec3))

## [0.16.1](https://github.com/propeller-heads/fynd/compare/0.16.0...0.16.1) (2026-03-17)


### Bug Fixes

* **ci:** build baseline rustdoc from full workspace to fix dep resolution ([8711131](https://github.com/propeller-heads/fynd/commit/8711131281834763ca2ceb901b70b78aa3eda3dc))
* **ci:** use --workspace to build baseline rustdoc for all member crates ([6cb6f67](https://github.com/propeller-heads/fynd/commit/6cb6f6780fff61750259a6bb25e6b4cae70bf570))
* correct indentation of types trigger in semver-check workflow ([7b586cb](https://github.com/propeller-heads/fynd/commit/7b586cbad41da53cf00c39578f79b83510f07254))
* **deps:** bump tycho minimum versions to match locked versions ([1da3534](https://github.com/propeller-heads/fynd/commit/1da3534a4641c37e3e4012e49b1ba6a8fb939bfc))

## [0.16.0](https://github.com/propeller-heads/fynd/compare/0.15.0...0.16.0) (2026-03-16)


### Features

* **client:** expose Permit2 transfer support in FyndClient ([08373bf](https://github.com/propeller-heads/fynd/commit/08373bfc5e6cb7150ef041aef6bbc0b40bb7dcb3))
* **client:** use server-supplied calldata from OrderQuote [ENG-5697] ([5e95a8a](https://github.com/propeller-heads/fynd/commit/5e95a8a65798d9d148d01fa6b34f8f12b78afbe7))
* **example:** add permit2 example using Permit2 token authorization ([0bbd0e1](https://github.com/propeller-heads/fynd/commit/0bbd0e156273c75915531ef43e45a8c1a4f441d2))
* **example:** check sell-token allowance before on-chain execution ([f4b6f0f](https://github.com/propeller-heads/fynd/commit/f4b6f0f28d62b213726cd49b03d408145a3b8cdd))
* **example:** detect ERC-20 balance/allowance slots via eth_call probing ([123bd0a](https://github.com/propeller-heads/fynd/commit/123bd0a8dab41e677518c4b934aa8979c63c34ce))
* **example:** rewrite tutorial using FyndClient, add --execute flag ([fb8a44b](https://github.com/propeller-heads/fynd/commit/fb8a44bb83f9d91f99d8d81321a4ebce07cddfde))


### Bug Fixes

* address PR [#90](https://github.com/propeller-heads/fynd/issues/90) review findings ([de2eee1](https://github.com/propeller-heads/fynd/commit/de2eee13f40d4701786add8ed9213ae33c8c686a))
* **client:** address PR review findings on Permit2 types ([629e426](https://github.com/propeller-heads/fynd/commit/629e4266ef5c7cf2fbdd275eaca4c2fd6f308736))
* **example:** approve exact sell amount instead of max uint256 ([2bdb963](https://github.com/propeller-heads/fynd/commit/2bdb9636bd3359e02fa0ed0fc5a2f3aa7290c0d9))

## [0.15.0](https://github.com/propeller-heads/fynd/compare/0.14.1...0.15.0) (2026-03-16)


### Features

* **ts-client:** add permit2 module with signing hash and builder helpers ([7baa252](https://github.com/propeller-heads/fynd/commit/7baa252da7de632cd3dfc93d637cb8c10cc49cef))
* **ts-client:** add Permit2, EncodingOptions, and Transaction domain types ([8efc71c](https://github.com/propeller-heads/fynd/commit/8efc71cc95d955e9f0fd37dc953455a2bbc3883c))
* **ts-client:** add viemProvider adapter for FyndClient ([f0663ff](https://github.com/propeller-heads/fynd/commit/f0663fff0ab91872c0986ae69b4d87a6df84081a))
* **ts-client:** export new Permit2 types and builder functions ([8640d56](https://github.com/propeller-heads/fynd/commit/8640d5602dea4fd7160dbb388aad515150846c15))
* **ts-client:** remove routerAddress, read transaction from quote ([f2951a2](https://github.com/propeller-heads/fynd/commit/f2951a224cc20aedd5ed79e9ec885cb1e3a29327))
* **ts-client:** update mapping layer for encoding options and transaction ([155a940](https://github.com/propeller-heads/fynd/commit/155a940e20c51f8f7efc2470a795a1b6db70b013))


### Bug Fixes

* **ts-client:** add timeout to settle() to prevent infinite polling ([8adcc28](https://github.com/propeller-heads/fynd/commit/8adcc2884d0b3fb1e1e59ca332f5780001bdc07d))
* **ts-client:** fix slippage serialization, error parsing, and viem receipt polling ([90f2fcc](https://github.com/propeller-heads/fynd/commit/90f2fcc8539a91cb7c690100df0e639d5243f204))

## [0.14.1](https://github.com/propeller-heads/fynd/compare/0.14.0...0.14.1) (2026-03-16)

## [0.14.0](https://github.com/propeller-heads/fynd/compare/0.13.0...0.14.0) (2026-03-16)


### Features

* add quote example exercising health check and two quote requests ([a43ab65](https://github.com/propeller-heads/fynd/commit/a43ab65a40276caefa84396826de0b1a6d238d3e))


### Bug Fixes

* **ci:** point drift check errors to update-openapi.sh ([61e1a84](https://github.com/propeller-heads/fynd/commit/61e1a84dd4da1b68ec5c5e0d587960b13c1eb5d3))
* strip spurious surrounding quotes from BlockInfo.hash on deserialize ([57c0f90](https://github.com/propeller-heads/fynd/commit/57c0f9075a98d4176250ee22625c5f50f9be52ce))
* use Display instead of Debug when formatting block hash in worker ([fae8700](https://github.com/propeller-heads/fynd/commit/fae8700966ab46e021231b6b2132f549d5c28570))

## [0.13.0](https://github.com/propeller-heads/fynd/compare/0.12.1...0.13.0) (2026-03-13)


### Features

* Comparison example ([d81668f](https://github.com/propeller-heads/fynd/commit/d81668f9f3f1f78cfafd19ba303ef7b4beb74680))


### Bug Fixes

* rename all refs of /v1/solve to /v1/quote ([0c4d661](https://github.com/propeller-heads/fynd/commit/0c4d6613a075a436ca1db039c4c95cbf9ccf6a26))


### Reverts

* Restore /v1/solve endpoint in FyndClient ([878fcb2](https://github.com/propeller-heads/fynd/commit/878fcb2a0aecb1f2fcd702b94e41eba41bd375c0))

## [0.12.1](https://github.com/propeller-heads/fynd/compare/0.12.0...0.12.1) (2026-03-13)


### Bug Fixes

* blacklist malfunctioning AMPL and Fluid Lite pools ([29e5114](https://github.com/propeller-heads/fynd/commit/29e5114c5825ebe492a6158e4ddf74a76d8f4814))

## [0.12.0](https://github.com/propeller-heads/fynd/compare/0.11.0...0.12.0) (2026-03-13)


### Features

* Don't allow intermediate cyclical swaps ([eb515e2](https://github.com/propeller-heads/fynd/commit/eb515e2d202c9607b6497d567d99e1f7ef294657))

## [0.11.0](https://github.com/propeller-heads/fynd/compare/0.10.0...0.11.0) (2026-03-12)


### Features

* Add encoding to fynd-core examples ([aea8a11](https://github.com/propeller-heads/fynd/commit/aea8a110412731f98d3718121b275238b9917057))

## [0.10.0](https://github.com/propeller-heads/fynd/compare/0.9.0...0.10.0) (2026-03-12)


### Features

* add @fynd/client package scaffold ([5ae612e](https://github.com/propeller-heads/fynd/commit/5ae612e03fdcf42a19c2e40d872b7d1393f0899e))
* add client types and FyndError ([ffcf3a7](https://github.com/propeller-heads/fynd/commit/ffcf3a71df885bad1a90c4e000f1730fb6846ded))
* add FyndClient with quote, health, sign, and execute ([ae453b3](https://github.com/propeller-heads/fynd/commit/ae453b32625eed32513672d425a76d204386d05a))
* add pnpm workspace and lockfile for TypeScript packages ([f60ef30](https://github.com/propeller-heads/fynd/commit/f60ef30d05e0ce43a91fbbd3cceefc0c70cf8f9c))
* add signing primitives and wire mapping ([6c57b82](https://github.com/propeller-heads/fynd/commit/6c57b82d89a19aa5aa41298ae4b139cdd5d640bb))


### Bug Fixes

* remove dead eslint-disable comments and add missing edge-case tests ([58d861f](https://github.com/propeller-heads/fynd/commit/58d861f3919282dff0e93d29adda6892d08c6030))
* update client to use /v1/quote endpoint (v0.7.0 API) ([c73f681](https://github.com/propeller-heads/fynd/commit/c73f68175054093fb6893a465ab877137aa62810))
* validate yParity before casting in signature parsing ([837644d](https://github.com/propeller-heads/fynd/commit/837644dfe4a0a5ba840c52c27c07fac100537e95))

## [0.9.0](https://github.com/propeller-heads/fynd/compare/0.8.1...0.9.0) (2026-03-11)


### Features

* Use encoder in order manager ([b2dd105](https://github.com/propeller-heads/fynd/commit/b2dd1055ea0a88a3ba887ba4dba27a945fb1bf0c))


### Bug Fixes

* Examples that directly init the order manager. ([cd4f588](https://github.com/propeller-heads/fynd/commit/cd4f588c23d4c78ecb51d7550dcd42c72b08ea23))

## [0.8.1](https://github.com/propeller-heads/fynd/compare/0.8.0...0.8.1) (2026-03-11)


### Bug Fixes

* **ci:** strip info.version from openapi drift check ([f291a1e](https://github.com/propeller-heads/fynd/commit/f291a1ed383433f7ba8954818c26a71b34df0421))

## [0.8.0](https://github.com/propeller-heads/fynd/compare/0.7.0...0.8.0) (2026-03-11)


### Features

* include derived data readiness in health check ([ba06b00](https://github.com/propeller-heads/fynd/commit/ba06b000a34af248cb08af25cf72a7313534f98e))


### Bug Fixes

* **ci:** checkout PR head for openapi drift check ([9386af9](https://github.com/propeller-heads/fynd/commit/9386af9b77592b4d9bac157a6d21ff34a98efdf5))
* update openapi spec after health endpoint changes ([80d08f0](https://github.com/propeller-heads/fynd/commit/80d08f072dbe2c40b2dc95813047b2edcb6663cd))

## [0.7.0](https://github.com/propeller-heads/fynd/compare/0.6.0...0.7.0) (2026-03-10)


### Features

* generate TypeScript autogen package from OpenAPI spec ([0d0415e](https://github.com/propeller-heads/fynd/commit/0d0415ea548d73acf605ca7fbec3e47a297c81be))

## [0.6.0](https://github.com/propeller-heads/fynd/compare/0.5.0...0.6.0) (2026-03-10)


### Features

* **rpc:** add GET /v1/prices endpoint for derived market data ([30c0e9b](https://github.com/propeller-heads/fynd/commit/30c0e9bc254a9df077c8468b48610152bfd6960d))
* **rpc:** gate /v1/prices endpoint behind "experimental" feature ([0850fe6](https://github.com/propeller-heads/fynd/commit/0850fe6ef87f0c1298cb44da89710c93b696909b))


### Bug Fixes

* apply nightly fmt and regenerate OpenAPI spec ([95c1a10](https://github.com/propeller-heads/fynd/commit/95c1a10690362f5f45c3e0b79458571660c6a8c2))
* merge main, apply nightly fmt, regenerate OpenAPI spec ([48a5376](https://github.com/propeller-heads/fynd/commit/48a5376767ae4f33376678d34211b3e898be9d19))
* regenerate openapi.json without experimental schemas ([7f8b77c](https://github.com/propeller-heads/fynd/commit/7f8b77c3bacfc27b5b87f4b9626fdedf115d000e))

## [0.5.0](https://github.com/propeller-heads/fynd/compare/0.4.0...0.5.0) (2026-03-10)


### Features

* add encoder ([d65b2fa](https://github.com/propeller-heads/fynd/commit/d65b2faf5fe35929ed52199054d4e58a5ad086d0))

## [0.4.0](https://github.com/propeller-heads/fynd/compare/0.3.1...0.4.0) (2026-03-10)


### Features

* add openapi subcommand and export spec ([ac2186c](https://github.com/propeller-heads/fynd/commit/ac2186cb6bed3869d26cd0e3d80e798a7213a02c))


### Bug Fixes

* add serve subcommand to Dockerfile entrypoint and README examples ([b13ff0f](https://github.com/propeller-heads/fynd/commit/b13ff0fb3ad02297b931c497900031f18656dcad))

## [0.3.1](https://github.com/propeller-heads/fynd/compare/0.3.0...0.3.1) (2026-03-10)


### Bug Fixes

* **deps:** update quinn-proto 0.11.13 -> 0.11.14 (RUSTSEC-2026-0037) ([868adee](https://github.com/propeller-heads/fynd/commit/868adee53efa77b65e179eaa1ae75168510c2a42))

## [0.3.0](https://github.com/propeller-heads/fynd/compare/0.2.0...0.3.0) (2026-03-09)


### Features

* Add split to Swap ([2abc895](https://github.com/propeller-heads/fynd/commit/2abc895586bfdc1ce9937ce62fbfb850172f1289))
* Use a public RPC by default ([62fd5d7](https://github.com/propeller-heads/fynd/commit/62fd5d79635d43a81dc9e5d1ce6d1f8ede55f96b))

## [0.2.0](https://github.com/propeller-heads/fynd/compare/0.1.0...0.2.0) (2026-03-09)


### Features

* allow external algorithms in WorkerPoolBuilder ([3a5defb](https://github.com/propeller-heads/fynd/commit/3a5defbf8c3a2e6be7c894fcc20b3fe9110ef134))


### Bug Fixes

* Make spawner in WorkerPoolConfig private ([b488c3e](https://github.com/propeller-heads/fynd/commit/b488c3e5e7cc61e0181598c2185ea2243f20c9e2))

## [0.1.0](https://github.com/propeller-heads/fynd/compare/0.0.0...0.1.0) (2026-03-09)


### Features

* commit produces 0.1.0 ([288aaf8](https://github.com/propeller-heads/fynd/commit/288aaf869eb6dbae5c34f09d535b078fd0b531f4))

## 1.0.0 (2026-03-09)


### Features

* Add all protocols by default, improve Readme.md ([833886f](https://github.com/propeller-heads/fynd/commit/833886f11541c55d8391ceef7e194bbb4e40fee5))
* add data store and computation types ([ac6cc8b](https://github.com/propeller-heads/fynd/commit/ac6cc8b66eadc1fd10c9f6e7c77d356395af09b8))
* add data store and computation types ([56ecc9a](https://github.com/propeller-heads/fynd/commit/56ecc9a0fbd93377b04ef8393125a3b154e64cdd))
* add data store and computation types ([25be75d](https://github.com/propeller-heads/fynd/commit/25be75dd78b472112723a89dfbfab9d16a2bc2c0))
* add debug logging for order processing details in worker module ([21b884a](https://github.com/propeller-heads/fynd/commit/21b884a3ad87cf6efd2faee50eb1948e7b27437a))
* add default blacklist file ([b573f51](https://github.com/propeller-heads/fynd/commit/b573f5182cb37d8708f880e98a4d11b310009821))
* add dependency for Transaction type ([040b93e](https://github.com/propeller-heads/fynd/commit/040b93e8c9fc8d4552fb54b38e15ef64eefc15d4))
* add DepthAndSpotPrice edge weight type ([431cd60](https://github.com/propeller-heads/fynd/commit/431cd60cf875fbe02823d6f9c62831296b586c93))
* Add derived computation to builder and plug into worker ([238ab34](https://github.com/propeller-heads/fynd/commit/238ab34177a1893f6a7881c64243ebfb5ff65d26))
* add dry-run mode to FyndClient::execute ([884b75c](https://github.com/propeller-heads/fynd/commit/884b75c961cc86dec25f236e81f94435bb23e675))
* add encoding_options to SolutionOptions ([0df7efa](https://github.com/propeller-heads/fynd/commit/0df7efa446f4c2da2c69b82ccab2be8bf8cbca83))
* add error handling for invalid dependency data in computation modules ([f7d38aa](https://github.com/propeller-heads/fynd/commit/f7d38aa05552b395dae46bb8b3b7dcab413a163c))
* add example to benchmark solving time ([df1442a](https://github.com/propeller-heads/fynd/commit/df1442ab24dbed84fb53db3a92c22030b3fd6e04))
* Add examples to schema ([f28ca3e](https://github.com/propeller-heads/fynd/commit/f28ca3ef560442c64e440f267d69ea648e887fe0))
* add FailedEncoding variant to SolveError ([b6dfed8](https://github.com/propeller-heads/fynd/commit/b6dfed8e32d0d0500d05538a31211f7057baed1a))
* add fynd-client Rust crate ([5aeedfc](https://github.com/propeller-heads/fynd/commit/5aeedfc52abf97a7e229fd9776961352733b1760))
* add gas_price to OrderSolution ([71dda7b](https://github.com/propeller-heads/fynd/commit/71dda7bc051c262b0b9c1cbb80019a1a41251e12))
* Add git URL to README.md ([dda844c](https://github.com/propeller-heads/fynd/commit/dda844c18c386bcaa9e3763ed386850e81c48a3e))
* add GraphError and improve error handling ([0570449](https://github.com/propeller-heads/fynd/commit/0570449c44be8ee5126e57560617d947c8210dfa))
* add metrics ([5319e76](https://github.com/propeller-heads/fynd/commit/5319e760b6e8beed8f619e070aa037a08916d95f))
* add metrics for algorithm simulation tracking ([3176db9](https://github.com/propeller-heads/fynd/commit/3176db9ede5480de33cba84a0cf5c51d1e79f2da))
* add missing docstrings ([753371a](https://github.com/propeller-heads/fynd/commit/753371a69b05cca21fefe9c0a330cb8ecddb07b6))
* add missing docstrings ([6ee08bc](https://github.com/propeller-heads/fynd/commit/6ee08bcf17f075af1094da86a1f2792a7752c087))
* add missing fields to input json to serialize to swap ([55b8257](https://github.com/propeller-heads/fynd/commit/55b82574d9b0d0f9114a4e60fa23c84a58256c7d))
* Add OpenAPI docs ([2e71d0d](https://github.com/propeller-heads/fynd/commit/2e71d0d63fac1ca050769ab0c37e1464bae09e70))
* add optional TVL buffer multiplier to the config. ([8b84713](https://github.com/propeller-heads/fynd/commit/8b84713a02896e758c9536802f96063c5dcf7d5a))
* add path description generation for routes with tests ([1a735c2](https://github.com/propeller-heads/fynd/commit/1a735c2f066f9b796a7aca58b6049294b3d5c7e8))
* add permit structs ([a2dc263](https://github.com/propeller-heads/fynd/commit/a2dc263f84ab9b74e63a5b17889f8a62fc63ef60))
* add pool depth, spot price and gas token price computation modules with error handling ([b766d50](https://github.com/propeller-heads/fynd/commit/b766d50e7579eb065cdbb9f2bb293511ca446639))
* add pool depth, spot price and gas token price computation modules with error handling ([9d065ea](https://github.com/propeller-heads/fynd/commit/9d065eab321c7b4cb57ce610c73729cdae14e479))
* add protocol component and protocol state ([ac0b986](https://github.com/propeller-heads/fynd/commit/ac0b9861e8ef1ff9537a6b726e9ed50726a187a1))
* Add README.md ([bb685e5](https://github.com/propeller-heads/fynd/commit/bb685e50cd7c9eaa73e451548a2886f9b9b6d2f2))
* add RFQ stream ([0384d81](https://github.com/propeller-heads/fynd/commit/0384d813a5810f2124797167189335f68b04c181))
* add rustfmt config and format ([63c2f05](https://github.com/propeller-heads/fynd/commit/63c2f0502c6fb75f90d1860be30dca803a0cf000))
* add shared test utilities and expand unit tests ([d44d447](https://github.com/propeller-heads/fynd/commit/d44d44785fef12d37bcc1afac30841f42c232931))
* add shared test utilities and expand unit tests ([6013d91](https://github.com/propeller-heads/fynd/commit/6013d91063552d65316c8859460df2407748c8a4))
* add SolverError and sigterm handler ([7c9f9ba](https://github.com/propeller-heads/fynd/commit/7c9f9ba5c57cc998f1b3358bf9fab60530e55c5a))
* add transaction to OrderSolution ([1732ebb](https://github.com/propeller-heads/fynd/commit/1732ebb903c37d40d97a09e491e371adc710ea82))
* Add tycho-solver dockerfile ([65093ed](https://github.com/propeller-heads/fynd/commit/65093ed4e459cbb52147b073d5d9f2391f3a1b0b))
* Add versioning ([aacc0b9](https://github.com/propeller-heads/fynd/commit/aacc0b965d25bb2c8a770665cc85e0870c84fb27))
* Add worker pools explanation to README.md ([f8ab6a6](https://github.com/propeller-heads/fynd/commit/f8ab6a62e3a785b6909b6e0ea6c31cac9909df5a))
* adjust solver worker to process single orders ([38b0727](https://github.com/propeller-heads/fynd/commit/38b0727d32519d5573863fbd621bf0e9856ee02b))
* allow updating graph edges ([f0271f5](https://github.com/propeller-heads/fynd/commit/f0271f5efef6904e70a840f68ea2f52eeaa7e128))
* Bump required rust version ([d16dc7e](https://github.com/propeller-heads/fynd/commit/d16dc7e4aa09cd9f6b102eae26920f95bc678ef3))
* change WorkerConfig to AlgorithmConfig ([dce4ee0](https://github.com/propeller-heads/fynd/commit/dce4ee092c0c04b9871923763b14591de963a145))
* clean up unused args and fns ([fbedcfc](https://github.com/propeller-heads/fynd/commit/fbedcfcc39299891d8604d3040439e8853bca46e))
* Cleanup API routes ([680ae5b](https://github.com/propeller-heads/fynd/commit/680ae5bed8e65b2d0455b9f17336dc3f8c4f5074))
* convert to StableDiGraph ([b4c90c1](https://github.com/propeller-heads/fynd/commit/b4c90c1a650068a967fd382546a8582f54d16046))
* Create quickstart ([12c7f59](https://github.com/propeller-heads/fynd/commit/12c7f59695e036121f10b17b80f4442bf9e1dab8))
* create Transaction type ([1e5987e](https://github.com/propeller-heads/fynd/commit/1e5987ea0b2027693aefc02d2a243aa65ca21b07))
* define computation trait ([b4d6857](https://github.com/propeller-heads/fynd/commit/b4d685740c2b1f8ff2338eb026ddeccb01e16acd))
* define computation trait ([c482c4a](https://github.com/propeller-heads/fynd/commit/c482c4a310be0379f19f0d2885bec9177e1ebcf2))
* define computation trait ([8538db0](https://github.com/propeller-heads/fynd/commit/8538db02a95c0a5aea55d503a2ec3e285e8b38fc))
* enhance token gas price computation with path discovery and spread calculation ([0b383c8](https://github.com/propeller-heads/fynd/commit/0b383c82dfbeaa8964c69276b9cb802d2d38ab2b))
* error on solve issues instead of return empty solution ([c7279d7](https://github.com/propeller-heads/fynd/commit/c7279d72c1a71a3e1f2b805c84022f0e4704419f))
* Explicit RFQ requirements on the README.md ([febc68f](https://github.com/propeller-heads/fynd/commit/febc68f011c74919a0ce0708a92137c17becfb09))
* expose min_token_quality filter ([8a319ff](https://github.com/propeller-heads/fynd/commit/8a319ff1e5e5905452344e6e95bd834bb694ea06))
* Fix derived computation elapsed time calculation ([3e5c885](https://github.com/propeller-heads/fynd/commit/3e5c88501ed147bcbb721e2441500ba00cd93311))
* Fix Dockerfile post project renaming ([d68a03a](https://github.com/propeller-heads/fynd/commit/d68a03aa74c303f237e3c50b28879804cb4b44ad))
* Fix interfaces after rebasing ([de84eb0](https://github.com/propeller-heads/fynd/commit/de84eb033f78d3ff1ae87a92192705baebbfa898))
* Fix spot price interface ([2cb0305](https://github.com/propeller-heads/fynd/commit/2cb03057eafe200885fdbb5ca12aff287fe5d724))
* Fix spot price interface ([14d1086](https://github.com/propeller-heads/fynd/commit/14d10862b6faa7224d97b4935ab6e57383f59aea))
* Fix spot price interface ([2413d0c](https://github.com/propeller-heads/fynd/commit/2413d0c8e72af8e4e263d859c757e01b29ba87f7))
* Group solution interfaces on solution.rs ([b6e8e3d](https://github.com/propeller-heads/fynd/commit/b6e8e3dc4eb4743ce29720ac3b21edaed8b66930))
* Handle partial failures on computations ([629087d](https://github.com/propeller-heads/fynd/commit/629087d8b54b9b8ea67ee2fbd48724a57ca27869))
* implement ComputationManager for handling market events and derived data computations ([b5653ae](https://github.com/propeller-heads/fynd/commit/b5653ae8e2f9713bf37a15351c037fab4802bf6d))
* implement From for AlgorithmError to SolutionStatus ([f5480c2](https://github.com/propeller-heads/fynd/commit/f5480c2151d6da5da231cdd3c184042a66e57960))
* implement gas price feed ([f601511](https://github.com/propeller-heads/fynd/commit/f601511be31faa4cdad48e02f3b02b9035531fe4))
* implement initialize graph ([fb0a0be](https://github.com/propeller-heads/fynd/commit/fb0a0be3198270316dde1fc94d5f9d101d80f703))
* implement MarketEventHandler for PetgraphGraphManager ([4d3acb7](https://github.com/propeller-heads/fynd/commit/4d3acb7a76f4a6360ba54d803c87be8889812197))
* Implement OrderManager ([36e1cd4](https://github.com/propeller-heads/fynd/commit/36e1cd4f46bfb70c29e79deb40a6d815fd203aea))
* implement query_pool_swap for pool depth computation with fallback to binary search ([c4d4966](https://github.com/propeller-heads/fynd/commit/c4d496641c61de61357b99cc7a286b1d16976ddd))
* implement ReadinessTracker and integrate on worker ([3f5a9f8](https://github.com/propeller-heads/fynd/commit/3f5a9f8982f4b1b6e977df9b856688dd21db9d51))
* implement solver builder and cli interface ([daf5e79](https://github.com/propeller-heads/fynd/commit/daf5e7971e4bb4611d721545c46af1b1935a61cb))
* implement Tycho Feed logic ([52fd6e6](https://github.com/propeller-heads/fynd/commit/52fd6e609fd2c0589df04d92aab99d3a1f202bd9))
* implement TychoFeedBuilder for improved TychoFeed configuration ([d060408](https://github.com/propeller-heads/fynd/commit/d060408d6bed3816fd5e2d266b76cc91ae3582e7))
* implement worker's run fn ([d00d59a](https://github.com/propeller-heads/fynd/commit/d00d59a8f81c3e1004cad412cfd00410f829e7a5))
* improve dockerfile and build script ([5b048fe](https://github.com/propeller-heads/fynd/commit/5b048febd108894c4e3b5a8ba8806a9cd7fb19cc))
* Improve documentation ([a8a28f9](https://github.com/propeller-heads/fynd/commit/a8a28f945ad7cca9824af2a6cc7261ac16897e28))
* improve node lookups with a node indices map ([5a8ec38](https://github.com/propeller-heads/fynd/commit/5a8ec389add0d1ab0d180f9ffc87c8e0ef3b01cf))
* Improve quickstart to account for errors and set fixed bucket sizes ([d62845c](https://github.com/propeller-heads/fynd/commit/d62845cfe089393dd30d0fcf0e94760dded9ff21))
* improve Readiness tracker interface, small improvements ([682014b](https://github.com/propeller-heads/fynd/commit/682014b8e8d43249ef3ff71d2b5541bb477ff775))
* Improve swap interfaces ([9c9a25d](https://github.com/propeller-heads/fynd/commit/9c9a25d85f361b9a65ade6c05361431e54716fa2))
* improved computation modules error handling and tracing ([404f4b0](https://github.com/propeller-heads/fynd/commit/404f4b0b0ce6ca92c7fd7ce143b6369d74063eea))
* Initial CI setup ([9849972](https://github.com/propeller-heads/fynd/commit/984997228627fb694bebc6038c33c6e6ed9a5253))
* initial impl of most liquid algo ([bcc33ab](https://github.com/propeller-heads/fynd/commit/bcc33ab363401fb64483f39d24c91a66c93135e8))
* Initial interfaces sketch ([7380758](https://github.com/propeller-heads/fynd/commit/7380758f5b1073fe889c0ccfa1cc1c52c16de1fc))
* make `PoolDepthComputation` use `SpotPriceComputation` dependency, add tests for missing data handling and clarify dependency handling ([9d7aae7](https://github.com/propeller-heads/fynd/commit/9d7aae78c3194b431ea00d42fe695fa083502247))
* Make block an option ([33fb392](https://github.com/propeller-heads/fynd/commit/33fb39266a5f95aed0fd3890bf86838d3e03174a))
* Make clippy happy ([498a43f](https://github.com/propeller-heads/fynd/commit/498a43f0a37720630e1ac52dbcb0a79cfc91d151))
* make edge weight optional and improve errors ([95b440f](https://github.com/propeller-heads/fynd/commit/95b440f04a5268fd96b345e8925c5941cae929ba))
* Make graph weights updates possible with any derived data ([e5491e0](https://github.com/propeller-heads/fynd/commit/e5491e0cf3cf76e3aaa9a5a083353ab91dbd31d3))
* merge market updates into one event ([49457ec](https://github.com/propeller-heads/fynd/commit/49457ec78174115d016d32cbdff8f27d689a29ac))
* Move component blacklist to feed. Add blacklist file ([9a3071c](https://github.com/propeller-heads/fynd/commit/9a3071c3e4dd3d413d84376435aec755a9e159fb))
* Move order_manager to fynd-core and add example ([813616c](https://github.com/propeller-heads/fynd/commit/813616cee18e31c6031bb26910dfec706e005926))
* Move worker creation to a registry pattern ([4c936ee](https://github.com/propeller-heads/fynd/commit/4c936ee6a3023adfbd60f46d4237d209b3db1ef9))
* Only recompute spot price and depth for pools that have are changed in the block ([6f902e2](https://github.com/propeller-heads/fynd/commit/6f902e2d558056671413f7a54ec6cf1a6785e421))
* Only recompute token price for pools that have are changed in the block ([3eec28d](https://github.com/propeller-heads/fynd/commit/3eec28d20dc6f69c73a6e645731d100341834cd7))
* optimise solving speed by capping number of routes simulated ([3391e71](https://github.com/propeller-heads/fynd/commit/3391e71fa9e1819bac5259ad8d979de9c1dffe5a))
* Plug Readiness tracker to Worker ([ae7e242](https://github.com/propeller-heads/fynd/commit/ae7e24254bd5ed703bdb76bfeed45cd9b84f2fd5))
* polish market data implementation ([1b9f2f4](https://github.com/propeller-heads/fynd/commit/1b9f2f4e8818b4bea81775b25d71a83e9a59107f))
* reduce public interfaces ([40d524b](https://github.com/propeller-heads/fynd/commit/40d524b42b5f30e0dca37ba93d70742d62575ef8))
* refactor computation modules to use async locks and improve locking strategies ([9042f2d](https://github.com/propeller-heads/fynd/commit/9042f2dd2756873ff42b2d9074a6c15eb8abd8b3))
* Refactor pool depth calculation ([2b920ad](https://github.com/propeller-heads/fynd/commit/2b920ada0a97438733feb5e6b6cb3f46063ae2a6))
* Refresh gas prices before emitting tycho msg updates ([7730859](https://github.com/propeller-heads/fynd/commit/773085990965accb34542be46a3e177cc8053c22))
* Remove mention to multi-chain on README.md ([d471de6](https://github.com/propeller-heads/fynd/commit/d471de6f191ca95950355295f844370889873bd8))
* Remove net_amount_out from Route public interface ([cbd3ba0](https://github.com/propeller-heads/fynd/commit/cbd3ba091e701eb6288a2565a8294b31a4433d68))
* Remove Other option from ProtocolSystem ([ba8d752](https://github.com/propeller-heads/fynd/commit/ba8d752c9861bf746039625c491eafc38b48a7cd))
* Remove useless ProtocolSystem struct ([e80d9fa](https://github.com/propeller-heads/fynd/commit/e80d9fa69a03805bbc3ee3c947a3b31420a72691))
* remove zero-hop paths support ([e5227c5](https://github.com/propeller-heads/fynd/commit/e5227c5a1c4f92d8f56a864d2ecb46359e9f325b))
* rename ComputationRequirements interfaces ([d69f2b2](https://github.com/propeller-heads/fynd/commit/d69f2b24a16106053359f6301875a765e964c853))
* rename MockProtocolSim to FeedMockProtocolSim to eliminate the typetag name collision ([b573bfc](https://github.com/propeller-heads/fynd/commit/b573bfc80c04dcb3c867d6649bab6a4b0ec46581))
* Rename OrderKind to OrderSide ([2b8a6e1](https://github.com/propeller-heads/fynd/commit/2b8a6e17b099bb443137e96df7d66f36532dcb5f))
* Rename Tycho solver/pathfinder -> Fynd ([f08730d](https://github.com/propeller-heads/fynd/commit/f08730d1dfdd34206c32a75d5a6a012acafae1de))
* rename worker_pool::worker_pool mod to worker_pool::pool ([69be4a2](https://github.com/propeller-heads/fynd/commit/69be4a25411a7cb9b95e9de2b5b29f93d515ef36))
* Return custom error if no solvers are ready ([7265ead](https://github.com/propeller-heads/fynd/commit/7265eadf13d75504470f8736283a02a6396827c0))
* Separate core models and dto types ([1970c06](https://github.com/propeller-heads/fynd/commit/1970c0646253f9daecac2f60bdbd24b707a53e52))
* set gas price in MostLiquid ([127e7f9](https://github.com/propeller-heads/fynd/commit/127e7f940e8ea4a84f485f6bbed74fdf70ad2628))
* Setup monitoring and add grafana docker compose ([801d6c6](https://github.com/propeller-heads/fynd/commit/801d6c61edc23690b38b7235915aa1e6cd300b72))
* Simplify interfaces, document future improvements ([8313597](https://github.com/propeller-heads/fynd/commit/8313597891a0c44f4cb4795f6c61de108e5091ca))
* Simplify quickstart, reuse tycho-common types ([47fc85d](https://github.com/propeller-heads/fynd/commit/47fc85d1560717ec885c7f39a732d294309f05c3))
* Simplify quote interface ([19e4287](https://github.com/propeller-heads/fynd/commit/19e42878c846884492cf0d7ed8798bfafbc15aaf))
* sketch the market graph/algorithm relation ([c0c1774](https://github.com/propeller-heads/fynd/commit/c0c1774780097dded7bdc299eb121b50625f1277))
* Split the monolithic Fynd codebase into two focused libraries ([4f9ea6b](https://github.com/propeller-heads/fynd/commit/4f9ea6be19aca68adee435b0eccb186802501c08))
* streamline path simulation and result handling, remove `SimulationResult` ([8e88a58](https://github.com/propeller-heads/fynd/commit/8e88a58491d29d7a6eda2d730d0c829fbe5ef0aa))
* support no tls connections to tycho ([b13db71](https://github.com/propeller-heads/fynd/commit/b13db7141a140ecc1ec7828f0806302b09ce1ebb))
* support single token components (self-loop) ([f64a0dd](https://github.com/propeller-heads/fynd/commit/f64a0dd77bf1fe0684c99a51cf63290f412a9367))
* track gas price update lag ([de969a3](https://github.com/propeller-heads/fynd/commit/de969a30b10ff779948999d30d8fad84f769fc18))
* Update ARCHITECTURE.md ([c72d254](https://github.com/propeller-heads/fynd/commit/c72d2547759046720361c9cd90eb972dbac75f8a))
* Update weights from pool depths ([61ccd60](https://github.com/propeller-heads/fynd/commit/61ccd604883cc58f657215a401a49c4e07c04cff))
* Upgrade bytes package ([8956ba9](https://github.com/propeller-heads/fynd/commit/8956ba9ba8fc8959daaa32d0d824f56b4a450c51))
* use a directed weighted graph ([3bcf1c7](https://github.com/propeller-heads/fynd/commit/3bcf1c79dca47f87d959a9723eaca9ee63ae8dc5))
* use async channel for worker tasks ([cf86d68](https://github.com/propeller-heads/fynd/commit/cf86d689336384ce3122407b6f078122c9d34b89))
* Use full Block structure on MarkedData ([ee2060f](https://github.com/propeller-heads/fynd/commit/ee2060f0770819e1500a94375b4bbcd1754b9769))
* Use string instead of int for amounts on serde ([09e3587](https://github.com/propeller-heads/fynd/commit/09e3587ace5b1b9b7b08efdc3cc19dd38e0cd3ed))
* Verify quickstart simulation output ([672e9a1](https://github.com/propeller-heads/fynd/commit/672e9a1a14ff6c969b3103d910de82618b48762f))
* wrap market data in `Rc` and introduce subset extraction for optimized locking ([ed5c615](https://github.com/propeller-heads/fynd/commit/ed5c61548e016a3b7d17a29a481460c8f2973768))


### Bug Fixes

* add error handling for missing spot prices in token gas price computation ([9382adb](https://github.com/propeller-heads/fynd/commit/9382adb7d33a24b097a2915d6540ebb6803c397c))
* add missing solve error variant ([528ae89](https://github.com/propeller-heads/fynd/commit/528ae89d66f21a13a5492fd6a8fced0933c7331c))
* adjust any rebasing issues ([d0a1cd9](https://github.com/propeller-heads/fynd/commit/d0a1cd951c823f4f05532aa751b35999c7f779d5))
* adjust any rebasing issues ([b91de0a](https://github.com/propeller-heads/fynd/commit/b91de0a6b9225b5f9c7ea9fcde6b4bfda142dc3e))
* adjust any rebasing issues ([4efaa85](https://github.com/propeller-heads/fynd/commit/4efaa857d6b10b14152ba61425e6136c9cd8dd62))
* clarify dependency name in simulation state error ([48610ba](https://github.com/propeller-heads/fynd/commit/48610ba373a9949f7a27f7c3202db85940bd985a))
* correct token order in spot price computation ([28e33dd](https://github.com/propeller-heads/fynd/commit/28e33dd00a67c1b1778f6be1a21da923044f80c5))
* correct wire deserialization and remove unused futures dep ([1a53f58](https://github.com/propeller-heads/fynd/commit/1a53f5882f97e79f05ede93e2f5ab1f108d7346f))
* correctly assign solution amounts according to swap type ([cb61d85](https://github.com/propeller-heads/fynd/commit/cb61d85d77f1b89ae5b6ee34e4cd21a3d34a597e))
* correctly export otel data to tempo ([b1af6fb](https://github.com/propeller-heads/fynd/commit/b1af6fb3e237240c07d2575a1e1c11e63089e1f5))
* Fix Dockerfile ([b578e47](https://github.com/propeller-heads/fynd/commit/b578e47eabde515f47a3bc6bca8bb4aaf3370712))
* fix imports after rebase ([345458a](https://github.com/propeller-heads/fynd/commit/345458abbf94cf820446eca9be9956222106697a))
* fix publicity of core structs ([5292b6e](https://github.com/propeller-heads/fynd/commit/5292b6e191d378e1bc283bf45aba12e8befb14eb))
* fix the docstring ([8ea3ae3](https://github.com/propeller-heads/fynd/commit/8ea3ae3ba4c474a7724c6497163ced9cdc33743d))
* handle case where gas token has no pools and add corresponding test ([85e8904](https://github.com/propeller-heads/fynd/commit/85e8904c8cfadc2092bfff5ef553b7d3ccb25c53))
* improve readability and populate node indices map on initialize ([d7d2bdf](https://github.com/propeller-heads/fynd/commit/d7d2bdf353a106679c94c39754fbdb7d0c8f0194))
* improve shared market data lock handling ([112d73d](https://github.com/propeller-heads/fynd/commit/112d73dcd044098f6246531c135ed97bdf47ff6c))
* **pool-depth:** account for token decimals in limit price scaling ([839d43e](https://github.com/propeller-heads/fynd/commit/839d43e0d349dccdd55eec34b820ed538fd9278e))
* reduce lock hold time on the depth calculator ([75b0e5f](https://github.com/propeller-heads/fynd/commit/75b0e5f12a72ccbc651a1362460f31b565979dfd))
* remove protocol related data from dto Swap ([7cd800e](https://github.com/propeller-heads/fynd/commit/7cd800e17218b31e58a933e31c4da64cb291c704))
* Remove rocketpool from tutorial ([b4b47aa](https://github.com/propeller-heads/fynd/commit/b4b47aa9fadb38aa09aa3b27edab56bbd70fe8ac))
* remove support for single token pools ([2189f91](https://github.com/propeller-heads/fynd/commit/2189f915b09a4061b4ee27a0fc9d9a4e2cfeadb9))
* remove unneeded asserts ([5c6a1ba](https://github.com/propeller-heads/fynd/commit/5c6a1ba00a4443d8a3261abdfe5914be92759fa9))
* remove unused alloy dependency ([dd749e2](https://github.com/propeller-heads/fynd/commit/dd749e2fd2699a89ca173412b9141b9355410fe8))
* resolve post-merge build errors and rename solution to quote ([e6beb62](https://github.com/propeller-heads/fynd/commit/e6beb62c823ab3aa67137e97c045f32ac4677658)), closes [#67](https://github.com/propeller-heads/fynd/issues/67)
* resolve rebase issues ([71a6d8d](https://github.com/propeller-heads/fynd/commit/71a6d8dbefb8ef3c2c74158cf2010690eb9c0ab1))
* return the mid-price in the `compute_spread_and_mid_price` instead of the buy price ([c28e31d](https://github.com/propeller-heads/fynd/commit/c28e31d4390cf4566fb8b6907786ad60f81ceec0))
* Set the right source for tycho-execution and fix initialization ([a9e136a](https://github.com/propeller-heads/fynd/commit/a9e136ac383c52164bac2472b9aea43935608838))
* simplify DepthAndPrice structure by removing fee and updating related logic ([f9c26c0](https://github.com/propeller-heads/fynd/commit/f9c26c0387c710df2e76ff29ee594b4eebd4879d))
* Skip adding a component if it's already in the graph ([43772c2](https://github.com/propeller-heads/fynd/commit/43772c24f2e2d122dbea7e203dfc568759539904))
* **test-utils:** align MockProtocolSim spot_price convention with get_amount_out ([3e5c4b3](https://github.com/propeller-heads/fynd/commit/3e5c4b351fc683cedbfb7cf54b9bc7eeac69ebf8))
* **test-utils:** make get_limits direction-aware and add missing test case ([6d800d3](https://github.com/propeller-heads/fynd/commit/6d800d34f8da5b85d5646a8e25069b3990f24849))
* **test-utils:** make MockProtocolSim decimal-aware and use f64 spot_price ([bc14060](https://github.com/propeller-heads/fynd/commit/bc14060fd047537185edfc11fd3c7877a1bbaed4))
* **test-utils:** remove useless .into() conversions ([7f822bd](https://github.com/propeller-heads/fynd/commit/7f822bd2b2b65a09231b9c3f80de4ba9b7b6006f))
* update cargo audit command to ignore specific security advisory ([67d9b23](https://github.com/propeller-heads/fynd/commit/67d9b236527c251a6fb445f2d6928a05e7a111f7))
* update component insertion logic in tests to use new API methods ([07ae8ab](https://github.com/propeller-heads/fynd/commit/07ae8ab63941ef59adc63ec66613cb2e5ddfc89f))
* update computation calls to use `market_read` for correct lock data handling ([b6375e9](https://github.com/propeller-heads/fynd/commit/b6375e96af9d08f8889c0e55f0351ae96b31be19))
* update computation methods to use non-optional block parameters and improve error handling ([dc11664](https://github.com/propeller-heads/fynd/commit/dc116646ba2f3b2212c4be8e43008f16fd2f78ca))
* update path scoring documentation to reflect removal of fee consideration ([36b3891](https://github.com/propeller-heads/fynd/commit/36b3891dd34149c7605c844ac53b694f47c99526))
* Update quickstart to use the new encoder registry ([a36ff76](https://github.com/propeller-heads/fynd/commit/a36ff7626f3c895755a4198ddbb3bc59bbfd08c1))
* update tycho execution version ([35533db](https://github.com/propeller-heads/fynd/commit/35533db1cb15807ff438f74c9dc2e12d274e2f30))
