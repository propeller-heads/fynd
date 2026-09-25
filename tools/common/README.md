# fynd-tools-common

Shared building blocks for the Fynd tools:

- `aggregator`: the quote model (`AggregatorClient`, `AggregatorQuote`) through which Fynd and
  external aggregators are compared.
- `fynd`: `FyndAggregator`, which turns a token pair and an amount into a Fynd quote.
- `swap_simulation`: `EthCallRunner`, which re-executes encoded calldata on-chain through
  `eth_simulateV1` (falling back to `eth_call`) to measure a swap's real output and gas.
- `bps`: basis-point comparison of two quote amounts.
- `constants`: shared address constants.

Part of [Fynd](https://github.com/propeller-heads/fynd).
