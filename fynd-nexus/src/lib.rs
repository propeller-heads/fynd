//! Holds the venue connections that Fynd pods would otherwise each open, and serves the
//! latest prices to them and the derived data of its market.
//!
//! The service serves one chain; a stream is one of its venues, and the service keeps its latest
//! message. [`price_levels`] starts the venue's prices feed. [`feed_runner`] keeps the feed running
//! and publishes its prices to [`latest_prices::LatestPrices`], which stores the stream's latest
//! message. [`api`] serves that message at `GET /v1/price-levels/{venue}`. [`cli`] defines the
//! command-line arguments and [`run`] starts the service from them. The message format is in
//! `fynd_core::nexus`. [`market`] runs the Tycho client, keeps the market and its derived data up
//! to date. [`derived::writer::DerivedSnapshotWriter`] writes its derived data into
//! [`derived::snapshot::DerivedSnapshots`], which [`api`] serves at `GET /v1/derived/{kind}`.

pub mod api;
pub mod cli;
pub mod derived;
pub mod feed_runner;
pub mod latest_prices;
pub mod market;
pub mod price_levels;
pub mod service;

pub use service::run;
