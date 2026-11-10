//! Holds the venue connections that Fynd pods would otherwise each open and serves their latest
//! prices, and monitors the health of the Tycho components of its chain.
//!
//! The service serves one chain; a stream is one of its venues or the excluded components, and the
//! service keeps its latest message. [`price_levels`] starts the venue's prices feed.
//! [`feed_runner`] keeps the feed running and publishes its prices to
//! [`latest_stream::LatestStream`], which stores the stream's latest message. [`market`] runs the
//! Tycho client and keeps the market and its derived data up to date. [`monitor`] probes every
//! component of that market on chain and publishes the broken and inaccurate ones to the excluded
//! components stream. [`api`] serves the streams at `GET /v1/price-levels/{venue}` and
//! `GET /v1/components/excluded`, and [`components_api`] serves the health of the components.
//! [`cli`] defines the command-line arguments and [`run`] starts the service from them. The
//! message format is in `fynd_core::nexus`.

pub mod api;
pub mod cli;
pub mod components_api;
pub mod feed_runner;
pub mod latest_stream;
pub mod market;
pub mod monitor;
pub mod price_levels;
pub mod service;
pub mod stream_kind;

pub use service::run;
