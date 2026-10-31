# fynd-nexus

Holds the RFQ venue connections that Fynd pods would otherwise each open, serves the latest prices
to them, and monitors the health of the chain's Tycho components. An instance serves one chain, and a stream is one of its venues; the service keeps
its latest message. Bebop allows one pricing connection per API key, so pods sharing a key close
each other's connections when they open their own. Run exactly one `fynd-nexus` per chain and
environment, such as `ethereum-fynd-nexus`, deploy it with the `Recreate` strategy so two
instances never hold the same Bebop key, and point each pod at the instance of its own chain with
`fynd serve --nexus-url`. The service does not check the pod's chain.

The service runs one prices feed per venue: Bebop if its client accepts the chain, Metric if Fynd
serves it on the chain, Hashflow on every chain. A venue that does not serve the chain is logged at
startup and skipped; if none of the listed venues does, startup fails. The pods build the same books they build from a direct
connection, with their own token list, TVL floor and `--aggregate-rfqs` layout, and ask the venues
for firm quotes themselves.

## Run

```bash
BEBOP_KEY=... HASHFLOW_USER=... HASHFLOW_KEY=... METRIC_API_KEY=... \
  fynd-nexus --chain ethereum
```

| Flag | Env | Default | |
|---|---|---|---|
| `--chain` | `CHAIN` | required | The chain to serve. |
| `--venues` | `VENUES` | `bebop,hashflow,metric` | Venues to connect to. |
| `--http-port` | `HTTP_PORT` | `3000` | Port of the HTTP API. |
| `--metrics-port` | `METRICS_PORT` | `9898` | Port of `/metrics`. |
| `--bebop-key` | `BEBOP_KEY` | | Bebop API key. |
| `--hashflow-user`, `--hashflow-key` | `HASHFLOW_USER`, `HASHFLOW_KEY` | | Hashflow credentials. |
| `--metric-api-key` | `METRIC_API_KEY` | | Metric API key. |
| `--metric-api-url` | `METRIC_API_URL` | Metric's public endpoint | Another Metric deployment. |
| `--rpc-url` | `RPC_URL` | | Node RPC URL. Turns the health monitor on. |
| `--tycho-url` | `TYCHO_URL` | | Tycho URL without a scheme. Required by the monitor. |
| `--tycho-api-key` | `TYCHO_API_KEY` | | Tycho API key. |
| `--tycho-use-tls` | `TYCHO_USE_TLS` | `true` | Connect to Tycho over TLS. |
| `--min-tvl` | `MIN_TVL` | chain's low threshold | Minimum TVL, in the native token, of a tracked component. |
| `--health-file` | `HEALTH_FILE` | `component-health.json` | File the health records are kept in. |
| `--probe-rate` | `PROBE_RATE` | `2` | Probes per second. |
| `--probe-usd` | `PROBE_USD` | `500` | Value of one probe swap in USD. |
| `--probe-depth-share` | `PROBE_DEPTH_SHARE` | `0.01` | Largest share of a component's depth one probe may sell. |
| `--refresh-interval` | `REFRESH_INTERVAL` | `6h` | Age of the last probe after which a healthy component is probed again. |
| `--max-retry-backoff` | `MAX_RETRY_BACKOFF` | `1h` | Longest wait before retrying a component whose last probe was not clean. |
| `--broken-after` | `BROKEN_AFTER` | `2` | Pool reverts in a row, on different blocks, that make a component broken. |
| `--inaccurate-after` | `INACCURATE_AFTER` | `3` | Shortfalls in a row that make a component inaccurate. |
| `--inaccuracy-bps` | `INACCURACY_BPS` | `100` | Shortfall, in basis points, that counts as inaccurate. |

Durations are a whole number with a unit: `90s`, `30m`, `6h`.

Another binary can start the service with `fynd_nexus::run(Cli::parse())`, where `Cli` is
`fynd_nexus::cli::Cli`, after it sets up logging.

A listed venue without its credentials fails startup. The service has no authentication: expose it
inside the cluster. The reset endpoints below change what pods exclude.

RFQ venues and the health monitor are independent. Without `--rpc-url` the service is the RFQ-only
service it always was. With it, the service runs the monitor next to the venues; pass `--venues`
with no value to run the monitor alone. Setting `--rpc-url` without `--tycho-url` fails startup.

## Endpoints

