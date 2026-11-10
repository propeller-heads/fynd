# fynd-nexus

Holds the venue connections that Fynd pods would otherwise each open, and serves the latest
prices to them, and the derived data of its market. An instance serves one chain, and a stream is
one of its venues; the service keeps its latest message. Bebop allows one pricing connection per API key, so pods sharing a key close
each other's connections when they open their own. Run exactly one `fynd-nexus` per chain and
environment, such as `ethereum-fynd-nexus`, deploy it with the `Recreate` strategy so two
instances never hold the same Bebop key, and point each pod at the instance of its own chain with
`fynd serve --nexus-url`. The service does not check the pod's chain. Nexus and the pods must run
the same Fynd version, because a pod reads the venue prices as the types of its own build. They do
today, as both are built from the same workspace and image.

The service runs one prices feed per venue: Bebop on Ethereum and Base, Hashflow on
Ethereum, and Metric on the chains Fynd serves it on. A venue that does not serve the chain is
logged at startup and skipped; if none of the listed venues does, startup fails. The pods build the same books they build from a direct
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
| `--tycho-url` | `TYCHO_URL` | | Tycho URL without a scheme. Turns the market on; requires `--rpc-url`. |
| `--tycho-api-key` | `TYCHO_API_KEY` | | Tycho API key. |
| `--disable-tls` | `DISABLE_TLS` | `false` | Disable TLS for the Tycho connection. |
| `--min-tvl` | `MIN_TVL` | the chain's low TVL threshold | Minimum TVL, in the chain's native token, for a component to be tracked. |
| `--rpc-url` | `RPC_URL` | | Node RPC URL, which the market reads gas prices from. |

Another binary can start the service with `fynd_nexus::run(Cli::parse())`, where `Cli` is
`fynd_nexus::cli::Cli`, after it sets up logging.

A listed venue without its credentials fails startup. Pass `--venues` with no value to run the
market alone; with no venue and no `--tycho-url`, startup fails. The service has no authentication and it
serves prices and derived data only: expose it inside the cluster.

## Market

With `--tycho-url` set, the service also runs the Tycho client for its chain. It tracks every
on-chain protocol system Tycho serves, above the TVL floor and without the default blocklist's
components, and keeps the market and the derived data computed from it (spot prices, depths, token
gas prices) up to date and serves the derived data (see [Derived data](#derived-data)). Startup
fails when `--tycho-url` is set without `--rpc-url`.

The service waits up to 15 minutes for the first block and the derived data, and logs while it
waits. It stops with an error when the Tycho feed ends or the market does not become ready, so the
orchestrator restarts it.

## Endpoints

| Endpoint | |
|---|---|
| `GET /v1/price-levels/{venue}` | The current message of the stream as JSON, with an `ETag`. |
| `GET /v1/derived/{kind}` | The latest derived data of the market as JSON, with an `ETag`. Needs `--tycho-url`. |
| `GET /v1/health` | 200 while at least one stream holds prices, 503 otherwise. A service that runs no venue is healthy once every derived kind holds a snapshot. The body lists every stream and those serving, and with the market, the derived kinds and those holding a snapshot. |
| `GET /metrics` (metrics port) | Prometheus metrics. |

`venue` is `bebop`, `hashflow` or `metric`. `GET /v1/price-levels/{venue}` answers:

| Status | When |
|---|---|
| `200` | The message is the one described below. The `ETag` header is `"{run_id}-{seq}"`, such as `"1761550000000-42"`. |
| `304` | The request's `If-None-Match` equals the current `ETag`. The body is empty. |
| `204` | The stream has had no snapshot yet in this run. The body is empty. |
| `404` | The service does not run that venue. |

Pods poll every 500 ms and send the last received `ETag` in `If-None-Match`, so an unchanged
response has no body. A request that fails, times out after 2 s, or gets any other status
(including `404`) is logged at warn level with the reason and retried after a wait that doubles from
500 ms up to 8 s. A pod never gives up polling.

## Derived data

With `--tycho-url` set, the service keeps the latest snapshot of each of the market's three default
derived computations and replaces it whenever the market finishes that computation at a new block.
Derived data is indexed by block, so it has no `run_id`, `seq`, `as_of` or withdrawals, and no
message envelope: the body is the snapshot itself. The types are in `fynd_core::nexus::derived`.

```json
{ "block": 23500000, "entries": [ { "token": "0xa0b8...", "numerator": "3000", "denominator": "1" } ] }
```

| `kind` | `entries` |
|---|---|
| `spot_prices` | `{ "component_id", "token_in", "token_out", "price" }`: 1 `token_in` buys `price` `token_out`. A price that is not finite is left out. |
| `pool_depths` | `{ "component_id", "token_in", "token_out", "depth" }`: `depth` is a decimal string. |
| `token_prices` | `{ "token", "numerator", "denominator" }`: the price in raw token units per raw native gas unit, as two decimal strings. |

`GET /v1/derived/{kind}` answers:

| Status | When |
|---|---|
| `200` | The snapshot. The `ETag` header is the block in quotes, such as `"23500000"`. |
| `304` | The request's `If-None-Match` equals the current `ETag`. The body is empty. |
| `204` | The kind has no snapshot yet. The body is empty. |
| `404` | The kind is unknown, or the service does not run the market. |

A snapshot is never withdrawn: when a computation fails or the Tycho feed stops sending blocks,
the last snapshot stays served, so a consumer compares `block` with the chain's head to know its
age.

The service compresses each snapshot with gzip once, when it writes it, and sends that copy
with `Content-Encoding: gzip` to a request whose `Accept-Encoding` lists gzip with a quality above
zero; other requests get plain JSON. The route always sends `Vary: Accept-Encoding`, and a `304`
has no body.

## Messages

The response body is one JSON message. The format is defined in `fynd_core::nexus`.

```json
{ "v": 1, "type": "snapshot",  "run_id": 1761550000000, "seq": 42, "as_of": "2026-10-27T09:00:00.123Z", "data": { } }
{ "v": 1, "type": "withdrawn", "run_id": 1761550000000, "seq": 43 }
```

- `v` is the protocol version. A reader rejects a version it does not know.
- `run_id` identifies one run of the service: its start time in milliseconds, so it changes on
  every restart.
- `seq` grows by one per snapshot or withdrawal on a venue's stream.
- `as_of` is when the service got the prices from the venue.
- `data` is the venue's parsed prices: Bebop's pairs, Hashflow's levels per market maker, or
  Metric's pools with their bid and ask.
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

Price level metrics carry `chain` and `venue` labels, and derived data metrics carry `chain` and
`computation` (`spot_prices`, `pool_depths` or `token_prices`); `chain` is the chain the instance
serves.

| Metric | Type | |
|---|---|---|
| `nexus_snapshot_age_seconds` | gauge | Age of the current prices. Without current prices, time since the last prices arrived, or since startup if no prices have arrived. |
| `nexus_venue_restarts_total` | counter | Restarts of a feed that ended. |
| `nexus_venue_ended_total` | counter | Streams that ended for good. |
| `nexus_message_bytes` | histogram | Size of each encoded snapshot and withdrawal. |
| `nexus_derived_snapshot_bytes` | histogram | Size of each encoded derived snapshot, labelled by `encoding`: `identity` or `gzip`. |
| `nexus_derived_block` | gauge | Block of the latest written snapshot. |
| `nexus_derived_skipped_entries_total` | counter | Spot prices left out of a snapshot for not being finite. |

Pods that read from the service export `nexus_client_snapshot_age_seconds` (the age of the
prices when applied) and `nexus_client_failed_polls_total`, labelled by `venue`.
