# Turbine Fynd Deployment Runbook

Repeatable guide for updating the `mp/turbine-deployment` branch to latest `main` and deploying to production.

## Overview

- **Branch**: `mp/turbine-deployment` (long-lived, never PRed)
- **Namespace**: `prod-turbine`
- **ECR**: `120569639765.dkr.ecr.eu-central-1.amazonaws.com/fynd`
- **Helm release**: `fynd`
- **Deployment is fully manual** (no CD pipeline)

## Prerequisites

- AWS CLI configured with access to account `120569639765` (eu-central-1)
- `kubectl` pointing at the EKS cluster with access to `prod-turbine` namespace
- `helm` installed
- Docker running (builds for `linux/amd64`)

## Step 1: Merge main into the deployment branch

```bash
git fetch origin
git checkout mp/turbine-deployment
git merge origin/main
```

### Expected merge conflicts

These files are maintained on both branches and commonly conflict:

| File | Resolution strategy |
|---|---|
| `Cargo.toml` | Accept main's dependency versions. Keep deployment branch's `[features]` if it adds `experimental`. |
| `Cargo.lock` | After resolving `Cargo.toml`, run `cargo generate-lockfile` to regenerate. |
| `Dockerfile` | Keep deployment branch version (has cache mounts, non-root user, healthcheck). Adapt any new crates added on main. |
| `src/main.rs` | Merge carefully — main may change OTel/metrics APIs, deployment branch adds `spawn_rss_reporter` and CORS. |
| `src/cli.rs` | Keep deployment branch's `cors_allowed_origins` field. Accept main's other changes. |
| `fynd-rpc/src/builder.rs` | Accept main's refactors. Verify deployment-specific code (CORS, metrics) is preserved. |

General rule: **accept main's code changes, preserve deployment-specific additions** (CORS, metrics reporter, deployment scripts, Helm chart).

### Post-merge checks

After resolving conflicts:

```bash
# Verify it compiles
cargo check --features experimental

# Run tests
cargo nextest run

# Verify formatting
cargo +nightly fmt --all --check
```

## Step 2: Update deployment configuration

After merging, review these deployment-specific files for staleness:

### `deployment/values.yaml`

| Field | Check |
|---|---|
| `tychoUrl` | Should match the current Tycho endpoint (check main's default in `src/cli.rs`) |
| `protocols` | Should include any new protocols added on main |
| `workerPoolsConfig` | Should match `worker_pools.toml` — add any new algorithm pools (e.g., `bellman_ford_5_hops`) |
| `blacklistConfig` | Should match `blacklist.toml` — add any new blacklisted components |
| `resources` | Adjust if new features increase memory/CPU requirements |

### `Dockerfile`

| Check | Action |
|---|---|
| New crates added on main | Add `COPY <crate>/Cargo.toml <crate>/Cargo.toml` to the dependency caching stage |
| Feature flags | Verify `--features experimental` still covers what's needed |

### Commit the merge and config updates

```bash
git add -A
git commit -m "chore: merge main into deployment branch"

# If config updates are needed, make them in a separate commit:
git commit -m "chore(deployment): update values.yaml for latest main"
```

## Step 3: Build and push Docker image

```bash
./build_and_push.sh
```

This will:
1. Authenticate to ECR using default AWS credentials
2. Build for `linux/amd64`
3. Tag with the current git short SHA
4. Push to ECR

The script prints the tag and the `deploy.sh` command to run next.

To use a custom tag: `./build_and_push.sh my-custom-tag`

## Step 4: Deploy

```bash
./deploy.sh $(git rev-parse --short HEAD)
```

This runs `helm upgrade --install` with:
- ExternalSecrets enabled (pulls `RPC_URL` and `TYCHO_API_KEY` from AWS Secrets Manager `prod/turbine/fynd`)
- The image tag you just pushed

## Step 5: Monitor the rollout

### Immediate checks (first 5 minutes)

```bash
# Watch pod rollout
kubectl rollout status deployment/fynd -n prod-turbine --timeout=300s

# Check pod is running and ready
kubectl get pods -n prod-turbine -l app.kubernetes.io/instance=fynd

# Check logs for startup errors
kubectl logs -n prod-turbine -l app.kubernetes.io/instance=fynd --tail=100

# Verify health endpoint responds
kubectl port-forward -n prod-turbine svc/fynd 3000:80 &
curl -s http://localhost:3000/v1/health
kill %1
```

### What to look for in logs

- `Listening on 0.0.0.0:3000` — API server started
- `Listening on 0.0.0.0:9898` — Metrics server started
- Tycho connection established (no repeated reconnect errors)
- No panic or OOM errors

### Grafana dashboards

- [fynd-api](https://grafana.propellerheads.xyz/d/fynd-api/fynd-api) — HTTP metrics, quote processing
- [tycho-solver](https://grafana.propellerheads.xyz/d/tycho-solver/tycho-solver) — solver-level metrics

Check for:
- Request rate returning to normal after deploy
- No spike in error rates (5xx)
- Latency within acceptable range
- Memory usage stable (no leak from new features)

### Extended monitoring (first 30 minutes)

```bash
# Check resource usage
kubectl top pod -n prod-turbine -l app.kubernetes.io/instance=fynd

# Check for restarts
kubectl get pods -n prod-turbine -l app.kubernetes.io/instance=fynd -o wide
```

## Rollback

If something goes wrong:

```bash
# Option 1: Helm rollback to previous release
helm rollback fynd -n prod-turbine

# Option 2: Deploy a known-good tag
./deploy.sh <previous-known-good-tag>

# Check rollback succeeded
kubectl get pods -n prod-turbine -l app.kubernetes.io/instance=fynd
kubectl logs -n prod-turbine -l app.kubernetes.io/instance=fynd --tail=50
```

To find the previous image tag:
```bash
helm history fynd -n prod-turbine
```

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Pod in `CrashLoopBackOff` | Binary panic, bad config, missing env vars | Check logs: `kubectl logs -n prod-turbine -l app.kubernetes.io/instance=fynd --previous` |
| Pod stuck in `Pending` | Resource limits too high, node scheduling | Check events: `kubectl describe pod -n prod-turbine -l app.kubernetes.io/instance=fynd` |
| Health check failing | App not ready within `initialDelaySeconds` (120s for liveness) | Check if Tycho connection is slow, increase delay if needed |
| ExternalSecret not syncing | AWS Secrets Manager access, ESO config | `kubectl get externalsecret -n prod-turbine` and check status |
| Metrics not appearing | Port 9898 not exposed, VMPodScrape misconfigured | `kubectl port-forward <pod> 9898:9898` and `curl localhost:9898/metrics` |
| Docker build fails on Mac | Platform mismatch | Ensure `--platform linux/amd64` is used (script does this) |
| ECR auth fails | AWS session expired | Re-authenticate: `aws sso login` |

## Deployment history

Track deployments here for quick rollback reference:

| Date | Git SHA | Main merge point | Notes |
|---|---|---|---|
| _template_ | `abc1234` | `main@def5678` | what changed |
