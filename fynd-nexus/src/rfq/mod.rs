//! The RFQ venues the service serves, and the one place that matches on [`RfqVenue`]: which chains
//! a venue supports, which credentials it needs, and how its prices feed starts. Each venue's own
//! settings and feed are in [`bebop`], [`hashflow`] and [`metric`].

pub mod bebop;
pub mod hashflow;
pub mod metric;

use std::sync::Arc;

use anyhow::bail;
use fynd_core::nexus::rfq::RfqVenue;
use tracing::warn;
use tycho_simulation::evm::tycho_models::Chain;

use crate::{
    feed_runner::{run_feed, RESTART_BACKOFF},
    latest_stream::LatestStream,
    rfq::{
        bebop::{BebopArgs, BebopCredentials},
        hashflow::{HashflowArgs, HashflowCredentials},
        metric::{MetricArgs, MetricCredentials},
    },
};

/// The venues' credentials and endpoints.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct RfqArgs {
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
pub enum RfqCredentials {
    /// Bebop's credentials.
    Bebop(BebopCredentials),
    /// Hashflow's credentials.
    Hashflow(HashflowCredentials),
    /// Metric's credentials.
    Metric(MetricCredentials),
}

impl RfqCredentials {
    fn resolve(venue: RfqVenue, args: &RfqArgs) -> Result<Self, &'static str> {
        match venue {
            RfqVenue::Bebop => args
                .bebop
                .credentials()
                .map(Self::Bebop),
            RfqVenue::Hashflow => args
                .hashflow
                .credentials()
                .map(Self::Hashflow),
            RfqVenue::Metric => args
                .metric
                .credentials()
                .map(Self::Metric),
        }
    }

    /// The venue these credentials are for.
    pub fn venue(&self) -> RfqVenue {
        match self {
            Self::Bebop(_) => RfqVenue::Bebop,
            Self::Hashflow(_) => RfqVenue::Hashflow,
            Self::Metric(_) => RfqVenue::Metric,
        }
    }
}

/// Whether `venue` supports `chain`. A venue that does not is skipped rather than started.
pub fn supports_chain(venue: RfqVenue, chain: Chain) -> bool {
    match venue {
        RfqVenue::Bebop => bebop::supports_chain(chain),
        RfqVenue::Hashflow => hashflow::supports_chain(chain),
        RfqVenue::Metric => metric::supports_chain(chain),
    }
}

/// Reads the credentials of every venue in `venues` from `args`.
///
/// # Errors
///
/// Returns an error listing the environment variables needed for missing venue credentials.
pub fn require_credentials(
    venues: &[RfqVenue],
    args: &RfqArgs,
) -> anyhow::Result<Vec<RfqCredentials>> {
    let mut credentials = Vec::new();
    let mut missing = Vec::new();
    for &venue in venues {
        match RfqCredentials::resolve(venue, args) {
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
    credentials: &[RfqCredentials],
) -> anyhow::Result<Vec<RfqCredentials>> {
    let mut streams: Vec<RfqCredentials> = Vec::new();
    for venue_credentials in credentials {
        let venue = venue_credentials.venue();
        if streams
            .iter()
            .any(|served| served.venue() == venue)
        {
            continue;
        }
        if supports_chain(venue, chain) {
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

/// Starts the prices feed of `latest_stream` on a task, and restarts it as `run_feed` decides.
pub fn spawn_prices_feed(latest_stream: Arc<LatestStream>, credentials: RfqCredentials) {
    let chain = latest_stream.chain();
    match credentials {
        RfqCredentials::Bebop(credentials) => {
            tokio::spawn(run_feed(
                latest_stream,
                move || bebop::prices_feed(chain, &credentials),
                RESTART_BACKOFF,
            ));
        }
        RfqCredentials::Hashflow(credentials) => {
            tokio::spawn(run_feed(
                latest_stream,
                move || hashflow::prices_feed(chain, &credentials),
                RESTART_BACKOFF,
            ));
        }
        RfqCredentials::Metric(credentials) => {
            tokio::spawn(run_feed(
                latest_stream,
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

    fn args(bebop: bool, hashflow: bool, metric: bool) -> RfqArgs {
        RfqArgs {
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
    #[case::all_set(args(true, true, true), RfqVenue::ALL.to_vec(), None)]
    #[case::unlisted_venues_need_nothing(
        args(true, false, false),
        vec![RfqVenue::Bebop],
        None
    )]
    #[case::bebop(args(false, true, true), RfqVenue::ALL.to_vec(), Some("BEBOP_KEY"))]
    #[case::hashflow(
        args(true, false, true),
        RfqVenue::ALL.to_vec(),
        Some("HASHFLOW_USER and HASHFLOW_KEY")
    )]
    #[case::hashflow_key_only_missing(
        hashflow_user_only(),
        vec![RfqVenue::Hashflow],
        Some("HASHFLOW_USER and HASHFLOW_KEY")
    )]
    #[case::metric(args(true, true, false), RfqVenue::ALL.to_vec(), Some("METRIC_API_KEY"))]
    fn test_required_credentials(
        #[case] args: RfqArgs,
        #[case] venues: Vec<RfqVenue>,
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

    fn hashflow_user_only() -> RfqArgs {
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

    #[rstest]
    #[case::bebop_ethereum(RfqVenue::Bebop, Chain::Ethereum, true)]
    #[case::bebop_base(RfqVenue::Bebop, Chain::Base, true)]
    #[case::bebop_unichain(RfqVenue::Bebop, Chain::Unichain, false)]
    #[case::metric_arbitrum(RfqVenue::Metric, Chain::Arbitrum, true)]
    #[case::metric_unichain(RfqVenue::Metric, Chain::Unichain, false)]
    #[case::hashflow_anywhere(RfqVenue::Hashflow, Chain::Unichain, true)]
    fn test_venues_support_their_chains(
        #[case] venue: RfqVenue,
        #[case] chain: Chain,
        #[case] expected: bool,
    ) {
        assert_eq!(supports_chain(venue, chain), expected);
    }

    #[test]
    fn test_streams_skip_venues_that_do_not_support_the_chain_and_repeated_venues() {
        let credentials = require_credentials(
            &[RfqVenue::Hashflow, RfqVenue::Bebop, RfqVenue::Metric, RfqVenue::Hashflow],
            &args(true, true, true),
        )
        .unwrap_or_default();

        let streams = streams_to_serve(Chain::Unichain, &credentials).unwrap();

        let venues: Vec<_> = streams
            .iter()
            .map(RfqCredentials::venue)
            .collect();
        assert_eq!(venues, [RfqVenue::Hashflow]);
    }

    #[test]
    fn test_streams_fail_when_no_venue_supports_the_chain() {
        let credentials =
            require_credentials(&[RfqVenue::Bebop, RfqVenue::Metric], &args(true, true, true))
                .unwrap_or_default();

        let error = streams_to_serve(Chain::Unichain, &credentials)
            .err()
            .unwrap()
            .to_string();

        assert_eq!(error, "none of the listed venues (bebop, metric) supports unichain");
    }
}
