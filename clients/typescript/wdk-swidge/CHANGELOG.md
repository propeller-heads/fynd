<!-- Copyright 2026 PropellerHeads. SPDX-License-Identifier: Apache-2.0 -->
# Changelog

## 0.1.0-alpha.0 — Unreleased

- Add a hosted Fynd Swidge adapter for same-chain, exact-input Ethereum/Base swaps using public WDK account methods.
- Add quote, execution, original-hash status and chain/token discovery methods; exact approval sequencing and partial-execution metadata.
- Validate supported router calls, caller minimums and slippage; expose itemized estimated fees and network-estimate completeness. Quotes do not enforce fee caps; execution network caps require an Ethereum swap with no required approval and a successful WDK estimate.
- Keep approval receipt fees in execution results, preserve post-approval failures, and make protocol helpers private at runtime. Document experimental token discovery and its bounded, non-restarting pagination.
- Add ESM/Bare entrypoints, generated declaration support, deterministic tests and usage documentation.

The package remains private and unpublished. Runtime, hosted execution and external acceptance evidence must be recorded before release; see `docs/compatibility.md`. This entry is a description of the initial implementation, not a release or certification announcement.