| Endpoint | |
|---|---|
| `GET /v1/rfq/{venue}` | The current message of the stream as JSON, with an `ETag`. |
| `GET /v1/health` | 200 while at least one stream holds a snapshot, 503 otherwise. With the monitor on, the body also reports it. |
| `GET /metrics` (metrics port) | Prometheus metrics. |

With the health monitor on:

| Endpoint | |
|---|---|
| `GET /v1/components/excluded` | The components pods should not route through, as a message with an `ETag` (below). |
| `GET /v1/components?status=&protocol=` | The health of every component as plain JSON, optionally filtered by status and protocol system. |
| `GET /v1/components/{id}` | The health of one component, or `404`. |
| `DELETE /v1/components/health` | Resets every component to `unknown`. |
| `DELETE /v1/components/{id}/health` | Resets one component to `unknown`, or `404` if it is not tracked. |

`venue` is `bebop`, `hashflow` or `metric`. `GET /v1/rfq/{venue}` answers:

| Status | When |
|---|---|
| `200` | The message is the one described below. The `ETag` header is `"{epoch}-{seq}"`, such as `"1761550000000-42"`. |
| `304` | The request's `If-None-Match` equals the current `ETag`. The body is empty. |
| `204` | The stream has had no snapshot yet in this run. The body is empty. |
| `404` | The service does not run that venue. |

Pods poll every 500 ms and send the last received `ETag` in `If-None-Match`, so an unchanged
response has no body. A request that fails, times out after 2 s, or gets any other status
(including `404`) is logged at warn level with the reason and retried after a wait that doubles from
500 ms up to 8 s. A pod never gives up polling.

## Messages

The response body is one JSON message. The format is defined in `fynd_core::nexus`.

```json
{ "v": 1, "type": "snapshot",  "epoch": 1761550000000, "seq": 42, "as_of": "2026-10-27T09:00:00.123Z", "data": { } }
{ "v": 1, "type": "withdrawn", "epoch": 1761550000000, "seq": 43 }
```

- `v` is the protocol version. A reader rejects a version it does not know.
- `epoch` is the service's start time in milliseconds, so it changes on every start.
- `seq` grows by one per snapshot or withdrawal on a venue's stream.
- `as_of` is when the service got the prices from the venue.
- `data` is the venue's parsed prices: Bebop's pairs, Hashflow's levels per market maker, or
  Metric's pools with their bid and ask. On the excluded components stream it is the list of
  excluded components (see Health monitor).
- `withdrawn` says the venue has no usable prices: they went stale or the venue's feed ended.
- A stream with no snapshot yet in this run answers `204` instead of `withdrawn`, so a restarted
  service does not clear the pods' books before they reach the maximum snapshot age. If no snapshot
  arrives, each pod withdraws its books at that age.

A pod withdraws a venue's books when the service's message has not changed for the venue's maximum
snapshot age (Bebop 60 s, Hashflow 30 s, Metric 15 s).

## Feed endings

A prices feed reconnects and goes stale on its own, so the service withdraws a stream whose prices
stop arriving. When a feed ends:

- with a fatal error (the venue refuses the chain) or invalid input (bad configuration), that
  stream ends for good, is logged at error level, and stays withdrawn;
- with any other error, when the feed stops without an error, or after a panic, the stream is
  withdrawn and the feed starts again after 30 s.

## Metrics

All carry `chain` and `venue` labels; `chain` is the chain the instance serves.

| Metric | Type | |
|---|---|---|
| `nexus_snapshot_age_seconds` | gauge | Age of the current prices. Without current prices, time since the last prices arrived, or since startup if no prices have arrived. |
| `nexus_venue_restarts_total` | counter | Restarts of a feed that ended. |
| `nexus_venue_ended_total` | counter | Streams that ended for good. |
| `nexus_message_bytes` | histogram | Size of each encoded snapshot and withdrawal. |

Pods that read from the service export `nexus_client_snapshot_age_seconds` (the age of the
prices when applied) and `nexus_client_failed_polls_total`, labelled by `venue`.

## Health monitor

With `--rpc-url` set, the service works through every Tycho component of its chain: all on-chain
protocol systems, above the TVL floor, from the same market feed the pods build. For each component
it plans one swap through the component, encodes it for the Tycho router, and simulates it on chain
with `eth_simulateV1` at the block of the state it priced the swap on. The node must also support
`debug_traceCall`, which gives the revert reasons. Components of kinds the router does not execute
alone (RFQ venues, pAMMs, exclusive liquidity) are marked `not_probeable` and are not probed.

