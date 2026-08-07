# Running the live APEX/Fynd monitor on the Hetzner box

The monitor ran on a laptop and died five times in one day — three times because background jobs
are torn down with the shell that started them, once because a rebuild overwrote the running
binary, once because the tycho feed wedged. This directory is the unattended replacement: a
systemd service that survives logout, a watchdog for the wedge `Restart=always` cannot see, and a
disk guard so a runaway is visible before it starves the box's co-tenants.

Box: `agent@100.116.92.13` (`ssh -i ~/.ssh/hetzner_agent`). Ubuntu 24.04, 8 cores, 30 GB RAM,
Rust toolchain already installed. Co-tenants: an idle PostgreSQL 15 and a `tycho-rewind` project —
leave both alone, which is what `CPUQuota=600%` is for.

## Files

| File | Installed as |
|---|---|
| `apex-monitor.service` | `/etc/systemd/system/apex-monitor.service` |
| `apex-monitor-health.{service,timer}` | `/etc/systemd/system/` |
| `apex-monitor-health.sh` | `/usr/local/bin/apex-monitor-health.sh` (0755, root) |
| `apex-monitor-compact.{service,timer}` | `/etc/systemd/system/` |
| `apex-monitor-compact.sh` | `/usr/local/bin/apex-monitor-compact.sh` (0755, root) |

Data lands in `/home/agent/apex-data` — both streams in one directory, because
`live_join.py` globs `comparisons-*.jsonl` and `apex-*.jsonl` from a single path.

## First install

```bash
sudo apt-get install -y cmake                      # the only missing build dep
git clone https://github.com/propeller-heads/fynd /home/agent/apex/fynd
cd /home/agent/apex/fynd && git checkout mp/feat/apex-live-monitor
cargo build --release -p hindsight                 # 15-25 min cold

# Secrets — populated by hand, never checked in and never echoed.
sudo install -m 600 /dev/null /etc/apex-monitor.env
sudo -e /etc/apex-monitor.env                      # TYCHO_API_KEY=… and BASE_RPC_URL=…

sudo install -m 0755 -o root -g root \
    docs/analysis/2026-08-base-cow-phase0/deploy/apex-monitor-health.sh \
    docs/analysis/2026-08-base-cow-phase0/deploy/apex-monitor-compact.sh /usr/local/bin/
sudo install -m 0644 -o root -g root \
    docs/analysis/2026-08-base-cow-phase0/deploy/apex-monitor*.service \
    docs/analysis/2026-08-base-cow-phase0/deploy/apex-monitor*.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now apex-monitor apex-monitor-health.timer apex-monitor-compact.timer
```

`BASE_RPC_URL` wins over `RPC_URL` for `--chain base`, so a multi-chain environment cannot point
the run at the wrong endpoint.

## Redeploying a new commit

Never build into the `target/` of a running binary — overwriting it mid-run is what killed one of
the laptop sessions. Build first, restart second:

```bash
cd /home/agent/apex/fynd && git pull
cargo build --release -p hindsight
sudo systemctl restart apex-monitor
```

## Checking on it

```bash
systemctl is-active apex-monitor
journalctl -u apex-monitor -f
journalctl -u apex-monitor-health --since '1 day ago'   # every restart is logged, so stalls are countable

cd /home/agent/apex-data
jq -r '.window_blocks' apex-*.jsonl | sort -n | uniq -c  # all three windows present?
journalctl -u apex-monitor --since '1 hour ago' | grep -c 'APEX job shed'
uv run docs/analysis/2026-08-base-cow-phase0/live_join.py /home/agent/apex-data --window 30
```

Shedding well above zero means the solves outrun the workers: raise `--apex-workers` or drop a
window.

## Why the TVL floor is 10, not 1

A 60-block trial at `--min-tvl 1` put ~22k pools in front of the subset selector and cut 82% of
components at the 1 s search deadline — 381 orders `cluster_cut` against 7 filled. That measures
the budget, not the batching.

Raising the floor is the right lever because `--apex-max-pools` is not a liquidity cut:
`subset.rs` keeps pools by class (direct → adjacent → linking) and then by component id, so at 22k
candidates the 400 it keeps are whichever ids sort first, not the deepest pools. `--min-tvl` is
the only knob that prunes by liquidity, and it prunes before the arbitrary cap applies.

`--apex-budget-ms 1500` uses the headroom a 2 s Base block leaves; the stage solves off the block
loop's critical path, so the budget bounds a component's search, not the monitor's pacing.

## Why the watchdog watches a metric, not a file

The plan called for comparing the newest data-file mtime against now. The apex stream writes
through a `BufWriter` and only touches its file when the buffer flushes, so a healthy monitor can
leave that mtime untouched for many minutes — the check would restart a working process.
`hindsight_block_processing_seconds_count` on `:9899` increments once per re-solved block, which
is exactly the "a block was applied" signal that went missing at 07:12, so the watchdog polls that
instead. An unreachable metrics endpoint counts as a stall too, past the startup grace window.
