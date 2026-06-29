#!/usr/bin/env bash
set -euo pipefail

usage() {
	cat <<'EOF'
Run an LLM-driven routing algorithm improvement campaign.

The script creates a scratch git worktree, starts a fresh agent session for each
iteration, asks it to improve one candidate algorithm, saves its local commit,
runs the offline routing-quality benchmark, and writes a campaign summary.

Defaults are tuned for the split-routing benchmark:

  scripts/routing-quality-evolve.sh --iterations 5

Useful options:

  --iterations N             Number of fresh LLM sessions to run (default: 3)
  --num-requests N           Sample size from exposed trades per iteration,
                             0 = all exposed trades (default: 1000)
  --holdout-size N           Trades hidden from agents and used for final scoring
                             (default: 2500)
  --holdout-seed N           Deterministic seed for exposed/holdout split
                             (default: 424242)
  --skip-holdout             Do not run the final hidden holdout benchmark
  --final-full               Run a full 10k benchmark after the last iteration
  --candidate-algorithm NAME Stable algorithm name the agent must expose
                             (default: agent_candidate)
  --base-algorithms LIST     Comma-separated baselines (default:
                             most_liquid,bellman_ford,split)
  --baseline NAME            Benchmark baseline (default: split)
  --snapshot FILE            Frozen market snapshot (default:
                             <repo>/market_snapshot.json)
  --requests-file FILE       Full request/trade JSON to split into exposed and
                             holdout sets (default: <repo>/aggregator_trades_10k.json)
  --base-ref REF             Git ref for the scratch worktree (default: HEAD)
  --check-command CMD        Compile/test command run after each agent
                             (default: cargo check for fynd-core and benchmark)
  --agent-command CMD        Custom command that reads the prompt from stdin.
                             It runs with WORKTREE, ITERATION_DIR and ITERATION
                             in the environment. Default uses:
                             codex --model gpt-5.5 --config model_reasoning_effort="xhigh"
                             --search --ask-for-approval never exec --sandbox workspace-write
  --strategy-file FILE       JSON array of per-iteration strategy briefs. If omitted,
                             a built-in set of bold routing approaches is used.
  --strategy-offset N        Offset into the strategy rotation (default: 0)
  --initial-learnings-file FILE
                             Markdown notes to append to campaign learnings before
                             the first agent prompt.
  --codex-model MODEL        Default Codex model when --agent-command is omitted
                             (default: gpt-5.5)
  --codex-effort LEVEL       Default Codex reasoning effort when --agent-command
                             is omitted (default: xhigh; highest supported by Codex)
  --cargo-target-dir DIR     Shared Cargo target dir (default: <repo>/target)
  --skip-agent               Only run checks/benchmarks for the current worktree
  --skip-check               Skip the compile/check command

The campaign output goes to .agents/routing-quality/runs/<timestamp>-<pid>/.
EOF
}

die() {
	printf 'error: %s\n' "$*" >&2
	exit 1
}

require_command() {
	command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

abs_existing_path() {
	local path=$1
	local dir
	local base

	[[ -e "$path" ]] || die "path does not exist: $path"
	dir=$(dirname "$path")
	base=$(basename "$path")
	(
		cd "$dir"
		printf '%s/%s\n' "$(pwd -P)" "$base"
	)
}

contains_algorithm() {
	local list=$1
	local name=$2
	[[ ",${list}," == *",${name},"* ]]
}

append_unique_algorithm() {
	local list=$1
	local name=$2

	if contains_algorithm "$list" "$name"; then
		printf '%s\n' "$list"
	else
		printf '%s,%s\n' "$list" "$name"
	fi
}

iteration_strategy() {
	local iteration=$1
	local strategy_iteration=$((iteration + STRATEGY_OFFSET))

	if [[ -n "$STRATEGY_FILE" ]]; then
		jq -r --argjson index "$((strategy_iteration - 1))" '
			if (type != "array") or (length == 0) then
				error("strategy file must be a non-empty JSON array")
			else
				.[($index % length)]
			end
		' "$STRATEGY_FILE"
		return
	fi

	case $(((strategy_iteration - 1) % 10 + 1)) in
	1)
		cat <<'EOF'
Penumbra-style routing. Adapt Penumbra DEX routing ideas: value-flow propagation through a route graph, marginal price/impact curves, and route composition. Look for a principled allocation method across competing paths instead of only increasing split chunk counts. A partial but valid implementation is acceptable if it reveals whether this family can beat `split`.
EOF
		;;
	2)
		cat <<'EOF'
Convex network-flow / min-cost-flow approach. Treat pools or paths as concave output curves and approximate routing as gas-aware flow allocation over a token graph. Explore dual decomposition, successive shortest augmenting paths with marginal costs, or piecewise-linear capacities. Preserve executable route semantics.
EOF
		;;
	3)
		cat <<'EOF'