A probe sells a fixed value in USD (`--probe-usd`), turned into tokens through the derived token
prices and a USD stablecoin of the market, and capped at `--probe-depth-share` of the component's
derived depth. It records whether the swap works, why it reverts, and the deviation: how far the
on-chain output is from `get_amount_out` on the component's state, after the router fee, in basis
points. A healthy component reads about 0. Components with more than two tokens are probed in a
different swap direction each time. A component whose state lags the block it is priced on can add
noise to the deviation.

### States

| Status | |
|---|---|
| `unknown` | Not probed yet, or reset. |
| `healthy` | The last probe executed and paid what the state predicted, within `--inaccuracy-bps`. |
| `suspect` | The last probe reverted or paid less than predicted, but not yet often enough to exclude the component. |
| `broken` | `--broken-after` pool reverts in a row, each on a different block. Excluded. |
| `inaccurate` | `--inaccurate-after` executed probes in a row paying more than `--inaccuracy-bps` less than predicted. Excluded. |
| `not_probeable` | The router cannot execute the component alone. |

One executed probe within the threshold makes a component `healthy` again. A surplus (paying more
than predicted) never counts. A revert that blames a token (a failed transfer, a tax, a blocked
transfer) is recorded but does not count towards `broken`: the token makes every pool that holds it
revert. A probe that could not run (funding, token layout, node error or timeout) or whose block the
node no longer has changes nothing.

### Probe order

One probe starts every `1 / --probe-rate` seconds. The next component is, in order:

1. a component never probed, the ones that joined last first. Components that Tycho adds join the
   front, and components it removes leave the queue and the saved file;
2. a component whose last probe was not clean (a revert, a shortfall, a probe that could not run),
   once its backoff has passed: one minute, doubling per probe, up to `--max-retry-backoff`;
3. a clean component whose last probe is older than `--refresh-interval`, the oldest first.

### Excluded components

`GET /v1/components/excluded` serves the `broken` and `inaccurate` components in the message format above, with `data` shaped as:

```json
{ "components": [ { "id": "0x...", "protocol_system": "uniswap_v3", "cause": "broken",
                    "since": "2026-10-27T09:00:00Z", "reason": "PoolEmpty" } ] }
```

`cause` is `broken` or `inaccurate`. The stream follows the same rules as the RFQ ones: `204` until
the first probe of this run has given a verdict (or the saved file holds one), `304` while the set is
unchanged. A new snapshot appears only when the set of excluded components or a cause changes.

The stream is withdrawn when probing stalls: no probe has given a verdict, and nothing was left to
probe, for ten probe intervals and at least five minutes. It is published again when probing
resumes. Pods treat a withdrawal, a `204`, and ten minutes without an answer as "exclude nothing".

### Resetting

The reset endpoints set components to `unknown`, clear their counts, put them at the front of the
probe queue, save the file and publish the new excluded set. Use them after fixing a cause the
monitor cannot see, such as a bad node.

### Health file

The records are kept in memory and written to `--health-file` every few seconds while they change,
through a temporary file that is renamed over it. The file is read on start: a missing file is a
fresh start, and a malformed file or another format version fails startup with its path. It holds:

```json
{ "version": 1, "components": { "0x...": { "protocol_system": "uniswap_v3", "status": "broken",
  "status_since": "...", "queued_at": "...", "probes": 3, "checked_at": "...",
  "last_outcome": "pool_reverted", "reason": "PoolEmpty", "deviation_bps": null, "block": 21000000,
  "consecutive_pool_reverts": 2, "last_pool_revert_block": 21000000, "consecutive_shortfalls": 0,
  "unsettled_checks": 2 } } }
```

`GET /v1/components` serves the same fields per component, plus `id` and `next_check_at`.

### Health monitor metrics

All carry the `chain` label.

| Metric | Type | |
|---|---|---|
| `nexus_probes_total` | counter | Probes, by `protocol` and `outcome`: `executed`, `pool_reverted`, `token_reverted`, `setup_failed`, `block_unavailable` or `not_probeable`. |
| `nexus_probe_deviation_bps` | histogram | Deviation of executed probes in basis points, by `protocol`, with buckets from -1000 to 1000. |
| `nexus_components` | gauge | Components by `protocol` and `status`. |
| `nexus_last_probe_age_seconds` | gauge | Time since a probe last gave a verdict, or since startup. |

Revert reasons are in the logs, never in labels.
