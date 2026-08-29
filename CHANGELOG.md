
## [0.99.21](https://github.com/propeller-heads/fynd/compare/0.99.20...0.99.21) (2026-08-29)

### Features

* raise tycho floor to 0.370.2 for Angstrom filter fix ([f0a7a9c](https://github.com/propeller-heads/fynd/commit/f0a7a9c3a5ef534f6e4eec3b13016147e0fd4574))


## [0.99.20](https://github.com/propeller-heads/fynd/compare/0.99.18...0.99.20) (2026-08-28)

### Features

* raise tycho lower bound to 0.370.0 ([d329e3a](https://github.com/propeller-heads/fynd/commit/d329e3a5d07d59025012998ae31a4920c3765fc6))
* **core:** add SolveError::InvalidWorkerPools for allowlist errors ([ab89495](https://github.com/propeller-heads/fynd/commit/ab89495233783fac5f6d1d285c56aa126ae64757))
* **core:** reject empty pool allowlists and rename finalize_quote ([95e3a42](https://github.com/propeller-heads/fynd/commit/95e3a4271ddcf68b631b362b08cc2d32b2fb76bb))
* **core:** expose solve, encode_quotes and finalize stages of the router ([38625b5](https://github.com/propeller-heads/fynd/commit/38625b5b0d6638578998cd3962b4421b1d719738))
* **core:** allow requests to restrict solving to named worker pools ([c80cdbe](https://github.com/propeller-heads/fynd/commit/c80cdbefec307894f032b6ac8e44db2af6db811e))
* **propamm:** name the pAMM components with no fallback pool ([3b48f69](https://github.com/propeller-heads/fynd/commit/3b48f69493ce1402ccee43c114f9688bac9151b2))
* **rpc:** expose the quote pipeline stages and app state for embedders ([0190e01](https://github.com/propeller-heads/fynd/commit/0190e017881559728d20b42bae812a1933be32df))
* **rpc:** add configure_routes hook to register overriding routes ([4b7291b](https://github.com/propeller-heads/fynd/commit/4b7291b998e4ec724d1f35bb2e3bb7634cdc9341))

### Bug Fixes

* validate before capturing and reject empty rankings everywhere ([a1a4049](https://github.com/propeller-heads/fynd/commit/a1a40498ccc6e1e3134e22800177e4ba8564c996))
* **core:** validate the worker pool allowlist once per request ([f050787](https://github.com/propeller-heads/fynd/commit/f05078712431745a9bc6fa40e6e220924510bf32))
* **core:** make RankedQuotes::new fallible instead of panicking ([8e92179](https://github.com/propeller-heads/fynd/commit/8e92179c4119fc95caa1bf839201f432ededcbd5))
* leave a pAMM out of the graph when its fallback cannot be priced ([d35d618](https://github.com/propeller-heads/fynd/commit/d35d618d81ef85bd3f36e0e15249c0860b8c5b64))
* **rpc:** restrict RequestOutcome and is_failure to crate-internal use ([d355097](https://github.com/propeller-heads/fynd/commit/d3550974a3dd6a776c8422824fdfc52e6d9b77c7))
* **rpc:** derive num_orders from capture in log_quote_outcome ([f2fdf67](https://github.com/propeller-heads/fynd/commit/f2fdf67eb91ea615291ec2b0e3047f4e48811912))
* **rpc:** harden RequestOutcome and document route override limits ([6a8277d](https://github.com/propeller-heads/fynd/commit/6a8277d0a948b50d9734d15979741581f8936e15))


## [0.99.19](https://github.com/propeller-heads/fynd/compare/0.99.18...0.99.19) (2026-08-28)

### Features

* raise tycho lower bound to 0.370.0 ([d329e3a](https://github.com/propeller-heads/fynd/commit/d329e3a5d07d59025012998ae31a4920c3765fc6))
* **core:** add SolveError::InvalidWorkerPools for allowlist errors ([ab89495](https://github.com/propeller-heads/fynd/commit/ab89495233783fac5f6d1d285c56aa126ae64757))
* **core:** reject empty pool allowlists and rename finalize_quote ([95e3a42](https://github.com/propeller-heads/fynd/commit/95e3a4271ddcf68b631b362b08cc2d32b2fb76bb))
* **core:** expose solve, encode_quotes and finalize stages of the router ([38625b5](https://github.com/propeller-heads/fynd/commit/38625b5b0d6638578998cd3962b4421b1d719738))
* **core:** allow requests to restrict solving to named worker pools ([c80cdbe](https://github.com/propeller-heads/fynd/commit/c80cdbefec307894f032b6ac8e44db2af6db811e))
* **propamm:** name the pAMM components with no fallback pool ([3b48f69](https://github.com/propeller-heads/fynd/commit/3b48f69493ce1402ccee43c114f9688bac9151b2))
* **rpc:** expose the quote pipeline stages and app state for embedders ([0190e01](https://github.com/propeller-heads/fynd/commit/0190e017881559728d20b42bae812a1933be32df))
* **rpc:** add configure_routes hook to register overriding routes ([4b7291b](https://github.com/propeller-heads/fynd/commit/4b7291b998e4ec724d1f35bb2e3bb7634cdc9341))

### Bug Fixes

* validate before capturing and reject empty rankings everywhere ([a1a4049](https://github.com/propeller-heads/fynd/commit/a1a40498ccc6e1e3134e22800177e4ba8564c996))
* **core:** validate the worker pool allowlist once per request ([f050787](https://github.com/propeller-heads/fynd/commit/f05078712431745a9bc6fa40e6e220924510bf32))
* **core:** make RankedQuotes::new fallible instead of panicking ([8e92179](https://github.com/propeller-heads/fynd/commit/8e92179c4119fc95caa1bf839201f432ededcbd5))
* leave a pAMM out of the graph when its fallback cannot be priced ([d35d618](https://github.com/propeller-heads/fynd/commit/d35d618d81ef85bd3f36e0e15249c0860b8c5b64))
* **rpc:** restrict RequestOutcome and is_failure to crate-internal use ([d355097](https://github.com/propeller-heads/fynd/commit/d3550974a3dd6a776c8422824fdfc52e6d9b77c7))
* **rpc:** derive num_orders from capture in log_quote_outcome ([f2fdf67](https://github.com/propeller-heads/fynd/commit/f2fdf67eb91ea615291ec2b0e3047f4e48811912))
* **rpc:** harden RequestOutcome and document route override limits ([6a8277d](https://github.com/propeller-heads/fynd/commit/6a8277d0a948b50d9734d15979741581f8936e15))


## [0.99.18](https://github.com/propeller-heads/fynd/compare/0.99.17...0.99.18) (2026-08-27)

### Bug Fixes

* **feed:** apply the hook filter to uniswap_v4_hooks only ([f9e9199](https://github.com/propeller-heads/fynd/commit/f9e91999b06a234aa705c72fb3551611920033db))
* **feed:** drop Uniswap V4 pools using hook 0x051c99a4 ([37929eb](https://github.com/propeller-heads/fynd/commit/37929eb7b70657fce0e5c33941025356bfc46d27))
* bind exclusive swap signatures to the Tycho router as locker ([2bfc2bc](https://github.com/propeller-heads/fynd/commit/2bfc2bc93ac9e7f1df77056e51b80005e6216cd6))
* **feed:** block the eight sibling hooks of 0x051c99a4 ([6cc37bc](https://github.com/propeller-heads/fynd/commit/6cc37bc5d718aa4f5721044e163769e5df6ae631))


## [0.99.17](https://github.com/propeller-heads/fynd/compare/0.99.16...0.99.17) (2026-08-26)

### Features

* add Robinhood to hosted API support ([54e06b8](https://github.com/propeller-heads/fynd/commit/54e06b858de3cd69eab76032964644a0cf7a00f4))

### Bug Fixes

* draw exclusive swap nonces from a random per-process prefix ([3398945](https://github.com/propeller-heads/fynd/commit/3398945417529c37f5215bbb60ec6550598da6c5))
* address PR #466 review comments ([299a8fb](https://github.com/propeller-heads/fynd/commit/299a8fbd26a7f2d6de07e7200f9b843d31228458))



## [0.99.15](https://github.com/propeller-heads/fynd/compare/0.99.14...0.99.15) (2026-08-25)

### Features

* exclude protocol systems per worker pool ([52807f3](https://github.com/propeller-heads/fynd/commit/52807f3cd18210fe01ae7ebeb5e727a9308c5314))

### Bug Fixes

* drop pAMM quotes under min_amount_out before ranking ([5df7259](https://github.com/propeller-heads/fynd/commit/5df72591f7276b44afbd601df6ba2ec8ef5ecb7d))


## [0.99.14](https://github.com/propeller-heads/fynd/compare/0.99.12...0.99.14) (2026-08-24)

### Features

* **core:** count pAMM quotes dropped for a weak fallback ([aa75d1a](https://github.com/propeller-heads/fynd/commit/aa75d1a4d55ef044eb7c5c9d1659b20aee9fcae0))
* **core:** drop a pAMM quote whose fallback misses the floor ([46c05b0](https://github.com/propeller-heads/fynd/commit/46c05b03b775f7037774fd34de4583bf10a354dc))
* **core:** stamp the fallback amount out on pAMM routes ([4c0a8f3](https://github.com/propeller-heads/fynd/commit/4c0a8f362a75d03665ef51797ec4bcdfe1da8d01))
* **core:** keep the fallback pool index current in each worker ([687c684](https://github.com/propeller-heads/fynd/commit/687c684da7d4d337071014d19dde77f368562ef9))
* **core:** read the PropAMMRouter fee tiers from chain on a timer ([1158486](https://github.com/propeller-heads/fynd/commit/11584867e67b8ea9971571a3ff6de3aa73751754))
* **core:** compute the route output when pAMM legs fall back ([b474bfb](https://github.com/propeller-heads/fynd/commit/b474bfb3c89bae07a58c411204c6c93bbf41134b))
* report exclusive surplus in gas-token units ([049ca4b](https://github.com/propeller-heads/fynd/commit/049ca4b9ba5d7c54c2068b1cf7992e00377a0e60))

### Bug Fixes

* **core:** abort the fee tier fetcher on shutdown ([3cc41ad](https://github.com/propeller-heads/fynd/commit/3cc41ad1502658aa7288f2dd66f4e5ea16e872be))
* **core:** price the pAMM fallback on the state it was solved on ([d378c45](https://github.com/propeller-heads/fynd/commit/d378c458788fb1ed67358d459c052edcdea2e7fd))


## [0.99.13](https://github.com/propeller-heads/fynd/compare/0.99.12...0.99.13) (2026-08-24)

### Features

* **core:** count pAMM quotes dropped for a weak fallback ([aa75d1a](https://github.com/propeller-heads/fynd/commit/aa75d1a4d55ef044eb7c5c9d1659b20aee9fcae0))
* **core:** drop a pAMM quote whose fallback misses the floor ([46c05b0](https://github.com/propeller-heads/fynd/commit/46c05b03b775f7037774fd34de4583bf10a354dc))
* **core:** stamp the fallback amount out on pAMM routes ([4c0a8f3](https://github.com/propeller-heads/fynd/commit/4c0a8f362a75d03665ef51797ec4bcdfe1da8d01))
* **core:** keep the fallback pool index current in each worker ([687c684](https://github.com/propeller-heads/fynd/commit/687c684da7d4d337071014d19dde77f368562ef9))
* **core:** read the PropAMMRouter fee tiers from chain on a timer ([1158486](https://github.com/propeller-heads/fynd/commit/11584867e67b8ea9971571a3ff6de3aa73751754))
* **core:** compute the route output when pAMM legs fall back ([b474bfb](https://github.com/propeller-heads/fynd/commit/b474bfb3c89bae07a58c411204c6c93bbf41134b))
* report exclusive surplus in gas-token units ([049ca4b](https://github.com/propeller-heads/fynd/commit/049ca4b9ba5d7c54c2068b1cf7992e00377a0e60))

### Bug Fixes

* **core:** abort the fee tier fetcher on shutdown ([3cc41ad](https://github.com/propeller-heads/fynd/commit/3cc41ad1502658aa7288f2dd66f4e5ea16e872be))
* **core:** price the pAMM fallback on the state it was solved on ([d378c45](https://github.com/propeller-heads/fynd/commit/d378c458788fb1ed67358d459c052edcdea2e7fd))


## [0.99.12](https://github.com/propeller-heads/fynd/compare/0.99.11...0.99.12) (2026-08-21)

### Features

* add configurable calldata watermark ([7232c0c](https://github.com/propeller-heads/fynd/commit/7232c0ca6ce92d63b4ef66f4a8cf5b1b3499e29b))
* count exclusive candidates dropped for invalid route shape ([dd16584](https://github.com/propeller-heads/fynd/commit/dd16584c41710f338245a4cf483e3c85df2b43be))

### Bug Fixes

* **api:** bound experimental list query limits ([521dd3c](https://github.com/propeller-heads/fynd/commit/521dd3c179af1f82401cd8ce50fc054ed927babb))


## [0.99.11](https://github.com/propeller-heads/fynd/compare/0.99.10...0.99.11) (2026-08-20)

### Features

* Reuse caching developed for WF in ML ([d5e3a91](https://github.com/propeller-heads/fynd/commit/d5e3a91c48b901c3b9d94a23bb5a1fa98fe61dd9))
* Waterfill - Slim Shady version ([5cadfd9](https://github.com/propeller-heads/fynd/commit/5cadfd94b7c51ce0dea095c20a4c156afde791f2))

### Bug Fixes

* rank water-fill candidate paths net of gas ([1b75b68](https://github.com/propeller-heads/fynd/commit/1b75b6891af29785319b8f3e95e4b981a47881f0))


## [0.99.10](https://github.com/propeller-heads/fynd/compare/0.99.9...0.99.10) (2026-08-19)

### Features

* accept exclusive legs that wrap into the output token ([320204e](https://github.com/propeller-heads/fynd/commit/320204ebf664facb0341a3cca3b730908babbc52))




## [0.99.7](https://github.com/propeller-heads/fynd/compare/0.99.6...0.99.7) (2026-08-17)

### Features

* Replace path data types with SmallVec ([4a7424b](https://github.com/propeller-heads/fynd/commit/4a7424b851084f9e6404e8d7df44d4012e1ecbdb))
* Use FxHash at all hashed fields ([75ea368](https://github.com/propeller-heads/fynd/commit/75ea368b4ab6085136f7d9c0109fbf94571be160))
* Add error tracking on benchmark ([3fd39fe](https://github.com/propeller-heads/fynd/commit/3fd39fef0e3187fe118ecdacda154c62c9399bbb))
* Keep public API backwards compatible ([fada11d](https://github.com/propeller-heads/fynd/commit/fada11d6ba92c0016fcab62681c2e579e20e254a))
* **most_liquid:** Never send a route through one pool twice ([d1fc9be](https://github.com/propeller-heads/fynd/commit/d1fc9be73197e2dbe61deb98aeb034f82df53465))
* Allow querying market data with reference IDs ([8bed02b](https://github.com/propeller-heads/fynd/commit/8bed02b8deb5aa7dc0d083591e2f47acbc8b0fda))
* Implement target node filtering on bellman ford ([cfb138f](https://github.com/propeller-heads/fynd/commit/cfb138f17067b4f14655123559d1b1b7375779d7))
* Implement MostLiquid V2, designed with token sequences ([06f4a74](https://github.com/propeller-heads/fynd/commit/06f4a7492f3d8d68a6dec9f0eb78e09f4829888b))
* Implement topology graph search methods ([b23372c](https://github.com/propeller-heads/fynd/commit/b23372c23ae50715dab87c0fa568972528c48fc5))
* Use shared pointer (ARC) for component and token data ([20b67e8](https://github.com/propeller-heads/fynd/commit/20b67e8016553c76c04a01741b99b61abcad54e5))
* Implement TopologyGraph - single edge between tokens ([9463fa1](https://github.com/propeller-heads/fynd/commit/9463fa1f40ca3c31bf4e85d565feba420c02bc17))

### Bug Fixes

* Update callsite after merging main ([d606fb8](https://github.com/propeller-heads/fynd/commit/d606fb8c949fd57e1aea5781deb6bc11a6eff8a4))
* Give the integration tests their own worker pool config ([0648090](https://github.com/propeller-heads/fynd/commit/06480908da73dfbec9a2a1dcc0f74db58d2056d1))

### Performance Improvements

* Extract subset loops optimized ([7b2624a](https://github.com/propeller-heads/fynd/commit/7b2624a88525e35379d678e9d54675e80456c116))


## [0.99.6](https://github.com/propeller-heads/fynd/compare/0.99.5...0.99.6) (2026-08-14)

### Features

* Address PR reviews, add a LIVE mode, and UI improvements. ([78d072e](https://github.com/propeller-heads/fynd/commit/78d072e4078a36e28b1842c485bc64e502aa306d))
* Add an offline benchmark and a profiling tool ([34f367c](https://github.com/propeller-heads/fynd/commit/34f367c0afc31c2d868a1c79cf6bc3d5ec91c2c3))
* A few more iterations on benchmark following usage ([cd2d86e](https://github.com/propeller-heads/fynd/commit/cd2d86e464ee0b2f82e35a0d7ef8933118fd7e37))

### Bug Fixes

* **rpc:** invert liquidity unit conversion in GET /v1/tokens ([03147a2](https://github.com/propeller-heads/fynd/commit/03147a2390f345632e984d962d11bcf9bf11b9f4))




## [0.99.3](https://github.com/propeller-heads/fynd/compare/0.99.2...0.99.3) (2026-08-11)

### Features

* **rpc:** add offset pagination to GET /v1/tokens ([da05b88](https://github.com/propeller-heads/fynd/commit/da05b88a06864b47088c01379e3d3736784202be))
* **rpc:** add experimental GET /v1/tokens graph token endpoint ([efdc327](https://github.com/propeller-heads/fynd/commit/efdc327d569a053a8d38a9ff8f66e5dc1113befd))


## [0.99.2](https://github.com/propeller-heads/fynd/compare/0.99.1...0.99.2) (2026-08-11)

### Features

* **rpc:** drop price unit contract and fix decimal truncation ([d6a81c0](https://github.com/propeller-heads/fynd/commit/d6a81c007a6d2728865ea2054211c8ddf4f02743))
* **prices:** expose decimal string unit contract ([9b570c0](https://github.com/propeller-heads/fynd/commit/9b570c0db472e1dac62b312c6abbfb6b1bf59817))
* **rpc:** define stable price unit contract ([55e7788](https://github.com/propeller-heads/fynd/commit/55e7788eab7ffb5450084eeff7d3cb6c05297c85))

### Bug Fixes

* **ci:** align OpenAPI drift contract ([cbc8177](https://github.com/propeller-heads/fynd/commit/cbc8177a106f6d4a4f7445c86ae03843e32d4045))


## [0.99.1](https://github.com/propeller-heads/fynd/compare/0.98.0...0.99.1) (2026-08-07)

### Features

* **core:** move tycho crates to the 0.354.0 release ([d44556f](https://github.com/propeller-heads/fynd/commit/d44556f527ae58933aa0f9893f917e6c671dd1e2))
* **client:** sign the 11-field ClientFee payload ([166a2dc](https://github.com/propeller-heads/fynd/commit/166a2dc2091739442b9b5d5ce457556cad75d201))
* **core:** encode router fees against tycho draft branch ([c37eb9b](https://github.com/propeller-heads/fynd/commit/c37eb9b490330d80fd25768d592dd05b5ad6a865))


## [0.99.0](https://github.com/propeller-heads/fynd/compare/0.98.0...0.99.0) (2026-08-07)

### Features

* **core:** move tycho crates to the 0.354.0 release ([d44556f](https://github.com/propeller-heads/fynd/commit/d44556f527ae58933aa0f9893f917e6c671dd1e2))
* **client:** sign the 11-field ClientFee payload ([166a2dc](https://github.com/propeller-heads/fynd/commit/166a2dc2091739442b9b5d5ce457556cad75d201))
* **core:** encode router fees against tycho draft branch ([c37eb9b](https://github.com/propeller-heads/fynd/commit/c37eb9b490330d80fd25768d592dd05b5ad6a865))


## [0.98.0](https://github.com/propeller-heads/fynd/compare/0.97.14...0.98.0) (2026-08-05)

### Features

* **monitoring:** add per-pool solve p95 panel to the local dashboard ([e73cd4d](https://github.com/propeller-heads/fynd/commit/e73cd4da76b0a2f88f75146d52cbf0b4488430e3))
* replace ExclusivityPolicy with scope based filtering ([c011dc6](https://github.com/propeller-heads/fynd/commit/c011dc62631ae4f093d185de44f609000df6e5d0))
* rename pool_depths and poolId in /v1/prices and TS client ([8eb73c8](https://github.com/propeller-heads/fynd/commit/8eb73c895b9ff361f128cf6ed43ca10d6bd5ee1d))
* quote exclusive routes when no public route exists ([2930167](https://github.com/propeller-heads/fynd/commit/2930167b896a6b7aa4ee3c288eea4da17bc69a87))
* **metrics:** add per-pool solve duration histogram ([bd59b68](https://github.com/propeller-heads/fynd/commit/bd59b6803058cb37082f0332948730f7e184c563))
* default to PublicOnly scope and rename All to IncludeExclusive ([aad3886](https://github.com/propeller-heads/fynd/commit/aad38864b90ecdf3ac9339f4710f7a301eb2b39e))
* give the user a share of the exclusive route improvement ([7beabb1](https://github.com/propeller-heads/fynd/commit/7beabb1bfb90e19b2ae43e72731dc0311ca6a097))
* require exclusive routes to beat public by 1 bps ([5fde9ba](https://github.com/propeller-heads/fynd/commit/5fde9baea7afc51ae92abee3ffaa4b47939f15f4))

### Bug Fixes

* use imported path ([6c07c5a](https://github.com/propeller-heads/fynd/commit/6c07c5aa3d938794fb8821a1c388387414ff1fe8))
* fix terminal-leg check in has_valid_exclusive_route ([4d10c88](https://github.com/propeller-heads/fynd/commit/4d10c88922db9a2446831252300336b1b8dd4b66))


## [0.97.14](https://github.com/propeller-heads/fynd/compare/0.97.13...0.97.14) (2026-08-03)

### Features

* **feed:** stream exclusive pools via the exclusive: protocol prefix ([d8510fd](https://github.com/propeller-heads/fynd/commit/d8510fdc22bea47132c79e1c84f361bfa37fad65))



## [0.97.12](https://github.com/propeller-heads/fynd/compare/0.97.11...0.97.12) (2026-07-30)

### Features

* **derived:** add coalesce_market_events for lag recovery ([5b655a2](https://github.com/propeller-heads/fynd/commit/5b655a2455b71e8cbeba8813c3063525f86353e8))

### Bug Fixes

* **derived:** correct lag-recovery doc comment and count drain-time lag skips ([2f35f07](https://github.com/propeller-heads/fynd/commit/2f35f07cced995c1f2ef5e90c78d000c213b92af))
* **derived:** recover from broadcast lag incrementally, not via full recompute ([42fcf69](https://github.com/propeller-heads/fynd/commit/42fcf69d111842fb6c7cd414da5efc7d9d2540b9))


## [0.97.11](https://github.com/propeller-heads/fynd/compare/0.97.10...0.97.11) (2026-07-30)

### Features

* **fynd-core:** derive route summaries from the solved route ([3f8c10c](https://github.com/propeller-heads/fynd/commit/3f8c10c6230fa15141838cf988769036b3f85d01))
* **hindsight:** solve fresh at back-of-block alongside route replay ([3ae070d](https://github.com/propeller-heads/fynd/commit/3ae070dca867ecda1eccc03a869ebf1288213ee3))
* **hindsight:** re-execute top routes to measure positive slippage ([a3a4946](https://github.com/propeller-heads/fynd/commit/a3a4946c7f103afac4c33da9b8e574935c618bb8))

### Bug Fixes

* **fynd-core:** guard replayed pool sims against panics ([73e289a](https://github.com/propeller-heads/fynd/commit/73e289ae13e8374bbc673a0493085daed4207a64))




## [0.97.8](https://github.com/propeller-heads/fynd/compare/0.97.7...0.97.8) (2026-07-29)

### Features

* **hindsight:** add offline HTML report subcommand ([7193561](https://github.com/propeller-heads/fynd/commit/7193561ab82947f1344aac90f5f14e531410165e))
* consolidate permission and pool-role types ([7c4e723](https://github.com/propeller-heads/fynd/commit/7c4e7235ba38c875ca4df86a82a2656d23f25782))
* scaffold permissioned surplus pools ([7fb9dbc](https://github.com/propeller-heads/fynd/commit/7fb9dbc6882fe699af64f549b0a8e263cc96ae61))
* **core:** carry the failing SolveError on OrderQuote and aggregate it ([9079077](https://github.com/propeller-heads/fynd/commit/9079077c5d0de72aef631d0ae5fbf6e6221c56da))
* **core:** classify algorithm errors into specific solve errors ([a2788d6](https://github.com/propeller-heads/fynd/commit/a2788d6dcf95120993656949af9385645256bb28))
* **core:** add MaxGasExceeded, MissingData, SimulationFailed solve errors ([6f98e3b](https://github.com/propeller-heads/fynd/commit/6f98e3ba4d9c6231a274e6b814b551709d509c34))
* **encoding:** rename to exclusive-swap and add taker buffer ([3d5db72](https://github.com/propeller-heads/fynd/commit/3d5db72a71c0273a5a4032de4d9f0073e68ee46d))
* **encoding:** sign exclusive-swap quotes into Ekubo user_data ([0477847](https://github.com/propeller-heads/fynd/commit/04778473fc9bbacb451812e2d23619df6c9e6cf9))
* require an exclusivity policy when liquidity_scope is set ([1b61ddf](https://github.com/propeller-heads/fynd/commit/1b61ddf98f812b983f1fbf88e46ecbf277cba765))
* cover exclusive candidate filters in combine_with_surplus ([44ba437](https://github.com/propeller-heads/fynd/commit/44ba437edba97f1a519cd9fc2c42c40db3fcb43a))
* add tests for early return cases ([e3398db](https://github.com/propeller-heads/fynd/commit/e3398db8c1c36fea65b921dc13c826993e73d700))
* implement permissioned surplus routing logic ([c2b31e2](https://github.com/propeller-heads/fynd/commit/c2b31e2c81ef70bddfd502667a510fdc38301207))
* **rpc:** log namespaced failure_reasons on the quote capture line ([fd375ce](https://github.com/propeller-heads/fynd/commit/fd375ce3802e6523a1407811b16656da96e1ab33))

### Bug Fixes

* **water-fill:** route simulation calls through the panic guard ([08b54b9](https://github.com/propeller-heads/fynd/commit/08b54b95fc82ff75301397ee7197efd3e6bf418b))
* **core:** derive max-gas cause from quote gas, not elimination ([4ad42c6](https://github.com/propeller-heads/fynd/commit/4ad42c685d952e73784894d0c64c6ff611ad7ff5))
* repair merge fallout from PR #340 rename ([e7c4640](https://github.com/propeller-heads/fynd/commit/e7c4640f9981303a2e7c47f624f5bd05f5704462))
* add struct fields introduced on main to rebased tests ([ec57451](https://github.com/propeller-heads/fynd/commit/ec57451e86253d07a249da93655e1fbe4bdafcaf))
* gas compensate the committed amount on surplus quotes ([853e8dc](https://github.com/propeller-heads/fynd/commit/853e8dc26f991c41a130d7944d6bb17f419b8d97))
* capture the full surplus on split routes ([bafedcb](https://github.com/propeller-heads/fynd/commit/bafedcb81e0df2474444ab6c7cc77175052b1b61))
* rename tests ([3e522df](https://github.com/propeller-heads/fynd/commit/3e522df82927b0524be9db1d90b2c3fedcd87088))
* ceil commited_leg for protocol to not overcapture ([193aa07](https://github.com/propeller-heads/fynd/commit/193aa07164ed7277d58e56ac12df02afa89eeba6))
* not consider gas in quote ([18ff332](https://github.com/propeller-heads/fynd/commit/18ff332414978f75f47e2c4348d80237ee124dd6))
* gas compensate the committed amount on exclusive quotes ([12ca21b](https://github.com/propeller-heads/fynd/commit/12ca21ba9218a3319e32b95d3bc20cae8d412806))
* use exclusive routes that tie the public gross output ([8d0f5c2](https://github.com/propeller-heads/fynd/commit/8d0f5c2c17e436e34ff395c4eb98c259c374d800))
* update docs link ([702a163](https://github.com/propeller-heads/fynd/commit/702a1635a77fd831d88fe2437ecd1dd8106d1ace))
* set permission field in path_frank_wolfe registry test ([fe47f65](https://github.com/propeller-heads/fynd/commit/fe47f657a2ade5973f78192dbe15e34ce173344c))
* **rpc:** repeat request-level failure reason per order ([c6b28c1](https://github.com/propeller-heads/fynd/commit/c6b28c12472ed6c1cf068709c43dcb93debe7752))


## [0.97.7](https://github.com/propeller-heads/fynd/compare/0.97.6...0.97.7) (2026-07-28)

### Features

* **core:** report AmountTooSmall for dust quotes via input-vs-single-hop-gas ([890b731](https://github.com/propeller-heads/fynd/commit/890b731f42eab803d77ce68992dfaf09d45cbaf2))
* **core:** add AmountTooSmall no-path reason and aggregate it ([ec57a0c](https://github.com/propeller-heads/fynd/commit/ec57a0cf03f47a92f2f4eda849e140a695e4b268))
* **rpc:** log amount_too_small no-route reason code ([087de5d](https://github.com/propeller-heads/fynd/commit/087de5d4956cee418701030424a9006e7bb36126))

### Bug Fixes

* log terminal errors so stdout log pipelines show them ([7bea194](https://github.com/propeller-heads/fynd/commit/7bea19483c7dc3736266d0ef7be96b45759e9c46))
* **core:** make AmountTooSmall docs and Display mode-neutral ([2039b19](https://github.com/propeller-heads/fynd/commit/2039b19d89b6784bbc6d3cbdf1b6292542e41c21))
* **core:** rank no-route reasons by tier when aggregating pools ([765cd93](https://github.com/propeller-heads/fynd/commit/765cd93d4a30ac477751d641b2eedd8553a608fe))

### Performance Improvements

* **core:** gate input-side dust check on uneconomic edges ([b4b72e5](https://github.com/propeller-heads/fynd/commit/b4b72e5c3a4c79b4936dc81df1df2a9504489408))


## [0.97.6](https://github.com/propeller-heads/fynd/compare/0.97.4...0.97.6) (2026-07-27)

### Features

* add aerodrome_v1 to protocol registry ([12c47ad](https://github.com/propeller-heads/fynd/commit/12c47ad482ec8559e03ff40c5680d63b84a06747))


## [0.97.5](https://github.com/propeller-heads/fynd/compare/0.97.4...0.97.5) (2026-07-27)

### Features

* **rpc:** make the hosted Swagger UI opt-in via --hosted-swagger-url ([4e0b883](https://github.com/propeller-heads/fynd/commit/4e0b8839dd49011386ceea62bbf704b516a93d89))
* **rpc:** serve a hosted-gateway Swagger UI at /docs/hosted/ ([473b18f](https://github.com/propeller-heads/fynd/commit/473b18f78cf288fd5c406be42021c702e4b4190b))
* log solve requests that take more than 200ms ([7c54ab7](https://github.com/propeller-heads/fynd/commit/7c54ab719f0ceef765f9a64a7dd1ec68649c4560))

### Bug Fixes

* **rpc:** declare hosted auth as raw Authorization API key ([dd9667d](https://github.com/propeller-heads/fynd/commit/dd9667db21d2d9589bdefbed4babba0a6ba61c54))


## [0.97.4](https://github.com/propeller-heads/fynd/compare/0.97.3...0.97.4) (2026-07-24)

### Bug Fixes

* **rpc:** fail fast when the computation manager task exits ([dd5755a](https://github.com/propeller-heads/fynd/commit/dd5755a9b00af21447a942b86b8999a369cc701f))


## [0.97.3](https://github.com/propeller-heads/fynd/compare/0.97.2...0.97.3) (2026-07-23)

### Features

* log inputs on contained pool simulation panic ([ee04f07](https://github.com/propeller-heads/fynd/commit/ee04f07e441ddf09fab508e6437a509c94b50471))
* add lunarbase to the protocol registry ([bcc69ae](https://github.com/propeller-heads/fynd/commit/bcc69ae4b5737e16c1e0771b0c454a9674205aaf))

### Bug Fixes

* guard pool depth diagnostic probe against panics ([b54c4ed](https://github.com/propeller-heads/fynd/commit/b54c4ed2094a885955e9eadc8801799b7160e420))
* contain pool simulation panics in solver algorithms ([7d3a070](https://github.com/propeller-heads/fynd/commit/7d3a070f7e7b8efa5d48a0ced4001cb32f76ad1b))


## [0.97.2](https://github.com/propeller-heads/fynd/compare/0.97.1...0.97.2) (2026-07-23)

### Features

* export per-protocol pool count and sync status metrics ([fcb5970](https://github.com/propeller-heads/fynd/commit/fcb597018edb4d9368b2a45e896d6984eba961f1))

## [0.97.1](https://github.com/propeller-heads/fynd/compare/0.97.0...0.97.1) (2026-07-22)


### ⚠ BREAKING CHANGES

* removes the --reconnect-delay-secs CLI flag and the public
reconnect_delay builder methods / RECONNECT_DELAY constant (all dead code).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>

### Bug Fixes

* correct hosted-api onboarding docs and improve serve-path observability ([595385c](https://github.com/propeller-heads/fynd/commit/595385c8c62d3f1588b68b22709bb5ff0af891f3))


### Reverts

* keep the reconnect_delay knob (avoid breaking change) ([c509a04](https://github.com/propeller-heads/fynd/commit/c509a040b79b36a32a4424c95aefab933dab4173))


### Code Refactoring

* address PR review comments ([aa2916c](https://github.com/propeller-heads/fynd/commit/aa2916c45100f1412a5851015f406b96824f8c7a))

## [0.97.0](https://github.com/propeller-heads/fynd/compare/0.96.0...0.97.0) (2026-07-21)


### Features

* **core:** aggregate and attach no-route reason to OrderQuote ([5260938](https://github.com/propeller-heads/fynd/commit/5260938704c024e08d0e41df52fccacf86898cab))
* **core:** carry NoPathReason on SolveError::NoRouteFound ([59eef1a](https://github.com/propeller-heads/fynd/commit/59eef1a393e40a93ed451df3b01b07b77b889868))
* **rpc:** add request capture log emission ([13d47d1](https://github.com/propeller-heads/fynd/commit/13d47d10b87c9e974801f8e7b277f9b2dd73ab36))
* **rpc:** add request replay-capture helpers ([53cbe7e](https://github.com/propeller-heads/fynd/commit/53cbe7e37d2275189b804b179342d484c485a795))
* **rpc:** log every accepted quote request for replay ([1d60c00](https://github.com/propeller-heads/fynd/commit/1d60c00703984bc08ecd579b73c9c85c5e7ce1b9))
* **rpc:** log no_route_reasons on the quote capture line ([394ba83](https://github.com/propeller-heads/fynd/commit/394ba831295d23c3494a24bb24eba11efc7b4097))
* **rpc:** log only failed quotes, rename event to quote_failure ([de6b0c9](https://github.com/propeller-heads/fynd/commit/de6b0c91a724ed1e786da9683f1389a1045deb32))

## [0.96.0](https://github.com/propeller-heads/fynd/compare/0.95.0...0.96.0) (2026-07-21)


### Features

* **hindsight:** record which strategy decoded each trade ([bed0cdc](https://github.com/propeller-heads/fynd/commit/bed0cdc8f160f6fe1fca85f8921a24764c184486))

## [0.95.0](https://github.com/propeller-heads/fynd/compare/0.94.0...0.95.0) (2026-07-21)


### Features

* add hosted-API auth and per-chain routing to clients ([d4405cb](https://github.com/propeller-heads/fynd/commit/d4405cbe64edbbef86a238bfcafafa9b6efe431c))


### Bug Fixes

* **client:** send API key as raw Authorization header, not Bearer ([4d63aaa](https://github.com/propeller-heads/fynd/commit/4d63aaafc4cbc9e790707f9cabf2a29d192af5e5))

## [0.94.0](https://github.com/propeller-heads/fynd/compare/0.93.0...0.94.0) (2026-07-20)


### Features

* **api:** report null router_address and 501 encoding on quote-only chains ([bda79ca](https://github.com/propeller-heads/fynd/commit/bda79ca6fa6d0e1a8db600b615d8082fe78485fa))
* **chain:** resolve custom chains and native token via the registry ([96ee352](https://github.com/propeller-heads/fynd/commit/96ee352599d8fb4e6cfe142fa0a0965b99948664))
* **cli:** add --chains-config to install the custom-chain registry ([a04a9cd](https://github.com/propeller-heads/fynd/commit/a04a9cd6751143cd78c58992410fcc878f8365bc))
* **encoding:** add disabled encoder state for router-less chains ([cd4c006](https://github.com/propeller-heads/fynd/commit/cd4c006a47d04e39d97ff793bc5d1d379c4509c5))
* make InstanceInfo.router_address optional for quote-only chains ([095063b](https://github.com/propeller-heads/fynd/commit/095063b9462ae8ea434b8754242e09467943541e))
* **solver:** make router address optional and gate the fee fetcher ([a849b54](https://github.com/propeller-heads/fynd/commit/a849b5445fb6b2c4d80abd42700fe557a6256043))
* upgrade tycho to 0.340.0 ([a08e77d](https://github.com/propeller-heads/fynd/commit/a08e77d266a09b3252540b2266edef06061849b2))


### Bug Fixes

* address custom-chain PR review comments ([38e486e](https://github.com/propeller-heads/fynd/commit/38e486ec3d39968506a4ee6fc0f610b379dc828e))
* **api:** map only encoding-unavailable to 501, keep encoding failures at 422 ([ecfa019](https://github.com/propeller-heads/fynd/commit/ecfa01978267b815f9ebc0b96109b09d355cd7a3))
* **chain:** preserve fail-fast for placeholder native token; drop dead lazy_static ([c3cc396](https://github.com/propeller-heads/fynd/commit/c3cc3965285822472ce40ec1a391559ca4340c1b))

## [0.93.0](https://github.com/propeller-heads/fynd/compare/0.92.0...0.93.0) (2026-07-20)


### Features

* add version to /v1/info instance info ([e9c0cd9](https://github.com/propeller-heads/fynd/commit/e9c0cd9c344bd85272e0a7969d86800051b2d935))
* emit fynd_build_info metric with binary version ([6843465](https://github.com/propeller-heads/fynd/commit/6843465ce53da6765683508e653e7f7622f2e9e5))

## [0.92.0](https://github.com/propeller-heads/fynd/compare/0.91.0...0.92.0) (2026-07-17)


### Features

* add chain label and histogram buckets to metrics exporter ([0bcbf29](https://github.com/propeller-heads/fynd/commit/0bcbf29b7ab1ffd5a044650857813443654d6272))
* add component and token count getters to MarketState ([3825175](https://github.com/propeller-heads/fynd/commit/3825175a231ef5f3ee62b3ef6d847637539738f7))
* add HTTP metrics middleware with per-client labels ([3f30279](https://github.com/propeller-heads/fynd/commit/3f30279dd645e7af7c7560e04522092c15403d28))
* record derived computation duration, failure, freshness metrics ([60080fc](https://github.com/propeller-heads/fynd/commit/60080fc99c51741e37320914dd0fe69d7041ef7f))
* record encoding duration and failure metrics ([b601716](https://github.com/propeller-heads/fynd/commit/b601716f15fda4280b52a2326ef498acb1f2d5bf))
* record feed freshness, update duration, and size metrics ([1c42bc3](https://github.com/propeller-heads/fynd/commit/1c42bc399fd90d69ce1d222e5ba21c2004c1f3ae))


### Bug Fixes

* bound per-client metric label values ([a9f5063](https://github.com/propeller-heads/fynd/commit/a9f50633f5b2c5a068f5b51c3dd4b8b25b99dcc0))

## [0.91.0](https://github.com/propeller-heads/fynd/compare/0.90.3...0.91.0) (2026-07-17)


### Features

* record per-pool queue wait and depth metrics at task pickup ([bef8139](https://github.com/propeller-heads/fynd/commit/bef8139b527e6b0db3094ac95032775bc9902ebf))

## [0.90.3](https://github.com/propeller-heads/fynd/compare/0.90.2...0.90.3) (2026-07-17)


### Bug Fixes

* restore native CurveState using upstream curve_filter ([79c616b](https://github.com/propeller-heads/fynd/commit/79c616b03afed2261ccee74f9f28205da1b969d2)), closes [#318](https://github.com/propeller-heads/fynd/issues/318)

## [0.90.2](https://github.com/propeller-heads/fynd/compare/0.90.1...0.90.2) (2026-07-16)


### Bug Fixes

* correct vm:curve revert rationale comment ([fd367f4](https://github.com/propeller-heads/fynd/commit/fd367f4c3f59fcb1b12d717d83b93c5bd80e7f9a))
* revert vm:curve to EVMPoolState VM simulation ([38f54bf](https://github.com/propeller-heads/fynd/commit/38f54bf09ee3d264c2ec858ff5100707c50fb66f))

## [0.90.1](https://github.com/propeller-heads/fynd/compare/0.90.0...0.90.1) (2026-07-16)

## [0.90.0](https://github.com/propeller-heads/fynd/compare/0.89.2...0.90.0) (2026-07-14)


### Features

* **hindsight:** detect sandwich attacks around decoded trades ([b8d4974](https://github.com/propeller-heads/fynd/commit/b8d4974e8e1a9353e26d5a008106b5c220978032))
* **hindsight:** require sandwich direction in the attacker's token flow ([0de2065](https://github.com/propeller-heads/fynd/commit/0de20651fa212fa52163fa5a2522e9801c695554))


### Bug Fixes

* **hindsight:** drop token and plumbing logs from sandwich pool overlap ([0faa6f8](https://github.com/propeller-heads/fynd/commit/0faa6f8d01731daaba79b7af04154ee083919a6c))
* **hindsight:** keep coverage verdicts on sandwiched unsolved states ([3eb36e2](https://github.com/propeller-heads/fynd/commit/3eb36e2700927cf34f3a7e7554ddf29b4ce7f938))

## [0.89.2](https://github.com/propeller-heads/fynd/compare/0.89.1...0.89.2) (2026-07-14)


### Bug Fixes

* **hindsight:** add outcome label to savings_bps histogram ([e8b149f](https://github.com/propeller-heads/fynd/commit/e8b149f3d3d4af57ef9eb9ed3940aece9be9e8ef))
* **hindsight:** replace U256→f64 string round-trips with u256_to_f64 ([d27de8c](https://github.com/propeller-heads/fynd/commit/d27de8c7498e118877215f4f37249b6434954889))

## [0.89.1](https://github.com/propeller-heads/fynd/compare/0.89.0...0.89.1) (2026-07-12)


### Bug Fixes

* **core:** stop worker busy-spin when derived-data channel closes ([bba8809](https://github.com/propeller-heads/fynd/commit/bba8809d939782d529ccca6777da0bd5e55f997d))

## [0.89.0](https://github.com/propeller-heads/fynd/compare/0.88.1...0.89.0) (2026-07-10)


### Features

* **feed:** report fynd_version to Tycho via client metadata ([0f088a3](https://github.com/propeller-heads/fynd/commit/0f088a393d15950a5aa1f845900a885172acbe2d))

## [0.88.1](https://github.com/propeller-heads/fynd/compare/0.88.0...0.88.1) (2026-07-10)


### Bug Fixes

* **core:** drop NaN-scored gas-price paths instead of panicking ([df1d7e0](https://github.com/propeller-heads/fynd/commit/df1d7e09645bf8214785b6bf705237db9f835699))

## [0.88.0](https://github.com/propeller-heads/fynd/compare/0.87.1...0.88.0) (2026-07-10)


### Features

* **hindsight:** settled, fynd, and quoted USD on the trade log line ([c762612](https://github.com/propeller-heads/fynd/commit/c762612bfe28ea8b6be31e71814b918c325f6575))
* **pfw:** report computed price impact on RouteResult ([b0f8b69](https://github.com/propeller-heads/fynd/commit/b0f8b6913adcf7e3099b54d785cd649a6f679d84))
* **quote:** add optional price_impact to RouteResult ([c9a3ea8](https://github.com/propeller-heads/fynd/commit/c9a3ea82a4d6f3e0a20bb56bd0a765b5d6b65af3))
* **quote:** populate price_impact_bps (algorithm value or spot-price fallback) ([1a47edb](https://github.com/propeller-heads/fynd/commit/1a47edba0ca48bdd7243297fcbf2c72bfeedab1b))
* **quote:** pure spot-product price-impact math + tests ([bea82a7](https://github.com/propeller-heads/fynd/commit/bea82a795da066e2b5779bd5a4c7b1367b707a13))
* **quote:** route-level spot-price fallback ([393558d](https://github.com/propeller-heads/fynd/commit/393558dc718a2ae04c9f0e7026807f5b44617f12))

## [0.87.1](https://github.com/propeller-heads/fynd/compare/0.87.0...0.87.1) (2026-07-09)


### Bug Fixes

* **hindsight:** no ANSI codes off-terminal, unquoted verdict field ([00745a4](https://github.com/propeller-heads/fynd/commit/00745a4ac4157a7693dedb54067ec479c259f8be))

## [0.87.0](https://github.com/propeller-heads/fynd/compare/0.86.0...0.87.0) (2026-07-09)


### Features

* **hindsight:** bound metric labels to the registry vocabulary ([5ec939e](https://github.com/propeller-heads/fynd/commit/5ec939e0979fed71ed3e434fdd97e4a5a01d2702))


### Bug Fixes

* **hindsight:** default dashboard filter variables to All ([7d0f0ac](https://github.com/propeller-heads/fynd/commit/7d0f0acd731a2f4b3c33229f46f35597516261ed))
* **hindsight:** pass the registry at the record_range call site ([c92265c](https://github.com/propeller-heads/fynd/commit/c92265cebe2403c9dc82bbd585264af2a9f9ceba))

## [0.86.0](https://github.com/propeller-heads/fynd/compare/0.85.0...0.86.0) (2026-07-09)


### Features

* **hindsight:** rebuild the solver when the monitor falls behind head ([4e19851](https://github.com/propeller-heads/fynd/commit/4e1985167baba938888bf66788d0da905925023b))
* **hindsight:** volume by outcome, per-trade log, solver label fix ([dc4efe3](https://github.com/propeller-heads/fynd/commit/dc4efe331585eb5dbc13b7cb85195f394b4c8849))


### Bug Fixes

* **hindsight:** attribution never picks the wrapped-native contract ([e9cb274](https://github.com/propeller-heads/fynd/commit/e9cb2744c27ef0a8481a89e7ea0e440bb34c178f))

## [0.85.0](https://github.com/propeller-heads/fynd/compare/0.84.0...0.85.0) (2026-07-09)


### Features

* use native CurveState decoder and add vm:bopamm/vm:fermiswap ([fba3c22](https://github.com/propeller-heads/fynd/commit/fba3c2234164722acf1d1164de2c249de513cefd))

## [0.84.0](https://github.com/propeller-heads/fynd/compare/0.83.0...0.84.0) (2026-07-09)


### Features

* **hindsight:** total client-savings stat panels on dashboard ([6cb5b50](https://github.com/propeller-heads/fynd/commit/6cb5b507267139f2f7e725440d39fd3bdc6cdb6d))


### Bug Fixes

* **hindsight:** collapse raw-address metric labels to unknown ([18a8f62](https://github.com/propeller-heads/fynd/commit/18a8f62f502a8df866c5fdbbe45d799cf6659d99))
* **hindsight:** export histograms with explicit buckets ([9c24a61](https://github.com/propeller-heads/fynd/commit/9c24a61e5295a64073d10e1b26412973c1b275ac))

## [0.83.0](https://github.com/propeller-heads/fynd/compare/0.82.3...0.83.0) (2026-07-08)


### Features

* add hindsight tool to decode on-chain aggregator swaps ([177a571](https://github.com/propeller-heads/fynd/commit/177a5710c9ed7c225c7bac6c181df34f13bd0006))
* **derived:** make ComputationManager a registry of computations ([b88d75f](https://github.com/propeller-heads/fynd/commit/b88d75f616be6354849af6e0459cf0a0df996ef4))
* **docker:** ship the hindsight binary in the fynd image ([f367cfb](https://github.com/propeller-heads/fynd/commit/f367cfba2e43e1c270b7aa0af602142b167eec26))
* **fynd-core:** gate step-controller build behind experimental feature ([a532358](https://github.com/propeller-heads/fynd/commit/a5323585f8e0ab6271d5cc8fd19308c0bf15e2fd))
* **fynd-core:** survive absent subscribers + block-step-controller wiring ([33a6634](https://github.com/propeller-heads/fynd/commit/33a663484e651f7498478e324d7ce8551efe1d5b))
* **hindsight:** add re-solve engine and resolve subcommand ([df81fc0](https://github.com/propeller-heads/fynd/commit/df81fc0b04cd615c402d17dd345b2fae149f79b7))
* **hindsight:** attribute MetaMask venue from router calldata ([19b5487](https://github.com/propeller-heads/fynd/commit/19b5487856fe22cd0d355028a428d4737e62b72d))
* **hindsight:** attribute the solver in one place with provenance ([172cfa3](https://github.com/propeller-heads/fynd/commit/172cfa3654bb550eb7ed8346521f4dd43cf02997))
* **hindsight:** back out output-side client fees in decode ([22e1690](https://github.com/propeller-heads/fynd/commit/22e1690ca9f003ec91a5db385c4f72e646c4fe7f))
* **hindsight:** capture Relay solver rebalancing fills ([155fb62](https://github.com/propeller-heads/fynd/commit/155fb62d14d6ee7a44e3b9c7d190145ca177de02))
* **hindsight:** charge the settled route's gas in net comparisons ([e04b87d](https://github.com/propeller-heads/fynd/commit/e04b87d5ac539a12f06e4180914c510b50d8aa3c))
* **hindsight:** decode swaps carrying provable residue legs ([19fed58](https://github.com/propeller-heads/fynd/commit/19fed584de3d87e307084a7937815a22ae71f1f1))
* **hindsight:** default monitor to serve's protocols and pools ([f92912d](https://github.com/propeller-heads/fynd/commit/f92912da590970deefda57cf8ceba0b79afe634b))
* **hindsight:** expand venue registry and label known entry points ([c57e263](https://github.com/propeller-heads/fynd/commit/c57e2636ad47975b7d2e879949f2280e52bb5c13))
* **hindsight:** gross-vs-gross is the headline comparison ([31463f7](https://github.com/propeller-heads/fynd/commit/31463f79414c4e52261afd94b2a531c27b7f23ee))
* **hindsight:** live two-state monitor with improvements JSONL ([a8647be](https://github.com/propeller-heads/fynd/commit/a8647be3e482624d2b0e9dc71a9af4df5ac00de4))
* **hindsight:** log all comparisons, not just wins ([5b34fd5](https://github.com/propeller-heads/fynd/commit/5b34fd5df10352a99e4b57fba2857188f70bc49a))
* **hindsight:** metrics, USD valuation, /metrics exporter ([8e1f804](https://github.com/propeller-heads/fynd/commit/8e1f804d9f132cd515862f30d9c9f1feb90c6fbc))
* **hindsight:** record back-of-block + USD in improvements JSONL ([3579e7b](https://github.com/propeller-heads/fynd/commit/3579e7b38f4205347595a8afd2598b0753029395))
* **hindsight:** record the solver's own quote from calldata ([4e4bc76](https://github.com/propeller-heads/fynd/commit/4e4bc76b153ab8bea04c58067e6a69794c36ff28))
* **hindsight:** record top and back block states with a state label ([4991328](https://github.com/propeller-heads/fynd/commit/49913281e8d8dda1baf9d616ecb24c0a4a975f5f))
* **hindsight:** reject NFT purchases decoded as swaps ([2889890](https://github.com/propeller-heads/fynd/commit/2889890e0e3b8bd28c978dea0e12fb6f95efe94c))
* **hindsight:** remove MetaMask fees from decoded swap amounts ([01bedbf](https://github.com/propeller-heads/fynd/commit/01bedbf605ee1b2a748d63e83a197514ad96f3be))
* **hindsight:** rotate comparisons JSONL daily for S3 sync ([7f78f02](https://github.com/propeller-heads/fynd/commit/7f78f028b2f01dd9a574568e5e29fdcae41b5b75))
* **hindsight:** surface ambiguous net-flow declines ([4e949cd](https://github.com/propeller-heads/fynd/commit/4e949cd558cf2a820d8adfad33418c76d14579b1))
* **hindsight:** warn when re-solving blocks far behind chain head ([7bd4869](https://github.com/propeller-heads/fynd/commit/7bd48691c7296e8e49a5f1ad0e6dc488d301b827))
* Include Allium verification ([3663a52](https://github.com/propeller-heads/fynd/commit/3663a529170e406f7fa2e7f32ddf5062ff0402d3))
* make path_frank_wolfe gas-aware with sequential shared-pool state ([555f7d8](https://github.com/propeller-heads/fynd/commit/555f7d8e2127239e83d6f9a72c32a9aeffff3565))
* **tools-common:** add shared crate for audit and Hindsight ([3d13ce5](https://github.com/propeller-heads/fynd/commit/3d13ce58c72c58863b3475ad5cb6e76ccb6a2560))


### Bug Fixes

* **docker:** include tools/common crate in dependency-cache layer ([862ee93](https://github.com/propeller-heads/fynd/commit/862ee93831ff303a52516c3cf5955c3ae4f915e1))
* **erc20-overrides:** keep MAX_PROBE_SLOT public ([1abf219](https://github.com/propeller-heads/fynd/commit/1abf2197bc2d1c5fe19af644c972cd7e2ec9ec6f))
* **fynd-core:** fail the feed when the market-event send fails ([c16b9bc](https://github.com/propeller-heads/fynd/commit/c16b9bcbc01bcf587867ea4dbb3da247bad11c9a))
* **hindsight:** adapt monitor to decoder API changes ([92c7faa](https://github.com/propeller-heads/fynd/commit/92c7faa1fe6fe4432c9eed3ccdaa5520e24b0c62))
* **hindsight:** decline batch settlements instead of guessing a swap ([28913a0](https://github.com/propeller-heads/fynd/commit/28913a0e6c28c7425fa324ff78cd7cc8edabcd45))
* **hindsight:** decline intent fills with no clean EOA maker ([d73ae22](https://github.com/propeller-heads/fynd/commit/d73ae224d415d34214d69027d48637bf3f3e0782))
* **hindsight:** decline unconverted Relay payouts in rebalance decode ([b5a21ad](https://github.com/propeller-heads/fynd/commit/b5a21adca09e39d599716081c5e914e769210f4c))
* **hindsight:** decline wrap-pair trades far off parity ([65cb7d6](https://github.com/propeller-heads/fynd/commit/65cb7d6d5f8ed8a19bc5d03c54608d62f8c6b5b3))
* **hindsight:** decode CoW batch settlements via the order maker ([239acd6](https://github.com/propeller-heads/fynd/commit/239acd66114d3d509a0051297f629475a77573c7))
* **hindsight:** don't treat the fee collector's own receipts as a fee ([41fbdbc](https://github.com/propeller-heads/fynd/commit/41fbdbc47c38f6f0bdb0e57e14c999a33e9c9ccd))
* **hindsight:** drop unbounded token-pair label from trades metric ([84fda97](https://github.com/propeller-heads/fynd/commit/84fda975744a9a27e3bfaf09e45c4291e8f10503))
* **hindsight:** keep decoding a range when one block fails ([36e4642](https://github.com/propeller-heads/fynd/commit/36e46426d5968865694088e417aa4b8362dcb2e2))
* **hindsight:** keep resolving a range when one block fails to decode ([9ea4a8c](https://github.com/propeller-heads/fynd/commit/9ea4a8ccb0ea7d196f7446c4eabd4d992dd6963f))
* **hindsight:** net client fee out of amount_in so it isn't counted as uplift ([9d658bf](https://github.com/propeller-heads/fynd/commit/9d658bf1c4e9acaecac4315463b72f46b9db5fe0))
* **hindsight:** pass BigUint amounts to bps after shared API change ([83bef20](https://github.com/propeller-heads/fynd/commit/83bef20d0791d181d116f6cc38e9ba257564fa23))
* **hindsight:** rebuild the solver when the tycho feed dies ([a56c1a9](https://github.com/propeller-heads/fynd/commit/a56c1a96aaa133035ce02bef003eb98a93a087cf))
* **hindsight:** replace chunks_exact with as_chunks for CI clippy ([691c374](https://github.com/propeller-heads/fynd/commit/691c37460e85b9bc8c91b17826cd95f0079ce5b1))
* **hindsight:** residue legs must be one-directional in netting ([7fc9def](https://github.com/propeller-heads/fynd/commit/7fc9defe7c056cd56efd2fc23c3a8c200025164d))
* **hindsight:** retry decode when the RPC lags the tycho stream ([86b606c](https://github.com/propeller-heads/fynd/commit/86b606c43495956c64129bd233d9d2fbe36f7b37))
* **hindsight:** skip cross-chain bridge orders ([590254e](https://github.com/propeller-heads/fynd/commit/590254e0d4ab8698f053522d2f5b2ce69b01f293))
* **hindsight:** stop attributing swaps to Permit2 ([6805bc7](https://github.com/propeller-heads/fynd/commit/6805bc73c3d795b3bcbe220682b130ba1b1a21ff))
* **hindsight:** tolerate null fields in Allium rows ([bc5b559](https://github.com/propeller-heads/fynd/commit/bc5b559b63590c9a2216df5466eaf4a005eb8a20))
* **hindsight:** treat near-total fill failures as coverage misses ([1419194](https://github.com/propeller-heads/fynd/commit/1419194e64f7d466a68bb779367a3faa353bb851))
* **hindsight:** warn when a quote is missing net-of-gas output ([1a6bb16](https://github.com/propeller-heads/fynd/commit/1a6bb160bdc65725dd7c0a319a8bad016a1d30cf))
* **hindsight:** warn when the back-of-block state isn't the target block ([c0cc34b](https://github.com/propeller-heads/fynd/commit/c0cc34b69e55812ddbd87571bde63a8513030cca))
* **tools-common,benchmark,erc20-overrides:** address PR 262 review comments ([4ebd789](https://github.com/propeller-heads/fynd/commit/4ebd78977d8d5416d677eb973a49c66aa143bbc0))


### Performance Improvements

* **hindsight:** build address registries once with LazyLock ([6f00726](https://github.com/propeller-heads/fynd/commit/6f007261c4137ab14ed10887e8857209c71087ce))

## [0.82.3](https://github.com/propeller-heads/fynd/compare/0.82.2...0.82.3) (2026-06-30)


### Bug Fixes

* **algo:** Call Route.validate() in the algorithms ([1614731](https://github.com/propeller-heads/fynd/commit/1614731587fc8a4d55b63b7bfad7ddaef01cafeb))
* **worker:** Call Route.validate() in the SolverWorker ([1e3e3ac](https://github.com/propeller-heads/fynd/commit/1e3e3ac4f0a00734a16fb195d4237cbc30b3349a))

## [0.82.2](https://github.com/propeller-heads/fynd/compare/0.82.1...0.82.2) (2026-06-30)


### Bug Fixes

* pin nightly version to work around rustc ICE ([faa03a7](https://github.com/propeller-heads/fynd/commit/faa03a7dd6306dcce1cc1a67d858f72a42981589))

## [0.82.1](https://github.com/propeller-heads/fynd/compare/0.82.0...0.82.1) (2026-06-29)


### Bug Fixes

* **Route:** Enforce no empty swaps on Router::new() ([33bf08b](https://github.com/propeller-heads/fynd/commit/33bf08b84511f606112314ca2857350428a53d77))
* **Route:** Fix test fixture and corresponding usages ([b1a0cf8](https://github.com/propeller-heads/fynd/commit/b1a0cf8f28b09e82ff59249e311e50fd500475e1))

## [0.82.0](https://github.com/propeller-heads/fynd/compare/0.81.1...0.82.0) (2026-06-25)


### Features

* **benchmark:** add audit subcommand for on-chain quote validation ([3aa5f1e](https://github.com/propeller-heads/fynd/commit/3aa5f1e781635fa4822abadeddd7b566e256fbfb))
* **benchmark:** add scale --min-tvl and native_onchain resolution ([7e10f66](https://github.com/propeller-heads/fynd/commit/7e10f66c696bb1a13f5a96e70c51869d6278a1fd))
* **benchmark:** capture pool-level route in audit output ([c21bcb9](https://github.com/propeller-heads/fynd/commit/c21bcb9ab8c66e961ccbc2d10fe53fef8cf77def))


### Bug Fixes

* **deps:** update quinn-proto to 0.11.15 for RUSTSEC-2026-0185 ([53b7bfc](https://github.com/propeller-heads/fynd/commit/53b7bfc4cbc254830497a019a6414dd2505d0229))
* include native/wrapped tokens in derive-connector-tokens ([59f7daa](https://github.com/propeller-heads/fynd/commit/59f7daaa6757ff4b4afb04ed6b3e75617b89badc))

## [0.81.1](https://github.com/propeller-heads/fynd/compare/0.81.0...0.81.1) (2026-06-19)


### Bug Fixes

* **release:** exclude fynd-core integration tests from published crate ([e6db8c5](https://github.com/propeller-heads/fynd/commit/e6db8c5c869c5adb4e51868958b653793dcae7bb))

## [0.81.0](https://github.com/propeller-heads/fynd/compare/0.80.0...0.81.0) (2026-06-17)


### Features

* Fall back to default router fee instead of gating on fee fetch ([9ca6335](https://github.com/propeller-heads/fynd/commit/9ca633520a7e52290bb2576c9fa93c85fc81599d))
* Gate health on router fee readiness and staleness ([7c36268](https://github.com/propeller-heads/fynd/commit/7c362689d5548373884110f2e9de42104ae5fe4b))
* Read router fees from on-chain FeeCalculator ([6e06eda](https://github.com/propeller-heads/fynd/commit/6e06edad04afc796dd781564c5cac1e63a3abbc6))

## [0.80.0](https://github.com/propeller-heads/fynd/compare/0.79.0...0.80.0) (2026-06-15)


### Features

* add e2e test helpers ([16d0259](https://github.com/propeller-heads/fynd/commit/16d0259243847194feac0d70248ebf02315365f0))
* add e2e tests ([eafc370](https://github.com/propeller-heads/fynd/commit/eafc370b5d89ae37c11c687d3d96c91a6e1582e1))
* add route to ScenarioResult to enable further validation ([9a15f65](https://github.com/propeller-heads/fynd/commit/9a15f653c063eebf0691f05d7e05fb5194d16b10))
* allow dead code for the unused field ([0afb73a](https://github.com/propeller-heads/fynd/commit/0afb73a49d0b2cd56cf2d0bdcafd8ce5f9c2e8d9))
* update per hop amounts ([4279a4f](https://github.com/propeller-heads/fynd/commit/4279a4f96439620e300a89b98cdb8b476d2bbe3d))
* update pfw tests to have the same set of asserts for all scenarios ([de1d7a9](https://github.com/propeller-heads/fynd/commit/de1d7a938fc549cf0046a8dce895891e94c253c6))


### Bug Fixes

* avoid resimulating hops to get correct per hop amount out ([b74ee3b](https://github.com/propeller-heads/fynd/commit/b74ee3be386323acac76359799f4e4edce62e87a))
* remove debug print ([f9076a7](https://github.com/propeller-heads/fynd/commit/f9076a7bc4e52d3591bd459293771aefbc81214f))
* remove encoding tests ([7a2f2a8](https://github.com/propeller-heads/fynd/commit/7a2f2a8130bdf9664fc450f8f613cee1327c1c90))
* remove unused field and update comment ([92494c6](https://github.com/propeller-heads/fynd/commit/92494c6619a25d7bc1e6e31a5f4c4e254210637a))
* use existing test harness ([9000f1f](https://github.com/propeller-heads/fynd/commit/9000f1f568f1f61a61b73b52c18bce306cfcd537))

## [0.79.0](https://github.com/propeller-heads/fynd/compare/0.78.1...0.79.0) (2026-06-15)


### Features

* add BLESS_GOLDEN workflow for golden output regeneration ([4175f45](https://github.com/propeller-heads/fynd/commit/4175f45e194ee58a8dd5767b7bca0303993e5157))
* add integration test harness with Update message replay ([3ab44ad](https://github.com/propeller-heads/fynd/commit/3ab44ad7d461ec8c074dbca682bda344118c5736))
* add record-market tool skeleton ([b7dbeb2](https://github.com/propeller-heads/fynd/commit/b7dbeb249a250876331bd605460a26b7e25432be))
* add RecordedUpdate and MarketRecording serialization types ([1beda3d](https://github.com/propeller-heads/fynd/commit/1beda3d95fb9cb718aab759a01974230024d556c))
* implement record-market tool and load_test_scenarios ([4e1784b](https://github.com/propeller-heads/fynd/commit/4e1784b585c84ada44eb9124b2f34aa5dd7eb4b3))
* implement recording I/O with serde_json + zstd ([c176b02](https://github.com/propeller-heads/fynd/commit/c176b02aa8f3fb2ed69a33ed009c64886f8b7104))
* make record-market and replay pipeline chain-agnostic ([78761e0](https://github.com/propeller-heads/fynd/commit/78761e055c213fff0cbb0ad7a6d1ea6b96a169ec))


### Bug Fixes

* add doc comments and workspace-manage tempfile dev-dependency ([6869368](https://github.com/propeller-heads/fynd/commit/686936839c195d4671c543b2defaa1942be90a13))
* add test-fixtures and record-market stubs to Dockerfile ([ccd2c40](https://github.com/propeller-heads/fynd/commit/ccd2c40c67921ccebfd6d0bca8720708d8c0cbda))
* address code quality review findings in test harness ([9c7a35b](https://github.com/propeller-heads/fynd/commit/9c7a35ba3fe730328c8ad00d8b98239e74ddb1a5))
* address PR review — top-level imports, warn on fallbacks, fix readme ([3590c1c](https://github.com/propeller-heads/fynd/commit/3590c1cf94b5e6235d7c48e21bc26bb4ec46d243))
* address review findings — remove duplicates, harden CI and test infra ([68fbdb6](https://github.com/propeller-heads/fynd/commit/68fbdb646a830b1efa937134d3670b5bf2fb5a01))
* handle EIP1559 gas price in recorder ([31a1be5](https://github.com/propeller-heads/fynd/commit/31a1be5e4b3858181fe1f1088e9997781a3d49ca))
* make graph construction and SPFA relaxation deterministic ([bdba9b1](https://github.com/propeller-heads/fynd/commit/bdba9b14799321c516fe6dc14e6427c2411e4f85))
* recorder improvements from live Tycho testing ([10059a7](https://github.com/propeller-heads/fynd/commit/10059a7a123cdbaf1d6f24b3dbc4b08c4d3d2711))
* regenerate fixtures from live Tycho, widen quality threshold ([bbfe41c](https://github.com/propeller-heads/fynd/commit/bbfe41c8eb21a8765378edc938f6aaea10b1b6e0))
* remove stale fixtures/integration/ and rename golden to expected ([5c23d27](https://github.com/propeller-heads/fynd/commit/5c23d270028251769c95af9668e33b08cea9f32f))
* require tycho-simulation >=0.275.0 for Update serde support ([46c0019](https://github.com/propeller-heads/fynd/commit/46c001977187c076a7e6642b830ced9917d1e725))
* resolve rebase conflicts with main ([8c6dd60](https://github.com/propeller-heads/fynd/commit/8c6dd60d6e0cef8172aeeaaba0f09131c7b3d0d5))
* resolve rebase conflicts with origin/main ([362b4d1](https://github.com/propeller-heads/fynd/commit/362b4d1b33d4ad2594dbc6bedba3856248a21ccb))
* resolve worker readiness race and serialization in replay pipeline ([897d168](https://github.com/propeller-heads/fynd/commit/897d1689cfbbe6220e7971658e5427aa6194cd4a))
* restore 1% quality threshold and add expected output regeneration ([d7f02d5](https://github.com/propeller-heads/fynd/commit/d7f02d5ba10f2d2178aaedaad0c5ea97b2f2957a))
* sync Cargo.lock with 0.78.1 workspace version bump ([0920c90](https://github.com/propeller-heads/fynd/commit/0920c908163d101d04a5d4d85558427cbc4c5d2d))
* unify expected-output block number, clarify canary purpose ([295e866](https://github.com/propeller-heads/fynd/commit/295e866307e333fdeafb36cdee193b8164c93c5e))


### Reverts

* restore fixtures/integration/ pending decision on removal ([db00f2a](https://github.com/propeller-heads/fynd/commit/db00f2ac7b40610edd35216d342fa5688bff0045))

## [0.78.1](https://github.com/propeller-heads/fynd/compare/0.78.0...0.78.1) (2026-06-12)


### Bug Fixes

* **pfw:** normalize spot_price by token decimals in PI calc ([1cc3754](https://github.com/propeller-heads/fynd/commit/1cc375488d881fd80c94c384c6482b8258e0620e))
* **pfw:** sum all output legs when computing amount_out for split routes ([0c379e1](https://github.com/propeller-heads/fynd/commit/0c379e1c4ce6ff42ad83b2eb9059cf15c48cbdc5))

## [0.78.0](https://github.com/propeller-heads/fynd/compare/0.77.0...0.78.0) (2026-06-10)


### Features

* wire module declarations and registry entry ([a924f25](https://github.com/propeller-heads/fynd/commit/a924f256fba709208205c64120058b6b0ec11764))


### Bug Fixes

* remove unneeded test ([ce89b25](https://github.com/propeller-heads/fynd/commit/ce89b2558e976e1927fc3c71c200eed2a47e0ec6))

## [0.77.0](https://github.com/propeller-heads/fynd/compare/0.76.0...0.77.0) (2026-06-09)


### Features

* add apply_step()  to shift the relevant part of flow to the candidate path ([49a13f2](https://github.com/propeller-heads/fynd/commit/49a13f2a54c853d13ed017a0f6547d7b19481911))
* add find_best_route tests for PFW main loop ([7e02e82](https://github.com/propeller-heads/fynd/commit/7e02e828b29494b4945a8e72df42f14df69f99b4))
* add optimize_step_size function for PFW ([7de9552](https://github.com/propeller-heads/fynd/commit/7de9552386250c8bf9a5ce6be28198c32e346089))
* implement main algo loop ([84a069f](https://github.com/propeller-heads/fynd/commit/84a069f79e257332dc841859a9c9931e4b24c01d))


### Bug Fixes

* error on empty vec of swaps ([54d4eb9](https://github.com/propeller-heads/fynd/commit/54d4eb9f58854fc8d6bf74923df68022879f160b))
* error on routes with no swap ([9ed31d3](https://github.com/propeller-heads/fynd/commit/9ed31d333a1c5010f41f62f05d2de80d1541890e))
* reuse logic from gas_cost_output_tokens instead of reimplementing ([08677c2](https://github.com/propeller-heads/fynd/commit/08677c24a92f65c4e7a5902b35db9806a6d03fd8))

## [0.76.0](https://github.com/propeller-heads/fynd/compare/0.75.3...0.76.0) (2026-06-05)


### Features

* **PFW:** implement find_candidate_path and is_duplicate_path ([34ffd45](https://github.com/propeller-heads/fynd/commit/34ffd45aad102a2420aafa1b6bd0b6c10fa49026))


### Bug Fixes

* compare tokens in is_duplicate_path, not just component IDs ([c869a81](https://github.com/propeller-heads/fynd/commit/c869a81cba31fdb0920c77936de2a3a1bd635304))
* complete HopDescriptor/SimulatedHop split after rebase ([6a280ce](https://github.com/propeller-heads/fynd/commit/6a280ce44f9113ef2d6ee76d307fe19ae9df8064))
* update error messages ([ab1b845](https://github.com/propeller-heads/fynd/commit/ab1b8450fe216561ea6d2582b3dee21ffb6bd750))
* zero gas per (pool, token_in, token_out), not per pool ([e8efb9b](https://github.com/propeller-heads/fynd/commit/e8efb9b305b64f75ccdd2419cf8cdc7497837a77))

## [0.75.3](https://github.com/propeller-heads/fynd/compare/0.75.2...0.75.3) (2026-06-05)


### Bug Fixes

* complete HopDescriptor/SimulatedHop split after rebase ([aa9a0c4](https://github.com/propeller-heads/fynd/commit/aa9a0c45507f83acc48827fccc862115bee853c8))

## [0.75.2](https://github.com/propeller-heads/fynd/compare/0.75.1...0.75.2) (2026-06-05)


### Bug Fixes

* **split routing:** reject cycle-creating path combos ([1813a0c](https://github.com/propeller-heads/fynd/commit/1813a0caa3262c33e869ddf4e2377fe7d73c4ed9))
* **split routing:** topological order for cross-depth hops ([5895616](https://github.com/propeller-heads/fynd/commit/5895616d501b6b72206715827a3c9b0412a33f45))

## [0.75.1](https://github.com/propeller-heads/fynd/compare/0.75.0...0.75.1) (2026-06-05)


### Bug Fixes

* **split routing:** validate paths before merge_shared_hops ([e110c77](https://github.com/propeller-heads/fynd/commit/e110c77690e1370b64f42fe4ec8681370da502d9))

## [0.75.0](https://github.com/propeller-heads/fynd/compare/0.74.0...0.75.0) (2026-06-05)


### Features

* **PFW:** compute_probe_amount and compute_average_price_impact ([9fcdb21](https://github.com/propeller-heads/fynd/commit/9fcdb2139251ebe5ee18a56b067db3febd0f5d9d))

## [0.74.0](https://github.com/propeller-heads/fynd/compare/0.73.0...0.74.0) (2026-06-05)


### Features

* PathFrankWolfe structs ([4bb5511](https://github.com/propeller-heads/fynd/commit/4bb5511978473f822d68051801842eceb447fde6))

## [0.73.0](https://github.com/propeller-heads/fynd/compare/0.72.2...0.73.0) (2026-06-04)


### Features

* add split route validation ([7fe41d2](https://github.com/propeller-heads/fynd/commit/7fe41d2e029ab9a306b5d8f0584a2f41f0ec3d0f))
* add test setup visualizations ([8b6501f](https://github.com/propeller-heads/fynd/commit/8b6501faf1ce04e221c3e975fe272c92b09250e9))
* allow multiple groups to cycle back to start in split round-trips ([45787c6](https://github.com/propeller-heads/fynd/commit/45787c6552c54eaa2db5a6b6535d83d5c6260836))
* reject split route if there is only one hop and it has a cycle ([921a739](https://github.com/propeller-heads/fynd/commit/921a73931efa8fe90306a41a56e3ce9e39db2e76))
* update Route struct description to include info about the split swaps ([e88e313](https://github.com/propeller-heads/fynd/commit/e88e313ba6fd3fbff9534cabeb3bdbf98911df8b))


### Bug Fixes

* add comment for clarity ([1bced29](https://github.com/propeller-heads/fynd/commit/1bced296a4e975ba89e764aa7832a83d296cc0e1))
* change error type for consistency ([7cc98e6](https://github.com/propeller-heads/fynd/commit/7cc98e6191c44b56014a1c9452c69d41d499e4dd))
* use safe .last() instead of direct indexing ([5d7d89d](https://github.com/propeller-heads/fynd/commit/5d7d89dc7b27cad0e775babb3edb811f22a2218c))

## [0.72.2](https://github.com/propeller-heads/fynd/compare/0.72.1...0.72.2) (2026-06-04)

## [0.72.1](https://github.com/propeller-heads/fynd/compare/0.72.0...0.72.1) (2026-06-04)


### Bug Fixes

* clear stale edge weights when spot price is unavailable ([bb63ba4](https://github.com/propeller-heads/fynd/commit/bb63ba47abb2266eac3dbcd7520fe419fcb435ed))

## [0.72.0](https://github.com/propeller-heads/fynd/compare/0.71.1...0.72.0) (2026-06-04)


### Features

* bump tycho-simulation and tycho-execution to 0.304 ([70c2e61](https://github.com/propeller-heads/fynd/commit/70c2e616baa61717d4709a300baefcfd4f3df631))

## [0.71.1](https://github.com/propeller-heads/fynd/compare/0.71.0...0.71.1) (2026-06-04)


### Bug Fixes

* pass TLS setting to protocol stream builder ([600ff69](https://github.com/propeller-heads/fynd/commit/600ff6997e5295ef0bfd674d2f9488380004c0fc))

## [0.71.0](https://github.com/propeller-heads/fynd/compare/0.70.0...0.71.0) (2026-06-04)


### Features

* **fynd-core:** add build_with_pending for embedded pending-block simulation ([f2b1d9a](https://github.com/propeller-heads/fynd/commit/f2b1d9a896f2d7a8b9badf186867e6bab34f1747))
* **fynd-core:** expose subscribe_market_events on Solver ([eac1011](https://github.com/propeller-heads/fynd/commit/eac101141ed16fce309ae471627984fb3f7ea037))
* **fynd-core:** expose with_pending_indexer on FyndBuilder ([e7a313f](https://github.com/propeller-heads/fynd/commit/e7a313f558a9faf69154cb64994ef57f7415839b))


### Bug Fixes

* **fynd-core:** handle RFQ protocols in run_with_pending ([b0b279b](https://github.com/propeller-heads/fynd/commit/b0b279b4c2c0221baf44399c6b99e93ee7feaf42))
* **fynd-core:** remove RFQ guard from run_with_pending ([4203c1c](https://github.com/propeller-heads/fynd/commit/4203c1cb83d00e755605f39b5933910ead9083e3))
* **fynd-core:** surface feed setup errors through pending oneshot channel ([115a6fb](https://github.com/propeller-heads/fynd/commit/115a6fbe0fd9547589b86539271bd110850c6a78))

## [0.70.0](https://github.com/propeller-heads/fynd/compare/0.69.0...0.70.0) (2026-06-03)


### Features

* add per-chain default RPC URLs ([835fc74](https://github.com/propeller-heads/fynd/commit/835fc744d23260ccacaf4a47bd4d0e52c2e30f3a))
* use chain-aware TVL default from tycho-common ([9965a4c](https://github.com/propeller-heads/fynd/commit/9965a4c92ca7e5657e74518b187e744671068a3f))


### Bug Fixes

* use keyless dRPC endpoints for default RPC URLs ([36e477b](https://github.com/propeller-heads/fynd/commit/36e477b4888cb2dac0fcaa15e17f1e16132c4fab))
* use wrapped WPOL as Polygon gas token ([88adf8f](https://github.com/propeller-heads/fynd/commit/88adf8f0d622d17b744cb888dcab6ec89efe8459))

## [0.69.0](https://github.com/propeller-heads/fynd/compare/0.68.0...0.69.0) (2026-06-03)


### Features

* **BF:** Extract find_single_route sync method with MarketOverrides support ([0b6221f](https://github.com/propeller-heads/fynd/commit/0b6221f26d36f952b2e02d524bcf61c7803b9de9))


### Bug Fixes

* **BF:** Update helper visibility ([c12a80a](https://github.com/propeller-heads/fynd/commit/c12a80aa83556df4a9e34ba8763db04bae589761))

## [0.68.0](https://github.com/propeller-heads/fynd/compare/0.67.1...0.68.0) (2026-06-02)


### Features

* build_split_route with shared-hop deduplication ([0c160b4](https://github.com/propeller-heads/fynd/commit/0c160b4f6e3bc516859252353d30ed43f4e22d7a))

## [0.67.1](https://github.com/propeller-heads/fynd/compare/0.67.0...0.67.1) (2026-06-02)


### Bug Fixes

* Put back assert_meets_lower_bound ([a6fa66a](https://github.com/propeller-heads/fynd/commit/a6fa66abb40df75c1c7b22786a88623dfa867308))

## [0.67.0](https://github.com/propeller-heads/fynd/compare/0.66.0...0.67.0) (2026-06-02)


### Features

* **protocols:** add support for Quickswap V2 and new chain addresses for Arbitrum, Polygon, and BSC ([3c3ecbc](https://github.com/propeller-heads/fynd/commit/3c3ecbc0b93e10e2001bb74b452dc8c7fcaeb245))

## [0.66.0](https://github.com/propeller-heads/fynd/compare/0.65.0...0.66.0) (2026-06-02)


### Features

* add --partial-blocks flag for flashblock support ([573f49d](https://github.com/propeller-heads/fynd/commit/573f49d670b9ff843f91f350181e1125f95389f6))


### Bug Fixes

* decouple gas price refresh from Tycho message loop ([f644cb1](https://github.com/propeller-heads/fynd/commit/f644cb19fa1f0c1455b2976b84acf640711c7d35))

## [0.65.0](https://github.com/propeller-heads/fynd/compare/0.64.0...0.65.0) (2026-06-02)


### Features

* add router fee env var ([33db956](https://github.com/propeller-heads/fynd/commit/33db9560e2be0935d87c013aeb2d3d2ee5ce0951))

## [0.64.0](https://github.com/propeller-heads/fynd/compare/0.63.0...0.64.0) (2026-06-01)


### Features

* simulation utils for split-route optimisation ([3d8e524](https://github.com/propeller-heads/fynd/commit/3d8e524055a4d0b7bda3fb28dd52ea4f1cbbafaf))


### Bug Fixes

* deduplicate gas by hop in evaluate_total_output ([3dd5f49](https://github.com/propeller-heads/fynd/commit/3dd5f4986fd9e8b5512dc558d23ad78338dfa5c6))
* use overrides in compute_marginal_price_product ([f5d8845](https://github.com/propeller-heads/fynd/commit/f5d884572e1feadb5dc57a17bbf11cd22aaad87d))


### Reverts

* use &MarketState instead of MarketDataView ([4463461](https://github.com/propeller-heads/fynd/commit/44634610279141dd8d9918254fa32c01dfa90770))

## [0.63.0](https://github.com/propeller-heads/fynd/compare/0.62.0...0.63.0) (2026-06-01)


### Features

* **BF:** Define BellmanFordContext, FindRouteOptions, and RouteScoringMode ([7c5465c](https://github.com/propeller-heads/fynd/commit/7c5465c5bb50c3390b892d3a0326aa62789008da))
* **BF:** Extract build_context async method ([40fe37d](https://github.com/propeller-heads/fynd/commit/40fe37d21c667b1ebc1de70f4ee1de368d6195ae))


### Bug Fixes

* **BF:** Remove token_nodes, component_ids, max_hops and timeout from ctx ([809447a](https://github.com/propeller-heads/fynd/commit/809447a6e8693073d93d0c00f350d016fa003345))

## [0.62.0](https://github.com/propeller-heads/fynd/compare/0.61.0...0.62.0) (2026-05-29)


### Features

* **client:** make rpc_url optional in FyndClientBuilder::new ([3aa7b1f](https://github.com/propeller-heads/fynd/commit/3aa7b1fbaa1c97b86fcb211ddad267430eacf094))
* **ts-client:** derive chainId from /v1/info instead of options ([278f33c](https://github.com/propeller-heads/fynd/commit/278f33c972f1cc83e8419d1ba5b1ad2a6cd75a0a))

## [0.61.0](https://github.com/propeller-heads/fynd/compare/0.60.0...0.61.0) (2026-05-28)


### Features

* Add single_swaps.toml and run gas audit tool for it ([346b361](https://github.com/propeller-heads/fynd/commit/346b361fe63f1bbd964da20706254cdf8c701fa5))
* Add tokens attribute to Route ([1d7eac2](https://github.com/propeller-heads/fynd/commit/1d7eac2370c9a2bd7653e089fa24b46bb87b6f12))
* **encoding:** allow client to patch calldata signature ([d35e761](https://github.com/propeller-heads/fynd/commit/d35e761347770a7b1c00c2704d805b87dbdb294a))
* **gas-audit:** add fynd-gas-audit crate ([25594de](https://github.com/propeller-heads/fynd/commit/25594defbe8d14198dce75bdea970ebb1e8c7513))
* **gas-estimation:** Use estimate_gas_usage to refine gas estimations ([af8b55d](https://github.com/propeller-heads/fynd/commit/af8b55dc0b24e6e4c9e82da980fd23840be3855a))
* **split-routing:** add math utilities for split-route optimisation ([e2d002e](https://github.com/propeller-heads/fynd/commit/e2d002e557038c8474396391dc82288c9d8a0131))
* Update to latest tycho version ([e2277be](https://github.com/propeller-heads/fynd/commit/e2277be0e9b09acf55cecba30b3356a37ba88fca))
* update with main ([e7f4369](https://github.com/propeller-heads/fynd/commit/e7f43694c49dcb6e753a3813431710c9ba181e8e))
* Upgrade to new tycho-execution that handles gas estimations ([c8e2e5a](https://github.com/propeller-heads/fynd/commit/c8e2e5a556fb1d736cdd4b3577c31bafd0505f98))


### Bug Fixes

* **client-fee:** update EIP-712 type hash to match tycho-execution 0.300.5 ([f73bc55](https://github.com/propeller-heads/fynd/commit/f73bc5504e9317be991d911a5f137e7c434dcc50))
* **docker:** include erc20-overrides and fynd-gas-audit in workspace stubs ([11ff2d0](https://github.com/propeller-heads/fynd/commit/11ff2d02dc6f187aa2a7a7039e22f9bd0bafa5f4))
* **encoding:** Use proper eth marker ([c5b45ae](https://github.com/propeller-heads/fynd/commit/c5b45ae5f3103d27ed28c83a8400b14d8494ab12))
* Fix erc20-overrides versioning ([8b602f7](https://github.com/propeller-heads/fynd/commit/8b602f779795aa03eabea3b6edb7cd4d78666169))
* **gas-estimation:** Add refined gas estimation to OrderQuote ([3a6634c](https://github.com/propeller-heads/fynd/commit/3a6634c9918269d6e8f124893bf15e71e1316cb0))
* **gas:** Interface change in Route ([ca171cb](https://github.com/propeller-heads/fynd/commit/ca171cb78b898abb97bd670921f103d26918ddc1))
* Handle edge cases in math utils ([39638ca](https://github.com/propeller-heads/fynd/commit/39638ca91c16f9d0e8adef4817ee58caa331117f))
* OpenAPI drift ([304d940](https://github.com/propeller-heads/fynd/commit/304d940ada16efabeebf165ef0e118140e99e157))
* Remove lost expects ([a83ba6e](https://github.com/propeller-heads/fynd/commit/a83ba6eb7078f64df56e966542b671f818fb4884))

## [0.60.0](https://github.com/propeller-heads/fynd/compare/0.59.1...0.60.0) (2026-05-26)


### Features

* **split-routing:** add split-route primitive types ([4b9779b](https://github.com/propeller-heads/fynd/commit/4b9779b2a33b8c6ed75bc9838239dab161577162))

## [0.59.1](https://github.com/propeller-heads/fynd/compare/0.59.0...0.59.1) (2026-05-25)

## [0.59.0](https://github.com/propeller-heads/fynd/compare/0.58.0...0.59.0) (2026-05-21)


### Features

* **fynd-core:** MarketState earns a label from block or overlay ([04a839e](https://github.com/propeller-heads/fynd/commit/04a839eec313649f832d8cfd8674fa42c5341cfc))
* **fynd-core:** overlay storage on SharedMarketDataRef with split lock ([e160556](https://github.com/propeller-heads/fynd/commit/e1605565ee30a3fbfad194da8c1ec06fbed5acf1)), closes [#199](https://github.com/propeller-heads/fynd/issues/199) [#199](https://github.com/propeller-heads/fynd/issues/199)
* **fynd-core:** thread state label through the solve path ([75e48c6](https://github.com/propeller-heads/fynd/commit/75e48c657f681cdf594442b63dac253a57db997c))

## [0.58.0](https://github.com/propeller-heads/fynd/compare/0.57.0...0.58.0) (2026-05-15)


### Features

* add per-pool connector token restriction and derive-connector-tokens subcommand ([520b685](https://github.com/propeller-heads/fynd/commit/520b68585487602f426bbc0d0b07414922cf8ca9))


### Bug Fixes

* address PR review comments ([6ec9de1](https://github.com/propeller-heads/fynd/commit/6ec9de1b892e53903cdd09a78bafc83590e652d8))

## [0.57.0](https://github.com/propeller-heads/fynd/compare/0.56.1...0.57.0) (2026-05-13)


### Features

* add --metrics-port flag to serve command ([b8967b7](https://github.com/propeller-heads/fynd/commit/b8967b716e53fc9d88ab21441f302149646e23c3))

## [0.56.1](https://github.com/propeller-heads/fynd/compare/0.56.0...0.56.1) (2026-04-27)


### Bug Fixes

* clarify determinism comments per PR review ([047a1ab](https://github.com/propeller-heads/fynd/commit/047a1ab1ff02f3934e1fe414cb3f14f2023adce4))
* make graph construction and SPFA relaxation deterministic ([342bff6](https://github.com/propeller-heads/fynd/commit/342bff664de7f143cd5fcddb674a4710b6a1974a))

## [0.56.0](https://github.com/propeller-heads/fynd/compare/0.55.0...0.56.0) (2026-04-27)


### Features

* gate Prometheus metrics behind a Cargo feature ([a980ba0](https://github.com/propeller-heads/fynd/commit/a980ba03b0e9701020e3c0dd3adc121c49044ed7))

## [0.55.0](https://github.com/propeller-heads/fynd/compare/0.54.0...0.55.0) (2026-04-24)


### Features

* **fynd-client:** make Quote::new and BlockInfo::new public ([c2f0e36](https://github.com/propeller-heads/fynd/commit/c2f0e368fb0567125ddebd529d60556767ac7059))
* **fynd-client:** make Route::new and Swap::new public ([7e0e9d4](https://github.com/propeller-heads/fynd/commit/7e0e9d447da88c0691faa7ab6f25b7679d3610cd))
* Make Transaction constructor pub ([4d7f082](https://github.com/propeller-heads/fynd/commit/4d7f082274b4a7cdeb849fb2ea3aec141ded895a))

## [0.54.0](https://github.com/propeller-heads/fynd/compare/0.53.0...0.54.0) (2026-04-24)


### Features

* simplify price guard config ([eb37af0](https://github.com/propeller-heads/fynd/commit/eb37af01cf7ed99d70c19e9b0c6534a9ae9787ff))

## [0.53.0](https://github.com/propeller-heads/fynd/compare/0.52.0...0.53.0) (2026-04-22)


### Features

* keep ranked quote candidates for price guard fallback ([b0bdfa3](https://github.com/propeller-heads/fynd/commit/b0bdfa39c3dd05cbf106c0f4ea11d3d8ae5b6ba0))


### Bug Fixes

* exit early if no successful quotes ([495e3e2](https://github.com/propeller-heads/fynd/commit/495e3e26b16af544d15b12e222af602fe15ac67f))
* return first quote if all quotes failed the price check ([164ba00](https://github.com/propeller-heads/fynd/commit/164ba008e9a83f58a87d266af88fffca82cd47c1))

## [0.52.0](https://github.com/propeller-heads/fynd/compare/0.51.0...0.52.0) (2026-04-17)


### Features

* rewrite custom algorithm example with standalone implementation ([4aec8a2](https://github.com/propeller-heads/fynd/commit/4aec8a2604054d408cc038371fbac87234f307d8)), closes [#188](https://github.com/propeller-heads/fynd/issues/188)

## [0.51.0](https://github.com/propeller-heads/fynd/compare/0.50.0...0.51.0) (2026-04-15)


### Features

* Cap tycho-execution version ([3342b9e](https://github.com/propeller-heads/fynd/commit/3342b9e099424965fca26561ec12bbfe4d0e201b))


### Bug Fixes

* patch rustls-webpki CVEs and drop unused dotenv dev-dep ([af92385](https://github.com/propeller-heads/fynd/commit/af92385c96d9563160d820837c288f9a322a62e6))

## [0.50.0](https://github.com/propeller-heads/fynd/compare/0.49.1...0.50.0) (2026-04-13)


### Features

* Enable using PriceGuard in FyndBuilder ([35e80d2](https://github.com/propeller-heads/fynd/commit/35e80d2360bf751217c96964f6e6ed01c52f2dcf))


### Bug Fixes

* After merge fixes ([d9e5f20](https://github.com/propeller-heads/fynd/commit/d9e5f208786671b7b32eb47a3fd62ebb20b23d2f))
* correct price guard fallback logic ([4d08c75](https://github.com/propeller-heads/fynd/commit/4d08c75c249dc3b2a24bc661be48318275a75e6c))
* default price guard allow flags to false so they can be toggled via CLI ([2a6ff1e](https://github.com/propeller-heads/fynd/commit/2a6ff1e139c7abe9d525d25f7d37a88b848228e9))
* move the price guard into the encoding options ([9e5932a](https://github.com/propeller-heads/fynd/commit/9e5932aeea959a4ad433d15d5a154c0dc495d7af))
* rename for clarity ([125f3e9](https://github.com/propeller-heads/fynd/commit/125f3e9dab07656158fa1eee491ab82fe703fe8c))
* return PriceNotFound instead of TokenNotFound when cannot resolve price ([c7e6e26](https://github.com/propeller-heads/fynd/commit/c7e6e26be900ee3edd99e0bcaabee04e7f12ca16))
* revert cli flags for backwards compatibility ([132864b](https://github.com/propeller-heads/fynd/commit/132864b5a1d26eaa4fb820d4f47de61d1a5315ce))
* set price_guard_config only when price_guard is set and prioritize request config ([294af2d](https://github.com/propeller-heads/fynd/commit/294af2d856493e3dbfdd7b5e4e997c1e98f6f00c))

## [0.49.1](https://github.com/propeller-heads/fynd/compare/0.49.0...0.49.1) (2026-04-07)

## [0.49.0](https://github.com/propeller-heads/fynd/compare/0.48.0...0.49.0) (2026-04-06)


### Features

* **fynd-client:** add batch_quote method for multi-order requests ([fd20117](https://github.com/propeller-heads/fynd/commit/fd20117019b47b00cbde21fba867d5a604096fc2))
* **fynd-client:** derive Clone on all public structs ([894d877](https://github.com/propeller-heads/fynd/commit/894d877d5cd2ce01daa3683869d7a10d2eba1806))


### Bug Fixes

* **fynd-client:** fix fmt, doc link, and orphaned comment ([a548513](https://github.com/propeller-heads/fynd/commit/a54851356a103ec4562ddc2dc4f4ec12840fade8))

## [0.48.0](https://github.com/propeller-heads/fynd/compare/0.47.0...0.48.0) (2026-04-06)


### Features

* fallback to tycho simulation blocklist defaults ([727eef8](https://github.com/propeller-heads/fynd/commit/727eef8e02960b39d0381eabeb9da5e57fafa992))


### Bug Fixes

* update tycho simulation version ([c9109b9](https://github.com/propeller-heads/fynd/commit/c9109b9baa2e50c6f66ae80f05284bb8b96bc7a3))

## [0.47.0](https://github.com/propeller-heads/fynd/compare/0.46.1...0.47.0) (2026-04-03)


### Features

* **client:** allow flexibility in approval check ([049f674](https://github.com/propeller-heads/fynd/commit/049f674833a99b34fb9db6b63ea6c64d787a7048))
* **swap-cli:** approve unlimited amount for Permit2 to streamline future swaps ([c224fb2](https://github.com/propeller-heads/fynd/commit/c224fb2ad4ec853eae510f5124e8a22ae12c1ac6))

## [0.46.1](https://github.com/propeller-heads/fynd/compare/0.46.0...0.46.1) (2026-04-02)


### Bug Fixes

* set min token quality filter on tycho stream ([e71fae0](https://github.com/propeller-heads/fynd/commit/e71fae0d5dcfeb845af6222b47ef76b4a6b4eaee))

## [0.46.0](https://github.com/propeller-heads/fynd/compare/0.45.0...0.46.0) (2026-04-02)


### Features

* add blocklisted_components setter to FyndBuilder ([c4d29b0](https://github.com/propeller-heads/fynd/commit/c4d29b0627ede8cb50acf1ce8a8a74d23c680a25))
* add missing components to blocklist ([3c90845](https://github.com/propeller-heads/fynd/commit/3c90845d39c3f506fd24bea3a9e250a6a07e1a45))
* move blocklist to tycho-simulation ([46bda43](https://github.com/propeller-heads/fynd/commit/46bda43c7d0824b4f670b016b6a9a4f63d052271))


### Bug Fixes

* remove dependency mentioned twice from cargo.lock ([84dce5b](https://github.com/propeller-heads/fynd/commit/84dce5b548bd00739cfd691df90bbc10dc03ad18))

## [0.45.0](https://github.com/propeller-heads/fynd/compare/0.44.1...0.45.0) (2026-04-01)


### Features

* **swap-cli:** poll health for 30s and show amount out net gas ([4018379](https://github.com/propeller-heads/fynd/commit/401837992311ea9c890674e23c8ab9290a25ff98))

## [0.44.1](https://github.com/propeller-heads/fynd/compare/0.44.0...0.44.1) (2026-04-01)


### Bug Fixes

* address PR review — use generic gas token naming, separate error paths ([0968040](https://github.com/propeller-heads/fynd/commit/096804069fc07086c7933b14dd87714746fae1b6))
* guard zero denominator and update computation_requirements comment ([e4558ef](https://github.com/propeller-heads/fynd/commit/e4558ef68b8ce2dbc738b48b27a1dc950b34f8a6))
* normalize pool depth to ETH in edge weight construction ([74bfa58](https://github.com/propeller-heads/fynd/commit/74bfa588ef0a5c7080f547132bd05136bf677491))

## [0.44.0](https://github.com/propeller-heads/fynd/compare/0.43.2...0.44.0) (2026-04-01)


### Features

* add Binance WebSocket price provider ([6722416](https://github.com/propeller-heads/fynd/commit/67224169f46d07ebb8e3f8f770ea2b417e48ef02))
* Update quote in all stablecoins when receiving USDC/USDT prices ([5b7be72](https://github.com/propeller-heads/fynd/commit/5b7be723a9c0c1993cabc24d349914bc08136453))


### Bug Fixes

* broken doc link and stale OpenAPI spec ([0903942](https://github.com/propeller-heads/fynd/commit/0903942bd9927d36841b71d802610ed7c5c85a8e))
* Skip token cache refresh when registry is unchanged ([6428da0](https://github.com/propeller-heads/fynd/commit/6428da063c3975c8880cf572949e678392d152ed))
* Support prices for 1000 tokens ([d24407b](https://github.com/propeller-heads/fynd/commit/d24407bccf59f5a388958039bcf7f2b70a9e5a90))

## [0.43.2](https://github.com/propeller-heads/fynd/compare/0.43.1...0.43.2) (2026-04-01)


### Bug Fixes

* add #[non_exhaustive] to OrderSide in all three crates ([92578ae](https://github.com/propeller-heads/fynd/commit/92578ae894a45bede28d1c77ea1bd9191bc3ac9d))
* **fynd-core:** add #[must_use] to builder and config types ([3895c8d](https://github.com/propeller-heads/fynd/commit/3895c8dbf6f06dedf90cc63ef2ce101858737280))
* **fynd-core:** add #[non_exhaustive] to extensible public enums ([331f306](https://github.com/propeller-heads/fynd/commit/331f3066dcb8ca6248e2269d4de657cf3dbddf40))
* **fynd-rpc-types:** add #[must_use] to request/response builder types ([62ea461](https://github.com/propeller-heads/fynd/commit/62ea461c7065bd6f83db14a1beb770c899057d34))

## [0.43.1](https://github.com/propeller-heads/fynd/compare/0.43.0...0.43.1) (2026-04-01)


### Bug Fixes

* **docs:** add doc build to check.sh, fix broken intra-doc links ([c1ecfab](https://github.com/propeller-heads/fynd/commit/c1ecfabb82a46154967017906921f687d0e3fbf2))
* **docs:** resolve broken intra-doc links across fynd-core and fynd-rpc ([ad80d8e](https://github.com/propeller-heads/fynd/commit/ad80d8e7a32157e5f615327d996935fa68200a32))

## [0.43.0](https://github.com/propeller-heads/fynd/compare/0.42.0...0.43.0) (2026-03-31)


### Features

* add Hyperliquid oracle price provider ([a090ef5](https://github.com/propeller-heads/fynd/commit/a090ef5f25803ffca7717fce3f3a5f08979e555a))
* Load USD stablecoins from shared stable_usd.json ([400f755](https://github.com/propeller-heads/fynd/commit/400f75585ea96005a2932d334e06def7d606bbbb))
* PriceGuard validation ([0963393](https://github.com/propeller-heads/fynd/commit/0963393f3618de6a13141146de26ca9532130e92))


### Bug Fixes

* Bring back PriceNotFound ([4f07faf](https://github.com/propeller-heads/fynd/commit/4f07fafca6a414ae3aaefd523bb6c821fbcc5b4e))
* Handle k tokens & use dedicated normalization ([0b08180](https://github.com/propeller-heads/fynd/commit/0b08180065548c3a466a263b994b3cbcac5d5df0))
* normalize_symbol returns uppercase on fallthrough ([03e0f44](https://github.com/propeller-heads/fynd/commit/03e0f44edbb8ccb554d7190f1816514b101c53fe))
* Skip token cache refresh when registry is unchanged ([02612e1](https://github.com/propeller-heads/fynd/commit/02612e17396a3d6e4ed71d98454cfbdcb2b08689))

## [0.42.0](https://github.com/propeller-heads/fynd/compare/0.41.0...0.42.0) (2026-03-31)


### Features

* add price_guard to fynd-rpc QuoteOptions ([ecad69c](https://github.com/propeller-heads/fynd/commit/ecad69cf15cb3a0c8681f64d8225ba6c86fd94a2))
* add separate field for cases when token prices are not found ([cab8cd8](https://github.com/propeller-heads/fynd/commit/cab8cd86a6b2dee8c392580abe3bd8b91d88e2e6))
* PriceGuard validation ([891322a](https://github.com/propeller-heads/fynd/commit/891322ae991a71a5e56ce2f093bb395747a4c83c))
* return error from PriceGuard when no providers are registered ([5eac8d4](https://github.com/propeller-heads/fynd/commit/5eac8d435782e82b857269039e2f3eab6d74f5bc))
* update openapi ([71c8c20](https://github.com/propeller-heads/fynd/commit/71c8c201d6aef1b91fddb21444b4d5341c62d710))
* update openapi ([937aee4](https://github.com/propeller-heads/fynd/commit/937aee4ea7306f6de2d291cd7cde90564fcfef12))


### Bug Fixes

* add early exit to avoid division by 0 ([89e1780](https://github.com/propeller-heads/fynd/commit/89e17807398dce556aefd2f7833c4c648edacccd))
* add missing solution status ([f5df406](https://github.com/propeller-heads/fynd/commit/f5df406e1162d8ccc8b52ca58b75db8a2883643b))

## [0.41.0](https://github.com/propeller-heads/fynd/compare/0.40.0...0.41.0) (2026-03-31)


### Features

* **swap-cli:** use client approval API, fetch router from /v1/info ([5c0a6e6](https://github.com/propeller-heads/fynd/commit/5c0a6e692d2f0278e610943b84f0672aff0eb998))


### Bug Fixes

* **swap-cli:** inject router allowance in addition to Permit2 for dry-run ([c999d3b](https://github.com/propeller-heads/fynd/commit/c999d3bb634007d98776ce23a6759b7c88b4e458))
* **swap-cli:** use ephemeral signer and zero gas price for dry-run ([94f6b81](https://github.com/propeller-heads/fynd/commit/94f6b817cdd924a51194a470a7e04018e6a9cc1c))
* **swap-cli:** use MAX>>1 for storage overrides to avoid USDC blacklist ([047e4ff](https://github.com/propeller-heads/fynd/commit/047e4ff328f3decdde207c29201191f869e025d1))

## [0.40.0](https://github.com/propeller-heads/fynd/compare/0.39.2...0.40.0) (2026-03-31)


### Features

* add docker-compose for swap-cli ([8a059fa](https://github.com/propeller-heads/fynd/commit/8a059fad5093222a5e88699fbc521db7dd7887bd))

## [0.39.2](https://github.com/propeller-heads/fynd/compare/0.39.1...0.39.2) (2026-03-30)


### Bug Fixes

* **fynd-rpc-types:** remove unconditional tycho-simulation dependency ([7d849aa](https://github.com/propeller-heads/fynd/commit/7d849aaeb0814bd7e9ac8d8396f81bdd02ff0079))
* **workspace:** add clap env feature to workspace dep ([5215f78](https://github.com/propeller-heads/fynd/commit/5215f78b2e24c2184e0addc59fa22e37df9707bc))

## [0.39.1](https://github.com/propeller-heads/fynd/compare/0.39.0...0.39.1) (2026-03-30)


### Bug Fixes

* remove runtime workaround for tycho-execution encoding ([0505997](https://github.com/propeller-heads/fynd/commit/050599775cf540a8877421e071ad15bd522aafba))
* Upgrade to new tycho-execution version ([fd1daae](https://github.com/propeller-heads/fynd/commit/fd1daae9be6f7400c1d38c2a7a22a3ee1c5b331e))
* Upgrade to new tycho-execution version instead of local ([9d840ad](https://github.com/propeller-heads/fynd/commit/9d840adbb38390a9afc392e56d40599f2bc8d30e))

## [0.39.0](https://github.com/propeller-heads/fynd/compare/0.38.0...0.39.0) (2026-03-30)


### Features

* **docker:** add fynd-swap-cli binary to Docker image ([ba98305](https://github.com/propeller-heads/fynd/commit/ba98305b87e0d5e8e8c68f62ae17247ecd8f6ac9))


### Bug Fixes

* **swap-cli:** fix balance slot detection for bit-packed tokens (e.g. USDC) ([a54e488](https://github.com/propeller-heads/fynd/commit/a54e488ca5a548d38391b3cf5666ee1c6e35c384))
* **swap-cli:** make dry-run work without real funds ([83ad717](https://github.com/propeller-heads/fynd/commit/83ad717720266c923f6843bed817be10587cc893))
* **swap-cli:** remove embedded solver to speed up builds ([e4fe30c](https://github.com/propeller-heads/fynd/commit/e4fe30c73081d8bab37829918b4b557e06729ad9))

## [0.38.0](https://github.com/propeller-heads/fynd/compare/0.37.1...0.38.0) (2026-03-30)


### Features

* **config:** embed blacklist defaults in binary ([abd1f2d](https://github.com/propeller-heads/fynd/commit/abd1f2d1c498c4cfc86bd2fcc2a9b3c7905ff132))

## [0.37.1](https://github.com/propeller-heads/fynd/compare/0.37.0...0.37.1) (2026-03-30)


### Bug Fixes

* **config:** inline worker_pools defaults to fix cargo publish ([02f66aa](https://github.com/propeller-heads/fynd/commit/02f66aa49799341547d6d9eb8cb757245b05d268))

## [0.37.0](https://github.com/propeller-heads/fynd/compare/0.36.0...0.37.0) (2026-03-27)


### Features

* **config:** embed worker_pools.toml defaults in binary ([4e8c968](https://github.com/propeller-heads/fynd/commit/4e8c968fd5c8d9e59bea0f790709ca1e6088271e))

## [0.36.0](https://github.com/propeller-heads/fynd/compare/0.35.0...0.36.0) (2026-03-27)


### Features

* add more tests ([304eed8](https://github.com/propeller-heads/fynd/commit/304eed82b66ee995d4ba9e45bffb0a22c094adfd))
* broadcast error for all failures ([fcedc4d](https://github.com/propeller-heads/fynd/commit/fcedc4da9f3285f56396d937a67057b2cbee674f))
* change graph weight to 0 for a pool with failed calculation ([c5598a7](https://github.com/propeller-heads/fynd/commit/c5598a7a233149d192f68e5151ea0b381aa3218c))
* expose per-computation block numbers ([b1206a9](https://github.com/propeller-heads/fynd/commit/b1206a927c48b8a8c55d5de2e0b5df2f6228f138))
* improve error handling in computation manager ([0d9168e](https://github.com/propeller-heads/fynd/commit/0d9168ef7ef9aab95138fef3e5c740e1b14c3041))
* keep track of the failed computations ([c45c7b8](https://github.com/propeller-heads/fynd/commit/c45c7b834029371b36ca5d3a4fcd8de698b8ea27))


### Bug Fixes

* add error handling ([d0f0b06](https://github.com/propeller-heads/fynd/commit/d0f0b066e683ae3735ca406a3db9d1c041534ec8))
* check blocked state before waiting in wait_until_ready ([bea0bd9](https://github.com/propeller-heads/fynd/commit/bea0bd96b1fe4950182cfab1c36dc98b6db41783))
* distinguish between partial and full failure ([62dbd83](https://github.com/propeller-heads/fynd/commit/62dbd83c965c54c88ddcf205206db9892db48442))
* make FailedItemError variants more explicit ([fd7daa6](https://github.com/propeller-heads/fynd/commit/fd7daa698f5230b7b48bb2732b8accbd5fa47d04))
* remove unneded Option wrapping ([c28e031](https://github.com/propeller-heads/fynd/commit/c28e0316af77fb7a531a2c6e568a829a6b33ff8d))
* replace method according to the previous changes ([0654fce](https://github.com/propeller-heads/fynd/commit/0654fce1e679a93dd211e7e2062cd57fca3b94c8))
* return None instead of zero-weight for failed edge lookups ([81e3a53](https://github.com/propeller-heads/fynd/commit/81e3a530c5a7d0cd4aba2381896c390b9d1d9595))
* update signatures after rebase ([c3ed276](https://github.com/propeller-heads/fynd/commit/c3ed276ee6b13c148b55a5e9598f311d18105331))

## [0.35.0](https://github.com/propeller-heads/fynd/compare/0.34.3...0.35.0) (2026-03-26)


### Features

* add Docker build and publish pipeline ([c1bdcc9](https://github.com/propeller-heads/fynd/commit/c1bdcc93cb6ad97ce90f0e65683e40b409bd22c4))
* publish fynd crate as an embeddable library ([559625e](https://github.com/propeller-heads/fynd/commit/559625e282eb92ebb2595d6967ccaf59db9825c9))


### Bug Fixes

* **docker:** restore workspace member stubs in rebuild stage ([c277f63](https://github.com/propeller-heads/fynd/commit/c277f63995ddb23c9b4655f10e1e979cb8d26e6e))
* restore fynd_core as core and fynd_rpc as rpc re-export aliases ([6cc7de0](https://github.com/propeller-heads/fynd/commit/6cc7de01eacf77047a3a2c3f94dc66504b7ad6fa))

## [0.34.3](https://github.com/propeller-heads/fynd/compare/0.34.2...0.34.3) (2026-03-26)


### Bug Fixes

* add versions to workspace dep entries for crates.io publish ([fec4cab](https://github.com/propeller-heads/fynd/commit/fec4cab9e99577efdc81cc54a9b764762d735d41))
* update Cargo.lock for internal crates after version bump ([b6aaef6](https://github.com/propeller-heads/fynd/commit/b6aaef65fc8977b2e3ee5b53daaeb1071303eb69))

## [0.34.2](https://github.com/propeller-heads/fynd/compare/0.34.1...0.34.2) (2026-03-26)


### Bug Fixes

* add repository field to package.json and rename cargo secret ([ce35e0b](https://github.com/propeller-heads/fynd/commit/ce35e0b0d43cf0ef6887a33be7d09f2f91eedd55))

## [0.34.1](https://github.com/propeller-heads/fynd/compare/0.34.0...0.34.1) (2026-03-26)


### Bug Fixes

* **benchmark:** wire scale subcommand and add shutdown tracing ([ea0e9e0](https://github.com/propeller-heads/fynd/commit/ea0e9e0206295fe030632a82811af1527d7fa46a))
* inject CARGO_REGISTRY_TOKEN into release publish job ([5e13325](https://github.com/propeller-heads/fynd/commit/5e133258082f829051faaf1738b2ab796f83cdb4))

## [0.34.0](https://github.com/propeller-heads/fynd/compare/0.33.2...0.34.0) (2026-03-26)


### Features

* add @kayibal/fynd-client TypeScript package ([f50eef4](https://github.com/propeller-heads/fynd/commit/f50eef4744add3eb2fce030f6fd90dfa00effe47))


### Bug Fixes

* **ts-client:** update example and build script to use @kayibal/fynd-client ([5755c37](https://github.com/propeller-heads/fynd/commit/5755c370749b59097107336de05388e3e2c146dd))

## [0.33.2](https://github.com/propeller-heads/fynd/compare/0.33.1...0.33.2) (2026-03-26)

## [0.33.1](https://github.com/propeller-heads/fynd/compare/0.33.0...0.33.1) (2026-03-26)


### Bug Fixes

* **ci:** move --silent before run in pnpm build commands ([48a64e3](https://github.com/propeller-heads/fynd/commit/48a64e395ff54fd1e88eb6ac68a8ea1de37e3775))

## [0.33.0](https://github.com/propeller-heads/fynd/compare/0.32.0...0.33.0) (2026-03-26)


### Features

* **ci:** add TypeScript examples to run-all-examples.sh ([496c06b](https://github.com/propeller-heads/fynd/commit/496c06b2a897fbcf11cef4183108c1292fe71954))
* **ts-client:** add approval flow with info(), approval(), and submit() ([c125b63](https://github.com/propeller-heads/fynd/commit/c125b634121a9f0e7922a25bd0fc7167a86a4168))
* **ts-client:** add gas estimation, signing fix, and revert reason fetching ([3f75d16](https://github.com/propeller-heads/fynd/commit/3f75d16f791251f04fcb26878413155e665f8e32))


### Bug Fixes

* **ci:** fix doc snippet anchor and add pnpm to examples workflow ([135078b](https://github.com/propeller-heads/fynd/commit/135078b672d8a43a3ca999853667a2562b937258))

## [0.32.0](https://github.com/propeller-heads/fynd/compare/0.31.0...0.32.0) (2026-03-26)


### Features

* Add support for ekubo_v3 exchange in protocol registry ([db945b3](https://github.com/propeller-heads/fynd/commit/db945b3112ba66e0cc50d177796e1ddbdd1d7c17))

## [0.31.0](https://github.com/propeller-heads/fynd/compare/0.30.0...0.31.0) (2026-03-26)


### Features

* **ci:** add Examples workflow to run all Rust client examples ([194c241](https://github.com/propeller-heads/fynd/commit/194c2411b6c35020dd6363343ea9afe7d9ec3287))
* **dev-env:** show fynd logs via RUST_LOG=info ([8221587](https://github.com/propeller-heads/fynd/commit/82215872fa47e0e9be52d0b7756bcf2f0c43dd68))
* **examples:** add dev-env scripts and self-documenting error messages ([48b4bc2](https://github.com/propeller-heads/fynd/commit/48b4bc29113d6ab0e69b629c4e6d9be230a63a65))
* **examples:** increase swap_client_fee slippage to 1%, add to CI ([114b650](https://github.com/propeller-heads/fynd/commit/114b65004b3140a4d3a381027c60eaef1c563b59))
* **examples:** simulate swap before signing to surface revert reason ([3c00541](https://github.com/propeller-heads/fynd/commit/3c005412bff373644452fb550012f4f48a26edd5))
* **rust-client:** add approval flow with info(), approval(), and submit() ([22cb236](https://github.com/propeller-heads/fynd/commit/22cb236e2e88918d7bac915abd1f85219306bc01))
* **rust-client:** return error with revert reason when tx reverts on-chain ([792548e](https://github.com/propeller-heads/fynd/commit/792548e0139be05dde119ab0c11933a0f36a6acb))
* **rust-client:** use eth_estimateGas by default for swap and approval ([6dcf39f](https://github.com/propeller-heads/fynd/commit/6dcf39f9430ff8cfa899d3157343f2c5b4b85eea))


### Bug Fixes

* **ci:** pre-build fynd release binary before running examples ([3f79da8](https://github.com/propeller-heads/fynd/commit/3f79da8cb1d2502a7772fbe81c3213d8c90fecd9))
* **ci:** run only swap_erc20 example, enable info logs for fynd serve ([b20291c](https://github.com/propeller-heads/fynd/commit/b20291c0825a4c347bbc0927f21784e701aa0469))
* **dev-env:** pass TYCHO_URL to fynd serve, quote TYCHO_URL var ([a9fc96b](https://github.com/propeller-heads/fynd/commit/a9fc96b72488d1a6284ba4cadb3987a1da95a841))
* **dev-env:** RUST_LOG=warn locally, info on CI ([b3795fa](https://github.com/propeller-heads/fynd/commit/b3795fa65adcd80efcf176bed506de7bfe39ddb9))
* **dev-env:** use mnemonic to fund dev account, update private key ([26d36fa](https://github.com/propeller-heads/fynd/commit/26d36faee1c4aed1f9e4fd7e21e7d97a7032c0c5))
* **examples:** explicitly set RPC_URL and FYND_URL when running examples ([c3e9918](https://github.com/propeller-heads/fynd/commit/c3e9918b78c0a4f6f6cef55bf6c151bf212712b6))
* **rust-client:** add 200k gas buffer for cold storage on forked chains ([11937da](https://github.com/propeller-heads/fynd/commit/11937dab0cb7b0d50d30b2d6f1f49def2e7b3014))
* **rust-client:** include raw hex in revert message when decoding fails ([8dbda82](https://github.com/propeller-heads/fynd/commit/8dbda8224d068c9af3ae551efb9f6af0d113b880))
* **rust-client:** make InstanceInfo fields private, fix direct field access, add missing test ([4ca4cbd](https://github.com/propeller-heads/fynd/commit/4ca4cbdc59daacdcab4b7cc1452e7725d59287f3))
* **rust-client:** resolve post-rebase API drift ([9861e64](https://github.com/propeller-heads/fynd/commit/9861e6479f5f16140ff0358a067c44c366b152b2))
* update broken intra-doc links to renamed methods ([3512d78](https://github.com/propeller-heads/fynd/commit/3512d7844d7467cb3c3ffe8bcec6bc42ab91c563))

## [0.30.0](https://github.com/propeller-heads/fynd/compare/0.29.0...0.30.0) (2026-03-26)


### Features

* account for router and client fees in min_amount_out ([12777c7](https://github.com/propeller-heads/fynd/commit/12777c7daa815d4b263c3cf896e36f903eff0dce))


### Bug Fixes

* Fix multi-threaded runtime panic and add RFQ attributes to Solution ([638f33b](https://github.com/propeller-heads/fynd/commit/638f33b0aa736e099b140581918e018401e61ea0))
* prevent race in Encoder runtime drop ([14f98f7](https://github.com/propeller-heads/fynd/commit/14f98f74c07ed17c21216c04e1867347e8653f8d))

## [0.29.0](https://github.com/propeller-heads/fynd/compare/0.28.0...0.29.0) (2026-03-25)


### Features

* Update tycho-execution to use the new routers ([03e5ef8](https://github.com/propeller-heads/fynd/commit/03e5ef8f1dd56b2669f035bb5dd28620c2e0ac37))

## [0.28.0](https://github.com/propeller-heads/fynd/compare/0.27.0...0.28.0) (2026-03-25)


### Features

* **rpc:** add GET /v1/info endpoint ([a502dfd](https://github.com/propeller-heads/fynd/commit/a502dfdc65a3c19171a314f4eeee1855addb9cb5))


### Bug Fixes

* **rpc:** remove dead into_components_with_info, add missing tests ([fda01c0](https://github.com/propeller-heads/fynd/commit/fda01c0fba1a0c0f07d7d691e3156ee249bd6421))

## [0.27.0](https://github.com/propeller-heads/fynd/compare/0.26.1...0.27.0) (2026-03-25)


### Features

* support Router v3 (tycho-execution=0.167.0) ([b46414d](https://github.com/propeller-heads/fynd/commit/b46414d301fe4c8d160f89d60d2348a162ef0185))

## [0.26.1](https://github.com/propeller-heads/fynd/compare/0.26.0...0.26.1) (2026-03-25)


### Bug Fixes

* **client:** map all known server error codes to typed ErrorCode variants ([f45071e](https://github.com/propeller-heads/fynd/commit/f45071ef794498ffb562c48c877294a61b051c56))
* **fynd-client:** mark ErrorCode as non_exhaustive ([facc5da](https://github.com/propeller-heads/fynd/commit/facc5da6b18555866e35c66e9d08a7d5759426fc))
* **rpc:** always return JSON bodies, including on extractor errors ([ae6ba4f](https://github.com/propeller-heads/fynd/commit/ae6ba4f0c02f3ab90f62b115af9601782aeeeaa1))
* **rpc:** return 503 for MarketDataStale, add missing test coverage ([0432dec](https://github.com/propeller-heads/fynd/commit/0432dec56ef7d60e1c34b7ff8bf5dfab159de5b4))
* **rpc:** return 503 for solver timeout instead of 504 ([95cc60d](https://github.com/propeller-heads/fynd/commit/95cc60d7c9177b664fb72e29a489b18849a5407e))

## [0.26.0](https://github.com/propeller-heads/fynd/compare/0.25.2...0.26.0) (2026-03-25)


### Features

* update tycho-simulation dependency ([b0a7647](https://github.com/propeller-heads/fynd/commit/b0a76478888ca3e37ebaa22f91a7a0d6c81f383f))

## [0.25.2](https://github.com/propeller-heads/fynd/compare/0.25.1...0.25.2) (2026-03-25)


### Bug Fixes

* update doc snippet checker for quickstart folder move ([a2770de](https://github.com/propeller-heads/fynd/commit/a2770de5f4e3e9928e2ba44ad44528424ffe5f76))

## [0.25.1](https://github.com/propeller-heads/fynd/compare/0.25.0...0.25.1) (2026-03-24)


### Bug Fixes

* fix after incorrect rebase ([200abcb](https://github.com/propeller-heads/fynd/commit/200abcbdf5cf2a07e92b0343e7bc7aac54a88fd6))
* track all candidate path pools in path_components for incremental invalidation ([5badb5c](https://github.com/propeller-heads/fynd/commit/5badb5c29fe33887e5a03907ac97587ee2f3cf94))
* update the path to the relevant docs ([a423bd0](https://github.com/propeller-heads/fynd/commit/a423bd029de7b9ac7b64707ee88dc3ec6e2b0a9c))

## [0.25.0](https://github.com/propeller-heads/fynd/compare/0.24.3...0.25.0) (2026-03-23)


### Features

* **benchmark:** improve compare tool with parallel requests, net-of-gas, timing ([a82e6af](https://github.com/propeller-heads/fynd/commit/a82e6af2eff980a5f05e8fe75941f3535a6daaca))


### Bug Fixes

* **benchmark:** add missing reqwest dep and apply formatting ([f4cc24e](https://github.com/propeller-heads/fynd/commit/f4cc24edae5d8aaf42b71df5c275d5289b4ff358))
* **benchmark:** clean up unused deps, naming, and API surface ([0bd89bc](https://github.com/propeller-heads/fynd/commit/0bd89bcd2032c61abcb48cfe55b1062649213e9a))
* **benchmark:** use server-side amount_out_net_gas and real aggregator trades ([a5080af](https://github.com/propeller-heads/fynd/commit/a5080af7ef1ae67088972dd22c3ef9a839967954))


### Reverts

* Revert "Replace committed Dune dataset with download-trades subcommand" ([7c580fe](https://github.com/propeller-heads/fynd/commit/7c580fe7c4af0b85b9dd0d93584feef4884a3434))

## [0.24.3](https://github.com/propeller-heads/fynd/compare/0.24.2...0.24.3) (2026-03-23)


### Bug Fixes

* remove unused SharedMarketData import ([824092b](https://github.com/propeller-heads/fynd/commit/824092bcc33bc2d88c73515b092e6b11176d4b72))
* update rustls-webpki to 0.103.10 for RUSTSEC-2026-0049 ([1aa85c2](https://github.com/propeller-heads/fynd/commit/1aa85c2706b7991fbc6c647fc49b85c87047ac14))

## [0.24.2](https://github.com/propeller-heads/fynd/compare/0.24.1...0.24.2) (2026-03-23)


### Bug Fixes

* remove accidentally committed .env and add to .gitignore ([bdcb25e](https://github.com/propeller-heads/fynd/commit/bdcb25e4da0d6a36db43a299a069c15c053f7039))

## [0.24.1](https://github.com/propeller-heads/fynd/compare/0.24.0...0.24.1) (2026-03-23)


### Bug Fixes

* **deps:** update rustls-webpki to 0.103.10 for RUSTSEC-2026-0049 ([73c4bd5](https://github.com/propeller-heads/fynd/commit/73c4bd5902c58b4e5880439208a62b28ad614036))
* **most_liquid:** use per-component state overrides in simulate_path ([62ae7b8](https://github.com/propeller-heads/fynd/commit/62ae7b8f95f5247d14b53d67d1ebfad938ef48d0))

## [0.24.0](https://github.com/propeller-heads/fynd/compare/0.23.1...0.24.0) (2026-03-20)


### Features

* PriceProvider skeleton + helpers ([c8f1e1d](https://github.com/propeller-heads/fynd/commit/c8f1e1dd8189ee538389eb229f521062a1dac006))

## [0.23.1](https://github.com/propeller-heads/fynd/compare/0.23.0...0.23.1) (2026-03-20)

## [0.23.0](https://github.com/propeller-heads/fynd/compare/0.22.1...0.23.0) (2026-03-20)


### Features

* **fynd-rpc-types:** harden public API surface ([b6b1539](https://github.com/propeller-heads/fynd/commit/b6b153956d429180bda5b819a6555dbfea75ba4a))

## [0.22.1](https://github.com/propeller-heads/fynd/compare/0.22.0...0.22.1) (2026-03-20)

## [0.22.0](https://github.com/propeller-heads/fynd/compare/0.21.1...0.22.0) (2026-03-19)


### Features

* **fynd-core:** add SolverBuilder and Solver for simplified setup ([60bfc2f](https://github.com/propeller-heads/fynd/commit/60bfc2f4f21102af1d34e23620571c590035e72d))


### Bug Fixes

* **fynd-core:** remove generic param from TransitionError after tycho-common 0.151.0 update ([137a8cb](https://github.com/propeller-heads/fynd/commit/137a8cbe261023ac71e810484938ecead6b8583e))
* **fynd-rpc:** remove redundant chain field from FyndBuilder ([acde0b7](https://github.com/propeller-heads/fynd/commit/acde0b7a5bf93cb41fc28c62e1e6271ef406f376))

## [0.21.1](https://github.com/propeller-heads/fynd/compare/0.21.0...0.21.1) (2026-03-19)


### Bug Fixes

* **fynd-client:** remove broken intra-doc link to non-existent OrderQuote ([4a5144b](https://github.com/propeller-heads/fynd/commit/4a5144b9c98421126ebb877f44c5c232c4d97495))

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