Frank-Wolfe / conditional-gradient path-flow approach. Generate a path set, optimize allocations continuously, and use line search or pairwise away steps. Focus on shared-pool cases and gas-aware objective value. Avoid merely retuning existing constants.
EOF
		;;
	4)
		cat <<'EOF'
Yen/Eppstein K-shortest-path frontier. Build a richer path frontier than the incumbent, including near-best paths not obvious by spot depth. Then select allocations across that frontier with a new criterion. The goal is to find paths current candidates never consider.
EOF
		;;
	5)
		cat <<'EOF'
Beam search / A* routing with bounds. Explore token-path construction as search with upper bounds from depth, price, and gas. Try to find better multi-hop candidates under timeout rather than optimizing allocation over the same path set.
EOF
		;;
	6)
		cat <<'EOF'
Shared-pool sequential execution optimizer. Treat route ordering as part of the optimization problem. For paths that share pools, search over execution order and fill-and-spill schedules so the reported route remains realizable under sequential pool-state updates.
EOF
		;;
	7)
		cat <<'EOF'
Local-search / simulated annealing over route portfolios. Start from the current best, then mutate path sets, split fractions, and orderings. Use deterministic seeded search and strict accept-if-better checks so improvements are reproducible and no worse than the incumbent.
EOF
		;;
	8)
		cat <<'EOF'
Protocol-aware specialization. Inspect which protocols appear in wins/losses and add targeted logic for constant-product, concentrated-liquidity, Balancer/Curve-like, or VM protocols. Try a specialized solver path rather than a uniform generic algorithm.
EOF
		;;
	9)
		cat <<'EOF'
Gas-aware route compression. Explore cases where gross output improves but gas erases the win. Optimize for net output by penalizing extra swaps earlier, merging redundant legs, or selecting route portfolios that share execution overhead where the encoder supports it.
EOF
		;;
	10)
		cat <<'EOF'
Adversarial failure mining and repair. Mine the exposed sample for trades where current candidates lose, tie suspiciously, timeout, or miss coverage. Build a targeted algorithmic repair for the largest net-output gaps rather than broad retuning.
EOF
		;;
	esac
}

run_shell_logged() {
	local log=$1
	local worktree=$2
	local command=$3

	set +e
	(
		cd "$worktree"
		CARGO_TARGET_DIR="$CARGO_TARGET_DIR" bash -lc "$command"
	) >"$log" 2>&1
	local status=$?
	set -e
	return "$status"
}

json_report_line() {
	local json=$1
	local algorithm=$2

	jq -r --arg algorithm "$algorithm" '
		.reports[]? | select(.algorithm == $algorithm) |
		[
			.coverage,
			.wins_vs_baseline,
			.losses_vs_baseline,
			(.mean_improvement_bps | tostring),
			(.median_improvement_bps | tostring)
		] | @tsv
	' "$json"
}

split_requests() {
	local source=$1
	local exposed=$2
	local holdout=$3
	local manifest=$4
	local holdout_size=$5
	local seed=$6

	local total
	total=$(jq 'length' "$source")
	[[ "$total" =~ ^[0-9]+$ ]] || die "could not read trade count from $source"
	if ((total == 0)); then
		die "request file is empty: $source"
	fi
	if ((holdout_size >= total)); then
		die "--holdout-size ($holdout_size) must be smaller than source trade count ($total)"
	fi

	# shellcheck disable=SC2016 # jq variables must remain literal until jq evaluates them.
	local shuffle_filter='
		def score($i): (($i * 1103515245 + $seed * 12345 + 1234567) % 2147483647);
		[to_entries[] | {i: .key, score: score(.key), value: .value}] | sort_by([.score, .i])
	'

	jq --argjson n "$holdout_size" --argjson seed "$seed" \
		"${shuffle_filter} | .[:\$n] | sort_by(.i) | map(.value)" \
		"$source" >"$holdout"
	jq --argjson n "$holdout_size" --argjson seed "$seed" \
		"${shuffle_filter} | .[\$n:] | sort_by(.i) | map(.value)" \
		"$source" >"$exposed"
	jq --argjson n "$holdout_size" --argjson seed "$seed" \
		"${shuffle_filter} | {
			source_total: length,
			holdout_size: \$n,
			exposed_size: (length - \$n),
			seed: \$seed,
			holdout_indices: (.[:\$n] | sort_by(.i) | map(.i))
		}" \
		"$source" >"$manifest"
}

