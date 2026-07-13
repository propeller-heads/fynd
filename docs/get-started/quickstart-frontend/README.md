---
description: Swap via Fynd using the web frontend — no CLI, no private keys.
icon: browser
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

# Quickstart (Frontend)

Run the Fynd swap frontend locally with Docker Compose. Connect your browser wallet (MetaMask, Ledger, etc.) and swap — no private key export required.

## Prerequisites

* **Docker** ([install Docker](https://docs.docker.com/get-started/get-docker/))
* **Tycho API key** (set as `TYCHO_API_KEY`, [get one here](https://t.me/fynd_portal_bot))

## Step 0 — Start Fynd + Frontend

```bash
export TYCHO_API_KEY=your-api-key
docker compose -f docker-compose.frontend.yml up
```

This pulls two images and starts them:
- **Fynd router** on port 3000 — finds optimal swap routes
- **Swap frontend** on port 3005 — the web UI you interact with

## Step 1 — Connect your wallet

Open [http://localhost:3005](http://localhost:3005) in your browser.

Click **Connect Wallet** in the top-right corner and connect your wallet (MetaMask, WalletConnect, Coinbase Wallet, or any injected wallet).

{% hint style="info" %}
The router needs ~30 seconds to load market data after starting. The UI will show a loading state until the router is ready — this is normal on first launch.
{% endhint %}

## Step 2 — Swap

1. Select the token you want to sell (top input)
2. Select the token you want to buy (bottom input)
3. Enter the amount to sell
4. Click **Get Route** to fetch the best route
5. Review the route details (price impact, gas estimate, route visualization)
6. Click **Swap** to execute — your wallet will prompt you to sign the transaction

## Stopping

```bash
docker compose -f docker-compose.frontend.yml down
```

## Next steps

* [Server configuration](../../guides/server-configuration.md) — customize the router (protocols, timeouts, worker pools)
* [Quickstart (CLI/SDK)](../quickstart/) — integrate Fynd programmatically with TypeScript or Rust
* [API reference](../../reference/api.md) — full endpoint documentation
