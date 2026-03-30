//! Core domain primitives for the HFT L2 orderbook engine.
//!
//! This module surfaces the fundamental types and wire-format structures
//! used throughout the engine: numeric aliases for price and quantity,
//! the side enum, and the serialisable message types that flow over the
//! WebSocket feed.

/// Numeric type aliases and the `Side` enum.
pub mod types;

/// Wire-format message structs (`L2Diff`, `L2UpdateMsg`) and their
/// serde validation logic.
pub mod messages;

// Flatten the most-used primitives to the crate root for ergonomic imports.
pub use messages::{L2Diff, L2UpdateMsg, MsgType};
pub use types::{Price, Qty, Side};