write_prompt() {
	local prompt=$1
	local iteration=$2
	local iteration_dir=$3
	local strategy

	strategy=$(iteration_strategy "$iteration")

	cat >"$prompt" <<EOF
You are iteration ${iteration} in a fresh-session Fynd routing algorithm evolution campaign.

Repository worktree:
${WORKTREE}

Cargo target cache:
${CARGO_TARGET_DIR}

Goal:
Beat the current '${BASELINE}' routing algorithm on the deterministic offline routing-quality
benchmark, using net_amount_out as the source-of-truth metric. The outer loop will evaluate:

  cargo run --release -p fynd-benchmark -- quality \\
    --snapshot ${SNAPSHOT_IN_WORKTREE} \\
    --requests-file ${REQUESTS_IN_WORKTREE} \\
    --algorithms ${EVAL_ALGORITHMS} \\
    --baseline ${BASELINE} \\
    --max-hops ${MAX_HOPS} \\
    --timeout-ms ${TIMEOUT_MS} \\
    --num-requests ${NUM_REQUESTS} \\
    --seed ${SEED}

Use only that request file for benchmark-driven iteration. Do not inspect, download, reconstruct, or
benchmark against alternate aggregator trade datasets while working in this campaign.

Required interface:
- Implement or improve a built-in Algorithm exposed as '${CANDIDATE_ALGORITHM}'.
- Keep that external algorithm name stable across iterations.
- Register it in the offline harness and worker registry if it is not already registered.
- Do not change the benchmark metric, dataset filtering, or the meaning of net_amount_out.
- Do not remove or weaken baseline algorithms.
- Do not commit, push, open PRs, or touch external services. The outer loop saves local commits.

Context to read before coding:
- docs/routing-quality-handover.md
- docs/routing-quality-bench.md
- fynd-core/src/algorithm/split.rs
- fynd-core/src/algorithm/split_primitives.rs
- fynd-core/src/offline.rs
- Campaign progress: ${PROGRESS_FILE}
- Campaign learnings: ${LEARNINGS_FILE}

Strategy mandate for this iteration:
${strategy}

Do not just incrementally tune the previous implementation unless the strategy above genuinely
requires it. Explore this approach boldly, even if the conclusion is that it is not viable. Keep a
valid fallback so the final '${CANDIDATE_ALGORITHM}' should never intentionally regress against the
best previous candidate on the exposed benchmark.

You may use all available local and external research context. In particular, use Obsidian if
helpful (vault: /Users/markusschmitt/Documents/notes; relevant notes include [[Fynd Alg]],
[[Fynd Algorithm Competition]], and [[Fynd Algorithm Improvement Project]]), and use web search for
algorithmic routing or SOR ideas if it helps.

Work style:
- First inspect prior campaign results and logs to avoid repeating failed ideas.
- Try a concrete hypothesis that can be evaluated by the harness.
- Prefer correctness and valid on-chain route semantics over benchmark tricks.
- Keep edits scoped to routing algorithm, benchmark, or focused test code. Do not run whole-repo
  formatting; if a formatter changes unrelated files, revert that churn before finishing.
- Add or adjust focused tests when the change has meaningful logic risk.
- Run targeted checks if time allows. The outer loop will run:
  ${CHECK_COMMAND}

Before finishing, write concise notes to:
${iteration_dir}/agent-notes.md

Use this structure:

# Iteration ${iteration} Notes
## Hypothesis
## Changes
## Expected impact
## Risks
## Follow-up ideas
EOF

	printf '%s\n' "$strategy" >"$iteration_dir/strategy.md"
}

run_agent() {
	local iteration=$1
	local iteration_dir=$2
	local prompt=$3
	local log=$4

	if [[ "$SKIP_AGENT" -eq 1 ]]; then
		printf 'agent skipped\n' >"$log"
		return 0
	fi

	set +e
	if [[ -n "$AGENT_COMMAND" ]]; then
		(
			cd "$WORKTREE"
			WORKTREE="$WORKTREE" \
				ITERATION_DIR="$iteration_dir" \
				ITERATION="$iteration" \
				CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
				bash -lc "$AGENT_COMMAND" <"$prompt"
		) >"$log" 2>&1
	else
		env CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
			codex \
			--model "$CODEX_MODEL" \
			--config "model_reasoning_effort=\"$CODEX_EFFORT\"" \
			--search \
			--ask-for-approval never \
			exec \
			--sandbox workspace-write \
			-C "$WORKTREE" \
			--add-dir "$PUBLIC_DIR" \
			--add-dir "$CARGO_TARGET_DIR" \
			--add-dir "/Users/markusschmitt/Documents/notes" \
			-o "$iteration_dir/agent-final.md" \
			- <"$prompt" >"$log" 2>&1
	fi
	local status=$?
	set -e
	return "$status"
}

