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
    latest_prices::LatestPrices,
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
/// Returns an error naming `chain` and the listed venues when none of them supports it.
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
    if streams.is_empty() {
        let listed: Vec<String> = credentials
            .iter()
            .map(|venue_credentials| venue_credentials.venue().to_string())
            .collect();
        bail!("none of the listed venues ({}) supports {chain}", listed.join(", "));
    }
    Ok(streams)
}

/// Starts the prices feed of `latest_prices` on a task, and restarts it as `run_feed` decides.
pub fn spawn_prices_feed(latest_prices: Arc<LatestPrices>, credentials: RfqCredentials) {
    let chain = latest_prices.chain();
    match credentials {
        RfqCredentials::Bebop(credentials) => {
            tokio::spawn(run_feed(
                latest_prices,
                move || bebop::prices_feed(chain, &credentials),
                RESTART_BACKOFF,
            ));
        }
        RfqCredentials::Hashflow(credentials) => {
            tokio::spawn(run_feed(
                latest_prices,
                move || hashflow::prices_feed(chain, &credentials),
                RESTART_BACKOFF,
            ));
        }
        RfqCredentials::Metric(credentials) => {
            tokio::spawn(run_feed(
                latest_prices,
                move || metric::prices_feed(chain, &credentials),
                RESTART_BACKOFF,
            ));
        }
    }
}
