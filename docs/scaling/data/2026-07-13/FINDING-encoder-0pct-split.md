# Finding: encoder rejects PFW routes where a 0% split is not the last swap

**Discovered:** 2026-07-13, staging capacity measurement with `--encoding` (harness 0.90.0-rc.3,
backend 0.90.0-rc.1 ≙ main/0.89.1 solver code).

Under load (10 rps, 10k aggregator trade mix), ~0.3% of quote requests with
`encoding_options` fail server-side:

```
API error (ServerError): solve failed: failed to encode: Invalid input:
The 0% split for token 0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2 must be the last swap
```

The `path_frank_wolfe` pool emits split routes where a leg's split rounds to 0%
without being reordered to the last position, which `tycho-execution`'s encoder
requires. The quote solves; only encoding fails — quote-only traffic never sees it,
which is why it went unnoticed until the encoding-enabled load test.

- Observed rate: 2/600 at 10 rps (`E1-baseline-encoding.json`, steps[1].http_error_rate = 0.0033)
- Repro: capacity Job with `ENCODING=true` against staging
- Likely fix: drop or reorder 0-split legs in the route→encoder mapping, or normalize
  PFW splits before encoding
- Suggested action: file a fynd issue (draft above is copy-paste ready)
