<!-- Copyright 2026 PropellerHeads. SPDX-License-Identifier: Apache-2.0 -->
# Security policy

This module is an unpublished alpha. No production-supported version or security response SLA is declared.

## Reporting

A dedicated private disclosure channel and named security owner have **not yet been confirmed** for this module. Establish and verify them before publishing a release. Use an existing private contact with PropellerHeads to arrange a confidential report; do not post vulnerabilities, seeds, API keys, RPC credentials or exploitable transaction payloads in public issues or the support group.

For non-sensitive hosted API support, the existing Fynd guide links the [Fynd support group](https://t.me/+B4CNQwv7dgIyYTJl). That public support channel is not a verified private vulnerability-disclosure route. No security email address is asserted here.

A useful private report includes the exact package/WDK versions, chain, relevant public transaction hashes, expected behavior, actual behavior and a minimal reproduction with secrets removed. Do not transfer funds or access another user's wallet to demonstrate a finding.

## Trust and operational boundaries

- WDK owns wallet keys, signing and broadcast. The adapter neither requests seed material nor creates a second signer. Application setup must bind the supplied account/provider to the configured chain.
- Fynd supplies routing and encoded transactions. The adapter checks supported outer router calls and pinned deployments; it does not prove arbitrary nested route bytes safe. Router/Fynd trust and upgrade review remain necessary.
- Keep hosted API keys on a trusted backend or behind an integrator-controlled proxy. The API key is a raw Authorization value. The package does not provide a proxy service.
- An RPC failure during submission can leave acceptance unknown. Preserve hashes and reconcile before resubmitting. Earlier approvals can remain after a failed swap.
- Quotes and fee caps are estimates, with explicit incomplete coverage. Review `networkFeeComplete`; Base L1/other rollup costs are not verified. Neither broadcast nor one confirmed receipt proves irreversible settlement.
- The library adds no direct logging or telemetry. Applications must avoid logging error causes or request configuration indiscriminately because upstream wallet/RPC errors can contain sensitive details.

Before release, complete dependency auditing, pack inspection, runtime and transaction-path checks, private disclosure ownership, and the review gates in [compatibility.md](docs/compatibility.md).
