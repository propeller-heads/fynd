<!-- Copyright 2026 PropellerHeads. SPDX-License-Identifier: Apache-2.0 -->
# Security

## Reporting

A private disclosure contact is not yet confirmed. Use an existing private PropellerHeads contact to arrange a report. Keep vulnerabilities, seeds, keys, RPC credentials and exploit payloads out of public issues and the [support group](https://t.me/+B4CNQwv7dgIyYTJl), which handles non-sensitive API support.

Include package/WDK versions, chain, public transaction hashes and a minimal reproduction with secrets removed.

## Integration precautions

- Keep API keys on a trusted backend or proxy; send the raw key in `Authorization`.
- Bind the WDK account/provider to the adapter's chain. WDK owns keys, signing and broadcast.
- Preserve transaction hashes and reconcile uncertain RPC submissions before retrying. Earlier approvals survive later failures.
- Avoid logging request configuration or upstream error causes. The library adds no logging or telemetry.

Read the [fee limits](README.md#fees-and-failures) and [router trust boundaries](docs/compatibility.md#router-contract).
