//! The venues the service serves, and the one place that matches on [`PriceLevelVenue`]: which
//! chains a venue supports, which credentials it needs, and how its prices feed starts. Each
//! venue's own settings and feed are in [`bebop`], [`hashflow`] and [`metric`].

pub mod bebop;
pub mod hashflow;
pub mod metric;

use std::sync::Arc;

use anyhow::bail;
use fynd_core::nexus::price_levels::PriceLevelVenue;
use tracing::warn;
use tycho_simulation::evm::tycho_models::Chain;

use crate::{
    feed_runner::{run_feed, RESTART_BACKOFF},
    latest_prices::LatestPrices,
    price_levels::{
        bebop::{BebopArgs, BebopCredentials},
        hashflow::{HashflowArgs, HashflowCredentials},
        metric::{MetricArgs, MetricCredentials},
    },
};

/// The venues' credentials and endpoints.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct VenueArgs {
    /// Bebop's settings.
    #[command(flatten)]
    pub bebop: BebopArgs,

    /// Hashflow's settings.
    #[command(flatten)]
    pub hashflow: HashflowArgs,

    /// Metric's settings.
    #[command(flatten)]
    pub metric: MetricArgs,
}

/// The credentials of one venue, checked to be complete.
#[derive(Clone)]
pub enum VenueCredentials {
    /// Bebop's credentials.
    Bebop(BebopCredentials),
    /// Hashflow's credentials.
    Hashflow(HashflowCredentials),
    /// Metric's credentials.
    Metric(MetricCredentials),
}

impl VenueCredentials {
    fn resolve(venue: PriceLevelVenue, args: &VenueArgs) -> Result<Self, &'static str> {
        match venue {
            PriceLevelVenue::Bebop => args
                .bebop
                .credentials()
                .map(Self::Bebop),
            PriceLevelVenue::Hashflow => args
                .hashflow
                .credentials()
                .map(Self::Hashflow),
            PriceLevelVenue::Metric => args
                .metric
                .credentials()
                .map(Self::Metric),
        }
    }

    /// The venue these credentials are for.
    pub fn venue(&self) -> PriceLevelVenue {
        match self {
            Self::Bebop(_) => PriceLevelVenue::Bebop,
            Self::Hashflow(_) => PriceLevelVenue::Hashflow,
            Self::Metric(_) => PriceLevelVenue::Metric,
        }
    }
}

/// Reads the credentials of every venue in `venues` from `args`.
///
/// # Errors
///
/// Returns an error listing the environment variables needed for missing venue credentials.
pub fn require_credentials(
    venues: &[PriceLevelVenue],
    args: &VenueArgs,
) -> anyhow::Result<Vec<VenueCredentials>> {
    let mut credentials = Vec::new();
    let mut missing = Vec::new();
    for &venue in venues {
        match VenueCredentials::resolve(venue, args) {
            Ok(venue_credentials) => credentials.push(venue_credentials),
            Err(variables) => missing.push(variables),
        }
    }
    if !missing.is_empty() {
        bail!("missing credentials for the listed venues: set {}", missing.join(", "));
    }
    Ok(credentials)
}

/// The streams to run on `chain`: the venues in `credentials` that support it, once each. A venue
/// that does not is logged and skipped.
///
/// # Errors
///
/// Returns an error naming `chain` and the listed venues when some are listed and none of them
/// supports it.
pub fn streams_to_serve(
    chain: Chain,
    credentials: &[VenueCredentials],
) -> anyhow::Result<Vec<VenueCredentials>> {
    let mut streams: Vec<VenueCredentials> = Vec::new();
    for venue_credentials in credentials {
        let venue = venue_credentials.venue();
        if streams
            .iter()
            .any(|served| served.venue() == venue)
        {
            continue;
        }
        if venue.supports_chain(chain) {
            streams.push(venue_credentials.clone());
        } else {
            warn!(%chain, %venue, "the venue does not support the chain; skipping");
        }
    }
    if streams.is_empty() && !credentials.is_empty() {
        let listed: Vec<String> = credentials
            .iter()
            .map(|venue_credentials| venue_credentials.venue().to_string())
            .collect();
        bail!("none of the listed venues ({}) supports {chain}", listed.join(", "));
    }
    Ok(streams)
}

