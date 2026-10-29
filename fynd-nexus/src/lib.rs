//! Holds the RFQ venue connections that Fynd pods would otherwise each open, and serves the
//! latest prices to them.
//!
//! The service serves one chain; a stream is one of its venues, and the service keeps its latest
//! message. [`rfq`] starts the
//! venue's prices feed. [`feed_runner`] keeps the feed running and publishes its prices to
//! [`latest_prices::LatestPrices`], which stores the stream's latest message. [`api`] serves that
//! message at `GET /v1/rfq/{venue}`. [`cli`] defines the command-line arguments. The
//! message format is in `fynd_core::nexus`.

pub mod api;
pub mod cli;
pub mod feed_runner;
pub mod latest_prices;
pub mod rfq;