save_local_commit() {
	local iteration=$1
	local iteration_dir=$2
	local commit_log=$3

	git -C "$WORKTREE" status --short >"$iteration_dir/git-status-before-commit.txt"
	git -C "$WORKTREE" diff --binary >"$iteration_dir/uncommitted.patch"

	git -C "$WORKTREE" add -A -- . \
		':!market_snapshot.json' \
		':!exposed_trades.json'
	if git -C "$WORKTREE" diff --cached --quiet; then
		printf 'no code changes to commit\n' >"$commit_log"
		printf 'none\n'
		return 0
	fi

	set +e
	git -C "$WORKTREE" commit --no-verify -m "Experiment routing quality iteration ${iteration}" \
		>"$commit_log" 2>&1
	local status=$?
	set -e

	if [[ "$status" -ne 0 ]]; then
		printf 'commit failed; staged diff left in worktree\n' >>"$commit_log"
		git -C "$WORKTREE" diff --cached --binary >"$iteration_dir/staged.patch"
		printf 'commit_failed\n'
		return 0
	fi

	local sha
	sha=$(git -C "$WORKTREE" rev-parse --short HEAD)
	git -C "$WORKTREE" show --stat --patch --binary --format=fuller HEAD \
		>"$iteration_dir/commit.patch"
	printf '%s\n' "$sha"
}

run_benchmark() {
	local output_json=$1
	local log=$2
	local num_requests=$3
	local algorithms=$4
	local requests_file=$5

	set +e
	(
		cd "$WORKTREE"
		CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo run --release -p fynd-benchmark -- quality \
			--snapshot "$SNAPSHOT_IN_WORKTREE" \
			--requests-file "$requests_file" \
			--algorithms "$algorithms" \
			--baseline "$BASELINE" \
			--max-hops "$MAX_HOPS" \
			--timeout-ms "$TIMEOUT_MS" \
			--num-requests "$num_requests" \
			--seed "$SEED" \
			--output "$output_json"
	) >"$log" 2>&1
	local status=$?
	set -e
	return "$status"
}

append_benchmark_summary() {
	local markdown=$1
	local label=$2
	local output_json=$3
	local status=$4
	local log=$5

	{
		printf '\n## %s\n\n' "$label"
		printf -- "- Benchmark status: \`%s\`\n" "$status"
		printf -- "- Log: \`%s\`\n" "$log"
		printf -- "- JSON: \`%s\`\n" "$output_json"
	} >>"$markdown"

	if [[ "$status" -eq 0 && -s "$output_json" ]]; then
		local candidate_line
		local baseline_line
		candidate_line=$(json_report_line "$output_json" "$CANDIDATE_ALGORITHM" || true)
		baseline_line=$(json_report_line "$output_json" "$BASELINE" || true)

		if [[ -n "$baseline_line" ]]; then
			IFS=$'\t' read -r cov wins losses mean median <<<"$baseline_line"
			{
				printf "\nBaseline \`%s\`: coverage=%s wins=%s losses=%s mean_bps=%s median_bps=%s\n" \
					"$BASELINE" "$cov" "$wins" "$losses" "$mean" "$median"
			} >>"$markdown"
		fi

		if [[ -n "$candidate_line" ]]; then
			IFS=$'\t' read -r cov wins losses mean median <<<"$candidate_line"
			{
				printf "\nCandidate \`%s\`: coverage=%s wins=%s losses=%s mean_bps=%s median_bps=%s\n" \
					"$CANDIDATE_ALGORITHM" "$cov" "$wins" "$losses" "$mean" "$median"
			} >>"$markdown"
		else
			printf "\nCandidate \`%s\` was not present in the benchmark JSON.\n" \
				"$CANDIDATE_ALGORITHM" >>"$markdown"
		fi
	else
		printf '\nBenchmark failed or did not produce JSON.\n' >>"$markdown"
	fi
}

