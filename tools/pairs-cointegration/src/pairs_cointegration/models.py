"""Typed configuration and result models."""

from dataclasses import dataclass
from pathlib import Path

MINIMUM_STATISTICAL_OBSERVATIONS = 30


@dataclass(frozen=True)
class AnalysisConfig:
    """Configuration for one universe-wide cointegration search."""

    min_observations: int = 60
    recommended_observations: int = 500
    significance_level: float = 0.05
    fdr_level: float = 0.05
    adf_autolag: str = "AIC"

    def __post_init__(self) -> None:
        """Reject configurations that cannot support meaningful test regressions."""
        if self.min_observations < MINIMUM_STATISTICAL_OBSERVATIONS:
            msg = "min_observations must be at least 30"
            raise ValueError(msg)
        if self.recommended_observations < self.min_observations:
            msg = "recommended_observations cannot be below min_observations"
            raise ValueError(msg)
        if not 0 < self.significance_level < 1 or not 0 < self.fdr_level < 1:
            msg = "significance levels must be between zero and one"
            raise ValueError(msg)


@dataclass(frozen=True)
class PairResult:
    """Statistics and decision gates for one aligned asset pair."""

    asset_a: str
    asset_b: str
    n_observations: int
    sample_quality: str
    warning: str
    alpha: float
    beta: float
    coint_stat_b_on_a: float
    coint_p_b_on_a: float
    coint_stat_a_on_b: float
    coint_p_a_on_b: float
    pair_p_value: float
    q_value: float
    asset_a_level_adf_p: float
    asset_a_diff_adf_p: float
    asset_b_level_adf_p: float
    asset_b_diff_adf_p: float
    spread_adf_p: float
    latest_z_score: float
    return_correlation: float
    half_life_blocks: float | None
    integration_gate: bool
    fdr_significant: bool
    research_candidate: bool


@dataclass(frozen=True)
class AnalysisRun:
    """Input and output provenance for one analysis invocation."""

    input_files: tuple[Path, ...]
    numeraire: str
    depth_index: int
    block_start: int
    block_end: int
    generated_at: str
