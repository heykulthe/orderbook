//! Core primitive types used throughout the orderbook engine.
//!
//! Prices are represented as integer ticks to avoid floating-point drift
//! during arithmetic. Quantities remain `f64` to preserve fractional lot
//! precision without sacrificing range.

use serde::{Deserialize, Serialize};

/// Tick-denominated price. One unit equals the instrument's minimum price increment.
/// Using a fixed-width integer eliminates rounding error in price comparisons and
/// makes level indexing into ring-buffer structures trivially cheap.
pub type Price = i64;

/// Order quantity expressed in base-asset units.
/// A value of `0.0` signals that a price level has been fully consumed and
/// should be removed from the visible book.
pub type Qty = f64;

/// Which side of the central limit-order book a level or delta belongs to.
///
/// - `Bid` levels are sorted descending from the best bid.
/// - `Ask` levels are sorted ascending from the best ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Bid,
    Ask,
}
