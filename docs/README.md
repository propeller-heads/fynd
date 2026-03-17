---
description: Overview of Fynd, its design and how to get started.
icon: book-open
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

# Overview

## What is Fynd?

Fynd is a DeFi route-finding engine built on [Tycho](https://www.propellerheads.xyz/tycho). It finds optimal swap routes across DeFi protocols in real-time and returns solutions you can encode and execute on-chain.

## Own Your Routing <a href="#own-your-dex-routing" id="own-your-dex-routing"></a>

Route APIs are simple, but the tradeoffs are painful: rate limits, network overhead, no transparency, unreliable uptime, and unexplainable slippage. And you can't fix any of it.

Fynd puts you in control:

1. **Real-time market state** via Tycho Stream, covering all [Tycho-supported protocols](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols)
2. **50ms solution times:** You choose the balance between routing quality and latency.
3. **Custom algorithms:** Plug in your own algorithm or customize the pre-built one. Fynd runs multiple algorithms in parallel and picks the best result.
4. **Execution on your terms:** Encode and execute swaps on-chain with full control over fees, slippage, and token transfer method.
5. **Vertical scaling:** Scale up to meet your speed requirements.

### Key Design Principles

* **Single source of truth**: All market data lives in one `SharedMarketData` structure. A single feed writes to it; all workers read from it. No duplication.
* **Algorithm-agnostic**: Built around a pluggable `Algorithm` trait. Different algorithms use different graph representations and strategies. Multiple algorithms compete in parallel; the best result wins.
* **Performance-first**: CPU-bound route finding runs on dedicated OS threads (not the async runtime). Each worker pool has its own task queue for independent backpressure and scaling.
* **Observability built-in**: Prometheus metrics, structured logging via `tracing`, and health endpoints are first-class citizens.

### Supported Chains

* Ethereum Mainnet
* **Coming soon:**
  * Base&#x20;
  * Unichain

### Supported Protocols

Fynd works with any protocol Tycho supports. See the [list of supported protocols](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols) and [supported RFQs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols#quickstart).

### How It Works

<figure><picture><source srcset=".gitbook/assets/This big artboard can be taller-1.png" media="(prefers-color-scheme: dark)"><img src=".gitbook/assets/This big artboard can be taller.png" alt=""></picture><figcaption></figcaption></figure>

1. **TychoFeed** connects to **Tycho Streams** ([on-chain protocols](https://docs.propellerheads.xyz/tycho/for-solvers/simulation#streaming-protocol-states) and [RFQs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols#stream-real-time-price-updates)) and processes market updates (added/removed components and state changes) every block.
2. **SharedMarketData** stores all component states, tokens, and gas prices in a single shared structure.
3. When a **quote request** arrives via HTTP, the **WorkerPoolRouter** fans it out to all worker pools in parallel.
4. Each **Worker Pool** runs a specific algorithm. Workers compete to pick up the task, find routes through their local graph, simulate swaps against shared market state, and return ranked results.
5. The **WorkerPoolRouter** collects results, picks the best solution by `amount_out_net_gas`, optionally encodes it into an on-chain transaction, and returns it.

## Try it out

Head to the [quickstart](get-started/quickstart/ "mention") to get Fynd running.