/// Starts the prices feed of `latest_prices` on a task, and restarts it as `run_feed` decides.
pub fn spawn_prices_feed(latest_prices: Arc<LatestPrices>, credentials: VenueCredentials) {
    let chain = latest_prices.chain();
    match credentials {
        VenueCredentials::Bebop(credentials) => {
            tokio::spawn(run_feed(
                latest_prices,
                move || bebop::prices_feed(chain, &credentials),
                RESTART_BACKOFF,
            ));
        }
        VenueCredentials::Hashflow(credentials) => {
            tokio::spawn(run_feed(
                latest_prices,
                move || hashflow::prices_feed(chain, &credentials),
                RESTART_BACKOFF,
            ));
        }
        VenueCredentials::Metric(credentials) => {
            tokio::spawn(run_feed(
                latest_prices,
                move || metric::prices_feed(chain, &credentials),
                RESTART_BACKOFF,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn args(bebop: bool, hashflow: bool, metric: bool) -> VenueArgs {
        VenueArgs {
            bebop: BebopArgs { key: bebop.then(|| "bebop-secret".to_string()) },
            hashflow: HashflowArgs {
                user: hashflow.then(|| "user".to_string()),
                key: hashflow.then(|| "hashflow-secret".to_string()),
            },
            metric: MetricArgs {
                api_key: metric.then(|| "metric-secret".to_string()),
                api_url: None,
            },
        }
    }

    #[rstest]
    #[case::all_set(args(true, true, true), PriceLevelVenue::ALL.to_vec(), None)]
    #[case::unlisted_venues_need_nothing(
        args(true, false, false),
        vec![PriceLevelVenue::Bebop],
        None
    )]
    #[case::bebop(args(false, true, true), PriceLevelVenue::ALL.to_vec(), Some("BEBOP_KEY"))]
    #[case::hashflow(
        args(true, false, true),
        PriceLevelVenue::ALL.to_vec(),
        Some("HASHFLOW_USER and HASHFLOW_KEY")
    )]
    #[case::hashflow_key_only_missing(
        hashflow_user_only(),
        vec![PriceLevelVenue::Hashflow],
        Some("HASHFLOW_USER and HASHFLOW_KEY")
    )]
    #[case::metric(args(true, true, false), PriceLevelVenue::ALL.to_vec(), Some("METRIC_API_KEY"))]
    fn test_required_credentials(
        #[case] args: VenueArgs,
        #[case] venues: Vec<PriceLevelVenue>,
        #[case] missing: Option<&str>,
    ) {
        let result = require_credentials(&venues, &args);

        match missing {
            None => assert_eq!(result.map(|found| found.len()).ok(), Some(venues.len())),
            Some(variable) => assert!(result
                .err()
                .unwrap()
                .to_string()
                .contains(variable)),
        }
    }

    fn hashflow_user_only() -> VenueArgs {
        let mut args = args(true, true, true);
        args.hashflow.key = None;
        args
    }

    #[test]
    fn test_args_debug_secret_redaction() {
        let shown = format!("{:?}", args(true, true, true));

        for secret in ["bebop-secret", "hashflow-secret", "metric-secret"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    #[test]
    fn test_streams_skip_venues_that_do_not_support_the_chain_and_repeated_venues() {
        let credentials = require_credentials(
            &[
                PriceLevelVenue::Hashflow,
                PriceLevelVenue::Bebop,
                PriceLevelVenue::Metric,
                PriceLevelVenue::Hashflow,
            ],
            &args(true, true, true),
        )
        .unwrap_or_default();

        let streams = streams_to_serve(Chain::Base, &credentials).unwrap();

        let venues: Vec<_> = streams
            .iter()
            .map(VenueCredentials::venue)
            .collect();
        assert_eq!(venues, [PriceLevelVenue::Bebop, PriceLevelVenue::Metric]);
    }

    #[test]
    fn test_streams_fail_when_no_venue_supports_the_chain() {
        let credentials =
            require_credentials(&[PriceLevelVenue::Hashflow], &args(true, true, true))
                .unwrap_or_default();

        let error = streams_to_serve(Chain::Base, &credentials)
            .err()
            .unwrap()
            .to_string();

        assert_eq!(error, "none of the listed venues (hashflow) supports base");
    }
}
