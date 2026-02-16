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

Fynd is a high-performance DeFi route-finding engine built on top of [Tycho](https://www.propellerheads.xyz/tycho). It discovers optimal swap routes across multiple DeFi protocols in real-time, returning structured solutions that can be encoded and executed on-chain.

## Own Your Routing <a href="#own-your-dex-routing" id="own-your-dex-routing"></a>

Route APIs are simple, but there's a big tradeoff. Rate limits, network overhead, lack of transparency, unreliable APIs, and unexplainable slippage are one of the main issues. And you, as an engineer, can't do anything to fix it.

Fynd solves this. With it, you have:

1. **Access to real-time market state** via Tycho Stream, with all [Tycho supported protocols](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols)
2. **Super fast solution times:** With results as fast as 50ms, you can decide the balance between routing quality and latency.
3. **Custom route-finding algorithms:** Plug in your own algorithm, or customize our pre-built one. Fynd is designed to support multiple algorithms, and always give you the best solution.
4. **Scale as you need:** A scalable architecture that allows you to scale vertically to fulfill your speed requirements.

### Key Design Principles

* **Single source of truth**: All market data lives in one `SharedMarketData` structure. It is written to by a single feed and read by all workers. No data duplication.
* **Algorithm-agnostic**: The system is designed around a pluggable `Algorithm` trait. Different algorithms can use different graph representations and strategies. Multiple algorithms compete in parallel, and the best result wins.
* **Performance-first**: CPU-bound route finding runs on dedicated OS threads (not the async runtime). Each worker pool has its own task queue for independent backpressure and scaling.
* **Observability built-in**: Prometheus metrics, structured logging via `tracing`, and health endpoints are first-class citizens.

### Supported Chains

* Ethereum Mainnet
* **Coming soon:**
  * Base&#x20;
  * Unichain

### Supported Protocols

Any protocol supported by Tycho can be used. See [list of supported protocols](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols) and [supported RFQs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols#quickstart)

### How It Works (High-Level Overview)

<figure><picture><source srcset=".gitbook/assets/This big artboard can be taller-1.png" media="(prefers-color-scheme: dark)"><img src=".gitbook/assets/This big artboard can be taller.png" alt=""></picture><figcaption></figcaption></figure>

1. **TychoFeed** connects to **Tycho Streams** (both [on-chain protocols](https://docs.propellerheads.xyz/tycho/for-solvers/simulation#streaming-protocol-states) and [RFQs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols#stream-real-time-price-updates) streams) and handle market updates (added/removed components and state changes) every block.
2. **SharedMarketData** stores all component states, tokens, and gas prices as a single shared structure.
3. When a **solve request** arrives via HTTP, the **OrderManager** fans it out to all configured worker pools in parallel.
4. Each **Worker Pool** runs a specific algorithm configuration. Workers within the pool compete to pick up the task, find routes through their local graph, simulate swaps against shared market state, and return ranked results.
5. The **OrderManager** collects results, selects the best solution by `amount_out_net_gas`, and returns it to the caller.

## Try it out!

Ready to try it out? Head to our [quickstart](get-started/quickstart/ "mention") page and try Fynd!
