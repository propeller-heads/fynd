"""Component-count distribution under the v3 pool-subset filter (grill round 3, finding 3).

Approximates the pinned subset filter's connectivity on the 10-day dataset: two orders share a
component when they share a token directly, or when each touches a major hub token (the filter's
"pools linking two order-adjacent tokens" clause — hub-hub pools always exist and survive any
TVL cap). If eligible blocks almost always collapse to one component, per-component isolation
(v3 item 11) is a no-op and the whole-batch-abort mitigation must change.
"""

import json
import sys
from collections import defaultdict
from pathlib import Path

BASE_HUBS = {
    "0x4200000000000000000000000000000000000006",  # WETH
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",  # USDC
    "0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca",  # USDbC
    "0x50c5725949a6f0c72e6c4a641f24049a917db0cb",  # DAI
    "0x2ae3f1ec7f1f5012cfeab0185bfc7aa3cf0dec22",  # cbETH
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf",  # cbBTC
    "0xfde4c96c8593536e31f229ea8f37b2ada2699bb2",  # USDT
}

data_dir = Path(sys.argv[1])

component_count_histogram = defaultdict(int)
eligible_blocks = 0
single_component_blocks = 0
orders_in_largest = 0
orders_total = 0

for day_file in sorted(data_dir.glob("comparisons-*.jsonl")):
    blocks: dict[int, list[tuple[str, str]]] = defaultdict(list)
    with day_file.open() as fh:
        for line in fh:
            rec = json.loads(line)
            blocks[rec["block"]].append((rec["token_in"], rec["token_out"]))

    for trades in blocks.values():
        if len(trades) < 2:
            continue
        eligible_blocks += 1

        parent = list(range(len(trades)))

        def find(i: int) -> int:
            while parent[i] != i:
                parent[i] = parent[parent[i]]
                i = parent[i]
            return i

        def union(a: int, b: int) -> None:
            parent[find(a)] = find(b)

        token_to_first_order: dict[str, int] = {}
        hub_anchor: int | None = None
        for order_index, (token_in, token_out) in enumerate(trades):
            for token in (token_in, token_out):
                if token in token_to_first_order:
                    union(order_index, token_to_first_order[token])
                else:
                    token_to_first_order[token] = order_index
                if token in BASE_HUBS:
                    if hub_anchor is None:
                        hub_anchor = order_index
                    else:
                        union(order_index, hub_anchor)

        component_sizes = defaultdict(int)
        for order_index in range(len(trades)):
            component_sizes[find(order_index)] += 1
        n_components = len(component_sizes)
        component_count_histogram[min(n_components, 5)] += 1
        if n_components == 1:
            single_component_blocks += 1
        orders_in_largest += max(component_sizes.values())
        orders_total += len(trades)

print(f"eligible blocks (>=2 trades): {eligible_blocks}")
print(f"single-component blocks: {single_component_blocks} "
      f"({100 * single_component_blocks / eligible_blocks:.1f}%)")
print("component-count histogram (5 = >=5):")
for count in sorted(component_count_histogram):
    n = component_count_histogram[count]
    print(f"  {count}: {n} ({100 * n / eligible_blocks:.1f}%)")
print(f"orders in largest component: {100 * orders_in_largest / orders_total:.1f}% of all orders")
