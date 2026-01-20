//! Scorer implementations for brute-force algorithms.
//!
//! Each scorer defines:
//! - An edge data type for storing scoring-relevant information
//! - A scoring function for prioritizing paths
//! - A method to create edge data from protocol simulations

mod most_liquid;

pub use most_liquid::{DepthAndPrice, MostLiquidScorer};
