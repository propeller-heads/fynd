#!/usr/bin/env python3
"""Migrate batching-experiment order records from schema 1 to schema 2 in place.

Schema 1 counted a partial fill at S0, with the batcher absorbing APEX's cleared slice
(batcher fields = the slice, sell-token denominated). Schema 2 executes the user's full
size at the clearing price: the batcher supplies the buy-token remainder
Y − Y' (Y = amount_in × apex_bought / apex_sold) and receives the unsold sell remainder
amount_in − apex_sold.

The re-derived amounts can differ from a live schema-2 run by a few atoms (the recorded
raw values were floor/ceil-rounded when scaled down) and the ETH rates are reconstructed
from ratios already in the record. Neither moves any aggregate.

Usage: migrate_batching_schema2.py <dir> [...]  # each dir holds apex-orders.jsonl

Back up the files first; the rewrite is in place (via a temp file + atomic rename).
"""

import json
import sys
from pathlib import Path


def migrate_record(rec: dict) -> dict:
    if rec.get("schema", 1) >= 2:
        return rec
    rec["schema"] = 2
    if rec["status"] != "partial":
        return rec

    amount_in = int(rec["amount_in"])
    apex_sold = int(rec["apex_sold"])
    apex_bought = int(rec["apex_bought"])
    if apex_sold <= 0 or apex_bought <= 0 or amount_in <= apex_sold:
        # Degenerate partial (should not occur); leave the batcher out of it.
        rec["batcher_sold"] = "0"
        rec["batcher_bought"] = "0"
        rec["batcher_sold_eth"] = 0.0
        rec["batcher_bought_eth"] = 0.0
        return rec

    full_bought = apex_bought * amount_in // apex_sold
    supply = full_bought - apex_bought
    receive = amount_in - apex_sold
    buy_rate = rec["apex_bought_eth"] / apex_bought
    sell_rate = rec["amount_in_eth"] / amount_in if amount_in else 0.0
    rec["batcher_sold"] = str(supply)
    rec["batcher_bought"] = str(receive)
    rec["batcher_sold_eth"] = supply * buy_rate
    rec["batcher_bought_eth"] = receive * sell_rate
    return rec


def migrate_file(path: Path) -> tuple[int, int]:
    migrated = total = 0
    tmp = path.with_suffix(".jsonl.tmp")
    with path.open() as src, tmp.open("w") as dst:
        for line in src:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            total += 1
            if rec.get("schema", 1) < 2:
                migrated += 1
            dst.write(json.dumps(migrate_record(rec)) + "\n")
    tmp.rename(path)
    return migrated, total


def main() -> None:
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    for directory in sys.argv[1:]:
        path = Path(directory) / "apex-orders.jsonl"
        if not path.exists():
            print(f"{directory}: no apex-orders.jsonl, skipped")
            continue
        migrated, total = migrate_file(path)
        print(f"{directory}: {migrated}/{total} records migrated to schema 2")


if __name__ == "__main__":
    main()