write_leaderboard() {
	local output=$1

	{
		printf '# Routing Quality Evolution Summary\n\n'
		printf -- "- Worktree: \`%s\`\n" "$WORKTREE"
		printf -- "- Branch: \`%s\`\n" "$BRANCH_NAME"
		printf -- "- Candidate algorithm: \`%s\`\n" "$CANDIDATE_ALGORITHM"
		printf -- "- Baseline: \`%s\`\n" "$BASELINE"
		printf -- "- Eval algorithms: \`%s\`\n" "$EVAL_ALGORITHMS"
		printf -- "- Source trades: \`%s\`\n" "$REQUESTS_TOTAL"
		printf -- "- Exposed trades: \`%s\`\n" "$EXPOSED_REQUESTS_COUNT"
		printf -- "- Hidden holdout trades: \`%s\`\n" "$HOLDOUT_SIZE"
		printf -- "- Per-iteration sample size: \`%s\` (\`0\` means all exposed trades)\n\n" \
			"$NUM_REQUESTS"
		printf '| iteration | commit | agent | check | benchmark | coverage | wins | losses | mean bps | median bps |\n'
		printf '|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|\n'

		local dir
		for dir in "$PUBLIC_DIR"/iter-*; do
			[[ -d "$dir" ]] || continue

			local meta="$dir/metadata.env"
			local iteration='?'
			local commit_sha='?'
			local agent_status='?'
			local check_status='?'
			local benchmark_status='?'
			if [[ -f "$meta" ]]; then
				# shellcheck disable=SC1090
				source "$meta"
			fi

			local cov='n/a'
			local wins='n/a'
			local losses='n/a'
			local mean='n/a'
			local median='n/a'
			local json="$dir/quality.json"
			if [[ -s "$json" ]]; then
				local line
				line=$(json_report_line "$json" "$CANDIDATE_ALGORITHM" || true)
				if [[ -n "$line" ]]; then
					IFS=$'\t' read -r cov wins losses mean median <<<"$line"
				fi
			fi

			printf "| %s | \`%s\` | %s | %s | %s | %s | %s | %s | %s | %s |\n" \
				"$iteration" \
				"$commit_sha" \
				"$agent_status" \
				"$check_status" \
				"$benchmark_status" \
				"$cov" \
				"$wins" \
				"$losses" \
				"$mean" \
				"$median"
		done

		local holdout_json="$PRIVATE_DIR/final-holdout.json"
		if [[ -s "$holdout_json" ]]; then
			printf '\n## Final Hidden Holdout\n\n'
			local candidate_line
			candidate_line=$(json_report_line "$holdout_json" "$CANDIDATE_ALGORITHM" || true)
			if [[ -n "$candidate_line" ]]; then
				IFS=$'\t' read -r cov wins losses mean median <<<"$candidate_line"
				printf -- "- Candidate \`%s\`: coverage=%s wins=%s losses=%s mean_bps=%s median_bps=%s\n" \
					"$CANDIDATE_ALGORITHM" "$cov" "$wins" "$losses" "$mean" "$median"
			else
				printf -- "- Candidate \`%s\` was not present in the holdout JSON.\n" \
					"$CANDIDATE_ALGORITHM"
			fi
			local baseline_line
			baseline_line=$(json_report_line "$holdout_json" "$BASELINE" || true)
			if [[ -n "$baseline_line" ]]; then
				IFS=$'\t' read -r cov wins losses mean median <<<"$baseline_line"
				printf -- "- Baseline \`%s\`: coverage=%s wins=%s losses=%s mean_bps=%s median_bps=%s\n" \
					"$BASELINE" "$cov" "$wins" "$losses" "$mean" "$median"
			fi
		fi

		printf '\n## Learnings\n\n'
		if [[ -s "$LEARNINGS_FILE" ]]; then
			cat "$LEARNINGS_FILE"
		else
			printf '_No learnings recorded yet._\n'
		fi
	} >"$output"
}

ROOT=$(git rev-parse --show-toplevel)
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-"$ROOT/target"}
RUNS_ROOT="$ROOT/.agents/routing-quality"
CAMPAIGN_ID="$(date -u +%Y%m%d-%H%M%S)-$$"
RUN_DIR="$RUNS_ROOT/runs/$CAMPAIGN_ID"
WORKTREE=""
BRANCH_NAME=""
BASE_REF="HEAD"
SNAPSHOT="$ROOT/market_snapshot.json"
REQUESTS="$ROOT/aggregator_trades_10k.json"
ITERATIONS=3
NUM_REQUESTS=1000
HOLDOUT_SIZE=2500
HOLDOUT_SEED=424242
MAX_HOPS=3
TIMEOUT_MS=5000
SEED=42
BASELINE="split"
BASE_ALGORITHMS="most_liquid,bellman_ford,split"
CANDIDATE_ALGORITHM="agent_candidate"
CHECK_COMMAND="cargo check -p fynd-core -p fynd-benchmark --all-targets --all-features"
AGENT_COMMAND=""
STRATEGY_FILE=""
STRATEGY_OFFSET=0
INITIAL_LEARNINGS_FILE=""
CODEX_MODEL="gpt-5.5"
CODEX_EFFORT="xhigh"
SKIP_AGENT=0
SKIP_CHECK=0
SKIP_HOLDOUT=0
FINAL_FULL=0

