---
allowed-tools: Bash(cargo:*), Bash(pnpm:*), Bash(npx:*), Bash(jq:*), Bash(diff:*), Bash(git diff:*), Bash(git branch:*), Bash(git status:*), Read
description: "Run the full CI pipeline locally to catch failures before pushing. Use this skill before creating a PR, before pushing commits, or whenever you want to verify that CI will pass. Also use it when the user says 'run ci', 'check ci', 'run tests', 'lint', or 'will ci pass'."
user-invocable: true
---

# Run CI Locally

Run the same checks that GitHub Actions CI runs, locally, to catch failures before they hit the
remote pipeline. The canonical checks live in `.github/workflows/ci.yaml`.

## Context

- Current branch: !`git branch --show-current`
- Working tree status: !`git status --short`

## Workflow

### Phase 1: Format (sequential)

Run formatting first because it modifies source files that all subsequent checks depend on.

```bash
cargo +nightly fmt --all
```

Check `git diff --stat -- '*.rs'` and report whether any files were reformatted.

### Phase 2: Clippy (sequential, gate for tests)

Run clippy next. If clippy fails, tests won't compile either, so there's no point running them.

```bash
cargo +nightly clippy --workspace --all-targets --all-features
```

Report pass/fail. If there are warnings or errors, list them.

**If clippy fails, stop here.** Report the errors and skip Phase 3.

### Phase 3: Parallel checks

Only run this phase if clippy passed. Launch all checks as **parallel foreground Bash calls
in a single message**. Do NOT use `run_in_background` — multiple Bash tool calls in one message
already execute concurrently.

#### Rust tests (parallel)

```bash
cargo nextest run --workspace --all-targets --all-features
```

If nextest is not installed, fall back to:
```bash
cargo test --workspace --all-targets --all-features
```

Report pass/fail with test count summary (passed, failed, ignored).

#### OpenAPI drift check (parallel)

```bash
cargo run --locked -- openapi | jq 'del(.info.version)' > /tmp/openapi_generated.json
jq 'del(.info.version)' clients/openapi.json > /tmp/openapi_committed.json
diff /tmp/openapi_committed.json /tmp/openapi_generated.json
```

Report pass/fail. If drift is detected, tell the user to run `./scripts/update-openapi.sh`.

#### TypeScript checks (parallel)

```bash
pnpm --dir clients/typescript install --frozen-lockfile && pnpm --dir clients/typescript --filter @fynd/autogen run build && pnpm --dir clients/typescript --filter @fynd/client run typecheck && pnpm --dir clients/typescript --filter @fynd/client run lint && pnpm --dir clients/typescript --filter @fynd/client run test
```

Report pass/fail. If pnpm is not available, skip and note it.

## Report

After all steps complete, provide a summary table:

| Step           | Status            | Details                              |
|----------------|-------------------|--------------------------------------|
| Format         | pass/fail         | files reformatted or clean           |
| Clippy         | pass/fail         | warning/error count                  |
| Tests          | pass/fail/skipped | X passed, Y failed, Z ignored        |
| OpenAPI drift  | pass/fail/skipped | spec matches or has drift            |
| TypeScript     | pass/fail/skipped | typecheck + lint + test results      |

If clippy failed, mark tests and subsequent checks as "skipped (clippy failed)".

If any step failed, list the specific errors below the table.
