use crate::common::types::{Price, Side};
use serde::{Deserialize, Deserializer, Serialize};

/// Identifies the category of a market-data message flowing through the feed pipeline.
/// Currently the engine handles L2 order-book update messages exclusively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MsgType {
    /// A level-2 incremental or snapshot update for a specific instrument.
    #[serde(rename = "l2update")]
    L2Update,
}

/// A single price-level change: the side, the tick-aligned price, and the new
/// resting quantity at that level. A size of 0.0 signals that the level has
/// been fully consumed and should be removed from the book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct L2Diff {
    /// Which side of the book this level belongs to.
    pub side: Side,
    /// Price expressed as an integer tick (raw_price / tick_size).
    #[serde(rename = "price")]
    pub price_tick: Price,
    /// Aggregate resting quantity at this price level after the update.
    /// Validated on deserialisation: must be finite and non-negative.
    #[serde(deserialize_with = "non_negative_f64")]
    pub size: f64,
}

/// A complete L2 book-update envelope carrying one or more price-level diffs
/// together with the metadata needed for sequencing and integrity verification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct L2UpdateMsg {
    /// Message category discriminator.
    #[serde(rename = "type")]
    pub msg_type: MsgType,
    /// Instrument identifier (e.g. "BTC-USD").
    pub symbol: String,
    /// Exchange-provided timestamp in microseconds since the Unix epoch.
    pub ts: i64,
    /// Monotonically increasing sequence number used for gap detection.
    pub seq: u64,
    /// Ordered list of price-level mutations to apply to the local book.
    #[serde(rename = "d")]
    pub diffs: Vec<L2Diff>,
    /// Rolling CRC-32 checksum over the top-of-book state for feed validation.
    pub checksum: u32,
}

/// Deserialisation guard that rejects any `f64` value that is negative or NaN.
/// Ensures quantities stored in the book are always well-defined and non-negative.
fn non_negative_f64<'de, D>(de: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let v = f64::deserialize(de)?;
    if v.is_sign_negative() {
        return Err(serde::de::Error::custom("size must be >= 0.0"));
    }
    if v.is_nan() {
        return Err(serde::de::Error::custom("size must not be NaN"));
    }
    Ok(v)
}
