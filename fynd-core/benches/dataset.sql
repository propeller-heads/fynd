-- Trade dataset for the offline algorithm benchmark.
--
-- Run against Dune (dex_aggregator.trades). The `<FIXTURE_TOKENS>` placeholder is the list of
-- token addresses the market fixture knows about; generate it with:
--
--   zstd -dc fynd-core/tests/fixtures/market_recording.json.zst | python3 -c "
--   import json,sys
--   d=json.load(sys.stdin)
--   t={tok['address'].lower() for u in d['updates'] for c in u['new_pairs'].values() for tok in c['tokens']}
--   print(', '.join(sorted(t)))"
--
-- Then convert the result rows to the benchmark's JSON shape: one object per trade, holding a
-- single sell order, plus block_time/tx_hash/project/amount_usd for provenance.
--
-- # Why these filters
--
-- * `dex_aggregator.trades`, not `dex.trades`: these are user-intent orders (someone wanted X for
--   Y). `dex.trades` is per-pool legs, including the individual hops of one aggregator route, so
--   benchmarking a router against them would be circular.
-- * The time window is one week either side of the market fixture's last block, 24838913
--   (2026-04-09 01:35:59Z). Orders from a different market state are sized for liquidity that did
--   not exist at the snapshot.
-- * `0xeeee…eeee` is the aggregator convention for native ETH; the fixture uses the zero address.
--   Without the mapping, real ETH flow is discarded as unroutable -- it was the single largest
--   "missing" token at ~53k trade sides.
-- * Both tokens must be in the fixture, so no order is unsolvable for reasons unrelated to the
--   algorithm under test.
-- * `amount_usd >= 1000` drops dust. Below that the best single pool almost always wins and every
--   algorithm returns the same route, which costs solve time and teaches nothing.
--
-- Yield when this was last run: 509,690 trades in the window, 408,958 eligible, 113,420 at or
-- above $1k, sampled down to 50,000.
WITH norm AS (
  SELECT
    CASE WHEN token_sold_address = 0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
         THEN 0x0000000000000000000000000000000000000000
         ELSE token_sold_address END AS token_in,
    CASE WHEN token_bought_address = 0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
         THEN 0x0000000000000000000000000000000000000000
         ELSE token_bought_address END AS token_out,
    token_sold_amount_raw AS amount,
    block_time,
    tx_hash,
    project,
    amount_usd
  FROM dex_aggregator.trades
  WHERE blockchain = 'ethereum'
    AND block_time BETWEEN timestamp '2026-04-02 01:35:59' AND timestamp '2026-04-16 01:35:59'
)
SELECT token_in, token_out, amount, block_time, tx_hash, project, amount_usd
FROM norm
WHERE token_in <> token_out
  AND amount > 0
  AND amount_usd >= 1000
  AND token_in IN (<FIXTURE_TOKENS>)
  AND token_out IN (<FIXTURE_TOKENS>)
ORDER BY random()
LIMIT 50000
