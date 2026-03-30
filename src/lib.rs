//! # HFT L2 Orderbook Engine
//!
//! A high-performance Level 2 orderbook engine designed for ultra-low latency
//! market data processing. The core data structure uses a fixed-size ring buffer
//! indexed by price tick, combined with a bitset for O(1) level presence checks —
//! avoiding heap allocation and hash-map overhead on the critical path.
//!
//! ## Modules
//! - [`common`]:  Shared price/qty primitives, side enum, and wire-format message types.
//! - [`engine`]:  Synthetic market-feed simulator (`MarketFeedSimulator`, `FeedConfig`).
//! - [`optimized`]: The production orderbook implementation backed by a ring buffer and bitset.

/// Foundational types and serialisable message structures used across the engine.
pub mod common;

/// Synthetic L2 market-feed simulator driven by a discrete Brownian motion process.
/// Exposes [`engine::MarketFeedSimulator`] and [`engine::FeedConfig`].
pub mod engine;

/// Production-grade L2 orderbook: ring-buffer price levels with bitset occupancy tracking.
pub mod optimized;

// Surface the most-used primitives at the crate root for ergonomic downstream imports.
pub use common::{L2Diff, L2UpdateMsg, MsgType, Price, Qty, Side};