while [[ $# -gt 0 ]]; do
	case "$1" in
	--help | -h)
		usage
		exit 0
		;;
	--iterations)
		ITERATIONS=$2
		shift 2
		;;
	--num-requests)
		NUM_REQUESTS=$2
		shift 2
		;;
	--holdout-size)
		HOLDOUT_SIZE=$2
		shift 2
		;;
	--holdout-seed)
		HOLDOUT_SEED=$2
		shift 2
		;;
	--max-hops)
		MAX_HOPS=$2
		shift 2
		;;
	--timeout-ms)
		TIMEOUT_MS=$2
		shift 2
		;;
	--seed)
		SEED=$2
		shift 2
		;;
	--snapshot)
		SNAPSHOT=$2
		shift 2
		;;
	--requests-file)
		REQUESTS=$2
		shift 2
		;;
	--baseline)
		BASELINE=$2
		shift 2
		;;
	--base-algorithms)
		BASE_ALGORITHMS=$2
		shift 2
		;;
	--candidate-algorithm)
		CANDIDATE_ALGORITHM=$2
		shift 2
		;;
	--check-command)
		CHECK_COMMAND=$2
		shift 2
		;;
	--agent-command)
		AGENT_COMMAND=$2
		shift 2
		;;
	--strategy-file)
		STRATEGY_FILE=$(abs_existing_path "$2")
		shift 2
		;;
	--strategy-offset)
		STRATEGY_OFFSET=$2
		shift 2
		;;
	--initial-learnings-file)
		INITIAL_LEARNINGS_FILE=$(abs_existing_path "$2")
		shift 2
		;;
	--codex-model)
		CODEX_MODEL=$2
		shift 2
		;;
	--codex-effort)
		CODEX_EFFORT=$2
		shift 2
		;;
	--cargo-target-dir)
		CARGO_TARGET_DIR=$2
		shift 2
		;;
	--run-dir)
		RUN_DIR=$2
		shift 2
		;;
	--worktree)
		WORKTREE=$2
		shift 2
		;;
	--branch)
		BRANCH_NAME=$2
		shift 2
		;;
	--base-ref)
		BASE_REF=$2
		shift 2
		;;
	--skip-agent)
		SKIP_AGENT=1
		shift
		;;
	--skip-check)
		SKIP_CHECK=1
		shift
		;;
	--skip-holdout)
		SKIP_HOLDOUT=1
		shift
		;;
	--final-full)
		FINAL_FULL=1
		shift
		;;
	*)
		die "unknown argument: $1"
		;;
	esac
done

[[ "$ITERATIONS" =~ ^[0-9]+$ ]] || die "--iterations must be an integer"
[[ "$NUM_REQUESTS" =~ ^[0-9]+$ ]] || die "--num-requests must be an integer"
[[ "$HOLDOUT_SIZE" =~ ^[0-9]+$ ]] || die "--holdout-size must be an integer"
[[ "$HOLDOUT_SEED" =~ ^[0-9]+$ ]] || die "--holdout-seed must be an integer"
[[ "$MAX_HOPS" =~ ^[0-9]+$ ]] || die "--max-hops must be an integer"
[[ "$TIMEOUT_MS" =~ ^[0-9]+$ ]] || die "--timeout-ms must be an integer"
[[ "$SEED" =~ ^[0-9]+$ ]] || die "--seed must be an integer"
[[ "$STRATEGY_OFFSET" =~ ^[0-9]+$ ]] || die "--strategy-offset must be an integer"
case "$CODEX_EFFORT" in
none | minimal | low | medium | high | xhigh) ;;
*) die "--codex-effort must be one of: none, minimal, low, medium, high, xhigh" ;;
esac

require_command git
require_command jq
if [[ "$SKIP_AGENT" -eq 0 && -z "$AGENT_COMMAND" ]]; then
	require_command codex
fi

SNAPSHOT=$(abs_existing_path "$SNAPSHOT")
REQUESTS=$(abs_existing_path "$REQUESTS")
EVAL_ALGORITHMS=$(append_unique_algorithm "$BASE_ALGORITHMS" "$CANDIDATE_ALGORITHM")
contains_algorithm "$EVAL_ALGORITHMS" "$BASELINE" ||
	die "--baseline must be included in --base-algorithms or be the candidate"

mkdir -p "$RUN_DIR"
RUN_DIR=$(abs_existing_path "$RUN_DIR")
PUBLIC_DIR="$RUN_DIR/public"
PRIVATE_DIR="$RUN_DIR/private"
mkdir -p "$PUBLIC_DIR" "$PRIVATE_DIR"
WORKTREE=${WORKTREE:-"$RUN_DIR/worktree"}
BRANCH_NAME=${BRANCH_NAME:-"agent/routing-quality-$(basename "$RUN_DIR")"}

if [[ -e "$WORKTREE" ]]; then
	die "worktree already exists: $WORKTREE"
fi

git worktree add -b "$BRANCH_NAME" "$WORKTREE" "$BASE_REF" >/dev/null

