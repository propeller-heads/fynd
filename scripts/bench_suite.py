#!/usr/bin/env python3
"""Run a multi-config Fynd benchmark suite on EC2 and plot the results.

Reads a single TOML suite file describing N named configurations, renders a
`worker_pools.toml` and a shell `env` file per config into a timestamped run
directory, hands the directory to the single-process remote driver
(`bench-suite-remote.sh`) which provisions one EC2 box and runs each config's
audit + scale sweep sequentially, then renders all plots locally.

Secrets (TYCHO_URL/TYCHO_API_KEY/RPC_URL/BEBOP_*/HASHFLOW_*/AWS_*) come from the
environment or a local `.env`; they never live in the suite file.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tomllib
import venv
from datetime import datetime, timezone
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPTS_DIR.parent
REMOTE_DRIVER = SCRIPTS_DIR / "bench-suite-remote.sh"
VENV_DIR = REPO_ROOT / ".venv-bench"
PLOT_PYTHON = "3.13"  # pinned interpreter for the plotting venv

# Keys whose value is a worker-pool field rendered into worker_pools_<name>.toml.
POOL_INT_FIELDS = ("num_workers", "task_queue_capacity", "max_hops", "pool_timeout_ms")

# Per-config audit/scale knobs written to env/<name>.env, with shell-var names.
ENV_FIELDS = {
    "protocols": "PROTOCOLS",
    "min_tvl": "MIN_TVL",
    "http_port": "HTTP_PORT",
    "dataset": "TRADE_DATA",
    "top_pairs": "TOP_PAIRS",
    "amounts_per_pair": "AMOUNTS_PER_PAIR",
    "min_amount_usd": "MIN_AMOUNT_USD",
    "quote_timeout_ms": "QUOTE_TIMEOUT_MS",
    "concurrency": "CONCURRENCY",
    "block_stride": "BLOCK_STRIDE",
    "nordstern_rps": "NORDSTERN_RPS",
    "kyberswap_rps": "KYBERSWAP_RPS",
    "zerox_rps": "ZEROX_RPS",
    "aggregator_max_retries": "AGGREGATOR_MAX_RETRIES",
    "aggregator_retry_base_ms": "AGGREGATOR_RETRY_BASE_MS",
    "eth_call_slippage_bps": "ETH_CALL_SLIPPAGE_BPS",
    "eth_call_baseline_fee_bps": "ETH_CALL_BASELINE_FEE_BPS",
    "nordstern_url": "NORDSTERN_URL",
    "chain_id": "CHAIN_ID",
    "worker_counts": "WORKER_COUNTS",
    "num_requests": "NUM_REQUESTS",
    "mode": "MODE",
    "warmup_secs": "WARMUP_SECS",
    "health_timeout_secs": "HEALTH_TIMEOUT_SECS",
    "requests_file": "REQUESTS_FILE",
}


def load_dotenv(path: Path) -> None:
    """Populate os.environ from a .env file without overriding existing vars."""
    if not path.exists():
        return
    for raw in path.read_text().splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip()
        value = value.strip().strip('"').strip("'")
        os.environ.setdefault(key, value)


def resolve_configs(suite: dict) -> list[dict]:
    """Merge [defaults] + [scale_defaults] under each [[config]] block."""
    base = {**suite.get("defaults", {}), **suite.get("scale_defaults", {})}
    configs = []
    seen = set()
    for entry in suite.get("config", []):
        cfg = {**base, **entry}
        name = cfg.get("name")
        if not name:
            raise ValueError("every [[config]] needs a `name`")
        if name in seen:
            raise ValueError(f"duplicate config name: {name}")
        seen.add(name)
        cfg.setdefault("label", name)
        validate_config(cfg)
        configs.append(cfg)
    if not configs:
        raise ValueError("suite has no [[config]] blocks")
    return configs


def validate_config(cfg: dict) -> None:
    """Check required fields and auto-bump `mode` to >= max(worker_counts)."""
    for required in ("protocols", "algorithm", "worker_counts", "dataset"):
        if cfg.get(required) in (None, "", []):
            raise ValueError(f"config {cfg['name']!r} missing required `{required}`")
    workers = cfg["worker_counts"]
    mode = str(cfg.get("mode", f"fixed:{max(workers)}"))
    if mode.startswith("fixed:"):
        cap = int(mode.split(":", 1)[1])
        if cap < max(workers):
            print(
                f"  warn: {cfg['name']}: mode {mode} < max workers {max(workers)}; "
                f"bumping to fixed:{max(workers)}",
                file=sys.stderr,
            )
            mode = f"fixed:{max(workers)}"
    cfg["mode"] = mode


def render_worker_pool(cfg: dict) -> str:
    """Render a single-pool worker_pools.toml string from a config."""
    lines = [f"[pools.{cfg['name']}]", f'algorithm = "{cfg["algorithm"]}"']
    for key in POOL_INT_FIELDS:
        if key in cfg:
            toml_key = "timeout_ms" if key == "pool_timeout_ms" else key
            lines.append(f"{toml_key} = {int(cfg[key])}")
    tokens = cfg.get("connector_tokens")
    if tokens:
        joined = ", ".join(f'"{t}"' for t in tokens)
        lines.append(f"connector_tokens = [{joined}]")
    return "\n".join(lines) + "\n"


def render_env(cfg: dict, run_rel: str) -> str:
    """Render the shell env file consumed by bench-suite-remote.sh."""
    out = [
        f'NAME="{cfg["name"]}"',
        f'LABEL="{cfg["label"]}"',
        f'POOL_CONFIG="{run_rel}/worker_pools_{cfg["name"]}.toml"',
        f'AUDIT_OUTPUT="{run_rel}/audit_{cfg["name"]}.json"',
        f'SCALE_OUTPUT="{run_rel}/scale_{cfg["name"]}.json"',
    ]
    for key, var in ENV_FIELDS.items():
        if key not in cfg:
            continue
        value = cfg[key]
        if isinstance(value, list):
            value = ",".join(str(v) for v in value)
        out.append(f'{var}="{value}"')
    return "\n".join(out) + "\n"


def write_run_dir(
    run_dir: Path, suite_path: Path, configs: list[dict], run_rel: str
) -> None:
    """Materialise worker-pool tomls, env files, ordering, and resolved suite."""
    (run_dir / "env").mkdir(parents=True, exist_ok=True)
    names = []
    for cfg in configs:
        (run_dir / f"worker_pools_{cfg['name']}.toml").write_text(
            render_worker_pool(cfg)
        )
        (run_dir / "env" / f"{cfg['name']}.env").write_text(render_env(cfg, run_rel))
        names.append(cfg["name"])
    (run_dir / "configs.list").write_text("\n".join(names) + "\n")
    shutil.copy(suite_path, run_dir / "suite.resolved.toml")


def run_remote(
    run_rel: str, suite: dict, enabled: tuple[bool, bool], poll: int
) -> None:
    """Invoke the single-process remote driver with suite-level env."""
    remote = suite.get("remote", {})
    env = dict(os.environ)
    env.update(
        {
            "OUTPUT_DIR_REL": run_rel,
            "REGION": remote.get("region", env.get("AWS_REGION", "eu-west-1")),
            "INSTANCE_TYPE": remote.get("instance_type", "c7a.8xlarge"),
            "VOLUME_SIZE": str(remote.get("volume_size", 60)),
            "AUDIT_ENABLED": "1" if enabled[0] else "0",
            "SCALE_ENABLED": "1" if enabled[1] else "0",
            "POLL_TIMEOUT_SECS": str(poll),
        }
    )
    subprocess.run(["bash", str(REMOTE_DRIVER)], cwd=REPO_ROOT, env=env, check=True)


def ensure_venv() -> Path:
    """Create/reuse the plotting venv (pinned to PLOT_PYTHON) and return its python."""
    python = VENV_DIR / "bin" / "python"
    has_uv = bool(shutil.which("uv"))
    if python.exists() and not _venv_is_pinned(python):
        shutil.rmtree(VENV_DIR)
    if not python.exists():
        if has_uv:
            subprocess.run(
                ["uv", "venv", "--python", PLOT_PYTHON, str(VENV_DIR)], check=True
            )
        else:
            venv.EnvBuilder(with_pip=True).create(VENV_DIR)
    installer = (
        ["uv", "pip", "install", "--python", str(python)]
        if has_uv
        else [str(python), "-m", "pip", "install", "-q"]
    )
    subprocess.run([*installer, "matplotlib", "numpy"], check=True)
    return python


def _venv_is_pinned(python: Path) -> bool:
    """Return True if the existing venv interpreter matches PLOT_PYTHON (major.minor)."""
    out = subprocess.run(
        [
            str(python),
            "-c",
            "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    return out.stdout.strip() == PLOT_PYTHON


def run_plots(
    run_dir: Path, configs: list[dict], enabled: tuple[bool, bool], title: str
) -> None:
    """Render per-config audit figures and one overlaid scale figure."""
    python = ensure_venv()
    plots = run_dir / "plots"
    plots.mkdir(exist_ok=True)
    audit_on, scale_on = enabled
    for cfg in configs:
        audit_json = run_dir / f"audit_{cfg['name']}.json"
        if audit_on and audit_json.exists():
            for script in ("plot_bps_histograms.py", "plot_audit_analysis.py"):
                subprocess.run(
                    [
                        str(python),
                        str(SCRIPTS_DIR / script),
                        str(audit_json),
                        "-o",
                        str(plots),
                        "--label",
                        cfg["label"],
                    ],
                    check=True,
                )
    scale_files, labels = [], []
    for cfg in configs:
        scale_json = run_dir / f"scale_{cfg['name']}.json"
        if scale_on and scale_json.exists():
            scale_files.append(str(scale_json))
            labels.append(cfg["label"])
    if scale_files:
        subprocess.run(
            [
                str(python),
                str(SCRIPTS_DIR / "plot_scale_rps.py"),
                *scale_files,
                "--labels",
                *labels,
                "-o",
                str(plots),
                "--name",
                "scale_rps",
                "--title",
                title,
            ],
            check=True,
        )
    print(f"\nplots written to {plots}")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("suite", help="path to the TOML suite file")
    ap.add_argument("--no-audit", action="store_true", help="skip the audit stage")
    ap.add_argument("--no-scale", action="store_true", help="skip the scale stage")
    ap.add_argument(
        "--plots-only",
        action="store_true",
        help="re-plot an existing run dir; requires --run-dir",
    )
    ap.add_argument(
        "--run-dir", help="existing bench_runs/<ts> dir (with --plots-only)"
    )
    ap.add_argument(
        "--poll-timeout-secs",
        type=int,
        default=7200,
        help="per-stage remote completion timeout",
    )
    args = ap.parse_args()

    load_dotenv(REPO_ROOT / ".env")
    suite_path = Path(args.suite)
    suite = tomllib.loads(suite_path.read_text())
    configs = resolve_configs(suite)
    title = suite.get("suite", {}).get("title", "Throughput scaling")
    enabled = (not args.no_audit, not args.no_scale)

    if args.plots_only:
        if not args.run_dir:
            ap.error("--plots-only requires --run-dir")
        run_plots(Path(args.run_dir), configs, enabled, title)
        return 0

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    run_rel = f"bench_runs/{stamp}"
    run_dir = REPO_ROOT / run_rel
    write_run_dir(run_dir, suite_path, configs, run_rel)
    print(f"run dir: {run_dir}\nconfigs: {', '.join(c['name'] for c in configs)}")

    run_remote(run_rel, suite, enabled, args.poll_timeout_secs)
    run_plots(run_dir, configs, enabled, title)
    return 0


if __name__ == "__main__":
    sys.exit(main())
