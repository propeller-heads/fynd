"""Integration diagnostics and universe-wide cointegration search."""

import warnings
from dataclasses import replace
from itertools import combinations

import numpy as np
import pandas as pd
from statsmodels.stats.multitest import multipletests
from statsmodels.tools.sm_exceptions import CollinearityWarning
from statsmodels.tsa.stattools import adfuller, coint

from pairs_cointegration.models import AnalysisConfig, PairResult

MINIMUM_ADF_OBSERVATIONS = 20


def _adf_p_value(values: np.ndarray, autolag: str) -> float:
    if (
        len(values) < MINIMUM_ADF_OBSERVATIONS
        or not np.isfinite(values).all()
        or np.ptp(values) == 0
    ):
        return np.nan
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("error", RuntimeWarning)
            return float(adfuller(values, regression="c", autolag=autolag)[1])
    except (RuntimeWarning, ValueError, np.linalg.LinAlgError):
        return np.nan


def _ols(y: np.ndarray, x: np.ndarray) -> tuple[float, float, np.ndarray]:
    design = np.column_stack((np.ones(len(x)), x))
    coefficients, _, _, _ = np.linalg.lstsq(design, y, rcond=None)
    alpha, beta = float(coefficients[0]), float(coefficients[1])
    return alpha, beta, y - alpha - beta * x


def _half_life(spread: np.ndarray) -> float | None:
    lagged = spread[:-1]
    current = spread[1:]
    _, phi, _ = _ols(current, lagged)
    if not 0 < phi < 1:
        return None
    value = -np.log(2.0) / np.log(phi)
    return float(value) if np.isfinite(value) else None


def _safe_coint(y: np.ndarray, x: np.ndarray, config: AnalysisConfig) -> tuple[float, float]:
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("error", CollinearityWarning)
            warnings.simplefilter("error", RuntimeWarning)
            statistic, p_value, _ = coint(y, x, trend="c", autolag=config.adf_autolag)
    except (CollinearityWarning, RuntimeWarning, ValueError, np.linalg.LinAlgError):
        return np.nan, np.nan
    return float(statistic), float(p_value)


def _analyze_pair(
    asset_a: str,
    asset_b: str,
    prices: pd.DataFrame,
    config: AnalysisConfig,
) -> PairResult | None:
    aligned = prices[[asset_a, asset_b]].dropna()
    if len(aligned) < config.min_observations:
        return None
    log_a = np.log(aligned[asset_a].to_numpy(dtype=float))
    log_b = np.log(aligned[asset_b].to_numpy(dtype=float))
    if not np.isfinite(log_a).all() or not np.isfinite(log_b).all():
        return None
    alpha, beta, spread = _ols(log_b, log_a)
    stat_ba, p_ba = _safe_coint(log_b, log_a, config)
    stat_ab, p_ab = _safe_coint(log_a, log_b, config)
    p_values = (p_ba, p_ab)
    pair_p = max(p_values) if np.isfinite(p_values).all() else np.nan
    a_level = _adf_p_value(log_a, config.adf_autolag)
    b_level = _adf_p_value(log_b, config.adf_autolag)
    a_diff = _adf_p_value(np.diff(log_a), config.adf_autolag)
    b_diff = _adf_p_value(np.diff(log_b), config.adf_autolag)
    integration = _integration_gate(a_level, a_diff, b_level, b_diff, config)
    spread_std = float(np.std(spread, ddof=1))
    latest_z = float((spread[-1] - np.mean(spread)) / spread_std) if spread_std > 0 else np.nan
    quality = "adequate" if len(aligned) >= config.recommended_observations else "exploratory"
    warning_parts = []
    if quality != "adequate":
        warning_parts.append(
            f"Below recommended minimum of {config.recommended_observations} observations."
        )
    if not np.isfinite(pair_p):
        warning_parts.append("Cointegration test unavailable due to numerical collinearity.")
    return PairResult(
        asset_a=asset_a,
        asset_b=asset_b,
        n_observations=len(aligned),
        sample_quality=quality,
        warning=" ".join(warning_parts),
        alpha=alpha,
        beta=beta,
        coint_stat_b_on_a=stat_ba,
        coint_p_b_on_a=p_ba,
        coint_stat_a_on_b=stat_ab,
        coint_p_a_on_b=p_ab,
        pair_p_value=pair_p,
        q_value=np.nan,
        asset_a_level_adf_p=a_level,
        asset_a_diff_adf_p=a_diff,
        asset_b_level_adf_p=b_level,
        asset_b_diff_adf_p=b_diff,
        spread_adf_p=_adf_p_value(spread, config.adf_autolag),
        latest_z_score=latest_z,
        return_correlation=float(np.corrcoef(np.diff(log_a), np.diff(log_b))[0, 1]),
        half_life_blocks=_half_life(spread),
        integration_gate=integration,
        fdr_significant=False,
        research_candidate=False,
    )


def _integration_gate(
    a_level: float,
    a_diff: float,
    b_level: float,
    b_diff: float,
    config: AnalysisConfig,
) -> bool:
    values = np.array([a_level, a_diff, b_level, b_diff])
    return bool(
        np.isfinite(values).all()
        and a_level > config.significance_level
        and b_level > config.significance_level
        and a_diff < config.significance_level
        and b_diff < config.significance_level
    )


def analyze_universe(prices: pd.DataFrame, config: AnalysisConfig) -> list[PairResult]:
    """Analyze every eligible asset pair and apply universe-wide FDR correction."""
    invalid = (prices <= 0).any(axis=None)
    if invalid:
        msg = "all executable prices must be positive"
        raise ValueError(msg)
    results = [
        result
        for asset_a, asset_b in combinations(sorted(prices.columns), 2)
        if (result := _analyze_pair(asset_a, asset_b, prices, config)) is not None
    ]
    finite_indexes = [
        index for index, result in enumerate(results) if np.isfinite(result.pair_p_value)
    ]
    if not finite_indexes:
        return results
    raw = [results[index].pair_p_value for index in finite_indexes]
    rejected, adjusted, _, _ = multipletests(raw, alpha=config.fdr_level, method="fdr_bh")
    for position, index in enumerate(finite_indexes):
        result = results[index]
        candidate = bool(
            rejected[position]
            and result.integration_gate
            and result.spread_adf_p < config.significance_level
            and result.sample_quality == "adequate"
        )
        results[index] = replace(
            result,
            q_value=float(adjusted[position]),
            fdr_significant=bool(rejected[position]),
            research_candidate=candidate,
        )
    return sorted(
        results,
        key=lambda item: (
            not np.isfinite(item.q_value),
            item.q_value if np.isfinite(item.q_value) else np.inf,
            item.asset_a,
            item.asset_b,
        ),
    )