SNAPSHOT_IN_WORKTREE="$WORKTREE/market_snapshot.json"
REQUESTS_IN_WORKTREE="$WORKTREE/exposed_trades.json"
EXPOSED_REQUESTS_FILE="$PUBLIC_DIR/exposed_trades.json"
HOLDOUT_REQUESTS_FILE="$PRIVATE_DIR/holdout_trades.json"
HOLDOUT_MANIFEST_FILE="$PRIVATE_DIR/holdout_manifest.json"
split_requests "$REQUESTS" \
	"$EXPOSED_REQUESTS_FILE" \
	"$HOLDOUT_REQUESTS_FILE" \
	"$HOLDOUT_MANIFEST_FILE" \
	"$HOLDOUT_SIZE" \
	"$HOLDOUT_SEED"
REQUESTS_TOTAL=$(jq -r '.source_total' "$HOLDOUT_MANIFEST_FILE")
EXPOSED_REQUESTS_COUNT=$(jq -r '.exposed_size' "$HOLDOUT_MANIFEST_FILE")
ln -s "$SNAPSHOT" "$SNAPSHOT_IN_WORKTREE"
ln -s "$EXPOSED_REQUESTS_FILE" "$REQUESTS_IN_WORKTREE"

PROGRESS_FILE="$PUBLIC_DIR/progress.md"
LEARNINGS_FILE="$PUBLIC_DIR/learnings.md"
SUMMARY_FILE="$RUN_DIR/summary.md"

cat >"$PROGRESS_FILE" <<EOF
# Routing Quality Evolution Progress

