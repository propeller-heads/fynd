---
description: Assert that a rolling deploy of the hosted Fynd API drops zero requests.
icon: heart-pulse
layout:
  width: default
  title:
    visible: true
  description:
    visible: true
  tableOfContents:
    visible: true
  outline:
    visible: true
  pagination:
    visible: true
  metadata:
    visible: true
  tags:
    visible: true
---

# Zero-Downtime Probe

`scripts/zero-downtime-probe.sh` drives steady quote load against a running API while a rolling
deploy executes, then reports whether any request was dropped. It is the launch gate for the hosted
API: run it against staging on every release and require a passing verdict before promoting.

The probe sends quote requests at a fixed rate for a fixed duration and tracks two failure signals:

- **non-2xx responses**, bucketed by status class (`2xx`, `3xx`, `4xx`, `5xx`, transport errors).
- **max success gap** — the longest interval with no successful response. This catches a silent
  stall (requests hanging or timing out with no error status) that a status-code check alone misses.

It exits `0` only if non-2xx is zero **and** the success gap never exceeds the threshold (default 3s).

## Running it during a deploy

1. Start the probe against staging (the token is read from the environment, never a flag):

   ```bash
   AUTH_TOKEN=<staging-token> \
     scripts/zero-downtime-probe.sh --url https://fynd-api-staging.example/v1/eth
   ```

   It POSTs to `<url>/quote` using the WETH→USDC sell fixture checked in beside the script.

2. While it runs, trigger the rolling deploy (`helmwave up`).

3. Wait for the probe to finish (default 300s), or press Ctrl-C for a clean partial summary.

4. Read the verdict. Exit `0` means zero requests were dropped; any nonzero exit means investigate.

## Options

| Flag | Default | Description |
|------|---------|-------------|
| `-u, --url` | (required) | Chain base URL, e.g. `https://host/v1/eth` |
| `-d, --duration` | `300` | Probe duration in seconds |
| `-r, --rate` | `5` | Requests per second |
| `-g, --max-gap` | `3` | Max allowed gap between successful responses, in seconds |
| `-t, --timeout` | `10` | Per-request timeout in seconds |
| `-b, --body` | fixture | Quote request body JSON |

`AUTH_TOKEN` is read from the environment only. It is passed to curl through a mode-0600 config file
so it never appears in `ps` output or curl's argv.

## Output

The probe prints a machine-readable summary line followed by a human table:

```
result=PASS total=1500 non_2xx=0 class_2xx=1500 class_3xx=0 class_4xx=0 class_5xx=0 class_err=0 max_gap_s=0.213 max_gap_threshold_s=3 elapsed_s=300 rate=5
```

`result=FAIL` with `non_2xx>0` means requests were dropped during the deploy; `result=FAIL` with
`non_2xx=0` but `max_gap_s` above the threshold means the API stalled without returning errors.
