# Routing Quality Evolution Loop

This is the repeatable process for using fresh LLM sessions to improve Fynd routing algorithms
against the deterministic offline quality harness.

The loop is implemented by `scripts/routing-quality-evolve.sh`. It creates a scratch git worktree,
splits the trade dataset into an exposed set and a hidden holdout set, asks a fresh agent to improve
a candidate algorithm using only the exposed set, saves that agent's work as a local commit, runs the
offline benchmark, records the result, then starts the next fresh agent with the previous exposed-set
progress and learnings available as context. After the last iteration, the orchestrator runs a final
benchmark on the hidden holdout.

## What it optimizes

The source of truth is the existing offline benchmark:

```bash
cargo run --release -p fynd-benchmark -- quality \
  --snapshot market_snapshot.json \
  --requests-file exposed_trades.json \
  --algorithms most_liquid,bellman_ford,split,agent_candidate \
  --baseline split \
  --max-hops 3 \
  --timeout-ms 5000 \
  --num-requests 1000 \
  --seed 42
```

During iteration, the candidate is evaluated only on `public/exposed_trades.json`. The final score is
computed on `private/holdout_trades.json`, which is not included in prompts or the default agent
sandbox. The candidate wins only by improving `net_amount_out` versus the baseline on the
common-success set. Coverage, wins/losses, mean bps, median bps and logs are preserved.

## Quick start

Run a campaign with the default 2500-trade hidden holdout and a reproducible 1000-trade exposed
sample per iteration:

```bash
scripts/routing-quality-evolve.sh --iterations 5
```

Run the same campaign and then also benchmark the final candidate against the original full dataset:

```bash
scripts/routing-quality-evolve.sh --iterations 5 --final-full
```

Run every iteration on all exposed trades:

```bash
scripts/routing-quality-evolve.sh --iterations 3 --num-requests 0
```

Use a larger holdout:

```bash
scripts/routing-quality-evolve.sh --iterations 5 --holdout-size 2000
```

The default agent is Codex with `gpt-5.5`, `xhigh` reasoning effort, web search enabled, and
`workspace-write` sandboxing. It can access the scratch worktree, `public/` campaign files, the
shared Cargo target dir, and Obsidian notes. It cannot access `private/` holdout files through the
default sandbox. To use another non-interactive agent, pass a command that reads the prompt from
stdin:

```bash
scripts/routing-quality-evolve.sh \
  --iterations 3 \
  --agent-command 'claude -p --model claude-opus-4-8 --effort max --permission-mode dontAsk --add-dir "$(dirname "$ITERATION_DIR")" --add-dir "$CARGO_TARGET_DIR" --add-dir /Users/markusschmitt/Documents/notes --output-format text'
```

Custom agent commands are responsible for preserving holdout isolation. Avoid commands that give the
agent full access to the campaign `private/` directory or the original full request file if you want
the final holdout to remain clean.

## Artifacts

Each campaign writes to:

```text
.agents/routing-quality/runs/<timestamp>-<pid>/
```

Important files:

| File | Purpose |
|---|---|
| `worktree/` | Scratch git worktree on a local `agent/routing-quality-*` branch. |
| `summary.md` | Compact leaderboard across iterations. |
| `public/exposed_trades.json` | Trade set visible to agents and used during iteration. |
| `public/progress.md` | Chronological log of exposed-set baseline and iteration results. |
| `public/learnings.md` | Durable notes from agents, copied into the next prompt. |
| `public/baseline.json` / `public/baseline.log` | Initial exposed-set benchmark before agent changes. |
| `public/iter-NNN/prompt.md` | Exact prompt given to that fresh LLM session. |
| `public/iter-NNN/agent.log` | Full agent transcript/output. |
| `public/iter-NNN/agent-notes.md` | Agent-written hypothesis, changes, risks and follow-ups. |
| `public/iter-NNN/commit.patch` | Patch for the local commit that saved that iteration's work. |
| `public/iter-NNN/check.log` | Compile/check output. |
| `public/iter-NNN/quality.json` / `quality.log` | Exposed-set benchmark output and logs. |
| `private/holdout_trades.json` | Hidden holdout set, not exposed to default agents. |
| `private/final-holdout.json` / `final-holdout.log` | Final hidden holdout benchmark. |
| `private/holdout_manifest.json` | Holdout split metadata and selected source indices. |

The branch and commits are local only. The script does not push, open PRs, or touch external
services.

## Candidate contract

The script asks each agent to expose one stable algorithm name, `agent_candidate` by default. That
name must be runnable by both production worker-pool dispatch and the offline harness. Agents can
change the internal implementation between iterations, but the benchmark command stays stable.

Override the name if needed:

```bash
scripts/routing-quality-evolve.sh \
  --candidate-algorithm shared_pool_split \
  --iterations 5
```

## How fresh sessions learn

Every fresh agent is told to read:

- `docs/routing-quality-handover.md`
- `docs/routing-quality-bench.md`
- the current campaign `progress.md`
- the current campaign `learnings.md`
- relevant Obsidian notes if useful: `[[Fynd Alg]]`, `[[Fynd Algorithm Competition]]`,
  `[[Fynd Algorithm Improvement Project]]`
- web research if useful

After each iteration, the script appends the agent's notes and benchmark result. The next agent gets
those files as input, so it can keep improving the same local branch without sharing a conversation
history.

## Safety and fairness

- The current working tree is not modified. Work happens in the scratch worktree.
- The scratch worktree starts from `HEAD` by default, not from uncommitted local edits. Commit the
  desired starting point first, or pass `--base-ref REF`.
- The scratch worktree gets `market_snapshot.json` and `exposed_trades.json` symlinks. It does not
  get the original full request file or the hidden holdout file.
- Generated campaign artifacts are ignored by git.
- Default Codex runs use `workspace-write` and are not given the campaign `private/` directory.
- Agents are explicitly told not to change the benchmark metric, dataset filtering, or baseline
  algorithms, and not to inspect alternate trade datasets.
- Failed compile or benchmark runs are still recorded, and the next iteration can fix them.

## Useful knobs

| Option | Use |
|---|---|
| `--iterations N` | Number of fresh LLM sessions. |
| `--num-requests N` | Per-iteration exposed-set sample size. `0` means all exposed trades. |
| `--holdout-size N` | Number of hidden holdout trades. |
| `--holdout-seed N` | Deterministic split seed. |
| `--skip-holdout` | Disable final hidden holdout benchmark. |
| `--final-full` | Also run original full-dataset benchmark after the final iteration. |
| `--snapshot FILE` | Frozen market snapshot to replay. Defaults to `market_snapshot.json`. |
| `--requests-file FILE` | Full request/trade JSON to split into exposed and holdout sets. Defaults to `aggregator_trades_10k.json`. |
| `--base-ref REF` | Start the campaign worktree from a different commit or branch. |
| `--baseline NAME` | Compare candidate against another algorithm. |
| `--base-algorithms LIST` | Include extra algorithms in each benchmark. |
| `--check-command CMD` | Change the compile/test command the loop runs after each agent. |
| `--cargo-target-dir DIR` | Reuse a shared Cargo target directory. Defaults to the main repo `target/`. |
| `--codex-model MODEL` | Default Codex model when `--agent-command` is omitted. Defaults to `gpt-5.5`. |
| `--codex-effort LEVEL` | Default Codex effort when `--agent-command` is omitted. Defaults to `xhigh`. |
| `--skip-agent` | Re-run check and benchmark plumbing without invoking an LLM. |
| `--skip-check` | Only run the benchmark after each agent. |