- Started: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
- Worktree: \`${WORKTREE}\`
- Branch: \`${BRANCH_NAME}\`
- Base ref: \`${BASE_REF}\`
- Baseline: \`${BASELINE}\`
- Candidate: \`${CANDIDATE_ALGORITHM}\`
- Eval algorithms: \`${EVAL_ALGORITHMS}\`
- Exposed trades: \`${EXPOSED_REQUESTS_COUNT}\`
- Hidden holdout trades: \`${HOLDOUT_SIZE}\`
- Per-iteration sample size: \`${NUM_REQUESTS}\`

EOF

cat >"$LEARNINGS_FILE" <<'EOF'
# Campaign Learnings

Append durable observations here. Keep failed hypotheses too, so later fresh sessions do not repeat
them.
EOF
if [[ -n "$INITIAL_LEARNINGS_FILE" ]]; then
	{
		printf '\n## Seeded Prior Learnings\n\n'
		cat "$INITIAL_LEARNINGS_FILE"
		printf '\n'
	} >>"$LEARNINGS_FILE"
fi

cat >"$RUN_DIR/config.env" <<EOF
ROOT=$ROOT
WORKTREE=$WORKTREE
BRANCH_NAME=$BRANCH_NAME
BASE_REF=$BASE_REF
SNAPSHOT=$SNAPSHOT
REQUESTS=$REQUESTS
EXPOSED_REQUESTS_FILE=$EXPOSED_REQUESTS_FILE
HOLDOUT_REQUESTS_FILE=$HOLDOUT_REQUESTS_FILE
HOLDOUT_MANIFEST_FILE=$HOLDOUT_MANIFEST_FILE
BASELINE=$BASELINE
BASE_ALGORITHMS=$BASE_ALGORITHMS
CANDIDATE_ALGORITHM=$CANDIDATE_ALGORITHM
EVAL_ALGORITHMS=$EVAL_ALGORITHMS
STRATEGY_FILE=$STRATEGY_FILE
STRATEGY_OFFSET=$STRATEGY_OFFSET
INITIAL_LEARNINGS_FILE=$INITIAL_LEARNINGS_FILE
CODEX_MODEL=$CODEX_MODEL
CODEX_EFFORT=$CODEX_EFFORT
REQUESTS_TOTAL=$REQUESTS_TOTAL
EXPOSED_REQUESTS_COUNT=$EXPOSED_REQUESTS_COUNT
HOLDOUT_SIZE=$HOLDOUT_SIZE
HOLDOUT_SEED=$HOLDOUT_SEED
NUM_REQUESTS=$NUM_REQUESTS
MAX_HOPS=$MAX_HOPS
TIMEOUT_MS=$TIMEOUT_MS
SEED=$SEED
CARGO_TARGET_DIR=$CARGO_TARGET_DIR
EOF

printf 'Created campaign worktree: %s\n' "$WORKTREE"
printf 'Campaign output: %s\n' "$RUN_DIR"

BASELINE_JSON="$PUBLIC_DIR/baseline.json"
BASELINE_LOG="$PUBLIC_DIR/baseline.log"
set +e
run_benchmark "$BASELINE_JSON" "$BASELINE_LOG" "$NUM_REQUESTS" "$BASE_ALGORITHMS" \
	"$REQUESTS_IN_WORKTREE"
baseline_status=$?
set -e
append_benchmark_summary "$PROGRESS_FILE" "Initial Baseline Snapshot" "$BASELINE_JSON" \
	"$baseline_status" "$BASELINE_LOG"

for ((iteration = 1; iteration <= ITERATIONS; iteration++)); do
	iteration_dir=$(printf '%s/iter-%03d' "$PUBLIC_DIR" "$iteration")
	mkdir -p "$iteration_dir"

	prompt="$iteration_dir/prompt.md"
	write_prompt "$prompt" "$iteration" "$iteration_dir"

	agent_log="$iteration_dir/agent.log"
	set +e
	run_agent "$iteration" "$iteration_dir" "$prompt" "$agent_log"
	agent_status=$?
	set -e

	commit_log="$iteration_dir/commit.log"
	commit_sha=$(save_local_commit "$iteration" "$iteration_dir" "$commit_log")

	check_status=0
	check_log="$iteration_dir/check.log"
	if [[ "$SKIP_CHECK" -eq 1 ]]; then
		printf 'check skipped\n' >"$check_log"
	else
		set +e
		run_shell_logged "$check_log" "$WORKTREE" "$CHECK_COMMAND"
		check_status=$?
		set -e
	fi

	quality_json="$iteration_dir/quality.json"
	quality_log="$iteration_dir/quality.log"
	set +e
	run_benchmark "$quality_json" "$quality_log" "$NUM_REQUESTS" "$EVAL_ALGORITHMS" \
		"$REQUESTS_IN_WORKTREE"
	benchmark_status=$?
	set -e

	cat >"$iteration_dir/metadata.env" <<EOF
iteration=$iteration
commit_sha=$commit_sha
agent_status=$agent_status
check_status=$check_status
benchmark_status=$benchmark_status
EOF

	{
		printf '\n## Iteration %03d\n\n' "$iteration"
		printf -- "- Commit: \`%s\`\n" "$commit_sha"
		printf -- "- Agent status: \`%s\` (\`%s\`)\n" "$agent_status" "$agent_log"
		printf -- "- Check status: \`%s\` (\`%s\`)\n" "$check_status" "$check_log"
		printf -- "- Benchmark status: \`%s\` (\`%s\`)\n" "$benchmark_status" "$quality_log"
		printf -- "- Notes: \`%s\`\n" "$iteration_dir/agent-notes.md"
	} >>"$PROGRESS_FILE"
	append_benchmark_summary "$PROGRESS_FILE" "Iteration ${iteration}" "$quality_json" \
		"$benchmark_status" "$quality_log"

	if [[ -s "$iteration_dir/agent-notes.md" ]]; then
		{
			printf '\n## Iteration %03d\n\n' "$iteration"
			cat "$iteration_dir/agent-notes.md"
			printf '\n'
		} >>"$LEARNINGS_FILE"
	fi

	write_leaderboard "$SUMMARY_FILE"
	printf 'Finished iteration %03d: agent=%s check=%s benchmark=%s commit=%s\n' \
		"$iteration" "$agent_status" "$check_status" "$benchmark_status" "$commit_sha"
done

if [[ "$SKIP_HOLDOUT" -eq 0 ]]; then
	holdout_json="$PRIVATE_DIR/final-holdout.json"
	holdout_log="$PRIVATE_DIR/final-holdout.log"
	set +e
	run_benchmark "$holdout_json" "$holdout_log" 0 "$EVAL_ALGORITHMS" "$HOLDOUT_REQUESTS_FILE"
	holdout_status=$?
	set -e
	append_benchmark_summary "$PROGRESS_FILE" "Final Hidden Holdout" "$holdout_json" \
		"$holdout_status" "$holdout_log"
	write_leaderboard "$SUMMARY_FILE"
	printf 'Finished final hidden holdout benchmark: status=%s\n' "$holdout_status"
fi

if [[ "$FINAL_FULL" -eq 1 ]]; then
	final_json="$RUN_DIR/final-full.json"
	final_log="$RUN_DIR/final-full.log"
	set +e
	run_benchmark "$final_json" "$final_log" 0 "$EVAL_ALGORITHMS" "$REQUESTS"
	final_status=$?
	set -e
	append_benchmark_summary "$PROGRESS_FILE" "Final Full Dataset" "$final_json" \
		"$final_status" "$final_log"
	write_leaderboard "$SUMMARY_FILE"
	printf 'Finished final full benchmark: status=%s\n' "$final_status"
fi

write_leaderboard "$SUMMARY_FILE"
printf '\nSummary: %s\n' "$SUMMARY_FILE"
printf 'Progress: %s\n' "$PROGRESS_FILE"
printf 'Worktree: %s\n' "$WORKTREE"
