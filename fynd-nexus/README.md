# fynd-nexus

Holds the RFQ venue connections that Fynd pods would otherwise each open, and serves the latest
prices to them. An instance serves one chain, and a stream is one of its venues; the service keeps
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

Another binary can start the service with `fynd_nexus::run(Cli::parse())`, where `Cli` is
`fynd_nexus::cli::Cli`, after it sets up logging.

A listed venue without its credentials fails startup. The service has no authentication and it
serves prices only: expose it inside the cluster.

## Endpoints

| Endpoint | |
|---|---|
| `GET /v1/rfq/{venue}` | The current message of the stream as JSON, with an `ETag`. |
| `GET /v1/health` | 200 while at least one stream holds prices, 503 otherwise. |
| `GET /metrics` (metrics port) | Prometheus metrics. |

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

All carry `chain` and `venue` labels; `chain` is the chain the instance serves.

| Metric | Type | |
|---|---|---|
| `nexus_snapshot_age_seconds` | gauge | Age of the current prices. Without current prices, time since the last prices arrived, or since startup if no prices have arrived. |
| `nexus_venue_restarts_total` | counter | Restarts of a feed that ended. |
| `nexus_venue_ended_total` | counter | Streams that ended for good. |
| `nexus_message_bytes` | histogram | Size of each encoded snapshot and withdrawal. |

Pods that read from the service export `nexus_client_snapshot_age_seconds` (the age of the
prices when applied) and `nexus_client_failed_polls_total`, labelled by `venue`.
