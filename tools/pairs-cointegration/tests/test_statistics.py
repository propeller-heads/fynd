import numpy as np
import pandas as pd

from pairs_cointegration.models import AnalysisConfig
from pairs_cointegration.statistics import analyze_universe


def synthetic_prices() -> pd.DataFrame:
    rng = np.random.default_rng(7)
    base = 5 + np.cumsum(rng.normal(0, 0.01, 500))
    linked = 1.2 + 0.8 * base + rng.normal(0, 0.01, 500)
    independent = 3 + np.cumsum(rng.normal(0, 0.02, 500))
    return pd.DataFrame(
        {
            "ALPHA": np.exp(base),
            "BETA": np.exp(linked),
            "GAMMA": np.exp(independent),
        }
    )


def test_analyze_universe_finds_cointegrated_pair_and_applies_fdr() -> None:
    config = AnalysisConfig(min_observations=100, recommended_observations=400)

    results = analyze_universe(synthetic_prices(), config)
    by_pair = {(result.asset_a, result.asset_b): result for result in results}

    linked = by_pair[("ALPHA", "BETA")]
    assert linked.pair_p_value < 0.01
    assert linked.q_value < 0.05
    assert linked.fdr_significant
    assert linked.n_observations == 500
    assert linked.half_life_blocks is not None


def test_short_history_is_retained_but_marked_exploratory() -> None:
    config = AnalysisConfig(min_observations=60, recommended_observations=400)

    result = analyze_universe(synthetic_prices().iloc[:97, :2], config)[0]

    assert result.n_observations == 97
    assert result.sample_quality == "exploratory"
    assert not result.research_candidate
    assert "recommended minimum" in result.warning


def test_perfect_collinearity_is_reported_as_untestable() -> None:
    base = synthetic_prices()["ALPHA"]
    prices = pd.DataFrame({"ALPHA": base, "COPY": base * 2})

    result = analyze_universe(prices, AnalysisConfig(min_observations=100))[0]

    assert np.isnan(result.pair_p_value)
    assert not result.fdr_significant
    assert "collinearity" in result.warning
