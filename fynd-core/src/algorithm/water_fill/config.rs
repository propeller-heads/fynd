/// Maximum candidate paths simulated per order after heuristic ranking.
pub(crate) const DEFAULT_MAX_CANDIDATES: usize = 5000;
/// Cap on candidates from the bounded amount-aware discovery added to the candidate set;
/// see the discovery section below.
pub(crate) const MAX_DISCOVERY_CANDIDATES: usize = 128;
/// How many of the top-ranked paths are built and simulated to settle the single-path baseline.
/// Ranking can read amounts across two nearby ones, so the top place it reports is not always the
/// best path; the baseline is the bar splits must beat, so it is taken from exact figures.
pub(crate) const BASELINE_CANDIDATES: usize = 8;
/// Maximum number of parallel paths in a split.
pub(crate) const DEFAULT_MAX_PATHS: usize = 4;
/// Chunk grid for the coarse set-selection pass.
pub(crate) const COARSE_CHUNKS: usize = 20;
/// Chunk grid for the fine allocation pass over the fixed active set.
pub(crate) const FINE_CHUNKS: usize = 256;
/// Number of top full-amount paths always considered for shared-component fill-and-spill.
pub(crate) const SHARED_FULL_PATHS: usize = 8;
/// Number of full-amount-ranked paths probed with the first chunk for fill-and-spill.
pub(crate) const SHARED_MARGIN_PROBE_PATHS: usize = 32;
/// Number of marginal-probe winners added to the fill-and-spill candidate set.
pub(crate) const SHARED_MARGIN_PATHS: usize = 8;
/// Upper bound on fill-and-spill candidate paths.
pub(crate) const SHARED_MAX_CANDIDATES: usize = 12;
/// Candidate states retained per intermediate token during bounded discovery expansion.
pub(crate) const CANDIDATE_STATES_PER_NODE: usize = 4;
/// Candidate edge expansions from one path state during discovery.
pub(crate) const CANDIDATE_EDGES_PER_STATE: usize = 16;
/// Parallel components kept for a discovery edge directly into the target token.
pub(crate) const CANDIDATE_DIRECT_EDGES_PER_TOKEN: usize = 4;
/// Parallel components kept for a discovery edge into an anchor or configured connector token.
pub(crate) const CANDIDATE_CONNECTOR_EDGES_PER_TOKEN: usize = 2;
/// Number of highest-connectivity tokens taken as bounded-discovery anchors, derived per solve
/// from the graph (see `derive_anchor_tokens`).
pub(crate) const DERIVED_ANCHOR_COUNT: usize = 16;
/// Exchange-refinement step floor: the pass stops once `delta` falls below one fine chunk divided
/// by this factor, i.e. `amount_in / (fine_chunks * EXCHANGE_DELTA_FLOOR)`.
pub(crate) const EXCHANGE_DELTA_FLOOR: usize = 64;
/// Safety bound on trial simulations across the whole exchange-refinement pass.
pub(crate) const EXCHANGE_MAX_SIMS: usize = 400;
